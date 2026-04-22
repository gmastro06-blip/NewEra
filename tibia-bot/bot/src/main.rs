//! main.rs — Bootstrap del bot.
//!
//! Orden de arranque:
//!   1. Logging
//!   2. Config
//!   3. SharedState y FrameBuffer
//!   4. PicoLink (task tokio)
//!   5. NDI receiver (thread dedicado)
//!   6. Actuator
//!   7. Vision (carga calibration.toml y templates)
//!   8. Canal comando HTTP → game loop
//!   9. HTTP server (task tokio)
//!  10. Game loop (thread dedicado)
//!  11. Esperar señal de shutdown (Ctrl+C)

// Cosmético: los doc-comments del proyecto usan una línea en blanco tras el
// bloque `///` antes del item declarado. Es más legible pero trips clippy.
// Silenciamos la lint a nivel de crate en vez de reformatear 19 sitios.
#![allow(clippy::empty_line_after_doc_comments)]

mod act;
mod cavebot;
mod config;
mod core;
mod health;
mod instrumentation;
mod remote;
mod safety;
mod scripting;
mod sense;
mod waypoints;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::act::{pico_link, Actuator, PresendJitter};
use crate::config::Config;
use crate::core::state::new_shared_state;
use crate::core::loop_::BotLoop;
use crate::remote::http::{serve, AppState};
use crate::sense::frame_buffer::FrameBuffer;
use crate::sense::vision::Vision;

#[tokio::main]
async fn main() -> Result<()> {
    // ── 1. Logging ─────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                // Generic filter name para que no aparezca `tibia_bot=info`
                // como string constant en el binary. RUST_LOG env var sigue
                // funcionando normalmente.
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    info!("=== NewEra runtime up ===");

    // ── 2. Config ──────────────────────────────────────────────────────────
    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let config = Config::load(&config_path)
        .with_context(|| format!(
            "No se pudo cargar '{}'. Copia config.toml.example a config.toml y edítalo.",
            config_path.display()
        ))?;

    let hotkeys = config.hotkeys()
        .context("Parseo de hotkeys en [actions]")?;

    info!("Config cargada desde '{}'", config_path.display());
    info!("NDI source: '{}'", config.ndi.source_name);
    info!("Bridge addr: '{}'", config.pico.bridge_addr);
    info!("HTTP listen: '{}'", config.http.listen_addr);
    info!(
        "Hotkeys: heal=0x{:02X} potion=0x{:02X} mana=0x{:02X} attack=0x{:02X}",
        hotkeys.heal_spell, hotkeys.heal_potion, hotkeys.mana_spell, hotkeys.attack_default
    );

    // ── 3. Estado compartido + FrameBuffer ─────────────────────────────────
    let shared_state = new_shared_state();
    let frame_buffer = Arc::new(FrameBuffer::new());

    // ── 4. PicoLink ────────────────────────────────────────────────────────
    let pico_handle = pico_link::spawn(config.pico.clone());
    info!("PicoLink lanzado, conectando a {}...", config.pico.bridge_addr);

    // ── 5. NDI receiver ────────────────────────────────────────────────────
    sense::ndi_receiver::spawn(config.ndi.clone(), Arc::clone(&frame_buffer));
    info!("NDI receiver lanzado, buscando fuente '{}'...", config.ndi.source_name);

    // ── 6. Coords auto-detect via bridge (WinAPI) ──────────────────────────
    // Espera a que pico_link establezca conexión + query la geometría real
    // del virtual desktop y ventana Tibia. Elimina la necesidad de calibrar
    // manual `desktop_total_w/h` y `tibia_window_x/y` en setups multi-monitor
    // (el config es solo un fallback por si el bridge no soporta GET_GEOMETRY
    // o no encuentra la ventana).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let mut coords_cfg = config.coords.clone();
    match pico_handle.query_geometry("Tibia").await {
        Some(geom) => {
            info!(
                "Geometry auto-detect: vscreen=origin({},{}) {}x{}, tibia={:?}",
                geom.vscreen_x, geom.vscreen_y, geom.vscreen_w, geom.vscreen_h, geom.tibia
            );
            // Aplicar virtual screen real (puede tener origen negativo si
            // hay monitores a la izquierda/arriba del primario).
            coords_cfg.vscreen_origin_x = geom.vscreen_x;
            coords_cfg.vscreen_origin_y = geom.vscreen_y;
            coords_cfg.desktop_total_w = geom.vscreen_w.max(1) as u32;
            coords_cfg.desktop_total_h = geom.vscreen_h.max(1) as u32;
            if let Some(t) = geom.tibia {
                coords_cfg.tibia_window_x = t.x;
                coords_cfg.tibia_window_y = t.y;
                coords_cfg.tibia_window_w = t.w.max(1) as u32;
                coords_cfg.tibia_window_h = t.h.max(1) as u32;
                info!(
                    "Auto-config: tibia_window=({},{},{}x{}), vscreen=origin({},{}) {}x{}",
                    t.x, t.y, t.w, t.h,
                    geom.vscreen_x, geom.vscreen_y, geom.vscreen_w, geom.vscreen_h
                );

                // Safety check (V7 blocker root cause): verificar que el
                // centro de Tibia cae dentro del vscreen reportado.
                //
                // En modo serial, el bridge reporta vscreen = primary monitor
                // porque Arduino HID solo targetea primary. Si Tibia está en
                // un monitor secundario, su centro cae FUERA de este rango y
                // los clicks HID nunca llegarán. Pausamos el bot con un
                // reason claro en lugar de fallar silencioso.
                //
                // En modo sendinput (vscreen = full virtual desktop), este
                // check solo dispararía si Tibia está fuera de TODOS los
                // monitores — situación imposible en práctica.
                let cx = t.x + t.w / 2;
                let cy = t.y + t.h / 2;
                let vx_min = geom.vscreen_x;
                let vy_min = geom.vscreen_y;
                let vx_max = geom.vscreen_x + geom.vscreen_w;
                let vy_max = geom.vscreen_y + geom.vscreen_h;
                if cx < vx_min || cx >= vx_max || cy < vy_min || cy >= vy_max {
                    let reason = format!(
                        "tibia_off_mapped_screen: center=({cx},{cy}) \
                         vscreen=[{vx_min}..{vx_max},{vy_min}..{vy_max}]. \
                         Con mode=serial, Tibia debe estar en PRIMARY monitor \
                         (HID Arduino solo targetea primary). Mové la ventana \
                         de Tibia al primary y reinicia el bot."
                    );
                    warn!("SAFETY: {}", reason);
                    let mut g = shared_state.write();
                    g.is_paused = true;
                    g.safety_pause_reason = Some(reason);
                }
            } else {
                warn!(
                    "Geometry auto-detect: ventana Tibia no encontrada. \
                     Usando config manual tibia_window_*={},{},{}x{}",
                    coords_cfg.tibia_window_x, coords_cfg.tibia_window_y,
                    coords_cfg.tibia_window_w, coords_cfg.tibia_window_h
                );
            }
        }
        None => {
            warn!(
                "Geometry auto-detect falló (bridge no disponible o \
                 protocolo antiguo). Usando config manual."
            );
        }
    }

    // ── 7. MetricsRegistry compartido ─────────────────────────────────────
    // Se construye ANTES del Actuator para que ambos (Actuator + BotLoop)
    // compartan el mismo Arc. Actuator usa record_action_ack al recibir
    // ACK del bridge; BotLoop usa record_tick + ArcSwap publish.
    let metrics = Arc::new(crate::instrumentation::MetricsRegistry::new());

    // ── 8. Actuator ────────────────────────────────────────────────────────
    // El Actuator toma ownership del PicoHandle; cualquier código que
    // necesite enviar comandos debe pasar por el Actuator compartido.
    // Si safety está activo, inyectamos pre-send jitter gaussiano.
    let actuator = if config.safety.humanize_timing {
        let jitter = PresendJitter {
            mean_ms: config.safety.presend_jitter_mean_ms,
            std_ms:  config.safety.presend_jitter_std_ms,
        };
        info!(
            "Safety: pre-send jitter ON ({:.0}±{:.0}ms)",
            jitter.mean_ms, jitter.std_ms
        );
        Arc::new(Actuator::with_jitter(pico_handle, &coords_cfg, jitter)
                 .with_metrics(Arc::clone(&metrics)))
    } else {
        Arc::new(Actuator::new(pico_handle, &coords_cfg)
                 .with_metrics(Arc::clone(&metrics)))
    };

    // ── 7. Vision (carga calibration.toml y templates desde assets/) ───────
    let assets_dir = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets"));
    let mut vision = Vision::load(&assets_dir);
    vision.load_map_index(&config.game_coords);
    // Fase 2.5: cargar classifier ML si config.ml.use_ml=true + paths válidos.
    // Sin feature `ml-runtime` o sin modelo en disco → reader inhábil,
    // fallback SSE matcher automático.
    vision.load_ml_model(&config.ml);
    if vision.is_calibrated() {
        info!("Vision calibrada y lista.");
    } else {
        info!("Vision sin calibrar — ejecuta `calibrate` para configurar ROIs.");
    }
    let calibration = Arc::new(vision.calibration.clone());

    // ── 8. Canal HTTP → game loop ──────────────────────────────────────────
    // Unbounded porque el volumen de comandos es bajo (hot-reloads ocasionales);
    // el loop drena max 4 por tick para no saturar el budget.
    let (loop_tx, loop_rx) = crossbeam_channel::unbounded::<crate::core::loop_::LoopCommand>();

    // ── 9. Game loop (construido ANTES del HTTP server para que el AppState
    //    reciba el handle al MetricsRegistry compartido).
    let rt_handle = tokio::runtime::Handle::current();
    let bot_loop = BotLoop::new(
        config.clone(),
        hotkeys,
        Arc::clone(&shared_state),
        Arc::clone(&frame_buffer),
        Arc::clone(&actuator),
        Arc::clone(&metrics),     // shared con Actuator (record_action_ack)
        rt_handle,
        vision,
        loop_rx,
    );
    let metrics_handle = bot_loop.metrics_handle();
    let health_handle  = bot_loop.health_handle();

    // ── 10. HTTP server ─────────────────────────────────────────────────────
    let app_state = AppState {
        game_state:  Arc::clone(&shared_state),
        buffer:      Arc::clone(&frame_buffer),
        actuator:    Arc::clone(&actuator),
        config:      config.clone(),
        calibration: Arc::clone(&calibration),
        loop_tx:     loop_tx.clone(),
        metrics:     metrics_handle,
        health:      health_handle,
    };

    tokio::spawn(async move {
        if let Err(e) = serve(app_state).await {
            error!("HTTP server error: {:#}", e);
        }
    });

    // ── 11. Lanzar el game loop ─────────────────────────────────────────────
    bot_loop.spawn();
    info!("Game loop lanzado a {} Hz", config.loop_config.target_fps);

    // ── 11. Esperar Ctrl+C ─────────────────────────────────────────────────
    tokio::signal::ctrl_c()
        .await
        .context("Error esperando Ctrl+C")?;

    info!("Shutdown solicitado — liberando recursos...");

    if let Err(e) = actuator.reset().await {
        error!("No se pudo enviar RESET a la Pico: {}", e);
    }

    info!("runtime terminado.");
    Ok(())
}
