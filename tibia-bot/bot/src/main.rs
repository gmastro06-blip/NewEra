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
mod remote;
mod safety;
mod scripting;
mod sense;
mod waypoints;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{error, info};
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
                .unwrap_or_else(|_| EnvFilter::new("tibia_bot=info")),
        )
        .with_target(false)
        .init();

    info!("=== tibia-bot arrancando ===");

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

    // ── 6. Actuator ────────────────────────────────────────────────────────
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
        Arc::new(Actuator::with_jitter(pico_handle, &config.coords, jitter))
    } else {
        Arc::new(Actuator::new(pico_handle, &config.coords))
    };

    // ── 7. Vision (carga calibration.toml y templates desde assets/) ───────
    let assets_dir = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets"));
    let mut vision = Vision::load(&assets_dir);
    vision.load_map_index(&config.game_coords);
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

    // ── 9. HTTP server ─────────────────────────────────────────────────────
    let app_state = AppState {
        game_state:  Arc::clone(&shared_state),
        buffer:      Arc::clone(&frame_buffer),
        actuator:    Arc::clone(&actuator),
        config:      config.clone(),
        calibration: Arc::clone(&calibration),
        loop_tx:     loop_tx.clone(),
    };

    tokio::spawn(async move {
        if let Err(e) = serve(app_state).await {
            error!("HTTP server error: {:#}", e);
        }
    });

    // ── 10. Game loop ───────────────────────────────────────────────────────
    // Capturamos el handle del runtime tokio para poder despachar acciones
    // async desde el thread síncrono del game loop (ver loop_.rs::dispatch_action).
    let rt_handle = tokio::runtime::Handle::current();
    let bot_loop = BotLoop::new(
        config.clone(),
        hotkeys,
        Arc::clone(&shared_state),
        Arc::clone(&frame_buffer),
        Arc::clone(&actuator),
        rt_handle,
        vision,
        loop_rx,
    );
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

    info!("tibia-bot terminado.");
    Ok(())
}
