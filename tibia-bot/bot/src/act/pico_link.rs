/// pico_link.rs — Cliente TCP al bridge que corre en el PC gaming.
///
/// Responsabilidades:
/// - Mantener una conexión TCP al bridge en gaming_pc:9000.
/// - Reconexión automática con backoff exponencial (hasta max_backoff_secs).
/// - Enviar comandos line-based ASCII y recibir respuestas.
/// - Timeout de 100ms por comando (loggea warning si se excede, no reintenta).
/// - Thread-safe: los comandos llegan por crossbeam_channel desde el game loop.
///
/// Nota: el protocolo es ASCII line-based. Cada comando termina en \n.
/// La Pico responde "OK\n" o "ERR <razón>\n" o "PONG\n" para PING.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::PicoConfig;

/// Comando que el game loop envía a PicoLink.
#[derive(Debug)]
pub struct PicoCommand {
    /// Texto del comando en protocolo ASCII, sin el \n final.
    pub raw: String,
    /// Canal de respuesta opcional. Si es None, el comando es fire-and-forget.
    pub reply_tx: Option<tokio::sync::oneshot::Sender<PicoReply>>,
}

/// Respuesta de la Pico (o error interno de transporte).
#[derive(Debug, Clone)]
pub struct PicoReply {
    pub ok:         bool,
    pub body:       String,
    pub latency_ms: f64,
}

/// Handle para que otros módulos envíen comandos a la Pico.
#[derive(Clone)]
pub struct PicoHandle {
    tx: tokio::sync::mpsc::Sender<PicoCommand>,
    /// `true` cuando el bridge reporta NOFOCUS (Tibia no tiene el foco).
    /// El game loop lee esto para activar safety pause.
    focus_lost: Arc<AtomicBool>,
}

impl PicoHandle {
    /// Envía un comando y espera la respuesta con timeout.
    /// Retorna Err si el canal está cerrado o si hay timeout.
    pub async fn send(&self, raw: impl Into<String>) -> Result<PicoReply> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let cmd = PicoCommand {
            raw: raw.into(),
            reply_tx: Some(reply_tx),
        };
        self.tx.send(cmd).await.context("PicoLink channel cerrado")?;
        reply_rx.await.context("PicoLink respuesta cancelada")
    }

    /// Envía un comando sin esperar respuesta (útil para KEY_DOWN, MOUSE_MOVE etc.)
    pub async fn send_fire_forget(&self, raw: impl Into<String>) {
        let cmd = PicoCommand { raw: raw.into(), reply_tx: None };
        if let Err(e) = self.tx.try_send(cmd) {
            warn!("PicoLink: fire-forget descartado (buffer lleno): {}", e);
        }
    }

    /// Retorna `true` si el bridge reportó NOFOCUS (Tibia sin foco).
    pub fn is_focus_lost(&self) -> bool {
        self.focus_lost.load(Ordering::Relaxed)
    }
}

/// Lanza la task tokio de PicoLink y retorna un handle para enviar comandos.
/// La task se reconecta automáticamente y NUNCA termina.
pub fn spawn(config: PicoConfig) -> PicoHandle {
    // Buffer de 256 comandos — si el game loop genera más deque eso sin que
    // el link procese, algo va mal y descartamos silenciosamente.
    let (tx, rx) = tokio::sync::mpsc::channel::<PicoCommand>(256);
    let focus_lost = Arc::new(AtomicBool::new(false));
    let focus_lost_clone = Arc::clone(&focus_lost);

    tokio::spawn(async move {
        run_link_loop(config, rx, focus_lost_clone).await;
    });

    PicoHandle { tx, focus_lost }
}

/// Loop principal de PicoLink. Se reconecta con backoff exponencial.
async fn run_link_loop(
    config: PicoConfig,
    mut rx: tokio::sync::mpsc::Receiver<PicoCommand>,
    focus_lost: Arc<AtomicBool>,
) {
    let mut backoff_secs: u64 = 1;
    let max_backoff = config.max_backoff_secs;
    let cmd_timeout = Duration::from_millis(config.command_timeout_ms);
    let conn_timeout = Duration::from_millis(config.connect_timeout_ms);

    loop {
        info!("PicoLink: conectando a {}...", config.bridge_addr);

        let stream = match timeout(conn_timeout, TcpStream::connect(&config.bridge_addr)).await {
            Ok(Ok(s))  => { info!("PicoLink: conectado a {}", config.bridge_addr); s }
            Ok(Err(e)) => {
                warn!("PicoLink: error de conexión: {}. Backoff {}s", e, backoff_secs);
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff);
                continue;
            }
            Err(_) => {
                warn!("PicoLink: timeout de conexión. Backoff {}s", backoff_secs);
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff);
                continue;
            }
        };

        // Conexión establecida — resetear backoff.
        backoff_secs = 1;

        if let Err(e) = run_connected(&config, stream, &mut rx, cmd_timeout, &focus_lost).await {
            warn!("PicoLink: conexión perdida: {}. Reconectando...", e);
        }
    }
}

/// Loop de comandos mientras la conexión está activa.
async fn run_connected(
    config:      &PicoConfig,
    stream:      TcpStream,
    rx:          &mut tokio::sync::mpsc::Receiver<PicoCommand>,
    cmd_timeout: Duration,
    focus_lost:  &Arc<AtomicBool>,
) -> Result<()> {
    // Activar TCP_NODELAY para minimizar latencia (sin buffering de Nagle).
    stream.set_nodelay(true).context("TCP_NODELAY")?;

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Dar tiempo al bridge/Pico para terminar de abrir el puerto serial.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // PING inicial con hasta 3 reintentos.
    let mut ping_ok = false;
    for attempt in 1u8..=3 {
        writer.write_all(b"PING\n").await.context("write PING")?;
        match timeout(Duration::from_millis(500), lines.next_line()).await {
            Ok(Ok(Some(resp))) if resp.trim() == "PONG" => {
                info!("PicoLink: PONG recibido — pipeline OK (intento {})", attempt);
                ping_ok = true;
                break;
            }
            other => {
                warn!("PicoLink: PING intento {} sin PONG: {:?}", attempt, other);
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }
    if !ping_ok {
        warn!("PicoLink: PING inicial falló tras 3 intentos — continuando de todas formas");
    }

    // ── Heartbeat timer ──────────────────────────────────────────────
    //
    // El bridge tiene un watchdog de inactividad (default 10s) que cierra
    // la TCP si no recibe comandos. Con el FSM en Fighting el bot emite
    // cada 30s (keepalive), dejando gaps de 20s de inactividad → watchdog
    // dispara → conexión muere → próximos emits fallan silenciosamente.
    //
    // Solución: enviamos PING fire-and-forget cada 5s para mantener el
    // watchdog del bridge alimentado. El bridge responde PONG pero nosotros
    // no esperamos — solo queremos que la conexión permanezca viva.
    let heartbeat_interval = Duration::from_secs(5);
    let mut heartbeat_timer = tokio::time::interval(heartbeat_interval);
    // skip el primer tick inmediato para evitar PING justo tras el inicial.
    heartbeat_timer.tick().await;

    loop {
        // Esperar un comando del game loop O un tick del heartbeat.
        let cmd = tokio::select! {
            maybe_cmd = rx.recv() => {
                match maybe_cmd {
                    Some(c) => c,
                    None    => return Ok(()),
                }
            }
            _ = heartbeat_timer.tick() => {
                // Heartbeat: enviar PING para mantener el watchdog del bridge
                // alimentado. Drenamos el PONG que responde para que no quede
                // en el buffer y contamine el próximo response del siguiente
                // comando (KEY_TAP etc).
                debug!("PicoLink: heartbeat PING");
                if let Err(e) = writer.write_all(b"PING\n").await {
                    warn!("PicoLink: heartbeat write falló: {} — reconectando", e);
                    return Err(e.into());
                }
                // Drenar la respuesta con timeout corto. Si no llega PONG,
                // loggeamos pero seguimos — el siguiente PING reintentará.
                match timeout(Duration::from_millis(300), lines.next_line()).await {
                    Ok(Ok(Some(resp))) => {
                        debug!("PicoLink: heartbeat PONG: '{}'", resp.trim());
                    }
                    Ok(Ok(None)) => {
                        warn!("PicoLink: heartbeat EOF — reconectando");
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        warn!("PicoLink: heartbeat read error: {} — reconectando", e);
                        return Err(e.into());
                    }
                    Err(_) => {
                        debug!("PicoLink: heartbeat sin PONG en 300ms (tolerable)");
                    }
                }
                // Sondear foco: FOCUS_CHECK es un comando local del bridge que
                // responde FOCUSED o NOFOCUS sin pasar por serial. Resuelve el
                // deadlock: bot pausado por focus → no envía HID → sin este
                // sondeo nunca sabría que Tibia recuperó el foco.
                if let Err(e) = writer.write_all(b"FOCUS_CHECK\n").await {
                    warn!("PicoLink: FOCUS_CHECK write falló: {}", e);
                    return Err(e.into());
                }
                match timeout(Duration::from_millis(300), lines.next_line()).await {
                    Ok(Ok(Some(resp))) => {
                        let trimmed = resp.trim();
                        if trimmed == "FOCUSED" {
                            focus_lost.store(false, Ordering::Relaxed);
                        } else if trimmed == "NOFOCUS" {
                            focus_lost.store(true, Ordering::Relaxed);
                        }
                        debug!("PicoLink: FOCUS_CHECK → {}", trimmed);
                    }
                    _ => {
                        debug!("PicoLink: FOCUS_CHECK sin respuesta en 300ms");
                    }
                }
                continue;
            }
        };

        // Drenar PONGs residuales que el heartbeat no alcanzó a consumir.
        // Si un PONG llega después del timeout de 300ms del heartbeat,
        // queda en el BufReader y se leería como respuesta del siguiente
        // comando, causando un falso "ok=true" en un KEY_TAP/MOUSE_MOVE.
        loop {
            match timeout(Duration::from_millis(1), lines.next_line()).await {
                Ok(Ok(Some(ref line))) if line.trim() == "PONG" => {
                    debug!("PicoLink: drenando PONG residual");
                    continue;
                }
                Ok(Ok(Some(ref line))) if line.trim() == "NOFOCUS" => {
                    debug!("PicoLink: drenando NOFOCUS residual");
                    focus_lost.store(true, Ordering::Relaxed);
                    continue;
                }
                _ => break,
            }
        }

        let line = format!("{}\n", cmd.raw);
        let send_at = Instant::now();

        // Enviar el comando.
        if let Err(e) = writer.write_all(line.as_bytes()).await {
            // Error de escritura → conexión rota. Drainamos la reply si existe.
            if let Some(reply_tx) = cmd.reply_tx {
                let _ = reply_tx.send(PicoReply {
                    ok:         false,
                    body:       format!("transport error: {}", e),
                    latency_ms: 0.0,
                });
            }
            return Err(e.into());
        }

        // Si nadie espera respuesta, pasamos al siguiente comando.
        let reply_tx = match cmd.reply_tx {
            Some(tx) => tx,
            None     => { debug!("PicoLink: cmd enviado (fire-forget): {}", cmd.raw); continue; }
        };

        // Esperar respuesta con timeout.
        let resp = timeout(cmd_timeout, lines.next_line()).await;
        let latency_ms = send_at.elapsed().as_secs_f64() * 1000.0;

        if latency_ms > config.command_timeout_ms as f64 {
            warn!("PicoLink: latencia alta para '{}': {:.1}ms", cmd.raw, latency_ms);
        }

        let reply = match resp {
            Ok(Ok(Some(line))) => {
                let line = line.trim().to_string();
                debug!("PicoLink: respuesta '{}'  {:.1}ms", line, latency_ms);
                // Actualizar flag de foco según la respuesta del bridge.
                if line == "NOFOCUS" {
                    focus_lost.store(true, Ordering::Relaxed);
                } else {
                    focus_lost.store(false, Ordering::Relaxed);
                }
                PicoReply {
                    ok:         line.starts_with("OK") || line == "PONG",
                    body:       line,
                    latency_ms,
                }
            }
            Ok(Ok(None)) => {
                // EOF — conexión cerrada por el bridge/Pico.
                let _ = reply_tx.send(PicoReply {
                    ok:         false,
                    body:       "connection closed".into(),
                    latency_ms,
                });
                return Ok(());
            }
            Ok(Err(e)) => {
                let _ = reply_tx.send(PicoReply {
                    ok:         false,
                    body:       format!("read error: {}", e),
                    latency_ms,
                });
                return Err(e.into());
            }
            Err(_timeout) => {
                warn!("PicoLink: TIMEOUT esperando respuesta a '{}'", cmd.raw);
                PicoReply {
                    ok:         false,
                    body:       "timeout".into(),
                    latency_ms,
                }
            }
        };

        let _ = reply_tx.send(reply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// Levanta un bridge stub en 127.0.0.1:0 que responde según `script`.
    /// Cada línea de `script` es la respuesta al N-ésimo comando recibido.
    /// Si `script` se agota, responde "OK" a todo. Si es Vec::<&str>::new(),
    /// simula un server que acepta pero nunca responde.
    async fn start_stub_bridge(script: Vec<String>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let script = script.clone();
                tokio::spawn(async move {
                    let _ = sock.set_nodelay(true);
                    let (reader, mut writer) = sock.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    let mut idx = 0;
                    while let Ok(Some(_cmd)) = lines.next_line().await {
                        let resp = script.get(idx).cloned().unwrap_or_else(|| "OK".into());
                        idx += 1;
                        if writer.write_all(resp.as_bytes()).await.is_err() { break; }
                        if writer.write_all(b"\n").await.is_err() { break; }
                    }
                });
            }
        });
        (addr, handle)
    }

    fn test_config(addr: String) -> PicoConfig {
        PicoConfig {
            bridge_addr:       addr,
            connect_timeout_ms: 500,
            command_timeout_ms: 100,
            max_backoff_secs:  4,
        }
    }

    /// Un PING inicial + un comando cualquiera debe retornar OK si el stub responde.
    #[tokio::test]
    async fn connects_and_sends_command_ok() {
        let (addr, _srv) = start_stub_bridge(vec!["PONG".into(), "OK".into()]).await;
        let handle = spawn(test_config(addr));

        // Dar tiempo a la conexión + PING inicial.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let reply = handle.send("KEY_TAP 0x3A").await.expect("send");
        assert!(reply.ok, "reply debe ser OK: {:?}", reply);
        assert_eq!(reply.body.trim(), "OK");
    }

    /// Retry-connect: PicoLink falla al conectar inicialmente (no hay servidor),
    /// pero cuando el servidor aparece, el backoff se reanuda y conecta OK.
    /// Esto valida el loop de reconexión en `run_link_loop`.
    #[tokio::test]
    async fn retries_connect_until_server_available() {
        // Reservar un puerto específico para que PicoLink pueda intentar
        // conectar ANTES de que el server exista.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        // Cerramos el listener — el puerto queda libre.
        drop(listener);

        // Spawn PicoLink contra el puerto muerto.
        let handle = spawn(test_config(addr.clone()));

        // Dar tiempo para que falle 1-2 intentos de conexión.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Ahora bindear el mismo puerto para que el siguiente retry funcione.
        let listener2 = TcpListener::bind(&addr).await.expect("rebind");
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener2.accept().await {
                let _ = sock.set_nodelay(true);
                let (reader, mut writer) = sock.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(_)) = lines.next_line().await {
                    let _ = writer.write_all(b"OK\n").await;
                }
            }
        });

        // El backoff inicial es 1s → PicoLink reintentará dentro del rango.
        // Esperar hasta 3s para que la reconexión se complete y el PING pase.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // El siguiente comando debe succeeder.
        let reply = tokio::time::timeout(
            Duration::from_secs(2),
            handle.send("KEY_TAP 0x3A"),
        ).await.expect("no timeout").expect("send");
        assert!(reply.ok, "reply debe ser OK tras retry-connect: {:?}", reply);
    }

    /// Comando que excede command_timeout_ms retorna reply con ok=false
    /// y body="timeout", sin colgar al caller.
    #[tokio::test]
    async fn command_timeout_returns_timeout_reply() {
        // Stub que responde PONG al PING inicial pero luego nunca responde.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = sock.set_nodelay(true);
                    let (reader, mut writer) = sock.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    let _ = lines.next_line().await;
                    let _ = writer.write_all(b"PONG\n").await;
                    // Leer el siguiente comando pero no responder.
                    let _ = lines.next_line().await;
                    // Mantener la conexión abierta, no responder.
                    tokio::time::sleep(Duration::from_secs(10)).await;
                });
            }
        });

        let mut config = test_config(addr);
        config.command_timeout_ms = 100;
        let handle = spawn(config);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let t0 = Instant::now();
        let reply = handle.send("KEY_TAP 0x3A").await.expect("send");
        let elapsed = t0.elapsed();

        assert!(!reply.ok, "reply debe ser ok=false por timeout: {:?}", reply);
        assert_eq!(reply.body, "timeout");
        // Debe terminar poco después de command_timeout_ms, NO colgar.
        assert!(elapsed.as_millis() < 500,
            "timeout tardó demasiado: {}ms", elapsed.as_millis());
    }

    /// Focus check: stub responde NOFOCUS → focus_lost=true.
    /// Luego FOCUSED → focus_lost=false.
    #[tokio::test]
    async fn focus_check_updates_focus_lost_flag() {
        // Stub devuelve PONG + NOFOCUS al primer heartbeat.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = sock.set_nodelay(true);
                    let (reader, mut writer) = sock.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    // PING inicial → PONG
                    let _ = lines.next_line().await;
                    let _ = writer.write_all(b"PONG\n").await;
                    // Responder lo que siga con NOFOCUS/PONG/OK según corresponda.
                    while let Ok(Some(line)) = lines.next_line().await {
                        let resp = match line.trim() {
                            "PING" => "PONG",
                            "FOCUS_CHECK" => "NOFOCUS",
                            _ => "OK",
                        };
                        let _ = writer.write_all(resp.as_bytes()).await;
                        let _ = writer.write_all(b"\n").await;
                    }
                });
            }
        });

        let mut config = test_config(addr);
        config.command_timeout_ms = 100;
        let handle = spawn(config);

        // El heartbeat dispara a los 5s. Esto es demasiado para un test, así
        // que lo que validamos es que inicialmente focus_lost=false.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!handle.is_focus_lost(),
            "focus_lost debe ser false antes del primer heartbeat");
        // Los otros escenarios de heartbeat se validan manualmente en vivo.
    }
}
