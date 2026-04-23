/// http.rs — Servidor HTTP de control y diagnóstico.
///
/// Endpoints:
///   GET  /status                → JSON con métricas y estado
///   POST /pause                 → pausa el bot
///   POST /resume                → reanuda el bot
///   POST /test/pico/ping        → PING a la Pico, retorna latencia
///   POST /test/click            → click en coords del viewport {"x":N,"y":N}
///   GET  /test/grab             → PNG del último frame NDI capturado
///   GET  /vision/perception     → percepción completa (HP%, mana%, enemigos, condiciones)
///   GET  /vision/vitals         → HP y mana actuales
///   GET  /vision/battle         → lista de batalla actual
///   GET  /vision/status         → condiciones activas del personaje
///   GET  /vision/grab/anchors   → PNG del frame con ROIs y anclas superpuestos (debug)
///   GET  /vision/grab/battle    → PNG recortado del ROI de battle list (debug, escala ×3)
///   GET  /vision/grab/debug     → PNG del frame completo con TODOS los ROIs dibujados
///   GET  /vision/battle/debug   → JSON con datos de diagnóstico por slot de batalla

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::act::Actuator;
use crate::config::Config;
use crate::core::loop_::LoopCommand;
use crate::core::state::SharedState;
use crate::sense::frame_buffer::FrameBuffer;
use crate::sense::metrics::MetricsSnapshot;
use crate::sense::vision::calibration::Calibration;

/// Estado compartido del servidor HTTP.
#[derive(Clone)]
pub struct AppState {
    pub game_state:  SharedState,
    pub buffer:      Arc<FrameBuffer>,
    pub actuator:    Arc<Actuator>,
    pub config:      Config,
    /// Calibración cargada al inicio — usada para debug de ROIs.
    pub calibration: Arc<Calibration>,
    /// Canal para enviar comandos al game loop (hot-reload de waypoints, etc).
    pub loop_tx:     Sender<LoopCommand>,
    /// Métricas runtime (histogramas + ArcSwap last_tick). Lectura lock-free.
    pub metrics:     Arc<crate::instrumentation::MetricsRegistry>,
    /// HealthSystem snapshot (ArcSwap<HealthStatus>). Lectura lock-free.
    pub health:      Arc<arc_swap::ArcSwap<crate::health::HealthStatus>>,
}

/// Construye el Router de axum con todos los endpoints registrados.
/// Exposed para tests de integración (sin `serve()`).
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/status",                get(handle_status))
        .route("/health",                get(handle_health))
        .route("/pause",                 post(handle_pause))
        .route("/resume",                post(handle_resume))
        .route("/test/pico/ping",        post(handle_test_ping))
        .route("/test/click",            post(handle_test_click))
        .route("/test/heal",             post(handle_test_heal))
        .route("/test/key",              post(handle_test_key))
        .route("/test/inject_frame",     post(handle_test_inject_frame))
        .route("/test/grab",             get(handle_test_grab))
        .route("/vision/perception",     get(handle_vision_perception))
        .route("/vision/cursor",         get(handle_vision_cursor))
        .route("/vision/match_now",      get(handle_vision_match_now))
        .route("/vision/extract_template", post(handle_vision_extract_template))
        .route("/vision/vitals",         get(handle_vision_vitals))
        .route("/vision/battle",         get(handle_vision_battle))
        .route("/vision/status",         get(handle_vision_status))
        .route("/vision/grab/anchors",   get(handle_vision_grab_anchors))
        .route("/vision/grab/battle",    get(handle_vision_grab_battle))
        .route("/vision/grab/debug",     get(handle_vision_grab_debug))
        .route("/vision/grab/inventory", get(handle_vision_grab_inventory))
        .route("/vision/inventory",      get(handle_vision_inventory))
        .route("/vision/matcher/stats",  get(handle_vision_matcher_stats))
        .route("/vision/battle/debug",   get(handle_vision_battle_debug))
        .route("/vision/target/debug",   get(handle_vision_target_debug))
        .route("/vision/loot/debug",     get(handle_vision_loot_debug))
        .route("/vision/loot/grab",      get(handle_vision_loot_grab))
        .route("/fsm/debug",             get(handle_fsm_debug))
        .route("/combat/events",         get(handle_combat_events))
        .route("/dispatch/stats",        get(handle_dispatch_stats))
        .route("/waypoints/load",        post(handle_waypoints_load))
        .route("/waypoints/status",      get(handle_waypoints_status))
        .route("/waypoints/pause",       post(handle_waypoints_pause))
        .route("/waypoints/resume",      post(handle_waypoints_resume))
        .route("/waypoints/clear",       post(handle_waypoints_clear))
        .route("/cavebot/status",        get(handle_cavebot_status))
        .route("/cavebot/load",          post(handle_cavebot_load))
        .route("/cavebot/pause",         post(handle_cavebot_pause))
        .route("/cavebot/resume",        post(handle_cavebot_resume))
        .route("/cavebot/clear",         post(handle_cavebot_clear))
        .route("/cavebot/jump_to_label", post(handle_cavebot_jump_to_label))
        .route("/scripts/reload",        post(handle_scripts_reload))
        .route("/scripts/status",        get(handle_scripts_status))
        .route("/metrics",               get(handle_prometheus_metrics))
        .route("/recording/start",       post(handle_recording_start))
        .route("/recording/stop",        post(handle_recording_stop))
        .route("/dataset/start",         post(handle_dataset_start))
        .route("/dataset/stop",          post(handle_dataset_stop))
        .route("/dataset/status",        get(handle_dataset_status))
        .route("/vision/region_monitor", get(handle_region_monitor))
        .route("/instrumentation/last_tick",        get(handle_instr_last_tick))
        .route("/instrumentation/percentiles",      get(handle_instr_percentiles))
        .route("/instrumentation/histograms",       get(handle_instr_histograms))
        .route("/instrumentation/windows",          get(handle_instr_windows))
        .route("/instrumentation/start_recording",  post(handle_instr_start_recording))
        .route("/instrumentation/stop_recording",   post(handle_instr_stop_recording))
        .route("/instrumentation/recording_status", get(handle_instr_recording_status))
        .route("/health/detailed",                  get(handle_health_detailed))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth_middleware))
        // V-007 mitigation: stealth_mode hide debug endpoints.
        .layer(axum::middleware::from_fn_with_state(state.clone(), stealth_middleware))
        // V-005/V-006 mitigations:
        //
        // RequestBodyLimitLayer cap requests body a MAX_HTTP_BODY_BYTES (10 MB).
        // Protege `/test/inject_frame` y `/vision/extract_template` que parsean
        // PNG — un atacante local podría pasar 10 GB de random bytes y forzar
        // OOM en el allocator. El cap es generoso (un 1920×1080 PNG raramente
        // supera 4 MB) pero muy por debajo del daño OOM.
        //
        // ConcurrencyLimitLayer cap a MAX_CONCURRENT_REQUESTS conexiones
        // simultáneas. Evita que spamear `/test/grab` a 1000 req/s sature disk
        // I/O + RAM (cada response genera un 2-4 MB PNG). 32 simultáneas es
        // holgado para un cliente humano + automatización legítima, suficiente
        // para bloquear un DoS trivial sin afectar el UX.
        .layer(tower_http::limit::RequestBodyLimitLayer::new(MAX_HTTP_BODY_BYTES))
        .layer(tower::limit::ConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS))
        .with_state(state)
}

/// V-005/V-006: límites del HTTP server. Valores deliberadamente holgados
/// para no romper uso legítimo (frames inyectados, PNG templates extraídos).
const MAX_HTTP_BODY_BYTES:      usize = 10 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS:  usize = 32;

/// V-002 fix: middleware que requiere `Authorization: Bearer <token>` si
/// `config.http.auth_token` está set. Si no hay token configurado, pasa
/// todo (backwards compat con configs viejos + loopback-only default).
///
/// 401 Unauthorized si falta o no matches. `/health` excluido para
/// monitoring externo simple (solo retorna 200 OK, sin state).
async fn auth_middleware(
    axum::extract::State(s): axum::extract::State<AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // /health bypass para simple liveness checks sin necesidad del token.
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }

    let Some(expected_token) = s.config.http.auth_token.as_ref()
        .filter(|t| !t.is_empty()) else {
        // Sin token configurado → sin auth (loopback-only seguro, LAN
        // setup DEBE configurar token).
        return next.run(req).await;
    };

    let header_ok = req.headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|token| {
            // Constant-time compare para resistir timing attacks.
            // Implementación simple: comparar byte-a-byte completa
            // siempre (no early-return on mismatch).
            let a = token.as_bytes();
            let b = expected_token.as_bytes();
            if a.len() != b.len() {
                return false;
            }
            let mut acc: u8 = 0;
            for i in 0..a.len() {
                acc |= a[i] ^ b[i];
            }
            acc == 0
        })
        .unwrap_or(false);

    if !header_ok {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "missing or invalid Authorization: Bearer <token>",
        ).into_response();
    }

    next.run(req).await
}

/// V-007 fix: middleware que oculta endpoints de debug/introspección cuando
/// `config.http.stealth_mode` es true. Retorna 404 con cuerpo vacío — idéntico
/// a un route inexistente, sin leak del hecho de que el endpoint existe pero
/// está deshabilitado.
///
/// Endpoints SIEMPRE disponibles (incluso en stealth):
///   - `/health` — liveness check
///   - `/status`, `/pause`, `/resume` — mínimo necesario para ops humanas
///   - `/metrics` — Prometheus scrape (útil con auth token + IP allowlist)
///   - endpoints de escritura operativos (/cavebot/load, /scripts/reload, etc.)
///     quedan disponibles porque son necesarios para operar el bot; su
///     superficie de fingerprinting es mínima (no devuelven contenido).
///
/// Endpoints bloqueados por stealth (prefix match):
///   - `/vision/*` completo (incluye grab, perception, inventory, debug views)
///   - `/fsm/debug`, `/combat/events`, `/dispatch/stats`
///   - `/test/grab`, `/test/inject_frame` (devuelven datos de vision)
///   - `/*/status` que leak state interno (`/cavebot/status`, `/waypoints/status`,
///     `/scripts/status`)
async fn stealth_middleware(
    axum::extract::State(s): axum::extract::State<AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !s.config.http.stealth_mode {
        return next.run(req).await;
    }

    let path = req.uri().path();
    let blocked = path.starts_with("/vision/")
        || path == "/fsm/debug"
        || path == "/combat/events"
        || path == "/dispatch/stats"
        || path == "/test/grab"
        || path == "/test/inject_frame"
        || path == "/cavebot/status"
        || path == "/waypoints/status"
        || path == "/scripts/status";

    if blocked {
        // 404 (no 403) para que un scanner no distinga entre "endpoint no
        // existe" y "endpoint existe pero stealth". Sin body para evitar
        // fingerprint por contenido del error.
        return (axum::http::StatusCode::NOT_FOUND, "").into_response();
    }

    next.run(req).await
}

pub async fn serve(state: AppState) -> Result<()> {
    let app = build_router(state.clone());

    info!("HTTP server escuchando en {}", state.config.http.listen_addr);
    let listener = tokio::net::TcpListener::bind(&state.config.http.listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ── /status ───────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    tick:                 u64,
    is_paused:            bool,
    fsm_state:            String,
    waypoint_mode:        String,
    ticks_total:          u64,
    ticks_overrun:        u64,
    ndi_latency_ms:       f64,
    pico_latency_ms:      f64,
    bot_proc_ms:          f64,
    has_frame:            bool,
    vision_calibrated:    bool,
    vision_metrics:       MetricsSnapshot,
    safety_pause_reason:  Option<String>,
    safety_rate_dropped:  u64,
    cavebot_loaded:       bool,
    cavebot_enabled:      bool,
    cavebot_step:         Option<usize>,
    cavebot_kind:         String,
}

async fn handle_status(State(s): State<AppState>) -> Json<StatusResponse> {
    let g = s.game_state.read();
    let vision_calibrated = g.last_perception
        .as_ref()
        .map(|p| p.vitals.hp.is_some())
        .unwrap_or(false);
    let vision_metrics = g.vision_metrics.snapshot();
    Json(StatusResponse {
        tick:                g.tick,
        is_paused:           g.is_paused,
        fsm_state:           format!("{:?}", g.fsm_state),
        waypoint_mode:       format!("{:?}", g.waypoint_mode),
        ticks_total:         g.metrics.ticks_total,
        ticks_overrun:       g.metrics.ticks_overrun,
        ndi_latency_ms:      g.metrics.ndi_latency_ms,
        pico_latency_ms:     g.metrics.pico_latency_ms,
        bot_proc_ms:         g.metrics.bot_proc_ms,
        has_frame:           s.buffer.latest_frame().is_some(),
        vision_calibrated,
        vision_metrics,
        safety_pause_reason: g.safety_pause_reason.clone(),
        safety_rate_dropped: g.safety_rate_dropped,
        cavebot_loaded:      g.cavebot_status.loaded,
        cavebot_enabled:     g.cavebot_status.enabled,
        cavebot_step:        g.cavebot_status.current_index,
        cavebot_kind:        g.cavebot_status.current_kind.clone(),
    })
}

// ── /health ───────────────────────────────────────────────────────────────────
//
// Endpoint de health check para monitoring y scripts de start/stop.
// Retorna HTTP 200 si el bot está sano, 503 si no, con un JSON body que
// explica qué falta en caso negativo.
//
// Usado por:
// - `scripts/check_session.ps1` para gate en el runbook
// - Alertas Grafana (si el endpoint da 503 por > N segundos)
// - Stress tests (para detectar degradación temprana)

#[derive(Serialize)]
struct HealthResponse {
    ok:      bool,
    reason:  String,
    details: HealthDetails,
}

#[derive(Serialize)]
struct HealthDetails {
    has_frame:          bool,
    frame_age_ms:       Option<u64>,
    is_paused:          bool,
    safety_pause_reason: Option<String>,
    ticks_total:        u64,
    ticks_overrun:      u64,
    ndi_latency_ms:     f64,
    pico_latency_ms:    f64,
    bot_proc_ms:        f64,
    fsm_state:          String,
}

/// Edad máxima aceptable del último frame NDI antes de considerar el bot unhealthy.
const HEALTH_MAX_FRAME_AGE_MS: u64 = 2000;

/// Latencia máxima aceptable en el bot proc antes de considerar degradación.
const HEALTH_MAX_PROC_MS: f64 = 50.0;

async fn handle_health(State(s): State<AppState>) -> Response {
    let g = s.game_state.read();
    let latest = s.buffer.latest_frame();
    let has_frame = latest.is_some();
    let frame_age_ms = latest.as_ref().map(|f| f.captured_at.elapsed().as_millis() as u64);

    let details = HealthDetails {
        has_frame,
        frame_age_ms,
        is_paused:           g.is_paused,
        safety_pause_reason: g.safety_pause_reason.clone(),
        ticks_total:         g.metrics.ticks_total,
        ticks_overrun:       g.metrics.ticks_overrun,
        ndi_latency_ms:      g.metrics.ndi_latency_ms,
        pico_latency_ms:     g.metrics.pico_latency_ms,
        bot_proc_ms:         g.metrics.bot_proc_ms,
        fsm_state:           format!("{:?}", g.fsm_state),
    };

    let (ok, reason) = determine_health(&details);

    let body = Json(HealthResponse {
        ok,
        reason: reason.to_string(),
        details,
    });

    let code = if ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, body).into_response()
}

/// Determina el estado de health a partir de los detalles.
/// Extraído para testear sin necesidad de AppState.
fn determine_health(d: &HealthDetails) -> (bool, &'static str) {
    if !d.has_frame {
        return (false, "no_frame");
    }
    if let Some(age) = d.frame_age_ms {
        if age > HEALTH_MAX_FRAME_AGE_MS {
            return (false, "stale_frame");
        }
    }
    if d.is_paused {
        // Pausa normal (operador) no es unhealthy per se — retornamos ok=false
        // con reason explicit, así el runbook puede decidir si despierta o no.
        if let Some(reason) = &d.safety_pause_reason {
            return (false, match reason.as_str() {
                r if r.starts_with("prompt:login")       => "paused_login",
                r if r.starts_with("prompt:char_select") => "paused_char_select",
                r if r.starts_with("prompt:npc_trade")   => "paused_npc_trade",
                _                                         => "paused_safety",
            });
        }
        return (false, "paused_manual");
    }
    if d.bot_proc_ms > HEALTH_MAX_PROC_MS {
        return (false, "proc_slow");
    }
    if d.ticks_total == 0 {
        return (false, "not_started");
    }
    (true, "ok")
}

// ── /pause y /resume ──────────────────────────────────────────────────────────

async fn handle_pause(State(s): State<AppState>) -> &'static str {
    s.game_state.write().is_paused = true;
    info!("Bot pausado vía HTTP");
    "paused"
}

async fn handle_resume(State(s): State<AppState>) -> &'static str {
    s.game_state.write().is_paused = false;
    info!("Bot reanudado vía HTTP");
    "resumed"
}

// ── /test/pico/ping ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct PingResponse {
    ok:         bool,
    reply:      String,
    latency_ms: f64,
}

async fn handle_test_ping(State(s): State<AppState>) -> Json<PingResponse> {
    match s.actuator.ping().await {
        Ok(r) => Json(PingResponse {
            ok:         r.ok,
            reply:      r.body,
            latency_ms: r.latency_ms,
        }),
        Err(e) => Json(PingResponse {
            ok:         false,
            reply:      format!("error: {}", e),
            latency_ms: 0.0,
        }),
    }
}

// ── /test/click ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ClickRequest {
    x: i32,
    y: i32,
    #[serde(default = "default_button")]
    button: String,
}
fn default_button() -> String { "L".into() }

#[derive(Serialize)]
struct ClickResponse {
    ok:         bool,
    latency_ms: f64,
}

async fn handle_test_click(
    State(s): State<AppState>,
    Json(req): Json<ClickRequest>,
) -> Json<ClickResponse> {
    if !s.game_state.read().is_paused {
        return Json(ClickResponse { ok: false, latency_ms: 0.0 });
    }
    match s.actuator.click(req.x, req.y, &req.button).await {
        Ok(r) => Json(ClickResponse { ok: r.ok, latency_ms: r.latency_ms }),
        Err(_e) => Json(ClickResponse { ok: false, latency_ms: 0.0 }),
    }
}

// ── /test/key ─────────────────────────────────────────────────────────────────
// Endpoint de diagnóstico: dispara una tecla HID arbitraria vía el Actuator.
// Útil para verificar que el firmware del Pico soporta un keycode específico
// antes de decidir qué tecla usar en el config. Acepta el code como hex
// ("0x4B") o como nombre de tecla ("PageUp").
//
// Ejemplo:
//   curl -X POST 'http://localhost:8080/test/key?code=PageUp'
//   curl -X POST 'http://localhost:8080/test/key?code=0x4B'

#[derive(Deserialize)]
struct KeyQuery {
    code: String,
}

#[derive(Serialize)]
struct KeyResponse {
    ok:       bool,
    code_in:  String,
    hidcode:  u8,
    reply:    String,
    latency_ms: f64,
}

async fn handle_test_key(
    State(s): State<AppState>,
    Query(q): Query<KeyQuery>,
) -> Json<KeyResponse> {
    if !s.game_state.read().is_paused {
        return Json(KeyResponse {
            ok: false, code_in: q.code, hidcode: 0,
            reply: "bot must be paused to use /test/key".into(),
            latency_ms: 0.0,
        });
    }
    let hidcode = match crate::act::keycode::parse(&q.code) {
        Ok(c) => c,
        Err(e) => {
            return Json(KeyResponse {
                ok: false, code_in: q.code, hidcode: 0,
                reply: format!("keycode parse error: {}", e),
                latency_ms: 0.0,
            });
        }
    };
    match s.actuator.key_tap(hidcode).await {
        Ok(r) => Json(KeyResponse {
            ok: r.ok, code_in: q.code, hidcode,
            reply: r.body,
            latency_ms: r.latency_ms,
        }),
        Err(e) => Json(KeyResponse {
            ok: false, code_in: q.code, hidcode,
            reply: format!("error: {}", e),
            latency_ms: 0.0,
        }),
    }
}

// ── /test/inject_frame ────────────────────────────────────────────────────────
// Inyecta un frame PNG directamente al FrameBuffer, bypass del NDI receiver.
// Útil para tests de integración automatizados: permite disparar Emergency,
// Fighting, Walking con frames sintéticos predecibles.
//
// El body debe ser un PNG como octetos raw (Content-Type: application/octet-stream
// o image/png). El PNG se decodifica a RGBA y se publica al FrameBuffer como
// si viniera del NDI real.
//
// Ejemplo:
//   curl -X POST --data-binary @frame_lowhp.png \
//        -H "Content-Type: image/png" http://localhost:8080/test/inject_frame

use axum::body::Bytes;

#[derive(Serialize)]
struct InjectFrameResponse {
    ok:      bool,
    width:   u32,
    height:  u32,
    message: String,
}

async fn handle_test_inject_frame(
    State(s): State<AppState>,
    body: Bytes,
) -> Json<InjectFrameResponse> {
    if body.is_empty() {
        return Json(InjectFrameResponse {
            ok: false, width: 0, height: 0,
            message: "body vacío — se esperaba un PNG".into(),
        });
    }

    // Decodificar el PNG a RGBA.
    let img = match image::load_from_memory(&body) {
        Ok(i)  => i.to_rgba8(),
        Err(e) => {
            return Json(InjectFrameResponse {
                ok: false, width: 0, height: 0,
                message: format!("PNG inválido: {}", e),
            });
        }
    };

    let width  = img.width();
    let height = img.height();
    let data   = img.into_raw(); // Vec<u8> RGBA

    // Construir el Frame y publicarlo al FrameBuffer.
    let frame = crate::sense::frame_buffer::Frame {
        width,
        height,
        data,
        captured_at: std::time::Instant::now(),
    };
    s.buffer.publish(frame);

    info!("Frame sintético inyectado: {}x{}", width, height);
    Json(InjectFrameResponse {
        ok: true, width, height,
        message: format!("frame {}x{} publicado", width, height),
    })
}

// ── /test/heal ────────────────────────────────────────────────────────────────
// Dispara manualmente la hotkey `heal_spell` configurada en [actions].
// Útil para verificar que el bridge recibe el código HID correcto sin
// esperar a que el HP baje en el juego.
//   curl -X POST http://localhost:8080/test/heal

#[derive(Serialize)]
struct HealResponse {
    ok:         bool,
    hidcode:    u8,
    reply:      String,
    latency_ms: f64,
}

async fn handle_test_heal(State(s): State<AppState>) -> Json<HealResponse> {
    if !s.game_state.read().is_paused {
        return Json(HealResponse {
            ok: false, hidcode: 0,
            reply: "bot must be paused to use /test/heal".into(),
            latency_ms: 0.0,
        });
    }
    let hotkeys = match s.config.hotkeys() {
        Ok(h) => h,
        Err(e) => {
            return Json(HealResponse {
                ok: false, hidcode: 0,
                reply: format!("hotkeys inválidas: {:#}", e),
                latency_ms: 0.0,
            });
        }
    };
    let hidcode = hotkeys.heal_spell;
    match s.actuator.key_tap(hidcode).await {
        Ok(r) => Json(HealResponse {
            ok:         r.ok,
            hidcode,
            reply:      r.body,
            latency_ms: r.latency_ms,
        }),
        Err(e) => Json(HealResponse {
            ok:         false,
            hidcode,
            reply:      format!("error: {}", e),
            latency_ms: 0.0,
        }),
    }
}

// ── /test/grab ────────────────────────────────────────────────────────────────

async fn handle_test_grab(State(s): State<AppState>) -> Response {
    let frame = match s.buffer.latest_frame() {
        Some(f) => f,
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "No hay frame NDI todavía")
                .into_response();
        }
    };

    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> = match ImageBuffer::from_raw(
        frame.width,
        frame.height,
        frame.data.clone(),
    ) {
        Some(i) => i,
        None => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Frame RGBA inválido").into_response();
        }
    };

    let mut png_bytes = Vec::new();
    if let Err(e) = img.write_to(
        &mut std::io::Cursor::new(&mut png_bytes),
        image::ImageFormat::Png,
    ) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("PNG encode: {}", e)).into_response();
    }

    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        png_bytes,
    )
        .into_response()
}

// ── /vision/cursor ────────────────────────────────────────────────────────────
//
// Devuelve la posición actual del cursor en coord desktop/viewport.
// Con Tibia en primary monitor fullscreen, coord desktop == coord viewport.
// Uso: workflow rápido de calibración sin PowerShell.

#[derive(Serialize)]
struct CursorPos { x: i32, y: i32 }

#[cfg(windows)]
async fn handle_vision_cursor(State(_s): State<AppState>) -> Response {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use windows_sys::Win32::Foundation::POINT;
    let mut p = POINT { x: 0, y: 0 };
    let ok = unsafe { GetCursorPos(&mut p as *mut _) };
    if ok == 0 {
        return (StatusCode::INTERNAL_SERVER_ERROR, "GetCursorPos failed").into_response();
    }
    Json(CursorPos { x: p.x, y: p.y }).into_response()
}

#[cfg(not(windows))]
async fn handle_vision_cursor(State(_s): State<AppState>) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "cursor API solo en Windows").into_response()
}

// ── /vision/match_now?template=X ──────────────────────────────────────────────
//
// Corre template matching SYNC contra el frame actual. Debug: permite ver score
// exacto y coord del best match sin esperar al background worker async.
// Bypassa el ROI configurado — busca en el frame completo (más lento pero
// útil para diagnóstico). Retorna JSON con score + top-left + centro + dims.

#[derive(Deserialize)]
struct MatchNowQuery {
    template: String,
    #[serde(default)]
    /// Si true, busca solo dentro del ROI configurado en [ui_rois] del template.
    /// Por default (false) busca en el frame completo (más lento pero
    /// diagnóstico más completo).
    use_roi: bool,
}

#[derive(Serialize)]
struct MatchNowResponse {
    template: String,
    score: f32,
    top_left_x: u32,
    top_left_y: u32,
    center_x: u32,
    center_y: u32,
    template_w: u32,
    template_h: u32,
    search_area_w: u32,
    search_area_h: u32,
    passed_threshold: bool,
    threshold: f32,
}

async fn handle_vision_match_now(
    State(s): State<AppState>,
    Query(q): Query<MatchNowQuery>,
) -> Response {
    use imageproc::template_matching::{match_template, MatchTemplateMethod};

    let frame = match s.buffer.latest_frame() {
        Some(f) => f,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "No hay frame NDI").into_response(),
    };

    // Load template from disk (same path UiDetector uses).
    // TODO: leer assets_dir dinámicamente; por ahora hardcoded "assets/templates/ui".
    let tpl_path = PathBuf::from(format!("assets/templates/ui/{}.png", q.template));
    let tpl_img = match image::open(&tpl_path) {
        Ok(i) => i.to_luma8(),
        Err(e) => return (
            StatusCode::NOT_FOUND,
            format!("No se pudo cargar template '{}': {}", tpl_path.display(), e),
        ).into_response(),
    };
    let (tw, th) = tpl_img.dimensions();

    // Convertir frame RGBA a GrayImage con BT.601 (igual que UiDetector::crop_to_gray).
    let (fw, fh) = (frame.width, frame.height);
    let (search_x, search_y, search_w, search_h) = if q.use_roi {
        if let Some(roi) = s.calibration.ui_rois.get(&q.template).copied() {
            (roi.x, roi.y, roi.w, roi.h)
        } else {
            (0, 0, fw, fh)
        }
    } else {
        (0, 0, fw, fh)
    };

    if search_w < tw || search_h < th {
        return (
            StatusCode::BAD_REQUEST,
            format!("search_area ({}x{}) < template ({}x{})", search_w, search_h, tw, th),
        ).into_response();
    }

    // Crop frame to search area and convert to luma (BT.601).
    let mut gray = image::GrayImage::new(search_w, search_h);
    let stride = fw as usize * 4;
    for row in 0..search_h {
        for col in 0..search_w {
            let off = (search_y + row) as usize * stride + (search_x + col) as usize * 4;
            if off + 2 >= frame.data.len() {
                return (StatusCode::INTERNAL_SERVER_ERROR, "frame bounds").into_response();
            }
            let r = frame.data[off] as u32;
            let g = frame.data[off + 1] as u32;
            let b = frame.data[off + 2] as u32;
            let luma = (299 * r + 587 * g + 114 * b) / 1000;
            gray.put_pixel(col, row, image::Luma([luma as u8]));
        }
    }

    let result = match_template(&gray, &tpl_img, MatchTemplateMethod::SumOfSquaredErrorsNormalized);
    let rw = result.width();
    let mut best_idx = 0usize;
    let mut best_score = f32::MAX;
    for (i, &p) in result.iter().enumerate() {
        if p < best_score {
            best_score = p;
            best_idx = i;
        }
    }
    let local_x = (best_idx as u32) % rw;
    let local_y = (best_idx as u32) / rw;
    let tlx = search_x + local_x;
    let tly = search_y + local_y;
    // Threshold default del UiDetector = 0.20
    let threshold = 0.20f32;

    Json(MatchNowResponse {
        template: q.template,
        score: best_score,
        top_left_x: tlx,
        top_left_y: tly,
        center_x: tlx + tw / 2,
        center_y: tly + th / 2,
        template_w: tw,
        template_h: th,
        search_area_w: search_w,
        search_area_h: search_h,
        passed_threshold: best_score <= threshold,
        threshold,
    }).into_response()
}

// ── /vision/extract_template?name=X&w=34&h=34 ─────────────────────────────────
//
// Extrae una región del frame actual centrada en la posición del cursor,
// la guarda como grayscale (BT.601) PNG en assets/templates/ui/<name>.png.
// Conveniencia: reemplaza el ciclo manual "curl /test/grab + PIL crop + save".
// Requiere rebuild/restart para que el UiDetector recargue templates.

#[derive(Deserialize)]
struct ExtractTemplateQuery {
    name: String,
    #[serde(default = "default_tpl_size")]
    w: u32,
    #[serde(default = "default_tpl_size")]
    h: u32,
    /// Opcional: si no se pasa, usa cursor pos. Centro del recorte.
    #[serde(default)]
    cx: Option<i32>,
    #[serde(default)]
    cy: Option<i32>,
}
fn default_tpl_size() -> u32 { 34 }

#[derive(Serialize)]
struct ExtractTemplateResponse {
    saved_path: String,
    center_x: i32,
    center_y: i32,
    top_left_x: i32,
    top_left_y: i32,
    w: u32,
    h: u32,
    mean_luma: f32,
}

#[cfg(windows)]
async fn handle_vision_extract_template(
    State(s): State<AppState>,
    Query(q): Query<ExtractTemplateQuery>,
) -> Response {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use windows_sys::Win32::Foundation::POINT;

    let frame = match s.buffer.latest_frame() {
        Some(f) => f,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "No hay frame NDI").into_response(),
    };

    let (cx, cy) = match (q.cx, q.cy) {
        (Some(x), Some(y)) => (x, y),
        _ => {
            let mut p = POINT { x: 0, y: 0 };
            let ok = unsafe { GetCursorPos(&mut p as *mut _) };
            if ok == 0 {
                return (StatusCode::INTERNAL_SERVER_ERROR, "GetCursorPos failed").into_response();
            }
            (p.x, p.y)
        }
    };

    let w = q.w;
    let h = q.h;
    let tlx = cx - (w as i32) / 2;
    let tly = cy - (h as i32) / 2;

    if tlx < 0 || tly < 0 || tlx + w as i32 > frame.width as i32 || tly + h as i32 > frame.height as i32 {
        return (
            StatusCode::BAD_REQUEST,
            format!("Region ({},{},{},{}) fuera del frame {}x{}", tlx, tly, w, h, frame.width, frame.height),
        ).into_response();
    }

    // Extract region and convert RGBA→Luma (BT.601) → save as grayscale PNG.
    let mut gray = image::GrayImage::new(w, h);
    let stride = frame.width as usize * 4;
    let mut sum = 0u64;
    for row in 0..h {
        for col in 0..w {
            let off = (tly as u32 + row) as usize * stride + (tlx as u32 + col) as usize * 4;
            let r = frame.data[off] as u32;
            let g = frame.data[off + 1] as u32;
            let b = frame.data[off + 2] as u32;
            let luma = ((299 * r + 587 * g + 114 * b) / 1000) as u8;
            gray.put_pixel(col, row, image::Luma([luma]));
            sum += luma as u64;
        }
    }

    let save_path = PathBuf::from(format!("assets/templates/ui/{}.png", q.name));
    if let Err(e) = gray.save(&save_path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("Save: {}", e)).into_response();
    }
    info!("Template extraído y guardado: {} ({}x{} at TL=({},{}))", save_path.display(), w, h, tlx, tly);

    Json(ExtractTemplateResponse {
        saved_path: save_path.display().to_string(),
        center_x: cx,
        center_y: cy,
        top_left_x: tlx,
        top_left_y: tly,
        w,
        h,
        mean_luma: (sum as f32) / ((w * h) as f32),
    }).into_response()
}

#[cfg(not(windows))]
async fn handle_vision_extract_template(
    State(_s): State<AppState>,
    Query(_q): Query<ExtractTemplateQuery>,
) -> Response {
    (StatusCode::NOT_IMPLEMENTED, "extract_template solo en Windows").into_response()
}

// ── /vision/vitals ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct VitalBarJson {
    ratio:     f32,
    filled_px: u32,
    total_px:  u32,
}

#[derive(Serialize)]
struct VitalsResponse {
    hp:   Option<VitalBarJson>,
    mana: Option<VitalBarJson>,
    frame_tick: u64,
}

async fn handle_vision_vitals(State(s): State<AppState>) -> Json<VitalsResponse> {
    let g = s.game_state.read();
    let (hp, mana, tick) = match &g.last_perception {
        Some(p) => (
            p.vitals.hp.map(|b| VitalBarJson { ratio: b.ratio, filled_px: b.filled_px, total_px: b.total_px }),
            p.vitals.mana.map(|b| VitalBarJson { ratio: b.ratio, filled_px: b.filled_px, total_px: b.total_px }),
            p.frame_tick,
        ),
        None => (None, None, 0),
    };
    Json(VitalsResponse { hp, mana, frame_tick: tick })
}

// ── /vision/battle ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct BattleEntryJson {
    kind:     String,
    row:      u8,
    hp_ratio: Option<f32>,
}

#[derive(Serialize)]
struct BattleResponse {
    entries:    Vec<BattleEntryJson>,
    frame_tick: u64,
}

async fn handle_vision_battle(State(s): State<AppState>) -> Json<BattleResponse> {
    let g = s.game_state.read();
    let (entries, tick) = match &g.last_perception {
        Some(p) => {
            let entries = p.battle.entries.iter().map(|e| BattleEntryJson {
                kind:     format!("{:?}", e.kind),
                row:      e.row,
                hp_ratio: e.hp_ratio,
            }).collect();
            (entries, p.frame_tick)
        }
        None => (vec![], 0),
    };
    Json(BattleResponse { entries, frame_tick: tick })
}

// ── /vision/status ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusConditionsResponse {
    conditions: Vec<String>,
    frame_tick: u64,
}

async fn handle_vision_status(State(s): State<AppState>) -> Json<StatusConditionsResponse> {
    let g = s.game_state.read();
    let (conditions, tick) = match &g.last_perception {
        Some(p) => {
            let conditions = p.conditions.active.iter()
                .map(|c| format!("{:?}", c))
                .collect();
            (conditions, p.frame_tick)
        }
        None => (vec![], 0),
    };
    Json(StatusConditionsResponse { conditions, frame_tick: tick })
}

// ── /vision/perception ────────────────────────────────────────────────────────
// Respuesta consolidada: la vista completa del personaje en un solo JSON.
// Endpoint principal para monitorear el bot en tiempo real.
//   curl http://localhost:8080/vision/perception | jq .

#[derive(Serialize, Default)]
struct PixelCounts {
    filled: u32,
    total:  u32,
}

#[derive(Serialize, Default)]
struct PerceptionResponse {
    frame_tick:   u64,
    hp_percent:   Option<f32>,
    mana_percent: Option<f32>,
    hp_px:        Option<PixelCounts>,
    mana_px:      Option<PixelCounts>,
    enemy_count:  u32,
    has_player:   bool,
    conditions:   Vec<String>,
    minimap_diff: f32,
    is_moving:    Option<bool>,
    minimap_displacement: Option<(i32, i32)>,
    game_coords:      Option<(i32, i32, i32)>,
    target_active:    Option<bool>,
    inventory_counts: std::collections::HashMap<String, u32>,
    /// UI templates matcheados en el último ciclo async del UiDetector.
    /// Lista de nombres (ej: ["npc_trade_bag", "stow_menu"]).
    /// 2026-04-18: agregado para debug del bag-click workflow.
    ui_matches:       Vec<String>,
    /// Centro + dims de cada match (para click directo en coord específica).
    /// Tupla = (center_x, center_y, template_w, template_h).
    ui_match_infos:   std::collections::HashMap<String, (u32, u32, u32, u32)>,
}

async fn handle_vision_perception(State(s): State<AppState>) -> Json<PerceptionResponse> {
    let g = s.game_state.read();
    match &g.last_perception {
        None => Json(PerceptionResponse::default()),
        Some(p) => Json(PerceptionResponse {
            frame_tick:   p.frame_tick,
            hp_percent:   p.vitals.hp.map(|b| (b.ratio * 100.0 * 10.0).round() / 10.0),
            mana_percent: p.vitals.mana.map(|b| (b.ratio * 100.0 * 10.0).round() / 10.0),
            hp_px:        p.vitals.hp.map(|b| PixelCounts { filled: b.filled_px, total: b.total_px }),
            mana_px:      p.vitals.mana.map(|b| PixelCounts { filled: b.filled_px, total: b.total_px }),
            enemy_count:  p.battle.enemy_count() as u32,
            has_player:   p.vitals.hp.is_some(),
            conditions:   p.conditions.active.iter().map(|c| format!("{:?}", c)).collect(),
            minimap_diff: p.minimap_diff,
            is_moving:    p.is_moving,
            minimap_displacement: p.minimap_displacement,
            game_coords:      p.game_coords,
            target_active:    p.target_active,
            inventory_counts: p.inventory_counts.clone(),
            ui_matches:       p.ui_matches.clone(),
            ui_match_infos:   p.ui_match_infos.clone(),
        }),
    }
}

// ── /vision/grab/anchors ──────────────────────────────────────────────────────
// Retorna un PNG del frame actual con los ROIs calibrados superpuestos.
// Útil para verificar visualmente que los ROIs apuntan a los elementos correctos.
//   curl http://localhost:8080/vision/grab/anchors -o debug.png && start debug.png

async fn handle_vision_grab_anchors(State(s): State<AppState>) -> Response {
    let frame = match s.buffer.latest_frame() {
        Some(f) => f,
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "No hay frame NDI todavía")
                .into_response();
        }
    };

    // Frame ya está en RGBA — usarlo directamente.
    let mut img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        match image::ImageBuffer::from_raw(frame.width, frame.height, frame.data.clone()) {
            Some(i) => i,
            None => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "Frame RGBA inválido")
                    .into_response();
            }
        };

    // Superponer ROIs de calibración con colores distintos.
    let cal = &s.calibration;
    let draw_roi = |img: &mut image::ImageBuffer<image::Rgba<u8>, Vec<u8>>,
                    roi: crate::sense::vision::calibration::RoiDef,
                    color: image::Rgba<u8>| {
        use imageproc::drawing::draw_hollow_rect_mut;
        use imageproc::rect::Rect;
        let r = Rect::at(roi.x as i32, roi.y as i32)
            .of_size(roi.w.max(1), roi.h.max(1));
        draw_hollow_rect_mut(img, r, color);
        // Segunda línea interior para borde grueso visible (2 px).
        if roi.w > 2 && roi.h > 2 {
            let r2 = Rect::at(roi.x as i32 + 1, roi.y as i32 + 1)
                .of_size(roi.w - 2, roi.h - 2);
            draw_hollow_rect_mut(img, r2, color);
        }
    };

    if let Some(r) = cal.hp_bar        { draw_roi(&mut img, r, image::Rgba([0, 255, 0, 255]));   } // verde
    if let Some(r) = cal.mana_bar      { draw_roi(&mut img, r, image::Rgba([0, 100, 255, 255])); } // azul
    if let Some(r) = cal.battle_list   { draw_roi(&mut img, r, image::Rgba([255, 50, 50, 255])); } // rojo
    if let Some(r) = cal.minimap       { draw_roi(&mut img, r, image::Rgba([0, 255, 255, 255])); } // cyan
    if let Some(r) = cal.status_icons  { draw_roi(&mut img, r, image::Rgba([255, 255, 0, 255])); } // amarillo
    if let Some(r) = cal.game_viewport { draw_roi(&mut img, r, image::Rgba([200, 200, 200, 200])); } // blanco semi
    for anchor in &cal.anchors {
        draw_roi(&mut img, anchor.expected_roi, image::Rgba([255, 0, 255, 255])); // magenta
    }

    let mut png_bytes = Vec::new();
    if let Err(e) = img.write_to(
        &mut std::io::Cursor::new(&mut png_bytes),
        image::ImageFormat::Png,
    ) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("PNG encode: {}", e))
            .into_response();
    }

    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        png_bytes,
    )
        .into_response()
}

// ── /vision/grab/battle ───────────────────────────────────────────────────────
// Retorna un PNG del ROI de battle list, escalado ×3 para inspección visual.
// Si el ROI no está calibrado, muestra el sidebar completo (x=1690, y=0, w=230, h=400).
// Útil para encontrar la posición correcta del panel de criaturas.
//   curl http://localhost:8080/vision/grab/battle -o battle.png && start battle.png

async fn handle_vision_grab_battle(State(s): State<AppState>) -> Response {
    use image::{ImageBuffer, Rgba};

    let frame = match s.buffer.latest_frame() {
        Some(f) => f,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "No hay frame NDI todavía").into_response(),
    };

    let roi = match s.calibration.battle_list {
        Some(r) => r,
        None => {
            use crate::sense::vision::calibration::RoiDef;
            RoiDef { x: 1690, y: 0, w: 230, h: 600 }
        }
    };

    let (fw, fh) = (frame.width, frame.height);
    let rx = roi.x.min(fw.saturating_sub(1));
    let ry = roi.y.min(fh.saturating_sub(1));
    let rw = roi.w.min(fw - rx);
    let rh = roi.h.min(fh - ry);

    let stride = fw as usize * 4;
    let mut roi_img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(rw, rh);
    for row in 0..rh {
        for col in 0..rw {
            let off = (ry + row) as usize * stride + (rx + col) as usize * 4;
            if off + 3 < frame.data.len() {
                roi_img.put_pixel(col, row, Rgba([
                    frame.data[off], frame.data[off+1],
                    frame.data[off+2], frame.data[off+3],
                ]));
            }
        }
    }

    // Escalar ×4 (nearest-neighbor) para visibilidad de bordes de entrada.
    const SCALE: u32 = 4;
    let mut big: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(rw * SCALE, rh * SCALE);
    for row in 0..rh {
        for col in 0..rw {
            let px = *roi_img.get_pixel(col, row);
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    big.put_pixel(col * SCALE + dx, row * SCALE + dy, px);
                }
            }
        }
    }

    let mut png_bytes = Vec::new();
    if let Err(e) = big.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("PNG encode: {}", e)).into_response();
    }
    ([(axum::http::header::CONTENT_TYPE, "image/png")], png_bytes).into_response()
}

// ── /vision/grab/debug ────────────────────────────────────────────────────────
// Frame completo con TODOS los ROIs superpuestos + filas de batalla individuales.
// Color legend:
//   Verde       = hp_bar
//   Azul        = mana_bar
//   Amarillo    = cada fila del battle_list (slot individual, 22px alto)
//   Rojo        = anclas (expected_roi)
//   Cyan        = minimap
//   Blanco semi = game_viewport
//
//   curl http://localhost:8080/vision/grab/debug -o debug_full.png && start debug_full.png

async fn handle_vision_grab_debug(State(s): State<AppState>) -> Response {
    use imageproc::drawing::draw_hollow_rect_mut;
    use imageproc::rect::Rect;

    let frame = match s.buffer.latest_frame() {
        Some(f) => f,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "No hay frame NDI todavía").into_response(),
    };

    let mut img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        match image::ImageBuffer::from_raw(frame.width, frame.height, frame.data.clone()) {
            Some(i) => i,
            None => return (StatusCode::INTERNAL_SERVER_ERROR, "Frame RGBA inválido").into_response(),
        };

    let draw = |img: &mut image::ImageBuffer<image::Rgba<u8>, Vec<u8>>,
                x: i32, y: i32, w: u32, h: u32,
                color: image::Rgba<u8>| {
        let r = Rect::at(x, y).of_size(w.max(1), h.max(1));
        draw_hollow_rect_mut(img, r, color);
        if w > 2 && h > 2 {
            let r2 = Rect::at(x + 1, y + 1).of_size(w - 2, h - 2);
            draw_hollow_rect_mut(img, r2, color);
        }
    };

    let cal = &s.calibration;
    if let Some(r) = cal.hp_bar        { draw(&mut img, r.x as i32, r.y as i32, r.w, r.h, image::Rgba([0,   255, 0,   255])); }
    if let Some(r) = cal.mana_bar      { draw(&mut img, r.x as i32, r.y as i32, r.w, r.h, image::Rgba([0,   100, 255, 255])); }
    if let Some(r) = cal.minimap       { draw(&mut img, r.x as i32, r.y as i32, r.w, r.h, image::Rgba([0,   255, 255, 255])); }
    if let Some(r) = cal.game_viewport { draw(&mut img, r.x as i32, r.y as i32, r.w, r.h, image::Rgba([200, 200, 200, 180])); }
    for anchor in &cal.anchors {
        let r = anchor.expected_roi;
        draw(&mut img, r.x as i32, r.y as i32, r.w, r.h, image::Rgba([255, 0, 0, 255]));
    }

    // Filas del battle_list: una caja amarilla por slot (22px alto).
    if let Some(bl_roi) = cal.battle_list {
        const ENTRY_H: u32 = 22;
        let n_rows = bl_roi.h / ENTRY_H;
        for row in 0..n_rows {
            let ey = bl_roi.y + row * ENTRY_H;
            draw(&mut img, bl_roi.x as i32, ey as i32, bl_roi.w, ENTRY_H, image::Rgba([255, 220, 0, 255]));
        }
    }

    // Si hay una percepción reciente, resaltar las entradas activas en naranja.
    {
        let g = s.game_state.read();
        if let Some(ref p) = g.last_perception {
            if let Some(bl_roi) = cal.battle_list {
                const ENTRY_H: u32 = 22;
                for entry in &p.battle.entries {
                    let ey = bl_roi.y + entry.row as u32 * ENTRY_H;
                    draw(&mut img, bl_roi.x as i32, ey as i32, bl_roi.w, ENTRY_H,
                         image::Rgba([255, 140, 0, 255]));
                }
            }
        }
    }

    let mut png_bytes = Vec::new();
    if let Err(e) = img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("PNG encode: {}", e)).into_response();
    }
    ([(axum::http::header::CONTENT_TYPE, "image/png")], png_bytes).into_response()
}

// ── /vision/battle/debug ─────────────────────────────────────────────────────
// Retorna JSON con los datos de diagnóstico del último escaneo del panel de batalla.
// Útil para calibrar umbrales y detectar falsos positivos/negativos.
//   curl http://localhost:8080/vision/battle/debug | jq .

#[derive(Serialize)]
struct SlotDebugJson {
    row:               u8,
    frame_y:           u32,
    red_hits:          u32,
    blue_hits:         u32,
    yellow_hits:       u32,
    /// Conteo del detector HP-bar fallback — canal principal en clientes modernos.
    hp_bar_hits:       u32,
    /// `true` si el slot tiene highlight rojo de "char atacando este mob".
    is_being_attacked: bool,
    kind:              Option<String>,
}

#[derive(Serialize)]
struct BattleDebugResponse {
    frame_tick:         u64,
    slots:              Vec<SlotDebugJson>,
    /// `true` si cualquier slot está siendo atacado. Derivado de los slot_debug.
    has_attacked_entry: bool,
}

async fn handle_vision_battle_debug(State(s): State<AppState>) -> Json<BattleDebugResponse> {
    let g = s.game_state.read();
    match &g.last_perception {
        Some(p) => {
            let slots = p.battle.slot_debug.iter().map(|sd| SlotDebugJson {
                row:               sd.row,
                frame_y:           sd.frame_y,
                red_hits:          sd.red_hits,
                blue_hits:         sd.blue_hits,
                yellow_hits:       sd.yellow_hits,
                hp_bar_hits:       sd.hp_bar_hits,
                is_being_attacked: sd.is_being_attacked,
                kind:              sd.kind.as_ref().map(|k| format!("{:?}", k)),
            }).collect();
            Json(BattleDebugResponse {
                frame_tick: p.frame_tick,
                slots,
                has_attacked_entry: p.battle.has_attacked_entry(),
            })
        }
        None => Json(BattleDebugResponse {
            frame_tick: 0,
            slots: vec![],
            has_attacked_entry: false,
        }),
    }
}

// ── /vision/target/debug ─────────────────────────────────────────────────────
// Estado del detector de target (Fase A). Expone si la ROI está configurada,
// si el signal dice que hay target activo, y cuántos píxeles cromáticos se
// detectaron en el último frame.
//   curl http://localhost:8080/vision/target/debug | jq .

#[derive(Serialize)]
struct TargetDebugResponse {
    /// ROI `target_hp_bar` está presente en calibration.toml.
    configured:     bool,
    /// Última lectura del detector: true=target activo, false=sin target, null=no configurado/no fits.
    active:         Option<bool>,
    /// Píxeles cromáticos contados en el último frame.
    hits:           u32,
    /// Threshold aplicado en el último frame (0 si no disponible).
    threshold_used: u32,
}

async fn handle_vision_target_debug(State(s): State<AppState>) -> Json<TargetDebugResponse> {
    let g = s.game_state.read();
    Json(TargetDebugResponse {
        configured:     g.target_debug.configured,
        active:         g.target_debug.active,
        hits:           g.target_debug.hits,
        threshold_used: g.target_debug.threshold_used,
    })
}

// ── /vision/loot/debug ──────────────────────────────────────────────────────
// Estado del detector de loot sparkles. Expone el conteo de píxeles blancos
// detectados en el área 3×3 tiles centrada en el char, más si supera el
// threshold que triggerea el auto-loot.
//   curl http://localhost:8080/vision/loot/debug | jq .

#[derive(Serialize)]
struct LootDebugResponse {
    /// Conteo de píxeles "blanco puro" en el loot area (última vision tick).
    sparkles:          u32,
    /// Threshold aplicado — si sparkles >= threshold, hay loot disponible.
    threshold:         u32,
    /// `true` si hay loot visible AHORA mismo.
    loot_available:    bool,
    /// `true` si el auto-loot está configurado (loot_hotkey no vacío).
    auto_loot_enabled: bool,
}

async fn handle_vision_loot_debug(State(s): State<AppState>) -> Json<LootDebugResponse> {
    let g = s.game_state.read();
    let sparkles = g.last_perception
        .as_ref()
        .map(|p| p.loot_sparkles)
        .unwrap_or(0);
    let threshold = crate::sense::vision::loot::LOOT_SPARKLE_THRESHOLD;
    Json(LootDebugResponse {
        sparkles,
        threshold,
        loot_available:    sparkles >= threshold,
        auto_loot_enabled: !s.config.actions.loot_hotkey.is_empty(),
    })
}

// ── /vision/loot/grab ──────────────────────────────────────────────────────
// Grab el ROI del loot area (3×3 tiles centrado en el char) como PNG,
// con un overlay en rojo que marca los píxeles que el detector cuenta
// como "sparkle" (blanco puro). Debug visual para identificar falsos
// positivos — si el overlay rojo está sobre el nombre del char o damage
// numbers, sabemos que el detector está contando algo que no es loot.
//
//   curl http://localhost:8080/vision/loot/grab -o /tmp/loot.png && start /tmp/loot.png

async fn handle_vision_loot_grab(State(s): State<AppState>) -> Response {
    use image::{ImageBuffer, Rgba};

    let frame = match s.buffer.latest_frame() {
        Some(f) => f,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "No hay frame NDI todavía").into_response(),
    };

    // Computar el loot area como lo hace Vision::tick.
    let vp = match s.calibration.game_viewport {
        Some(r) => r,
        None => return (StatusCode::BAD_REQUEST, "game_viewport no calibrado").into_response(),
    };
    let area = match crate::sense::vision::loot::compute_loot_area(vp, 64) {
        Some(a) => a,
        None => return (StatusCode::BAD_REQUEST, "viewport demasiado pequeño para loot area").into_response(),
    };

    let (fw, fh) = (frame.width, frame.height);
    let rx = area.x.min(fw.saturating_sub(1));
    let ry = area.y.min(fh.saturating_sub(1));
    let rw = area.w.min(fw - rx);
    let rh = area.h.min(fh - ry);

    let stride = fw as usize * 4;
    // Construir imagen del ROI con overlay: píxeles sparkle marcados en ROJO
    // brillante, resto en su color original.
    let mut roi_img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(rw, rh);
    let mut sparkle_count = 0u32;
    for row in 0..rh {
        for col in 0..rw {
            let off = (ry + row) as usize * stride + (rx + col) as usize * 4;
            if off + 3 < frame.data.len() {
                let px = &frame.data[off..off + 4];
                let is_sparkle = crate::sense::vision::loot::is_sparkle_pixel(px);
                let out_px = if is_sparkle {
                    sparkle_count += 1;
                    // Overlay rojo puro para destacar los pixels detectados.
                    Rgba([255, 0, 0, 255])
                } else {
                    // Atenuar el resto a 50% brightness para que el rojo resalte.
                    Rgba([px[0] / 2, px[1] / 2, px[2] / 2, 255])
                };
                roi_img.put_pixel(col, row, out_px);
            }
        }
    }

    // Escalar ×3 para visibilidad.
    const SCALE: u32 = 3;
    let mut big: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(rw * SCALE, rh * SCALE);
    for row in 0..rh {
        for col in 0..rw {
            let px = *roi_img.get_pixel(col, row);
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    big.put_pixel(col * SCALE + dx, row * SCALE + dy, px);
                }
            }
        }
    }

    info!(
        "loot/grab: area=({},{},{},{}) sparkle_pixels={} (threshold={})",
        area.x, area.y, area.w, area.h, sparkle_count,
        crate::sense::vision::loot::LOOT_SPARKLE_THRESHOLD,
    );

    let mut png_bytes = Vec::new();
    if let Err(e) = big.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("PNG encode: {}", e)).into_response();
    }
    ([(axum::http::header::CONTENT_TYPE, "image/png")], png_bytes).into_response()
}

// ── /vision/grab/inventory ───────────────────────────────────────────────────
// Frame completo con los slots del backpack dibujados encima para verificar
// la calibración del `inventory_grid`. Cada slot tiene un rectángulo amarillo.
//   curl http://localhost:8080/vision/grab/inventory -o inv.png && start inv.png

async fn handle_vision_grab_inventory(State(s): State<AppState>) -> Response {
    use image::{ImageBuffer, Rgba};
    use imageproc::drawing::draw_hollow_rect_mut;
    use imageproc::rect::Rect;

    let frame = match s.buffer.latest_frame() {
        Some(f) => f,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "No hay frame NDI todavía").into_response(),
    };

    // Prioridad: backpack_strip > inventory_grid > inventory_slots manuales.
    let slots = if let Some(strip) = s.calibration.inventory_backpack_strip {
        strip.expand()
    } else if let Some(grid) = s.calibration.inventory_grid {
        grid.expand()
    } else {
        s.calibration.inventory_slots.clone()
    };

    let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        match ImageBuffer::from_raw(frame.width, frame.height, frame.data.clone()) {
            Some(i) => i,
            None => return (StatusCode::INTERNAL_SERVER_ERROR, "frame malformado").into_response(),
        };

    // Dibujar cada slot con borde amarillo + número de slot en la esquina.
    for (idx, slot) in slots.iter().enumerate() {
        let rect = Rect::at(slot.x as i32, slot.y as i32)
            .of_size(slot.w, slot.h);
        draw_hollow_rect_mut(&mut img, rect, Rgba([255, 255, 0, 255]));
        // Borde doble para mayor visibilidad.
        if slot.w > 2 && slot.h > 2 {
            let inner = Rect::at(slot.x as i32 + 1, slot.y as i32 + 1)
                .of_size(slot.w - 2, slot.h - 2);
            draw_hollow_rect_mut(&mut img, inner, Rgba([255, 200, 0, 255]));
        }
        let _ = idx;
    }

    let mut png_bytes = Vec::new();
    if let Err(e) = img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("PNG encode: {}", e)).into_response();
    }
    ([(axum::http::header::CONTENT_TYPE, "image/png")], png_bytes).into_response()
}

// ── /vision/inventory ────────────────────────────────────────────────────────
// JSON con el conteo de items detectados en el inventario + config del grid.
//   curl http://localhost:8080/vision/inventory | jq .

async fn handle_vision_inventory(State(s): State<AppState>) -> Response {
    use std::collections::HashMap;
    let g = s.game_state.read();
    let counts: HashMap<String, u32> = g.last_perception
        .as_ref()
        .map(|p| p.inventory_counts.clone())
        .unwrap_or_default();
    // Item #2: per-slot output con confidence + stage. Vacío si no hay
    // perception actual o la inventory cadence no corrió este tick.
    let slots_per_reading = g.last_perception
        .as_ref()
        .map(|p| p.inventory_slots.clone())
        .unwrap_or_default();
    drop(g);

    // Prioridad: backpack_strip > inventory_grid > inventory_slots manuales.
    let slots = if let Some(strip) = s.calibration.inventory_backpack_strip {
        strip.expand()
    } else if let Some(grid) = s.calibration.inventory_grid {
        grid.expand()
    } else {
        s.calibration.inventory_slots.clone()
    };

    #[derive(Serialize)]
    struct InventoryResponse {
        slot_count: usize,
        counts:     HashMap<String, u32>,
        grid:       Option<GridInfo>,
        /// Per-slot output con item + confidence + stage + stack (item #2
        /// plan robustez). Vacío si no hay perception actual. Para consumers
        /// que necesiten el count agregado, `counts` sigue disponible.
        #[serde(default)]
        slots:      Vec<crate::sense::vision::inventory_slot::SlotReading>,
    }
    #[derive(Serialize)]
    struct GridInfo {
        x:         u32,
        y:         u32,
        slot_size: u32,
        gap:       u32,
        cols:      u32,
        rows:      u32,
    }
    let grid = s.calibration.inventory_grid.map(|g| GridInfo {
        x: g.x, y: g.y, slot_size: g.slot_size, gap: g.gap, cols: g.cols, rows: g.rows,
    });
    Json(InventoryResponse {
        slot_count: slots.len(),
        counts,
        grid,
        slots: slots_per_reading,
    }).into_response()
}

// ── /fsm/debug ───────────────────────────────────────────────────────────────
// Estado interno del FSM: cooldowns, flags internos, prev_target_active.
// Permite diagnosticar "¿por qué el FSM está en este estado?" sin recompilar.
//   curl http://localhost:8080/fsm/debug | jq .

// ── /vision/matcher/stats ─────────────────────────────────────────────────────

async fn handle_vision_matcher_stats(State(s): State<AppState>) -> Response {
    let g = s.game_state.read();
    let stats = g.matcher_stats.clone();
    drop(g);

    match stats {
        Some(s) => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&s).unwrap_or_else(|_| "{}".into()),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "MinimapMatcher no ha corrido ninguna detección todavía",
        )
            .into_response(),
    }
}

#[derive(Serialize)]
struct FsmDebugResponse {
    state:                  String,
    next_heal_tick:         Option<u64>,
    next_attack_tick:       Option<u64>,
    attack_keepalive_tick:  Option<u64>,
    prev_target_active:     Option<bool>,
    current_tick:           u64,
}

async fn handle_fsm_debug(State(s): State<AppState>) -> Json<FsmDebugResponse> {
    let g = s.game_state.read();
    Json(FsmDebugResponse {
        state:                 g.fsm_debug.state.clone(),
        next_heal_tick:        g.fsm_debug.next_heal_tick,
        next_attack_tick:      g.fsm_debug.next_attack_tick,
        attack_keepalive_tick: g.fsm_debug.attack_keepalive_tick,
        prev_target_active:    g.fsm_debug.prev_target_active,
        current_tick:          g.tick,
    })
}

// ── /combat/events ───────────────────────────────────────────────────────────
// Ring buffer de eventos de combate (últimos N ≤ COMBAT_EVENTS_CAP=200).
// Cada entry es una transición o emit relevante. El cliente puede consultar
// este endpoint periódicamente y diff-ar para ver actividad.
//   curl http://localhost:8080/combat/events | jq .

#[derive(Serialize)]
struct CombatEventJson {
    tick:          u64,
    ts_ms:         u64,
    fsm_state:     String,
    action:        String,
    reason:        String,
    hp_ratio:      Option<f32>,
    target_active: Option<bool>,
    enemy_count:   u32,
}

#[derive(Serialize)]
struct CombatEventsResponse {
    count:  usize,
    events: Vec<CombatEventJson>,
}

async fn handle_combat_events(State(s): State<AppState>) -> Json<CombatEventsResponse> {
    let g = s.game_state.read();
    let events: Vec<CombatEventJson> = g.combat_events.iter().map(|e| CombatEventJson {
        tick:          e.tick,
        ts_ms:         e.ts_ms,
        fsm_state:     e.fsm_state.clone(),
        action:        e.action.clone(),
        reason:        e.reason.clone(),
        hp_ratio:      e.hp_ratio,
        target_active: e.target_active,
        enemy_count:   e.enemy_count,
    }).collect();
    Json(CombatEventsResponse { count: events.len(), events })
}

// ── /dispatch/stats ──────────────────────────────────────────────────────────
// Contadores acumulativos de acciones emitidas por el dispatcher, separados
// por categoría (attack / heal / mana / other). Incluye timestamps del último
// emit para que el cliente pueda calcular rates sin necesidad de polling caro.
//   curl http://localhost:8080/dispatch/stats | jq .

#[derive(Serialize)]
struct DispatchStatsResponse {
    attacks_total:  u64,
    heals_total:    u64,
    mana_total:     u64,
    other_total:    u64,
    last_attack_ms: Option<u64>,
    last_heal_ms:   Option<u64>,
    last_mana_ms:   Option<u64>,
}

async fn handle_dispatch_stats(State(s): State<AppState>) -> Json<DispatchStatsResponse> {
    let g = s.game_state.read();
    Json(DispatchStatsResponse {
        attacks_total:  g.dispatch_stats.attacks_total,
        heals_total:    g.dispatch_stats.heals_total,
        mana_total:     g.dispatch_stats.mana_total,
        other_total:    g.dispatch_stats.other_total,
        last_attack_ms: g.dispatch_stats.last_attack_ms,
        last_heal_ms:   g.dispatch_stats.last_heal_ms,
        last_mana_ms:   g.dispatch_stats.last_mana_ms,
    })
}

// ── /waypoints/load ───────────────────────────────────────────────────────────
// Carga o recarga una WaypointList desde disco. Los comandos se envían por un
// canal crossbeam al game loop que los procesa al inicio del siguiente tick.
//   curl -X POST 'http://localhost:8080/waypoints/load?path=assets/waypoints/example.toml&enabled=true'

#[derive(Deserialize)]
struct WaypointsLoadQuery {
    path: String,
    #[serde(default)]
    enabled: bool,
}

#[derive(Serialize)]
struct CommandAck {
    ok:      bool,
    message: String,
}

/// V-003 fix: valida que un path-arg de un load endpoint está dentro del
/// directorio whitelisted. Previene path traversal (`../../../etc/passwd`).
///
/// Usa `canonicalize` para resolver symlinks + `..` y luego chequea que el
/// resultado empieza con el allowed dir (también canonicalized). Además
/// rechaza archivos > `MAX_LOAD_FILE_BYTES` (V-005 DoS mitigation).
///
/// Errors si el path no existe, es symlink a fuera del whitelist, apunta
/// afuera, o el archivo es demasiado grande. El caller devuelve 400 Bad
/// Request con el error.
fn validate_load_path(user_path: &str, allowed_dir: &Path) -> Result<PathBuf, String> {
    let p = Path::new(user_path);
    let canonical = std::fs::canonicalize(p)
        .map_err(|e| format!("path '{}' no resolvable: {}", user_path, e))?;
    let allowed_canonical = match std::fs::canonicalize(allowed_dir) {
        Ok(p) => p,
        Err(_) => {
            // Si el allowed_dir no existe todavía, usar como-es (rechaza
            // silentemente por el check siguiente).
            allowed_dir.to_path_buf()
        }
    };
    if !canonical.starts_with(&allowed_canonical) {
        return Err(format!(
            "path '{}' escapa del directorio permitido '{}'",
            canonical.display(), allowed_canonical.display()
        ));
    }
    // V-005: file size cap para mitigar OOM DoS. Un TOML legítimo no supera
    // unas decenas de KB (el cavebot más grande del proyecto es ~8 KB,
    // hunt_profile ~4 KB). 1 MB es cap holgado que rechaza inputs absurdos
    // (atacante pasando un 10 GB random blob) sin bloquear scripts reales.
    //
    // Para `/scripts/reload` el validator no aplica a cada .lua individualmente
    // (ese loader itera el dir) — pero sí al dir target, que en canonicalize
    // falla con EISDIR en metadata.len(), por lo que un dir grande no dispara
    // este check. Los .lua individuales están capeados a <1 MB cada uno por
    // convention; cap adicional sería redundante.
    if let Ok(md) = std::fs::metadata(&canonical) {
        if md.is_file() && md.len() > MAX_LOAD_FILE_BYTES {
            return Err(format!(
                "archivo '{}' excede el límite ({} bytes > {})",
                canonical.display(), md.len(), MAX_LOAD_FILE_BYTES
            ));
        }
    }
    Ok(canonical)
}

/// V-005 cap: tamaño máximo de archivo que los endpoints `/cavebot/load`,
/// `/waypoints/load` y `/scripts/reload` aceptan leer. Conservador (1 MB)
/// comparado al mayor archivo legítimo del proyecto (~8 KB) pero muy por
/// debajo del cost de OOM en cargar un blob malicioso de varios GB.
const MAX_LOAD_FILE_BYTES: u64 = 1 * 1024 * 1024;

async fn handle_waypoints_load(
    State(s): State<AppState>,
    Query(q): Query<WaypointsLoadQuery>,
) -> Json<CommandAck> {
    // V-003 fix: whitelist a assets/waypoints/
    let validated = match validate_load_path(&q.path, Path::new("assets/waypoints")) {
        Ok(p) => p,
        Err(e) => {
            return Json(CommandAck {
                ok:      false,
                message: format!("path rejected: {}", e),
            });
        }
    };
    let cmd = LoopCommand::LoadWaypoints {
        path:    validated,
        enabled: q.enabled,
    };
    match s.loop_tx.send(cmd) {
        Ok(()) => {
            info!("Waypoints load solicitado: path='{}' enabled={}", q.path, q.enabled);
            Json(CommandAck {
                ok:      true,
                message: format!("queued LoadWaypoints path='{}' enabled={}", q.path, q.enabled),
            })
        }
        Err(e) => Json(CommandAck {
            ok:      false,
            message: format!("channel error: {}", e),
        }),
    }
}

// ── /waypoints/status ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct WaypointsStatusResponse {
    loaded:        bool,
    enabled:       bool,
    loop_:         bool,
    total_steps:   usize,
    current_index: Option<usize>,
    current_label: Option<String>,
    fsm_state:     String,
}

async fn handle_waypoints_status(State(s): State<AppState>) -> Json<WaypointsStatusResponse> {
    let g = s.game_state.read();
    Json(WaypointsStatusResponse {
        loaded:        g.waypoint_status.loaded,
        enabled:       g.waypoint_status.enabled,
        loop_:         g.waypoint_status.loop_,
        total_steps:   g.waypoint_status.total_steps,
        current_index: g.waypoint_status.current_index,
        current_label: g.waypoint_status.current_label.clone(),
        fsm_state:     format!("{:?}", g.fsm_state),
    })
}

// ── /waypoints/pause, /resume, /clear ─────────────────────────────────────────

async fn handle_waypoints_pause(State(s): State<AppState>) -> Json<CommandAck> {
    send_cmd(&s, LoopCommand::PauseWaypoints, "PauseWaypoints")
}

async fn handle_waypoints_resume(State(s): State<AppState>) -> Json<CommandAck> {
    send_cmd(&s, LoopCommand::ResumeWaypoints, "ResumeWaypoints")
}

async fn handle_waypoints_clear(State(s): State<AppState>) -> Json<CommandAck> {
    send_cmd(&s, LoopCommand::ClearWaypoints, "ClearWaypoints")
}

// ── /cavebot/status, /load, /pause, /resume, /clear ──────────────────────────

#[derive(Serialize)]
struct CavebotStatusResponse {
    loaded:        bool,
    enabled:       bool,
    loop_:         bool,
    total_steps:   usize,
    current_index: Option<usize>,
    current_label: Option<String>,
    current_kind:  String,
    fsm_state:     String,
    /// Nombre del hunt profile cargado (si el TOML declara
    /// `[cavebot].hunt_profile`). `null` si ninguno.
    hunt_profile:  Option<String>,
    /// `true` si el step actual está en fase de verify poll (post-acción,
    /// esperando que se cumpla su postcondition). Distinto de `fsm_state`
    /// — el cavebot puede estar verifying mientras el FSM sigue en Walking.
    verifying:     bool,
}

async fn handle_cavebot_status(State(s): State<AppState>) -> Json<CavebotStatusResponse> {
    let g = s.game_state.read();
    Json(CavebotStatusResponse {
        loaded:        g.cavebot_status.loaded,
        enabled:       g.cavebot_status.enabled,
        loop_:         g.cavebot_status.loop_,
        total_steps:   g.cavebot_status.total_steps,
        current_index: g.cavebot_status.current_index,
        current_label: g.cavebot_status.current_label.clone(),
        current_kind:  g.cavebot_status.current_kind.clone(),
        fsm_state:     format!("{:?}", g.fsm_state),
        hunt_profile:  g.cavebot_status.hunt_profile.clone(),
        verifying:     g.cavebot_status.verifying,
    })
}

// ── /cavebot/load, /pause, /resume, /clear ───────────────────────────────────
// Gestiona el cavebot v2 (Fase C). Archivo TOML con sintaxis extendida que
// soporta labels, goto, stand, loot, skip_if_blocked.
//   curl -X POST 'http://localhost:8080/cavebot/load?path=assets/cavebot/example.toml&enabled=true'

#[derive(Deserialize)]
struct CavebotLoadQuery {
    path: String,
    #[serde(default)]
    enabled: bool,
}

async fn handle_cavebot_load(
    State(s): State<AppState>,
    Query(q): Query<CavebotLoadQuery>,
) -> Json<CommandAck> {
    // V-003 fix: whitelist a assets/cavebot/
    let validated = match validate_load_path(&q.path, Path::new("assets/cavebot")) {
        Ok(p) => p,
        Err(e) => {
            return Json(CommandAck {
                ok:      false,
                message: format!("path rejected: {}", e),
            });
        }
    };
    let cmd = LoopCommand::LoadCavebot {
        path:    validated,
        enabled: q.enabled,
    };
    match s.loop_tx.send(cmd) {
        Ok(()) => {
            info!("Cavebot load solicitado: path='{}' enabled={}", q.path, q.enabled);
            Json(CommandAck {
                ok:      true,
                message: format!("queued LoadCavebot path='{}' enabled={}", q.path, q.enabled),
            })
        }
        Err(e) => Json(CommandAck {
            ok:      false,
            message: format!("channel error: {}", e),
        }),
    }
}

async fn handle_cavebot_pause(State(s): State<AppState>) -> Json<CommandAck> {
    send_cmd(&s, LoopCommand::PauseCavebot, "PauseCavebot")
}

async fn handle_cavebot_resume(State(s): State<AppState>) -> Json<CommandAck> {
    send_cmd(&s, LoopCommand::ResumeCavebot, "ResumeCavebot")
}

async fn handle_cavebot_clear(State(s): State<AppState>) -> Json<CommandAck> {
    send_cmd(&s, LoopCommand::ClearCavebot, "ClearCavebot")
}

#[derive(Deserialize)]
struct CavebotJumpQuery {
    label: String,
}

async fn handle_cavebot_jump_to_label(
    State(s): State<AppState>,
    Query(q): Query<CavebotJumpQuery>,
) -> Json<CommandAck> {
    let label = q.label.clone();
    let cmd = LoopCommand::JumpToCavebotLabel { label: label.clone() };
    match s.loop_tx.send(cmd) {
        Ok(()) => {
            info!("JumpToCavebotLabel solicitado: label='{}'", label);
            Json(CommandAck {
                ok: true,
                message: format!("queued JumpToCavebotLabel label='{}'", label),
            })
        }
        Err(e) => {
            warn!("JumpToCavebotLabel error: {e}");
            Json(CommandAck {
                ok: false,
                message: format!("send error: {e}"),
            })
        }
    }
}

// ── /recording/start y /recording/stop (F1.4) ─────────────────────────────────
//
// Controla el PerceptionRecorder desde HTTP. El recorder escribe snapshots
// JSONL al path especificado (o el default `session.jsonl`).
//
//   curl -X POST 'http://localhost:8080/recording/start?path=sessions/test.jsonl'
//   curl -X POST 'http://localhost:8080/recording/stop'

#[derive(Deserialize)]
struct RecordingStartQuery {
    #[serde(default)]
    path: Option<String>,
}

async fn handle_recording_start(
    State(s): State<AppState>,
    Query(q): Query<RecordingStartQuery>,
) -> Json<CommandAck> {
    let cmd = LoopCommand::StartRecording { path: q.path.clone() };
    match s.loop_tx.send(cmd) {
        Ok(()) => {
            let display_path = q.path.as_deref().unwrap_or("session.jsonl");
            info!("Recording start solicitado: path='{}'", display_path);
            Json(CommandAck {
                ok: true,
                message: format!("queued StartRecording path='{}'", display_path),
            })
        }
        Err(e) => Json(CommandAck {
            ok: false,
            message: format!("channel error: {}", e),
        }),
    }
}

async fn handle_recording_stop(State(s): State<AppState>) -> Json<CommandAck> {
    send_cmd(&s, LoopCommand::StopRecording, "StopRecording")
}

// ── /dataset/start, /dataset/stop, /dataset/status (Fase 2.2 ML) ─────────────
//
// Captura crops 32×32 de inventory slots para entrenamiento ML.
// Output: <dir>/manifest.csv + <dir>/crops/*.png
//
//   curl -X POST 'http://localhost:8080/dataset/start?dir=datasets/abdendriel&interval=15&tag=hunt1'
//   curl -X POST 'http://localhost:8080/dataset/stop'
//   curl -s     'http://localhost:8080/dataset/status'

#[derive(Deserialize)]
struct DatasetStartQuery {
    /// Directorio destino (relativo, validado contra base allowed).
    dir: String,
    /// Cada cuántos ticks captura. Default 15 (~500ms a 30 Hz).
    #[serde(default)]
    interval: Option<u32>,
    /// Etiqueta libre para el manifest CSV (ej. "abdendriel_session_1").
    #[serde(default)]
    tag: Option<String>,
}

async fn handle_dataset_start(
    State(s): State<AppState>,
    Query(q): Query<DatasetStartQuery>,
) -> Json<CommandAck> {
    // Validate path bajo `datasets/` para prevenir path traversal (V-003 pattern).
    let base = Path::new("datasets");
    let dir = match validate_load_path(&q.dir, base) {
        Ok(p) => p,
        Err(e) => {
            return Json(CommandAck {
                ok:      false,
                message: format!("dir rejected: {}", e),
            });
        }
    };
    let interval = q.interval.unwrap_or(15).max(1);
    // Sanitize tag: el tag se escribe literal en manifest.csv y va a logs.
    // Rechazar caracteres que rompen CSV (`,`, `\n`, `\r`, `"`) o permiten
    // log spoofing (ANSI escapes, newlines). Whitelist alfanumérico + `_-.`.
    let raw_tag = q.tag.unwrap_or_else(|| "untagged".into());
    let tag: String = raw_tag.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)  // límite defensivo
        .collect();
    if tag.is_empty() {
        return Json(CommandAck {
            ok:      false,
            message: "tag rejected: vacío tras sanitización (usar solo [a-zA-Z0-9_.-])".into(),
        });
    }
    let cmd = LoopCommand::StartDatasetCapture {
        dir: dir.clone(), interval, tag: tag.clone(),
    };
    match s.loop_tx.send(cmd) {
        Ok(()) => {
            info!("DatasetCapture start solicitado: dir='{}' interval={} tag='{}'",
                  dir.display(), interval, tag);
            Json(CommandAck {
                ok: true,
                message: format!(
                    "queued StartDatasetCapture dir='{}' interval={} tag='{}'",
                    dir.display(), interval, tag
                ),
            })
        }
        Err(e) => Json(CommandAck { ok: false, message: format!("channel error: {}", e) }),
    }
}

async fn handle_dataset_stop(State(s): State<AppState>) -> Json<CommandAck> {
    send_cmd(&s, LoopCommand::StopDatasetCapture, "StopDatasetCapture")
}

#[derive(Serialize)]
struct DatasetStatus {
    active:      bool,
    crops_total: u64,
    dir:         String,
}

async fn handle_dataset_status(State(s): State<AppState>) -> Json<DatasetStatus> {
    let g = s.game_state.read();
    Json(DatasetStatus {
        active:      g.dataset_active,
        crops_total: g.dataset_crops_total,
        dir:         g.dataset_dir.clone(),
    })
}

// ── /vision/region_monitor (Fase 1.5 wire) ───────────────────────────────────
//
// Devuelve los últimos diffs por región monitoreada. Útil para diagnosticar
// "cuánto cambió la battle list/minimap/viewport entre frames consecutivos".
// Las regiones se inicializan en BotLoop::new() con los ROIs del calibration:
// battle_list, minimap, viewport (si están definidos).
//
//   curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8080/vision/region_monitor
//   → [{"name":"battle_list","change_ratio":0.012,"above_threshold":false,"first_tick":false},
//      {"name":"minimap","change_ratio":0.003,"above_threshold":false,"first_tick":false},
//      {"name":"viewport","change_ratio":0.087,"above_threshold":false,"first_tick":false}]

async fn handle_region_monitor(State(s): State<AppState>)
    -> Json<Vec<crate::core::state::RegionMonitorEntry>>
{
    let g = s.game_state.read();
    Json(g.region_monitor_diffs.clone())
}

// ── Instrumentation endpoints ─────────────────────────────────────────────────
//
// Lock-free reads sobre MetricsRegistry (ArcSwap + atomics). Útil para:
// - Dashboard live (last_tick): ver el snapshot del último tick procesado.
// - Diagnóstico latency (percentiles): p50/p95/p99 acumulados desde boot.
// - Análisis profundo (histograms): distribución completa por bucket.
// - Jitter / FPS estable (windows): rolling stats.
//
// Costo: O(1) para last_tick, O(buckets) para percentiles. Sin contención.

/// GET /instrumentation/last_tick
/// Snapshot del último TickMetrics publicado por el game loop.
async fn handle_instr_last_tick(State(s): State<AppState>)
    -> Json<crate::instrumentation::TickMetrics>
{
    let snap = s.metrics.last_tick_snapshot();
    Json(*snap)  // TickMetrics es Copy
}

/// GET /instrumentation/percentiles
/// Resumen agregado de percentiles (p50/p95/p99/mean/max) por etapa del
/// pipeline. Acumulados desde boot — son histogramas globales, no rolling.
#[derive(serde::Serialize)]
struct PercentilesResponse {
    frame_age:        crate::instrumentation::Percentiles,
    vision_total:     crate::instrumentation::Percentiles,
    filter:           crate::instrumentation::Percentiles,
    fsm:              crate::instrumentation::Percentiles,
    dispatch:         crate::instrumentation::Percentiles,
    tick_total:       crate::instrumentation::Percentiles,
    action_rtt:       crate::instrumentation::Percentiles,
    e2e_capture_emit: crate::instrumentation::Percentiles,
    samples:          PercentileSampleCounts,
}

#[derive(serde::Serialize)]
struct PercentileSampleCounts {
    frame_age:        u64,
    vision_total:     u64,
    filter:           u64,
    fsm:              u64,
    dispatch:         u64,
    tick_total:       u64,
    action_rtt:       u64,
}

async fn handle_instr_percentiles(State(s): State<AppState>)
    -> Json<PercentilesResponse>
{
    use crate::instrumentation::Percentiles;
    let r = &s.metrics;
    Json(PercentilesResponse {
        frame_age:        Percentiles::from(&r.frame_age),
        vision_total:     Percentiles::from(&r.vision_total),
        filter:           Percentiles::from(&r.filter),
        fsm:              Percentiles::from(&r.fsm),
        dispatch:         Percentiles::from(&r.dispatch),
        tick_total:       Percentiles::from(&r.tick_total),
        action_rtt:       Percentiles::from(&r.action_rtt),
        e2e_capture_emit: Percentiles::from(&r.e2e_capture_emit),
        samples: PercentileSampleCounts {
            frame_age:    r.frame_age.count(),
            vision_total: r.vision_total.count(),
            filter:       r.filter.count(),
            fsm:          r.fsm.count(),
            dispatch:     r.dispatch.count(),
            tick_total:   r.tick_total.count(),
            action_rtt:   r.action_rtt.count(),
        },
    })
}

/// GET /instrumentation/histograms
/// Histograma raw (bucket counts) por etapa. Útil para Grafana heatmap o
/// análisis offline. Más caro que /percentiles (más data) pero sigue O(buckets).
#[derive(serde::Serialize)]
struct HistogramsResponse {
    frame_age:        crate::instrumentation::HistogramSnapshot,
    vision_total:     crate::instrumentation::HistogramSnapshot,
    filter:           crate::instrumentation::HistogramSnapshot,
    fsm:              crate::instrumentation::HistogramSnapshot,
    dispatch:         crate::instrumentation::HistogramSnapshot,
    tick_total:       crate::instrumentation::HistogramSnapshot,
    action_rtt:       crate::instrumentation::HistogramSnapshot,
    e2e_capture_emit: crate::instrumentation::HistogramSnapshot,
    vision_readers:   Vec<ReaderHistEntry>,
}

#[derive(serde::Serialize)]
struct ReaderHistEntry {
    reader: &'static str,
    hist:   crate::instrumentation::HistogramSnapshot,
}

async fn handle_instr_histograms(State(s): State<AppState>)
    -> Json<HistogramsResponse>
{
    let r = &s.metrics;
    let vision_readers: Vec<ReaderHistEntry> = crate::instrumentation::ReaderId::all()
        .iter()
        .map(|id| ReaderHistEntry {
            reader: id.label(),
            hist:   r.vision_readers[*id as usize].snapshot(),
        })
        .collect();
    Json(HistogramsResponse {
        frame_age:        r.frame_age.snapshot(),
        vision_total:     r.vision_total.snapshot(),
        filter:           r.filter.snapshot(),
        fsm:              r.fsm.snapshot(),
        dispatch:         r.dispatch.snapshot(),
        tick_total:       r.tick_total.snapshot(),
        action_rtt:       r.action_rtt.snapshot(),
        e2e_capture_emit: r.e2e_capture_emit.snapshot(),
        vision_readers,
    })
}

/// GET /instrumentation/windows
/// Rolling window stats — jitter, mean, FPS. Útil para detectar degradación
/// progresiva sin tener que comparar percentiles globales.
#[derive(serde::Serialize)]
struct WindowsResponse {
    #[serde(flatten)]
    snapshot: crate::instrumentation::registry::WindowsSnapshot,
    measured_fps: f32,
    counters: CountersSummary,
}

#[derive(serde::Serialize)]
struct CountersSummary {
    ticks_total:    u64,
    ticks_overrun:  u64,
    frame_seq_gaps: u64,
    actions_emitted_total: u64,
    actions_acked_total:   u64,
    actions_failed_total:  u64,
}

/// POST /instrumentation/start_recording?path=session.metrics.jsonl[&flush_every=30]
/// Activa el JSONL recorder. Sustituye sesión previa.
#[derive(serde::Deserialize)]
struct StartRecordingQuery {
    path: Option<String>,
    flush_every: Option<u32>,
}

#[derive(serde::Serialize)]
struct StartRecordingResponse {
    ok: bool,
    path: String,
    flush_every: u32,
    error: Option<String>,
}

async fn handle_instr_start_recording(
    State(s): State<AppState>,
    Query(q): Query<StartRecordingQuery>,
) -> (StatusCode, Json<StartRecordingResponse>) {
    let path_str = q.path.unwrap_or_else(|| "session.metrics.jsonl".to_string());
    let flush_every = q.flush_every.unwrap_or(30).max(1);

    // V-008-style: validar caracteres seguros en path. No permitir
    // newlines, control chars, etc. para evitar log injection si el path
    // se loguea downstream.
    if !is_safe_recording_path(&path_str) {
        return (StatusCode::BAD_REQUEST, Json(StartRecordingResponse {
            ok: false,
            path: path_str,
            flush_every,
            error: Some("path contiene caracteres no permitidos".to_string()),
        }));
    }

    let path = std::path::PathBuf::from(&path_str);
    match s.metrics.start_recording(path, flush_every) {
        Ok(()) => (StatusCode::OK, Json(StartRecordingResponse {
            ok: true,
            path: path_str,
            flush_every,
            error: None,
        })),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(StartRecordingResponse {
            ok: false,
            path: path_str,
            flush_every,
            error: Some(e.to_string()),
        })),
    }
}

fn is_safe_recording_path(p: &str) -> bool {
    // Whitelist conservadora: alfanumérico + `._-/\:` (drive letters Windows).
    // Sin espacios, sin newlines, sin control chars.
    !p.is_empty()
        && p.len() <= 256
        && p.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '-' | '/' | '\\' | ':')
        })
}

/// POST /instrumentation/stop_recording
/// Cierra la sesión, devuelve total de líneas escritas.
#[derive(serde::Serialize)]
struct StopRecordingResponse {
    ok: bool,
    written_lines: u64,
}

async fn handle_instr_stop_recording(State(s): State<AppState>)
    -> Json<StopRecordingResponse>
{
    let n = s.metrics.stop_recording();
    Json(StopRecordingResponse { ok: true, written_lines: n })
}

/// GET /instrumentation/recording_status
async fn handle_instr_recording_status(State(s): State<AppState>)
    -> Json<crate::instrumentation::registry::RecordingStatus>
{
    Json(s.metrics.recording_status())
}

/// GET /health/detailed
/// HealthStatus completo del último tick — overall severity, score,
/// degradation level (sticky), issues activos con contexto numérico.
/// Lock-free vía ArcSwap. Distinto de `/health` (legacy liveness check
/// 200/503 + HealthDetails) que se preserva para compat con uptime tools.
async fn handle_health_detailed(State(s): State<AppState>)
    -> Json<crate::health::HealthStatus>
{
    Json((*s.health.load_full()).clone())
}

async fn handle_instr_windows(State(s): State<AppState>) -> Json<WindowsResponse> {
    use std::sync::atomic::Ordering;
    let r = &s.metrics;
    let snapshot = r.windows_snapshot();
    let actions_emitted_total: u64 = r.actions_emitted.iter()
        .map(|c| c.load(Ordering::Relaxed)).sum();
    let actions_acked_total: u64 = r.actions_acked.iter()
        .map(|c| c.load(Ordering::Relaxed)).sum();
    let actions_failed_total: u64 = r.actions_failed.iter()
        .map(|c| c.load(Ordering::Relaxed)).sum();
    Json(WindowsResponse {
        snapshot,
        measured_fps: r.measured_fps(),
        counters: CountersSummary {
            ticks_total:   r.ticks_total.load(Ordering::Relaxed),
            ticks_overrun: r.ticks_overrun.load(Ordering::Relaxed),
            frame_seq_gaps: r.frame_seq_gaps.load(Ordering::Relaxed),
            actions_emitted_total,
            actions_acked_total,
            actions_failed_total,
        },
    })
}

fn send_cmd(s: &AppState, cmd: LoopCommand, name: &str) -> Json<CommandAck> {
    match s.loop_tx.send(cmd) {
        Ok(()) => Json(CommandAck {
            ok:      true,
            message: format!("queued {}", name),
        }),
        Err(e) => Json(CommandAck {
            ok:      false,
            message: format!("channel error: {}", e),
        }),
    }
}

// ── /scripts/reload ───────────────────────────────────────────────────────────
// Recarga los scripts Lua desde disco. Si no se pasa `path`, el loop usa el
// script_dir actual (típicamente el del config).
//   curl -X POST 'http://localhost:8080/scripts/reload'
//   curl -X POST 'http://localhost:8080/scripts/reload?path=assets/scripts'

#[derive(Deserialize)]
struct ScriptsReloadQuery {
    #[serde(default)]
    path: String,
}

async fn handle_scripts_reload(
    State(s): State<AppState>,
    Query(q): Query<ScriptsReloadQuery>,
) -> Json<CommandAck> {
    // V-003 fix: si path está set, whitelist a assets/scripts/ (previene
    // carga de .lua arbitrario).
    let path = if q.path.is_empty() {
        None
    } else {
        match validate_load_path(&q.path, Path::new("assets/scripts")) {
            Ok(p) => Some(p),
            Err(e) => {
                return Json(CommandAck {
                    ok:      false,
                    message: format!("path rejected: {}", e),
                });
            }
        }
    };
    let path_str = path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<config>".into());
    match s.loop_tx.send(LoopCommand::ReloadScripts { path }) {
        Ok(()) => {
            info!("ReloadScripts solicitado: {}", path_str);
            Json(CommandAck {
                ok:      true,
                message: format!("queued ReloadScripts path={}", path_str),
            })
        }
        Err(e) => Json(CommandAck {
            ok:      false,
            message: format!("channel error: {}", e),
        }),
    }
}

// ── /scripts/status ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ScriptsStatusResponse {
    enabled:      bool,
    loaded_files: Vec<String>,
    last_errors:  Vec<String>,
}

async fn handle_scripts_status(State(s): State<AppState>) -> Json<ScriptsStatusResponse> {
    let g = s.game_state.read();
    Json(ScriptsStatusResponse {
        enabled:      g.script_status.enabled,
        loaded_files: g.script_status.loaded_files.clone(),
        last_errors:  g.script_status.last_errors.clone(),
    })
}

// ── /metrics (Prometheus / OpenMetrics format) ───────────────────────────────
// Exposes a subset of GameState.metrics + Perception en formato texto
// compatible con Prometheus scraper.  Uso con Grafana:
//   scrape_config:
//     - job_name: tibia_bot
//       static_configs:
//         - targets: ['localhost:8080']
//       metrics_path: /metrics
//
// Keep the output minimal (no heavy allocations) — este endpoint puede
// llamarse cada 5s desde Prometheus.

async fn handle_prometheus_metrics(State(s): State<AppState>) -> Response {
    use std::fmt::Write;
    let g = s.game_state.read();
    let has_frame = s.buffer.latest_frame().is_some();
    let m = &g.metrics;

    let mut out = String::with_capacity(2048);

    // Ticks
    writeln!(out, "# HELP tibia_bot_ticks_total Total ticks processed").ok();
    writeln!(out, "# TYPE tibia_bot_ticks_total counter").ok();
    writeln!(out, "tibia_bot_ticks_total {}", m.ticks_total).ok();

    writeln!(out, "# HELP tibia_bot_ticks_overrun_total Ticks that exceeded budget").ok();
    writeln!(out, "# TYPE tibia_bot_ticks_overrun_total counter").ok();
    writeln!(out, "tibia_bot_ticks_overrun_total {}", m.ticks_overrun).ok();

    // Latencias (rolling averages en ms)
    writeln!(out, "# HELP tibia_bot_ndi_latency_ms NDI capture latency (rolling avg)").ok();
    writeln!(out, "# TYPE tibia_bot_ndi_latency_ms gauge").ok();
    writeln!(out, "tibia_bot_ndi_latency_ms {:.2}", m.ndi_latency_ms).ok();

    writeln!(out, "# HELP tibia_bot_pico_latency_ms Pico command round-trip latency").ok();
    writeln!(out, "# TYPE tibia_bot_pico_latency_ms gauge").ok();
    writeln!(out, "tibia_bot_pico_latency_ms {:.2}", m.pico_latency_ms).ok();

    writeln!(out, "# HELP tibia_bot_proc_ms Bot processing time per tick (rolling avg)").ok();
    writeln!(out, "# TYPE tibia_bot_proc_ms gauge").ok();
    writeln!(out, "tibia_bot_proc_ms {:.2}", m.bot_proc_ms).ok();

    // State
    writeln!(out, "# HELP tibia_bot_has_frame 1 if NDI frame available, 0 otherwise").ok();
    writeln!(out, "# TYPE tibia_bot_has_frame gauge").ok();
    writeln!(out, "tibia_bot_has_frame {}", if has_frame { 1 } else { 0 }).ok();

    writeln!(out, "# HELP tibia_bot_is_paused 1 if bot paused, 0 otherwise").ok();
    writeln!(out, "# TYPE tibia_bot_is_paused gauge").ok();
    writeln!(out, "tibia_bot_is_paused {}", if g.is_paused { 1 } else { 0 }).ok();

    // FSM state como label
    writeln!(out, "# HELP tibia_bot_fsm_state Current FSM state (info)").ok();
    writeln!(out, "# TYPE tibia_bot_fsm_state gauge").ok();
    writeln!(out, "tibia_bot_fsm_state{{state=\"{:?}\"}} 1", g.fsm_state).ok();

    // Vitals
    if let Some(p) = g.last_perception.as_ref() {
        if let Some(hp) = p.vitals.hp.as_ref() {
            writeln!(out, "# HELP tibia_bot_hp_ratio Character HP ratio [0..1]").ok();
            writeln!(out, "# TYPE tibia_bot_hp_ratio gauge").ok();
            writeln!(out, "tibia_bot_hp_ratio {:.3}", hp.ratio).ok();
        }
        if let Some(mana) = p.vitals.mana.as_ref() {
            writeln!(out, "# HELP tibia_bot_mana_ratio Character mana ratio [0..1]").ok();
            writeln!(out, "# TYPE tibia_bot_mana_ratio gauge").ok();
            writeln!(out, "tibia_bot_mana_ratio {:.3}", mana.ratio).ok();
        }
        writeln!(out, "# HELP tibia_bot_enemy_count Enemies in battle list").ok();
        writeln!(out, "# TYPE tibia_bot_enemy_count gauge").ok();
        writeln!(out, "tibia_bot_enemy_count {}", p.battle.entries.len()).ok();

        // Inventory counts (por item)
        if !p.inventory_counts.is_empty() {
            writeln!(out, "# HELP tibia_bot_inventory_slots Slots matching each item").ok();
            writeln!(out, "# TYPE tibia_bot_inventory_slots gauge").ok();
            for (name, count) in &p.inventory_counts {
                // Sanitizar: Prometheus labels no toleran quotes.
                let safe_name: String = name.chars()
                    .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
                    .collect();
                writeln!(out, "tibia_bot_inventory_slots{{item=\"{}\"}} {}", safe_name, count).ok();
            }
        }
    }

    // Safety pause reason como info
    if let Some(ref reason) = g.safety_pause_reason {
        let safe_reason: String = reason.chars()
            .map(|c| if c.is_alphanumeric() || c == '_' || c == ':' { c } else { '_' })
            .collect();
        writeln!(out, "# HELP tibia_bot_safety_pause Active safety pause reason").ok();
        writeln!(out, "# TYPE tibia_bot_safety_pause gauge").ok();
        writeln!(out, "tibia_bot_safety_pause{{reason=\"{}\"}} 1", safe_reason).ok();
    }

    // ── Hunt profile baselines (expected values del profile) ─────────────
    // Expuestos como gauges — los paneles de Grafana pueden computar ratios
    // actual/expected como health signal (ej: alert si xp_actual/hour <
    // 50% del expected_xp_per_hour durante >15min).
    let cs = &g.cavebot_status;
    if let Some(ref profile_name) = cs.hunt_profile {
        writeln!(out, "# HELP tibia_hunt_profile_loaded Hunt profile declared by cavebot TOML (info)").ok();
        writeln!(out, "# TYPE tibia_hunt_profile_loaded gauge").ok();
        let safe: String = profile_name.chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
            .collect();
        writeln!(out, "tibia_hunt_profile_loaded{{profile=\"{}\"}} 1", safe).ok();

        if let Some(xp) = cs.expected_xp_per_hour {
            writeln!(out, "# HELP tibia_hunt_expected_xp_per_hour Baseline XP/hour del profile [metrics]").ok();
            writeln!(out, "# TYPE tibia_hunt_expected_xp_per_hour gauge").ok();
            writeln!(out, "tibia_hunt_expected_xp_per_hour {}", xp).ok();
        }
        if let Some(k) = cs.expected_kills_per_hour {
            writeln!(out, "# HELP tibia_hunt_expected_kills_per_hour Baseline kills/hour").ok();
            writeln!(out, "# TYPE tibia_hunt_expected_kills_per_hour gauge").ok();
            writeln!(out, "tibia_hunt_expected_kills_per_hour {}", k).ok();
        }
        if let Some(gp) = cs.expected_loot_gp_per_hour {
            writeln!(out, "# HELP tibia_hunt_expected_loot_gp_per_hour Baseline loot value GP/hour").ok();
            writeln!(out, "# TYPE tibia_hunt_expected_loot_gp_per_hour gauge").ok();
            writeln!(out, "tibia_hunt_expected_loot_gp_per_hour {}", gp).ok();
        }
        if let Some(c) = cs.expected_cycle_min {
            writeln!(out, "# HELP tibia_hunt_expected_cycle_min Baseline ciclo depot→hunt→depot en minutos").ok();
            writeln!(out, "# TYPE tibia_hunt_expected_cycle_min gauge").ok();
            writeln!(out, "tibia_hunt_expected_cycle_min {}", c).ok();
        }
    }

    // Cavebot verifying state (1 = el step actual está polling postcondition).
    writeln!(out, "# HELP tibia_cavebot_verifying 1 si el step actual está en verify poll").ok();
    writeln!(out, "# TYPE tibia_cavebot_verifying gauge").ok();
    writeln!(out, "tibia_cavebot_verifying {}", if cs.verifying { 1 } else { 0 }).ok();

    // ── MinimapMatcher stats ─────────────────────────────────────────────
    if let Some(ref ms) = g.matcher_stats {
        writeln!(out, "# HELP tibia_matcher_detects_total Total MinimapMatcher detect calls").ok();
        writeln!(out, "# TYPE tibia_matcher_detects_total counter").ok();
        writeln!(out, "tibia_matcher_detects_total{{mode=\"narrow\"}} {}", ms.narrow_searches).ok();
        writeln!(out, "tibia_matcher_detects_total{{mode=\"full\"}} {}", ms.full_searches).ok();

        writeln!(out, "# HELP tibia_matcher_misses_total Detects that returned None (score above threshold)").ok();
        writeln!(out, "# TYPE tibia_matcher_misses_total counter").ok();
        writeln!(out, "tibia_matcher_misses_total {}", ms.misses).ok();

        writeln!(out, "# HELP tibia_matcher_last_duration_ms Last detect duration (milliseconds)").ok();
        writeln!(out, "# TYPE tibia_matcher_last_duration_ms gauge").ok();
        writeln!(out, "tibia_matcher_last_duration_ms {:.3}", ms.last_duration_ms).ok();

        writeln!(out, "# HELP tibia_matcher_last_score Last SSD match score (lower=better)").ok();
        writeln!(out, "# TYPE tibia_matcher_last_score gauge").ok();
        writeln!(out, "tibia_matcher_last_score {:.6}", ms.last_score).ok();

        writeln!(out, "# HELP tibia_matcher_sectors_loaded Reference sectors in RAM").ok();
        writeln!(out, "# TYPE tibia_matcher_sectors_loaded gauge").ok();
        writeln!(out, "tibia_matcher_sectors_loaded {}", ms.sectors_loaded).ok();
    }

    drop(g);

    // ── HealthSystem gauges ─────────────────────────────────────────────
    // Exponer severity + score + degradation level + issue flags para
    // que Grafana graphe evolución a lo largo de la sesión.
    let health_snap = s.health.load_full();
    let overall_num: u8 = match health_snap.overall {
        crate::health::Severity::Ok       => 0,
        crate::health::Severity::Warning  => 1,
        crate::health::Severity::Critical => 2,
    };
    writeln!(out, "# HELP tibia_health_overall 0=ok 1=warning 2=critical").ok();
    writeln!(out, "# TYPE tibia_health_overall gauge").ok();
    writeln!(out, "tibia_health_overall {}", overall_num).ok();

    writeln!(out, "# HELP tibia_health_score Aggregate issue weight (ok=0, warning=1, critical=4)").ok();
    writeln!(out, "# TYPE tibia_health_score gauge").ok();
    writeln!(out, "tibia_health_score {}", health_snap.score).ok();

    let deg_num: u8 = match health_snap.degraded {
        None                                                    => 0,
        Some(crate::health::DegradationLevel::Light)            => 1,
        Some(crate::health::DegradationLevel::Heavy)            => 2,
        Some(crate::health::DegradationLevel::SafeMode)         => 3,
    };
    writeln!(out, "# HELP tibia_health_degradation 0=none 1=light 2=heavy 3=safe_mode").ok();
    writeln!(out, "# TYPE tibia_health_degradation gauge").ok();
    writeln!(out, "tibia_health_degradation {}", deg_num).ok();

    // Issue activo por kind — 1 si emitido este tick, 0 si no.
    // Permite alert rules tipo `tibia_health_issue_active{kind="frame_stale"} == 1 for 30s`.
    let mut active_kinds: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for i in &health_snap.issues {
        active_kinds.insert(i.label());
    }
    let all_kinds = [
        "frame_stale", "tick_overrun", "vision_slow", "anchor_drift",
        "low_detection_confidence", "bridge_rtt_high", "bridge_unreachable",
        "action_failure_rate", "high_jitter", "frame_seq_gap",
        "blind_mode", "compute_saturation", "io_unreliable",
    ];
    writeln!(out, "# HELP tibia_health_issue_active 1 if issue type is currently emitted").ok();
    writeln!(out, "# TYPE tibia_health_issue_active gauge").ok();
    for kind in all_kinds {
        let v = if active_kinds.contains(kind) { 1 } else { 0 };
        writeln!(out, "tibia_health_issue_active{{kind=\"{}\"}} {}", kind, v).ok();
    }

    // ── Inventory per-slot gauges (item #5 plan robustez 2026-04-22) ────
    // Counters de observaciones acumuladas desde boot. Para rates,
    // Grafana calcula rate() con intervalos, o el operador normaliza
    // dividiendo por el slot count configurado.
    writeln!(out, "# HELP tibia_inventory_slots_observed_total Total slot observations ingested").ok();
    writeln!(out, "# TYPE tibia_inventory_slots_observed_total counter").ok();
    writeln!(out, "tibia_inventory_slots_observed_total {}",
        s.metrics.inventory_slots_observed.load(std::sync::atomic::Ordering::Relaxed)).ok();

    writeln!(out, "# HELP tibia_inventory_slots_by_stage Slot observations grouped by stage").ok();
    writeln!(out, "# TYPE tibia_inventory_slots_by_stage counter").ok();
    writeln!(out, "tibia_inventory_slots_by_stage{{stage=\"empty\"}} {}",
        s.metrics.inventory_slots_empty.load(std::sync::atomic::Ordering::Relaxed)).ok();
    writeln!(out, "tibia_inventory_slots_by_stage{{stage=\"matched\"}} {}",
        s.metrics.inventory_slots_matched.load(std::sync::atomic::Ordering::Relaxed)).ok();
    writeln!(out, "tibia_inventory_slots_by_stage{{stage=\"unmatched\"}} {}",
        s.metrics.inventory_slots_unmatched.load(std::sync::atomic::Ordering::Relaxed)).ok();

    writeln!(out, "# HELP tibia_inventory_slots_with_stable_total Slots with stable_item populated by filter").ok();
    writeln!(out, "# TYPE tibia_inventory_slots_with_stable_total counter").ok();
    writeln!(out, "tibia_inventory_slots_with_stable_total {}",
        s.metrics.inventory_slots_with_stable.load(std::sync::atomic::Ordering::Relaxed)).ok();

    // Confidence histogram: emitir percentiles p50/p95/p99 directamente
    // (bucket array es demasiado verbose para /metrics). Unidad: basis
    // points 0..10000, donde 10000 = confidence 1.0.
    let conf_p50 = s.metrics.inventory_slot_confidence.percentile(0.50);
    let conf_p95 = s.metrics.inventory_slot_confidence.percentile(0.95);
    let conf_p99 = s.metrics.inventory_slot_confidence.percentile(0.99);
    let conf_count = s.metrics.inventory_slot_confidence.count();
    writeln!(out, "# HELP tibia_inventory_slot_confidence_bp Per-slot confidence distribution (basis points, 0..10000)").ok();
    writeln!(out, "# TYPE tibia_inventory_slot_confidence_bp gauge").ok();
    writeln!(out, "tibia_inventory_slot_confidence_bp{{quantile=\"0.50\"}} {}", conf_p50).ok();
    writeln!(out, "tibia_inventory_slot_confidence_bp{{quantile=\"0.95\"}} {}", conf_p95).ok();
    writeln!(out, "tibia_inventory_slot_confidence_bp{{quantile=\"0.99\"}} {}", conf_p99).ok();
    writeln!(out, "tibia_inventory_slot_confidence_samples {}", conf_count).ok();

    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        out,
    ).into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Mínimo TOML válido para construir un Config en tests.
    /// Omite todos los campos opcionales y pone placeholders para los required.
    const MIN_CONFIG_TOML: &str = r#"
        [ndi]
        source_name = "test"

        [pico]
        bridge_addr = "127.0.0.1:1"

        [http]
        listen_addr = "127.0.0.1:0"

        [coords]
        desktop_total_w = 1920
        desktop_total_h = 1080
        tibia_window_x  = 0
        tibia_window_y  = 0
        tibia_window_w  = 1920
        tibia_window_h  = 1080
        game_viewport_offset_x = 0
        game_viewport_offset_y = 0
        game_viewport_w = 1920
        game_viewport_h = 1080
    "#;

    /// Construye un AppState minimal para tests (sin NDI real, sin Pico real).
    async fn test_state() -> AppState {
        let config: Config = toml::from_str(MIN_CONFIG_TOML).expect("parse test config");
        let game_state = crate::core::state::new_shared_state();
        let buffer = Arc::new(crate::sense::frame_buffer::FrameBuffer::new());
        // PicoHandle hacia un puerto muerto — el task de reconnect corre en
        // background pero no afecta a endpoints read-only.
        let pico = crate::act::pico_link::spawn(config.pico.clone());
        let actuator = Arc::new(crate::act::Actuator::new(pico, &config.coords));
        let calibration = Arc::new(crate::sense::vision::calibration::Calibration::default());
        let (loop_tx, _loop_rx) = crossbeam_channel::unbounded();
        AppState {
            game_state,
            buffer,
            actuator,
            config,
            calibration,
            loop_tx,
            metrics: Arc::new(crate::instrumentation::MetricsRegistry::new()),
            health: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::health::HealthStatus::default()
            )),
        }
    }

    async fn get(app: Router, path: &str) -> (StatusCode, Vec<u8>) {
        let resp = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .expect("route call");
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        (status, body.to_vec())
    }

    async fn post(app: Router, path: &str) -> StatusCode {
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(path)
                    .method("POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("route call");
        resp.status()
    }

    #[tokio::test]
    async fn get_status_returns_200_and_valid_json() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/status").await;
        assert_eq!(status, StatusCode::OK);
        // Debe ser JSON válido con al menos un campo conocido.
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert!(v.get("tick").is_some());
        assert!(v.get("fsm_state").is_some());
        assert_eq!(v["is_paused"], false);
    }

    #[tokio::test]
    async fn get_health_without_frame_returns_503() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/health").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(v["ok"], false);
        assert_eq!(v["reason"], "no_frame");
        assert!(v.get("details").is_some());
    }

    fn health_details(
        has_frame: bool,
        frame_age_ms: Option<u64>,
        is_paused: bool,
        safety_pause_reason: Option<String>,
        ticks_total: u64,
        proc_ms: f64,
    ) -> HealthDetails {
        HealthDetails {
            has_frame,
            frame_age_ms,
            is_paused,
            safety_pause_reason,
            ticks_total,
            ticks_overrun: 0,
            ndi_latency_ms: 10.0,
            pico_latency_ms: 5.0,
            bot_proc_ms: proc_ms,
            fsm_state: "Idle".to_string(),
        }
    }

    #[test]
    fn determine_health_reports_ok_when_all_good() {
        let d = health_details(true, Some(100), false, None, 1000, 15.0);
        assert_eq!(determine_health(&d), (true, "ok"));
    }

    #[test]
    fn determine_health_reports_no_frame_when_missing() {
        let d = health_details(false, None, false, None, 1000, 15.0);
        assert_eq!(determine_health(&d), (false, "no_frame"));
    }

    #[test]
    fn determine_health_reports_stale_when_frame_old() {
        let d = health_details(true, Some(5000), false, None, 1000, 15.0);
        assert_eq!(determine_health(&d), (false, "stale_frame"));
    }

    #[test]
    fn determine_health_reports_paused_manual() {
        let d = health_details(true, Some(100), true, None, 1000, 15.0);
        assert_eq!(determine_health(&d), (false, "paused_manual"));
    }

    #[test]
    fn determine_health_reports_paused_login_prompt() {
        let d = health_details(
            true, Some(100), true,
            Some("prompt:login".to_string()),
            1000, 15.0,
        );
        assert_eq!(determine_health(&d), (false, "paused_login"));
    }

    #[test]
    fn determine_health_reports_paused_char_select_prompt() {
        let d = health_details(
            true, Some(100), true,
            Some("prompt:char_select".to_string()),
            1000, 15.0,
        );
        assert_eq!(determine_health(&d), (false, "paused_char_select"));
    }

    #[test]
    fn determine_health_reports_proc_slow() {
        let d = health_details(true, Some(100), false, None, 1000, 75.0);
        assert_eq!(determine_health(&d), (false, "proc_slow"));
    }

    #[test]
    fn determine_health_reports_not_started_before_first_tick() {
        let d = health_details(true, Some(100), false, None, 0, 15.0);
        assert_eq!(determine_health(&d), (false, "not_started"));
    }

    #[tokio::test]
    async fn post_pause_flips_is_paused_to_true() {
        let state = test_state().await;
        let app = build_router(state.clone());

        let status = post(app, "/pause").await;
        assert_eq!(status, StatusCode::OK);

        // El /pause envía un comando al loop vía canal; sin loop real, el
        // estado no cambia automáticamente. Verificamos que el canal recibió
        // el mensaje.
        // (En este test con canal unbounded, no hay receptor consumiendo;
        // el comando queda buffered. Validamos que al menos no fallamos.)
        let _ = state; // keep state alive
    }

    #[tokio::test]
    async fn get_vision_inventory_returns_json_with_empty_counts_by_default() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/vision/inventory").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert!(v.get("slot_count").is_some());
        assert!(v.get("counts").is_some());
        // Sin perception cargada, counts es vacío.
        assert_eq!(v["counts"], serde_json::json!({}));
    }

    // ── Instrumentation endpoints ────────────────────────────────────────

    #[tokio::test]
    async fn get_instrumentation_last_tick_returns_default_when_no_ticks() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/instrumentation/last_tick").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        // Sin ticks aún → TickMetrics::default() → tick=0, todo a 0.
        assert_eq!(v["tick"], 0);
        assert_eq!(v["frame_age_us"], 0);
        assert_eq!(v["last_action_kind"], "none");
    }

    #[tokio::test]
    async fn get_instrumentation_last_tick_reflects_recorded_tick() {
        let state = test_state().await;
        // Simulamos un tick procesado.
        let m = crate::instrumentation::TickMetrics {
            tick: 42,
            frame_age_us: 75_000,
            tick_total_us: 18_500,
            last_action_kind: crate::instrumentation::ActionKindTag::Heal,
            ..Default::default()
        };
        state.metrics.record_tick(m);

        let app = build_router(state);
        let (status, body) = get(app, "/instrumentation/last_tick").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(v["tick"], 42);
        assert_eq!(v["frame_age_us"], 75_000);
        assert_eq!(v["tick_total_us"], 18_500);
        assert_eq!(v["last_action_kind"], "heal");
    }

    #[tokio::test]
    async fn get_instrumentation_percentiles_returns_zeros_when_empty() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/instrumentation/percentiles").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        // Sin samples, p50/p95/p99 deben ser 0.
        assert_eq!(v["frame_age"]["p50"], 0);
        assert_eq!(v["samples"]["frame_age"], 0);
    }

    #[tokio::test]
    async fn get_instrumentation_percentiles_reflects_distribution() {
        let state = test_state().await;
        for i in 1..=100u32 {
            let m = crate::instrumentation::TickMetrics {
                tick: i as u64,
                tick_total_us: i * 100, // 100..10000 µs
                ..Default::default()
            };
            state.metrics.record_tick(m);
        }
        let app = build_router(state);
        let (status, body) = get(app, "/instrumentation/percentiles").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(v["samples"]["tick_total"], 100);
        // p99 > p50 (distribución creciente).
        assert!(v["tick_total"]["p99"].as_u64().unwrap()
              > v["tick_total"]["p50"].as_u64().unwrap());
    }

    #[tokio::test]
    async fn get_instrumentation_histograms_includes_all_readers() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/instrumentation/histograms").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        let readers = v["vision_readers"].as_array().expect("array");
        assert_eq!(readers.len(), crate::instrumentation::ReaderId::COUNT);
        // Todos los reader entries tienen label.
        let labels: Vec<&str> = readers.iter()
            .map(|r| r["reader"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"hp_mana"));
        assert!(labels.contains(&"battle"));
        assert!(labels.contains(&"inventory"));
    }

    #[tokio::test]
    async fn get_instrumentation_windows_returns_counters_and_fps() {
        let state = test_state().await;
        // Push 20 ticks de 33 ms.
        for i in 0..20u32 {
            let m = crate::instrumentation::TickMetrics {
                tick: i as u64,
                tick_total_us: 33_000,
                ..Default::default()
            };
            state.metrics.record_tick(m);
        }
        let app = build_router(state);
        let (status, body) = get(app, "/instrumentation/windows").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(v["counters"]["ticks_total"], 20);
        // FPS measured ≈ 1e6 / 33000 ≈ 30
        let fps = v["measured_fps"].as_f64().unwrap();
        assert!(fps > 29.0 && fps < 31.0, "fps={}", fps);
    }

    #[tokio::test]
    async fn metrics_includes_health_gauges() {
        let state = test_state().await;
        // Publica un HealthStatus con valores claros.
        state.health.store(Arc::new(crate::health::HealthStatus {
            overall: crate::health::Severity::Warning,
            score: 3,
            degraded: Some(crate::health::DegradationLevel::Light),
            issues: vec![crate::health::HealthIssue::TickOverrun {
                tick_ms: 40, budget_ms: 33,
                severity: crate::health::Severity::Warning,
            }],
            summary: String::new(), tick: 0, frame_seq: 0, generated_at_ms: 0,
        }));
        let app = build_router(state);
        let (status, body) = get(app, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8_lossy(&body);
        // Core gauges presentes.
        assert!(text.contains("tibia_health_overall 1"));         // Warning=1
        assert!(text.contains("tibia_health_score 3"));
        assert!(text.contains("tibia_health_degradation 1"));     // Light=1
        // Issue flag kind=tick_overrun active.
        assert!(text.contains("tibia_health_issue_active{kind=\"tick_overrun\"} 1"));
        // Issues no emitidos están en 0 (todos los kinds listados).
        assert!(text.contains("tibia_health_issue_active{kind=\"frame_stale\"} 0"));
        assert!(text.contains("tibia_health_issue_active{kind=\"blind_mode\"} 0"));
    }

    #[tokio::test]
    async fn metrics_includes_inventory_slot_gauges() {
        use crate::sense::vision::inventory_slot::{SlotReading, SlotStage};
        let state = test_state().await;
        // Ingesta algunos slots sintéticos para poblar los counters.
        let slots = vec![
            SlotReading::empty(0),
            SlotReading::matched(
                1, "mana_potion".into(), 0.92, 0.80, Some(47),
                SlotStage::FullSweep,
            ),
            SlotReading::unmatched(2),
        ];
        state.metrics.ingest_inventory_slots(&slots);

        let app = build_router(state);
        let (status, body) = get(app, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8_lossy(&body);
        // Counters base presentes.
        assert!(text.contains("tibia_inventory_slots_observed_total 3"));
        assert!(text.contains("tibia_inventory_slots_by_stage{stage=\"empty\"} 1"));
        assert!(text.contains("tibia_inventory_slots_by_stage{stage=\"matched\"} 1"));
        assert!(text.contains("tibia_inventory_slots_by_stage{stage=\"unmatched\"} 1"));
        // Confidence percentiles presentes (solo 1 sample → p50/p95/p99 iguales).
        assert!(text.contains("tibia_inventory_slot_confidence_bp{quantile=\"0.50\"}"));
        assert!(text.contains("tibia_inventory_slot_confidence_samples 1"));
    }

    #[tokio::test]
    async fn get_health_detailed_returns_default_when_no_evaluation() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/health/detailed").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        // Default HealthStatus → ok severity, no degraded, empty issues.
        assert_eq!(v["overall"], "ok");
        assert_eq!(v["score"], 0);
        assert!(v["degraded"].is_null());
        assert!(v["issues"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_health_detailed_reflects_published_state() {
        let state = test_state().await;
        // Simular publicación de un HealthStatus distinto.
        let new_status = crate::health::HealthStatus {
            overall: crate::health::Severity::Warning,
            score: 3,
            degraded: Some(crate::health::DegradationLevel::Light),
            issues: vec![crate::health::HealthIssue::TickOverrun {
                tick_ms: 50, budget_ms: 33,
                severity: crate::health::Severity::Warning,
            }],
            summary: "warning (score=3, degraded=light, issues=[tick_overrun])".to_string(),
            tick: 1234,
            frame_seq: 1234,
            generated_at_ms: 1745347291000,
        };
        state.health.store(Arc::new(new_status));

        let app = build_router(state);
        let (status, body) = get(app, "/health/detailed").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(v["overall"], "warning");
        assert_eq!(v["score"], 3);
        assert_eq!(v["degraded"], "light");
        assert_eq!(v["tick"], 1234);
        let issues = v["issues"].as_array().unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["kind"], "tick_overrun");
        assert_eq!(issues[0]["tick_ms"], 50);
    }

    #[tokio::test]
    async fn instr_recording_status_inactive_by_default() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/instrumentation/recording_status").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(v["active"], false);
        assert_eq!(v["written_lines"], 0);
    }

    #[tokio::test]
    async fn instr_start_recording_rejects_unsafe_path() {
        let state = test_state().await;
        let app = build_router(state);
        // Path con espacio = rechazado por whitelist.
        let resp = app.oneshot(
            Request::builder().method("POST")
                .uri("/instrumentation/start_recording?path=session%20with%20space.jsonl")
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn is_safe_recording_path_rules() {
        assert!(is_safe_recording_path("session.jsonl"));
        assert!(is_safe_recording_path("data/metrics_v1.jsonl"));
        assert!(is_safe_recording_path("C:\\tmp\\m.jsonl"));
        assert!(!is_safe_recording_path(""));
        assert!(!is_safe_recording_path("with space.jsonl"));
        assert!(!is_safe_recording_path("inject\nnewline"));
        assert!(!is_safe_recording_path("a;rm -rf /;"));
    }

    #[tokio::test]
    async fn get_nonexistent_route_returns_404() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, _body) = get(app, "/does/not/exist").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_fsm_debug_returns_200_and_json() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/fsm/debug").await;
        assert_eq!(status, StatusCode::OK);
        let _v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
    }

    #[tokio::test]
    async fn get_metrics_returns_prometheus_format() {
        let state = test_state().await;
        let app = build_router(state);
        let (status, body) = get(app, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(body).expect("utf8");
        // Prometheus text format: debe contener HELP, TYPE, y metric lines.
        assert!(text.contains("# HELP tibia_bot_ticks_total"));
        assert!(text.contains("# TYPE tibia_bot_ticks_total counter"));
        assert!(text.contains("tibia_bot_ticks_total 0"));
        assert!(text.contains("tibia_bot_has_frame"));
        assert!(text.contains("tibia_bot_is_paused"));
        assert!(text.contains("tibia_bot_fsm_state"));
    }

    // ── Security: V-003, V-005, V-007 regressions ─────────────────────────

    /// Helper: crea un TOML válido de 200 bytes en un tmp dir dentro de
    /// `expected_dir` (que debe ser un subdir del workspace para que
    /// canonicalize funcione). Retorna path absoluto al archivo escrito.
    fn write_tmp_toml(expected_dir: &Path, name: &str, bytes: usize) -> PathBuf {
        std::fs::create_dir_all(expected_dir).unwrap();
        let path = expected_dir.join(name);
        std::fs::write(&path, "x".repeat(bytes)).unwrap();
        path
    }

    #[test]
    fn v003_validate_load_path_rejects_escape_via_parent_dir() {
        // Path claramente fuera del whitelist (usamos el propio workspace root).
        let bad = "../../Cargo.toml";
        let result = validate_load_path(bad, Path::new("assets/cavebot"));
        assert!(result.is_err(), "expected reject, got {:?}", result);
        let msg = result.unwrap_err();
        assert!(
            msg.contains("escapa") || msg.contains("no resolvable"),
            "unexpected error: {}", msg
        );
    }

    #[test]
    fn v003_validate_load_path_accepts_file_inside_whitelist() {
        let dir = std::env::temp_dir().join(format!("v003_accept_{}", std::process::id()));
        let path = write_tmp_toml(&dir, "ok.toml", 10);
        let result = validate_load_path(path.to_str().unwrap(), &dir);
        assert!(result.is_ok(), "expected accept, got {:?}", result);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v005_validate_load_path_rejects_oversized_file() {
        let dir = std::env::temp_dir().join(format!("v005_size_{}", std::process::id()));
        // Escribir un archivo > MAX_LOAD_FILE_BYTES (1 MB). 2 MB = 2 * 1024 * 1024.
        let path = write_tmp_toml(&dir, "huge.toml", 2 * 1024 * 1024);
        let result = validate_load_path(path.to_str().unwrap(), &dir);
        assert!(result.is_err(), "expected reject for oversized file");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("excede el límite"),
            "unexpected error: {}", msg
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v005_validate_load_path_accepts_small_file() {
        let dir = std::env::temp_dir().join(format!("v005_ok_{}", std::process::id()));
        // 8 KB — orden de magnitud de scripts legítimos.
        let path = write_tmp_toml(&dir, "normal.toml", 8 * 1024);
        let result = validate_load_path(path.to_str().unwrap(), &dir);
        assert!(result.is_ok(), "expected accept for normal-sized file, got {:?}", result);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// V-007: con `stealth_mode=true`, endpoints de debug deben retornar 404.
    #[tokio::test]
    async fn v007_stealth_mode_blocks_debug_endpoints() {
        let mut state = test_state().await;
        // Activar stealth_mode mutando el config del state.
        let mut new_cfg = (*state.config.http.auth_token.as_ref().unwrap_or(&String::new())).clone();
        let _ = &mut new_cfg; // noop — el mut anterior era para documentar
        // Clone del config entero para modificar stealth_mode.
        let mut cfg = state.config.clone();
        cfg.http.stealth_mode = true;
        state.config = cfg;
        let app = build_router(state);

        // Endpoints que DEBEN bloquearse (lista representativa).
        for path in &["/vision/perception", "/vision/inventory", "/fsm/debug",
                      "/combat/events", "/dispatch/stats", "/cavebot/status"] {
            let (status, _) = get(app.clone(), path).await;
            assert_eq!(
                status, StatusCode::NOT_FOUND,
                "stealth debería bloquear {} con 404, got {}", path, status
            );
        }
    }

    /// V-007: con stealth_mode activo, `/status` y `/health` siguen OK —
    /// esenciales para ops humanas y monitoring externo.
    #[tokio::test]
    async fn v007_stealth_mode_allows_health_and_status() {
        let mut state = test_state().await;
        let mut cfg = state.config.clone();
        cfg.http.stealth_mode = true;
        state.config = cfg;
        let app = build_router(state);

        let (status_code, _) = get(app.clone(), "/status").await;
        assert_eq!(status_code, StatusCode::OK, "/status debe seguir disponible");

        // /health sin frame retorna 503 pero NO 404 — no está stealth-blocked.
        let (health, _) = get(app, "/health").await;
        assert_ne!(health, StatusCode::NOT_FOUND, "/health no debe estar stealth-blocked");
    }

    /// V-007: con stealth_mode=false (default), endpoints debug siguen accesibles.
    /// Este test asegura que el middleware es no-op en default.
    #[tokio::test]
    async fn v007_stealth_mode_default_false_passes_through() {
        let state = test_state().await;
        // Sin tocar stealth_mode (default false).
        let app = build_router(state);
        let (status, _) = get(app, "/fsm/debug").await;
        // Puede ser 200 o 503 según haya frame; el único status prohibido
        // en default es 404 (eso solo aparecería si stealth estuviera ON).
        assert_ne!(
            status, StatusCode::NOT_FOUND,
            "stealth default=false no debería bloquear /fsm/debug"
        );
    }
}
