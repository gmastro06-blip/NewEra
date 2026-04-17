/// pico_bridge — Puente TCP ↔ Serial para la Raspberry Pi Pico 2.
///
/// Comportamiento:
/// - Carga bridge_config.toml del directorio de trabajo.
/// - Abre el puerto serial configurado. Si falla, reintenta cada 2s.
/// - Escucha en TCP. Acepta UN cliente a la vez.
///   Si llega un segundo, cierra el anterior y acepta el nuevo.
/// - Proxy bidireccional entre el socket TCP y el puerto serial.
/// - Si el serial muere: cierra el cliente TCP y vuelve a abrir el serial.
/// - Si el cliente se desconecta: vuelve a aceptar, el serial sigue abierto.
/// - **Focus gate**: si está habilitado, verifica que Tibia tenga el foco.
///   Cuando Tibia pierde el foco, responde NOFOCUS a los comandos HID
///   (PING y RESET siempre pasan).
/// - Log solo a stderr. Sin GUI, sin tray. Ctrl+C para salir.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use serialport::SerialPort;
use tracing::{debug, error, info, warn};

mod sendinput;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    serial: Option<SerialConfig>,
    tcp:    TcpConfig,
    #[serde(default)]
    watchdog: WatchdogConfig,
    #[serde(default)]
    focus: FocusConfig,
    #[serde(default)]
    input: InputConfig,
}

/// Modo de inyección de input.
#[derive(Debug, Deserialize, Clone, PartialEq)]
struct InputConfig {
    /// "serial" = Pico2 por serial (default), "sendinput" = Windows SendInput API.
    #[serde(default = "default_input_mode")]
    mode: String,
}

impl Default for InputConfig {
    fn default() -> Self { Self { mode: default_input_mode() } }
}

/// Default input mode: `serial` = Arduino HID (indistinguible de hardware,
/// inmune a BattleEye). `sendinput` usa la Windows API y es detectable por
/// anti-cheats que monitorean SetWindowsHookEx / SendInput calls.
///
/// ADR-001 recomienda serial como default en todos los setups live.
fn default_input_mode() -> String { "serial".to_string() }

#[derive(Debug, Deserialize)]
struct SerialConfig {
    port: String,
    baud: u32,
}

#[derive(Debug, Deserialize)]
struct TcpConfig {
    listen_addr: String,
}

/// Watchdog de inactividad: si no llega ningún comando del bot en
/// `idle_timeout_secs` segundos, cerramos la conexión TCP. Esto previene
/// "clicks fantasma" si el bot crashea o se cuelga mid-action: al perder
/// la conexión, el cliente del Pico vuelve a un estado seguro (sin teclas
/// presionadas tras el siguiente RESET).
///
/// `0` desactiva el watchdog (comportamiento pre-Fase 5).
#[derive(Debug, Deserialize)]
struct WatchdogConfig {
    #[serde(default = "default_idle_timeout")]
    idle_timeout_secs: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self { Self { idle_timeout_secs: default_idle_timeout() } }
}

fn default_idle_timeout() -> u64 { 10 } // 10s por default (muy conservador)

/// Detección de foco de ventana: verifica periódicamente si Tibia es la ventana
/// activa. Cuando no lo es, bloquea comandos HID para evitar que keystrokes y
/// clicks vayan a la ventana equivocada.
#[derive(Debug, Deserialize)]
struct FocusConfig {
    /// Habilitar detección de foco. Si `false`, todos los comandos pasan (legacy).
    #[serde(default)]
    enabled: bool,
    /// Substring a buscar en el título de la ventana activa.
    /// Tibia usa "Tibia - NombrePersonaje" como título.
    #[serde(default = "default_window_title")]
    window_title_contains: String,
    /// Intervalo de polling en milisegundos.
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u64,
    /// Polls consecutivos "sin foco" antes de declarar pérdida.
    /// Default 5 → 5 × 100ms = 500ms de debounce. Evita pausar por popups.
    #[serde(default = "default_debounce_count")]
    debounce_count: u32,
}

impl Default for FocusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_title_contains: default_window_title(),
            poll_interval_ms: default_poll_interval(),
            debounce_count: default_debounce_count(),
        }
    }
}

fn default_window_title() -> String { "Tibia".to_string() }
/// Intervalo default de polling para focus detection, en ms.
///
/// **Anti-detection tuning 2026-04-17**: subido de 100ms a 2000ms. Un
/// proceso llamando GetForegroundWindow + GetWindowTextW cada 100ms es un
/// fingerprint textbook de software de automatización (anti-cheats como
/// BattleEye flagean este patrón). A 2s el intervalo es indistinguible
/// de cualquier monitoring utility comercial.
///
/// Trade-off: si el usuario alt-tab por <2s, el bot puede ejecutar 1-2
/// acciones con foco perdido antes de pausar. Aceptable — un SafetyPause
/// + manual resume cubre el caso.
///
/// Para detección más precisa sin polling, ver `focus_watcher_event_driven`
/// (WinEvents hook) abajo — fires solo en cambios reales de foco.
fn default_poll_interval() -> u64 { 2000 }
/// Polls consecutivos sin foco para declarar foco perdido. Con default
/// poll_interval=2000ms y debounce=2 → 4s de tolerancia. Valor anterior
/// (debounce=5 @ 100ms = 500ms) era demasiado reactivo y también un
/// fingerprint de high-frequency polling.
fn default_debounce_count() -> u32 { 2 }

fn load_config(path: &str) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!(
            "No se encontró {path}.\n\
             Copia bridge_config.toml.example a bridge_config.toml y edita el puerto COM."
        ))?;
    toml::from_str(&raw).context("bridge_config.toml inválido")
}

// ── Focus detection ──────────────────────────────────────────────────────────

#[cfg(windows)]
fn check_tibia_focused(title_pattern: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len == 0 {
            return false;
        }
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        title.contains(title_pattern)
    }
}

#[cfg(not(windows))]
fn check_tibia_focused(_title_pattern: &str) -> bool {
    true // Non-Windows: siempre asume foco (no-op)
}

// ── Geometry query (WinAPI) ──────────────────────────────────────────────────
//
// Responde con las coords del virtual screen + la posición actual de la
// ventana de Tibia. Formato ASCII línea:
//
//   "GEOMETRY <vscreen_x> <vscreen_y> <vscreen_w> <vscreen_h> \
//             <tibia_x> <tibia_y> <tibia_w> <tibia_h>\n"
//
// Donde vscreen_x/y pueden ser NEGATIVOS (si hay monitores a la izquierda/
// arriba del primario). El bot usa estos valores para auto-configurar el
// mapeo viewport → HID absoluto sin necesidad de calibración manual.
//
// Si Tibia no se encuentra (ventana cerrada, título no matches), retorna:
//   "GEOMETRY <vx> <vy> <vw> <vh> ERR window_not_found\n"

/// Query de geometría que el bot usa para auto-configurar sus Coords.
///
/// `hid_is_primary_only`: cuando true, reporta vscreen = primary monitor dims.
/// Esto es necesario en modo serial (Arduino HID) porque el descriptor del
/// AbsoluteMouse targetea SOLO el primary monitor (rango 0..primary_w, 0..primary_h)
/// — no el virtual desktop completo. Si el bot usa el vscreen real (multi-monitor),
/// calcula HID coords contra una extensión mayor y el cursor cae escalado a la
/// mitad (o cuadrante, si también mismatchea Y).
///
/// Fix 2026-04-16 V7: sin este flag, con Tibia en primary y mode=serial, clicks
/// del bot caían en primary_w/2 de la coord esperada, haciendo inalcanzable la
/// mitad izquierda del monitor.
/// Cache TTL para el resultado de `query_geometry`. La ventana de Tibia
/// no se mueve frecuentemente; cachear 10s reduce drásticamente el
/// footprint de `EnumWindows` + `GetWindowTextW` en el process scan de
/// BattleEye.
///
/// Anti-detection 2026-04-17: sin cache cada HTTP GEOMETRY del bot
/// enumeraba todas las ventanas top-level del sistema + leía su título.
/// Un anti-cheat que trackea "procesos que enumeran ventanas buscando
/// strings específicos" flagearía este patrón rápidamente.
#[cfg(windows)]
const GEOMETRY_CACHE_TTL: Duration = Duration::from_secs(10);

#[cfg(windows)]
struct GeometryCache {
    result: String,
    stored_at: std::time::Instant,
    primary_only: bool,
    pattern: String,
}

#[cfg(windows)]
static GEOMETRY_CACHE: std::sync::Mutex<Option<GeometryCache>> =
    std::sync::Mutex::new(None);

#[cfg(windows)]
fn query_geometry(title_pattern: &str) -> String {
    query_geometry_ex(title_pattern, false)
}

#[cfg(windows)]
fn query_geometry_ex(title_pattern: &str, hid_is_primary_only: bool) -> String {
    // Cache check — reutilizamos resultado si la query es idéntica y fresca.
    // Reduce la frecuencia de EnumWindows/GetWindowTextW (anti-BattleEye).
    {
        let cache = GEOMETRY_CACHE.lock().unwrap();
        if let Some(c) = cache.as_ref() {
            if c.primary_only == hid_is_primary_only
                && c.pattern == title_pattern
                && c.stored_at.elapsed() < GEOMETRY_CACHE_TTL
            {
                return c.result.clone();
            }
        }
    }

    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetSystemMetrics, GetWindowRect, GetWindowTextW,
        IsWindowVisible, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
        SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};

    unsafe {
        // Virtual screen bbox real (todos los monitores).
        let real_vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let real_vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let real_vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let real_vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);

        // Primary monitor dims (siempre en origen 0,0 per Windows).
        let pw = GetSystemMetrics(SM_CXSCREEN);
        let ph = GetSystemMetrics(SM_CYSCREEN);

        // Reporta PRIMARY como vscreen cuando HID solo targetea primary (mode=serial).
        // Esto hace que el bot compute HID coords contra primary_w/h (el rango real
        // del AbsoluteMouse descriptor), no contra el vscreen completo.
        let (vx, vy, vw, vh) = if hid_is_primary_only {
            (0, 0, pw, ph)
        } else {
            (real_vx, real_vy, real_vw, real_vh)
        };

        // Buscar HWND por título conteniendo el pattern via EnumWindows
        // (FindWindow solo matchea exact title; necesitamos "contains").
        struct SearchCtx {
            pattern: String,
            found:   Option<HWND>,
        }
        unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let ctx = &mut *(lparam.0 as *mut SearchCtx);
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1); // continue
            }
            let mut buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, &mut buf);
            if len > 0 {
                let title = String::from_utf16_lossy(&buf[..len as usize]);
                // Exclude OBS Projector / StreamLabs Projector / recording tools
                // whose titles often contain "Tibia" as the SOURCE name (not
                // the actual game window). These are NOT the Tibia client.
                //
                // Fix 2026-04-16: la sesión live descubrió que OBS Projector
                // "Proyector - Fuente: Tibia_Fuente" matcheaba y se tomaba
                // su RECT en lugar del cliente real.
                let title_lower = title.to_lowercase();
                let is_excluded = title_lower.contains("proyector")
                    || title_lower.contains("projector")
                    || title_lower.contains("obs ")
                    || title_lower.starts_with("obs")
                    || title_lower.contains("streamlabs")
                    || title_lower.contains("xsplit");
                if !is_excluded && title.contains(&ctx.pattern) {
                    ctx.found = Some(hwnd);
                    return BOOL(0); // stop
                }
            }
            BOOL(1)
        }

        let mut ctx = SearchCtx { pattern: title_pattern.to_string(), found: None };
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));

        let result = match ctx.found {
            Some(hwnd) => {
                // Fix 2026-04-16: GetWindowRect incluye bordes invisibles en
                // ventanas maximizadas (típicamente 8px cada lado + titlebar).
                // GetClientRect + ClientToScreen devuelven el area de dibujado
                // REAL que coincide con lo que NDI/OBS captura. Sin este fix,
                // clicks en sidebar caían ~8px off.
                let mut client = RECT::default();
                if GetClientRect(hwnd, &mut client).is_ok() {
                    let mut origin = POINT { x: 0, y: 0 };
                    let _ = ClientToScreen(hwnd, &mut origin);
                    let tx = origin.x;
                    let ty = origin.y;
                    let tw = client.right - client.left;
                    let th = client.bottom - client.top;
                    format!(
                        "GEOMETRY {} {} {} {} {} {} {} {}\n",
                        vx, vy, vw, vh, tx, ty, tw, th
                    )
                } else {
                    // Fallback: usa GetWindowRect si GetClientRect falla.
                    let mut rect = RECT::default();
                    if GetWindowRect(hwnd, &mut rect).is_ok() {
                        let tx = rect.left;
                        let ty = rect.top;
                        let tw = rect.right - rect.left;
                        let th = rect.bottom - rect.top;
                        format!(
                            "GEOMETRY {} {} {} {} {} {} {} {}\n",
                            vx, vy, vw, vh, tx, ty, tw, th
                        )
                    } else {
                        format!("GEOMETRY {} {} {} {} ERR getwindowrect_failed\n", vx, vy, vw, vh)
                    }
                }
            }
            None => format!("GEOMETRY {} {} {} {} ERR window_not_found\n", vx, vy, vw, vh),
        };

        // Guardar en cache (solo resultados válidos — no cachear errors
        // permanentes como "window_not_found", que pueden resolverse al
        // abrir Tibia; nuevo query retry en próxima call).
        if !result.contains("ERR") {
            let mut cache = GEOMETRY_CACHE.lock().unwrap();
            *cache = Some(GeometryCache {
                result: result.clone(),
                stored_at: std::time::Instant::now(),
                primary_only: hid_is_primary_only,
                pattern: title_pattern.to_string(),
            });
        }

        result
    }
}

#[cfg(not(windows))]
fn query_geometry(_title_pattern: &str) -> String {
    // Non-Windows: retornar valores dummy para tests/CI.
    "GEOMETRY 0 0 1920 1080 0 0 1920 1080\n".to_string()
}

async fn focus_poll_task(
    pattern: String,
    interval_ms: u64,
    debounce_count: u32,
    focused: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
    let mut was_focused = true;
    let mut unfocused_streak: u32 = 0;
    loop {
        interval.tick().await;
        let is_focused = check_tibia_focused(&pattern);

        if is_focused {
            unfocused_streak = 0;
            focused.store(true, Ordering::Relaxed);
            if !was_focused {
                info!("Focus: Tibia recuperó el foco");
                was_focused = true;
            }
        } else {
            unfocused_streak += 1;
            if unfocused_streak >= debounce_count && was_focused {
                warn!(
                    "Focus: Tibia perdió el foco ({}ms debounce) — bloqueando comandos HID",
                    unfocused_streak as u64 * interval_ms
                );
                focused.store(false, Ordering::Relaxed);
                was_focused = false;
            }
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("pico_bridge=info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "bridge_config.toml".to_string());

    let cfg = load_config(&config_path)?;
    let use_sendinput = cfg.input.mode == "sendinput";

    if use_sendinput {
        info!("bridge_config.toml cargado: mode=sendinput (sin serial)");
    } else {
        let serial_cfg = cfg.serial.as_ref()
            .context("mode='serial' requiere sección [serial] en config")?;
        info!("bridge_config.toml cargado: port={} baud={}", serial_cfg.port, serial_cfg.baud);
    }

    // Iniciar focus polling si está habilitado.
    let tibia_focused = Arc::new(AtomicBool::new(true));
    let focus_enabled = cfg.focus.enabled;
    if focus_enabled {
        info!(
            "Focus detection habilitado: buscando '{}' cada {}ms",
            cfg.focus.window_title_contains, cfg.focus.poll_interval_ms
        );
        let focused_clone = Arc::clone(&tibia_focused);
        let pattern = cfg.focus.window_title_contains.clone();
        let interval = cfg.focus.poll_interval_ms;
        let debounce = cfg.focus.debounce_count;
        info!("Focus debounce: {} polls ({}ms)", debounce, debounce as u64 * interval);
        tokio::spawn(async move {
            focus_poll_task(pattern, interval, debounce, focused_clone).await;
        });
    }

    let listener = TcpListener::bind(&cfg.tcp.listen_addr)
        .await
        .with_context(|| format!("No se pudo escuchar en {}", cfg.tcp.listen_addr))?;

    info!("Escuchando en {} — esperando cliente TCP...", cfg.tcp.listen_addr);

    if use_sendinput {
        // ── Modo SendInput: sin serial, ejecutar localmente ──────────────────
        loop {
            info!("Esperando cliente TCP en {}...", cfg.tcp.listen_addr);
            let (socket, peer_addr) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => { error!("Error aceptando conexión TCP: {}", e); continue; }
            };
            info!("Cliente TCP conectado desde {}", peer_addr);
            let _ = socket.set_nodelay(true);

            let watchdog_timeout = cfg.watchdog.idle_timeout_secs;
            let exit_reason = run_proxy_sendinput(
                socket,
                watchdog_timeout,
                Arc::clone(&tibia_focused),
                focus_enabled,
            ).await;

            match exit_reason {
                ProxyExit::ClientDisconnected => {
                    info!("Cliente {} desconectado. Esperando nuevo cliente...", peer_addr);
                }
                ProxyExit::WatchdogExpired => {
                    warn!("Watchdog expiró ({peer_addr}, {watchdog_timeout}s). Bot debe reconectar.");
                }
                ProxyExit::SerialDead => {} // no aplica
            }
        }
    } else {
        // ── Modo Serial: comportamiento original con Pico2 ───────────────────
        let serial_cfg = cfg.serial.as_ref().unwrap();
        loop {
            let serial = open_serial_with_retry(serial_cfg).await;
            info!("Puerto serial {} abierto a {} baud", serial_cfg.port, serial_cfg.baud);
            info!("Esperando cliente TCP en {}...", cfg.tcp.listen_addr);

            let (socket, peer_addr) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => { error!("Error aceptando conexión TCP: {}", e); continue; }
            };
            info!("Cliente TCP conectado desde {}", peer_addr);
            let _ = socket.set_nodelay(true);

            let watchdog_timeout = cfg.watchdog.idle_timeout_secs;
            let exit_reason = run_proxy(
                socket, serial, watchdog_timeout,
                Arc::clone(&tibia_focused), focus_enabled,
            ).await;

            match &exit_reason {
                ProxyExit::ClientDisconnected | ProxyExit::WatchdogExpired => {
                    info!("Enviando RESET al Pico...");
                    if let Ok(mut s) = tokio_serial::new(&serial_cfg.port, serial_cfg.baud)
                        .timeout(Duration::from_millis(500))
                        .open_native_async()
                    {
                        let _ = s.write_data_terminal_ready(true);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let _ = s.write_all(b"RESET\n").await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                ProxyExit::SerialDead => {}
            }

            match exit_reason {
                ProxyExit::SerialDead => warn!("Serial muerto. Reconectando..."),
                ProxyExit::ClientDisconnected => info!("Cliente {peer_addr} desconectado."),
                ProxyExit::WatchdogExpired => warn!("Watchdog expiró ({peer_addr}, {watchdog_timeout}s)."),
            }
        }
    }
}

/// Intenta abrir el puerto serial indefinidamente cada 2 segundos.
async fn open_serial_with_retry(cfg: &SerialConfig) -> SerialStream {
    let port_name = cfg.port.clone();
    let baud = cfg.baud;
    open_with_retry(
        Duration::from_secs(2),
        move || {
            let port_name = port_name.clone();
            async move {
                tokio_serial::new(&port_name, baud)
                    .timeout(Duration::from_millis(500))
                    .open_native_async()
                    .map_err(|e| e.to_string())
            }
        },
        |s: &mut SerialStream| {
            // Asertamos DTR para que el firmware CDC active la recepción.
            let _ = s.write_data_terminal_ready(true);
        },
    )
    .await
}

/// Retry loop genérico: invoca `try_open` hasta que retorne Ok.
/// Aplica `on_success` al recurso obtenido. Espera `retry_interval` entre fallos.
///
/// Usado por `open_serial_with_retry` y testable con un mock que cuenta
/// llamadas y retorna Ok tras N fallos.
async fn open_with_retry<F, Fut, T, S>(
    retry_interval: Duration,
    mut try_open: F,
    on_success: S,
) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
    S: FnOnce(&mut T),
{
    loop {
        match try_open().await {
            Ok(mut resource) => {
                on_success(&mut resource);
                tokio::time::sleep(Duration::from_millis(200)).await;
                return resource;
            }
            Err(e) => {
                warn!("open_with_retry: {}. Reintentando en {:?}...", e, retry_interval);
                tokio::time::sleep(retry_interval).await;
            }
        }
    }
}

/// Razón por la que el proxy terminó.
enum ProxyExit {
    SerialDead,
    ClientDisconnected,
    /// El watchdog de inactividad expiró: sin comandos del bot durante N segundos.
    WatchdogExpired,
}

/// Retorna `true` si la línea es un comando passthrough que siempre debe
/// reenviarse al serial, incluso cuando Tibia no tiene foco.
fn is_passthrough_command(line: &[u8]) -> bool {
    line.starts_with(b"PING") || line.starts_with(b"RESET")
}

/// Documenta el protocolo ASCII del bridge TCP (puerto 9000).
///
/// Comandos locales (manejados por el bridge, no llegan al serial/sendinput):
///   FOCUS_CHECK       → "FOCUSED\n" o "NOFOCUS\n" según estado del FocusGate
///   GET_GEOMETRY      → "GEOMETRY vx vy vw vh tx ty tw th\n" via WinAPI
///   GET_GEOMETRY <t>  → ... usando pattern `<t>` en lugar de "Tibia"
///
/// Comandos passthrough (al serial/Arduino):
///   PING              → "PONG\n"
///   MOUSE_MOVE X Y    → "OK\n" (X, Y en HID absoluto 0-32767)
///   MOUSE_CLICK       → "OK\n" (left click default)
///   MOUSE_CLICK L/R/M → "OK\n" (left/right/middle click)
///   KEY_TAP 0xNN      → "OK\n" (HID usage ID)
///   RESET             → "OK\n" (release all keys + buttons)
#[allow(dead_code)]
const PROTOCOL_DOC: () = ();

/// Retorna `true` si el comando es manejado localmente por el bridge
/// (no se reenvía al serial).
fn is_bridge_local_command(line: &[u8]) -> bool {
    line.starts_with(b"FOCUS_CHECK") || line.starts_with(b"GET_GEOMETRY")
}

/// Proxy SendInput: recibe comandos TCP y los ejecuta localmente via Windows SendInput.
/// No usa serial. Las respuestas (OK/PONG/NOFOCUS) se generan localmente.
async fn run_proxy_sendinput(
    socket: TcpStream,
    watchdog_secs: u64,
    tibia_focused: Arc<AtomicBool>,
    focus_enabled: bool,
) -> ProxyExit {
    let (mut tcp_reader, mut tcp_writer) = socket.into_split();
    let mut buf = [0u8; 4096];
    let mut line_buf = Vec::with_capacity(256);

    loop {
        let read_result = if watchdog_secs > 0 {
            match tokio::time::timeout(
                Duration::from_secs(watchdog_secs),
                tcp_reader.read(&mut buf),
            ).await {
                Ok(r) => r,
                Err(_) => return ProxyExit::WatchdogExpired,
            }
        } else {
            tcp_reader.read(&mut buf).await
        };

        match read_result {
            Ok(0) => {
                info!("SendInput proxy: cliente cerró conexión (EOF)");
                return ProxyExit::ClientDisconnected;
            }
            Ok(n) => {
                for &byte in &buf[..n] {
                    line_buf.push(byte);
                    if byte == b'\n' {
                        let line_str = String::from_utf8_lossy(&line_buf).to_string();
                        let trimmed = line_str.trim();

                        // Local bridge commands (FOCUS_CHECK, GET_GEOMETRY)
                        if is_bridge_local_command(&line_buf) {
                            let resp: Vec<u8> = if trimmed.starts_with("FOCUS_CHECK") {
                                let focused = !focus_enabled
                                    || tibia_focused.load(Ordering::Relaxed);
                                if focused { b"FOCUSED\n".to_vec() } else { b"NOFOCUS\n".to_vec() }
                            } else if trimmed.starts_with("GET_GEOMETRY") {
                                // Accept: "GET_GEOMETRY" or "GET_GEOMETRY <pattern>"
                                // Default pattern is "Tibia" (matches the client title).
                                let pattern = trimmed
                                    .strip_prefix("GET_GEOMETRY")
                                    .unwrap_or("")
                                    .trim();
                                let pattern = if pattern.is_empty() { "Tibia" } else { pattern };
                                query_geometry(pattern).into_bytes()
                            } else {
                                b"ERR unknown_local_cmd\n".to_vec()
                            };
                            if let Err(e) = tcp_writer.write_all(&resp).await {
                                warn!("SendInput proxy: write error: {}", e);
                                return ProxyExit::ClientDisconnected;
                            }
                            line_buf.clear();
                            continue;
                        }

                        // PING: local
                        if trimmed == "PING" {
                            if let Err(e) = tcp_writer.write_all(b"PONG\n").await {
                                warn!("SendInput proxy: write error: {}", e);
                                return ProxyExit::ClientDisconnected;
                            }
                            line_buf.clear();
                            continue;
                        }

                        // Focus gate
                        let passthrough = trimmed == "RESET";
                        if focus_enabled
                            && !tibia_focused.load(Ordering::Relaxed)
                            && !passthrough
                        {
                            debug!("SendInput: bloqueando comando (sin foco)");
                            if let Err(e) = tcp_writer.write_all(b"NOFOCUS\n").await {
                                warn!("SendInput proxy: write error: {}", e);
                                return ProxyExit::ClientDisconnected;
                            }
                            line_buf.clear();
                            continue;
                        }

                        // Ejecutar via SendInput
                        let result = sendinput::execute_command(trimmed);
                        let resp = if result.ok { b"OK\n".as_slice() } else { b"ERR\n".as_slice() };
                        if let Err(e) = tcp_writer.write_all(resp).await {
                            warn!("SendInput proxy: write error: {}", e);
                            return ProxyExit::ClientDisconnected;
                        }

                        line_buf.clear();
                    }
                }
                if line_buf.len() > 4096 {
                    warn!("SendInput proxy: línea >4096 bytes — descartando");
                    line_buf.clear();
                }
            }
            Err(e) => {
                warn!("SendInput proxy: read error: {}", e);
                return ProxyExit::ClientDisconnected;
            }
        }
    }
}

/// Proxy bidireccional entre el socket TCP y el puerto serial.
/// Usa dos tareas async: tcp→serial y serial→tcp.
/// Termina cuando cualquiera de las dos detecta un error o EOF,
/// o cuando el watchdog de inactividad expira.
///
/// `watchdog_secs`: si es > 0 y no llega ningún byte del bot durante ese
/// periodo, se cierra la conexión con `ProxyExit::WatchdogExpired`.
///
/// `tibia_focused`: AtomicBool actualizado por el focus poll task.
/// `focus_enabled`: si `false`, el gate de foco se desactiva (legacy).
async fn run_proxy(
    socket: TcpStream,
    serial: SerialStream,
    watchdog_secs: u64,
    tibia_focused: Arc<AtomicBool>,
    focus_enabled: bool,
) -> ProxyExit {
    // Dividimos en mitades de lectura/escritura para que cada tarea tenga
    // ownership exclusivo de su dirección.
    let (mut tcp_reader, tcp_writer) = socket.into_split();
    let tcp_writer = Arc::new(tokio::sync::Mutex::new(tcp_writer));
    let (mut serial_reader, mut serial_writer) = tokio::io::split(serial);

    let tcp_writer_for_b = Arc::clone(&tcp_writer);

    // ── Tarea A: TCP → Serial con watchdog + focus gate ───────────────────
    let task_a = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let mut line_buf = Vec::with_capacity(256);

        loop {
            // Si watchdog está activo, usamos un read con timeout.
            let read_result = if watchdog_secs > 0 {
                match tokio::time::timeout(
                    Duration::from_secs(watchdog_secs),
                    tcp_reader.read(&mut buf),
                ).await {
                    Ok(r)  => r,
                    Err(_) => {
                        // Timeout: no llegó ningún byte del bot en N segundos.
                        return ProxyExit::WatchdogExpired;
                    }
                }
            } else {
                tcp_reader.read(&mut buf).await
            };

            match read_result {
                Ok(0) => {
                    // EOF del cliente TCP.
                    info!("TCP→serial: cliente cerró la conexión (EOF)");
                    return ProxyExit::ClientDisconnected;
                }
                Ok(n) => {
                    // Acumular bytes en line_buf y procesar líneas completas.
                    for &byte in &buf[..n] {
                        line_buf.push(byte);
                        if byte == b'\n' {
                            // Línea completa — decidir si reenviar o bloquear.
                            // Comandos locales del bridge (no van al serial).
                            if is_bridge_local_command(&line_buf) {
                                let line_str = String::from_utf8_lossy(&line_buf).to_string();
                                let trimmed = line_str.trim();
                                let resp: Vec<u8> = if trimmed.starts_with("FOCUS_CHECK") {
                                    let focused = !focus_enabled
                                        || tibia_focused.load(Ordering::Relaxed);
                                    if focused { b"FOCUSED\n".to_vec() } else { b"NOFOCUS\n".to_vec() }
                                } else if trimmed.starts_with("GET_GEOMETRY") {
                                    let pattern = trimmed
                                        .strip_prefix("GET_GEOMETRY")
                                        .unwrap_or("")
                                        .trim();
                                    let pattern = if pattern.is_empty() { "Tibia" } else { pattern };
                                    // Modo serial: Arduino HID targetea primary only.
                                    // Reportamos vscreen = primary para que el bot compute
                                    // HID coords contra el rango real del HID descriptor.
                                    #[cfg(windows)]
                                    let reply = query_geometry_ex(pattern, true);
                                    #[cfg(not(windows))]
                                    let reply = query_geometry(pattern);
                                    reply.into_bytes()
                                } else {
                                    b"ERR unknown_local_cmd\n".to_vec()
                                };
                                let mut w = tcp_writer.lock().await;
                                if let Err(e) = w.write_all(&resp).await {
                                    warn!("TCP→serial: error escribiendo respuesta comando local: {}", e);
                                    return ProxyExit::ClientDisconnected;
                                }
                                line_buf.clear();
                                continue;
                            }

                            let passthrough = is_passthrough_command(&line_buf);

                            if !focus_enabled
                                || tibia_focused.load(Ordering::Relaxed)
                                || passthrough
                            {
                                // Foco OK o comando passthrough → reenviar al serial.
                                if let Err(e) = serial_writer.write_all(&line_buf).await {
                                    warn!("TCP→serial: error escribiendo al serial: {}", e);
                                    return ProxyExit::SerialDead;
                                }
                            } else {
                                // Sin foco + comando HID → responder NOFOCUS.
                                debug!("Focus gate: bloqueando comando (Tibia sin foco)");
                                let mut w = tcp_writer.lock().await;
                                if let Err(e) = w.write_all(b"NOFOCUS\n").await {
                                    warn!("TCP→serial: error escribiendo NOFOCUS: {}", e);
                                    return ProxyExit::ClientDisconnected;
                                }
                            }
                            line_buf.clear();
                        }
                    }
                    // Protección contra líneas absurdamente largas (sin \n).
                    if line_buf.len() > 4096 {
                        warn!("TCP→serial: línea >4096 bytes sin \\n — descartando");
                        line_buf.clear();
                    }
                }
                Err(e) => {
                    warn!("TCP→serial: error leyendo del socket: {}", e);
                    return ProxyExit::ClientDisconnected;
                }
            }
        }
    });

    // ── Tarea B: Serial → TCP ─────────────────────────────────────────────────
    let task_b = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match serial_reader.read(&mut buf).await {
                Ok(0) => {
                    info!("Serial→TCP: serial cerrado (EOF)");
                    return ProxyExit::SerialDead;
                }
                Ok(n) => {
                    let mut w = tcp_writer_for_b.lock().await;
                    if let Err(e) = w.write_all(&buf[..n]).await {
                        warn!("Serial→TCP: error escribiendo al socket: {}", e);
                        return ProxyExit::ClientDisconnected;
                    }
                }
                Err(e) => {
                    warn!("Serial→TCP: error leyendo del serial: {}", e);
                    return ProxyExit::SerialDead;
                }
            }
        }
    });

    // Sacamos los abort handles antes de que select consuma los JoinHandles.
    let abort_a = task_a.abort_handle();
    let abort_b = task_b.abort_handle();

    // Esperamos a que la primera tarea termine y abortamos la otra.
    tokio::select! {
        result = task_a => {
            abort_b.abort();
            result.unwrap_or(ProxyExit::ClientDisconnected)
        }
        result = task_b => {
            abort_a.abort();
            result.unwrap_or(ProxyExit::SerialDead)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Nota sobre el watchdog: el watchdog NO bloquea `accept()`. Se implementa
// como `tokio::time::timeout` alrededor de la lectura TCP dentro del proxy
// (ver `run_proxy_sendinput` línea ~373 y `run_proxy` línea ~491).
// Cuando expira, el proxy retorna `ProxyExit::WatchdogExpired` y el loop
// externo vuelve a llamar a `listener.accept().await` limpiamente.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_local_commands_are_detected() {
        assert!(is_bridge_local_command(b"FOCUS_CHECK"));
        assert!(is_bridge_local_command(b"FOCUS_CHECK\n"));
        assert!(!is_bridge_local_command(b"PING"));
        assert!(!is_bridge_local_command(b"KEY_TAP 0x3A"));
        assert!(!is_bridge_local_command(b"MOUSE_MOVE 100 200"));
    }

    #[test]
    fn passthrough_commands_are_detected() {
        assert!(is_passthrough_command(b"PING"));
        assert!(is_passthrough_command(b"PING\n"));
        assert!(is_passthrough_command(b"RESET"));
        assert!(is_passthrough_command(b"RESET\n"));
        assert!(!is_passthrough_command(b"KEY_TAP 0x3A"));
        assert!(!is_passthrough_command(b"MOUSE_MOVE 100 200"));
        assert!(!is_passthrough_command(b"FOCUS_CHECK"));
    }

    #[test]
    fn input_config_default_is_sendinput() {
        let cfg = InputConfig::default();
        assert_eq!(cfg.mode, "sendinput");
    }

    #[test]
    fn watchdog_config_default_is_10s() {
        let cfg = WatchdogConfig::default();
        assert_eq!(cfg.idle_timeout_secs, 10);
    }

    // ── M6: open_with_retry tests ────────────────────────────────────

    use std::cell::Cell;
    use std::rc::Rc;

    #[tokio::test]
    async fn open_with_retry_success_first_try() {
        let attempts = Rc::new(Cell::new(0u32));
        let attempts_cb = attempts.clone();
        let result: i32 = open_with_retry(
            Duration::from_millis(10),
            move || {
                attempts_cb.set(attempts_cb.get() + 1);
                async { Ok::<i32, String>(42) }
            },
            |v| { *v += 1; }, // on_success: incrementa
        ).await;
        assert_eq!(result, 43);  // 42 + 1 del on_success
        assert_eq!(attempts.get(), 1);
    }

    #[tokio::test]
    async fn open_with_retry_succeeds_after_n_failures() {
        let attempts = Rc::new(Cell::new(0u32));
        let attempts_cb = attempts.clone();
        let result: i32 = open_with_retry(
            Duration::from_millis(5),
            move || {
                let n = attempts_cb.get();
                attempts_cb.set(n + 1);
                async move {
                    if n < 3 {
                        Err::<i32, _>("not yet".to_string())
                    } else {
                        Ok(99)
                    }
                }
            },
            |_| {},
        ).await;
        assert_eq!(result, 99);
        assert_eq!(attempts.get(), 4);  // 3 fails + 1 success
    }

    #[tokio::test]
    async fn open_with_retry_respects_interval() {
        let attempts = Rc::new(Cell::new(0u32));
        let attempts_cb = attempts.clone();
        let interval = Duration::from_millis(50);
        let t0 = std::time::Instant::now();
        let _result: i32 = open_with_retry(
            interval,
            move || {
                let n = attempts_cb.get();
                attempts_cb.set(n + 1);
                async move {
                    if n < 2 {
                        Err::<i32, _>("not yet".to_string())
                    } else {
                        Ok(1)
                    }
                }
            },
            |_| {},
        ).await;
        // 2 failures × 50ms + ~200ms post-success sleep = ~300ms.
        // Damos margen amplio para CI lento.
        let elapsed = t0.elapsed();
        assert!(elapsed >= Duration::from_millis(100),
            "esperaba >=100ms, got {:?}", elapsed);
        assert_eq!(attempts.get(), 3);
    }
}
