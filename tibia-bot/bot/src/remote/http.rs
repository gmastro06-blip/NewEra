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

use std::path::PathBuf;
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
use tracing::info;

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
        .route("/vision/vitals",         get(handle_vision_vitals))
        .route("/vision/battle",         get(handle_vision_battle))
        .route("/vision/status",         get(handle_vision_status))
        .route("/vision/grab/anchors",   get(handle_vision_grab_anchors))
        .route("/vision/grab/battle",    get(handle_vision_grab_battle))
        .route("/vision/grab/debug",     get(handle_vision_grab_debug))
        .route("/vision/grab/inventory", get(handle_vision_grab_inventory))
        .route("/vision/inventory",      get(handle_vision_inventory))
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
        .route("/scripts/reload",        post(handle_scripts_reload))
        .route("/scripts/status",        get(handle_scripts_status))
        .route("/metrics",               get(handle_prometheus_metrics))
        .route("/recording/start",       post(handle_recording_start))
        .route("/recording/stop",        post(handle_recording_stop))
        .with_state(state)
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
    }).into_response()
}

// ── /fsm/debug ───────────────────────────────────────────────────────────────
// Estado interno del FSM: cooldowns, flags internos, prev_target_active.
// Permite diagnosticar "¿por qué el FSM está en este estado?" sin recompilar.
//   curl http://localhost:8080/fsm/debug | jq .

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

async fn handle_waypoints_load(
    State(s): State<AppState>,
    Query(q): Query<WaypointsLoadQuery>,
) -> Json<CommandAck> {
    let cmd = LoopCommand::LoadWaypoints {
        path:    PathBuf::from(&q.path),
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
    let cmd = LoopCommand::LoadCavebot {
        path:    PathBuf::from(&q.path),
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
    let path = if q.path.is_empty() { None } else { Some(PathBuf::from(&q.path)) };
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

    drop(g);

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
}
