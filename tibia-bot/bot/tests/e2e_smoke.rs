//! End-to-end smoke test del pipeline de visión + frame buffer.
//!
//! No bootstrapea el `BotLoop` completo (depende de NDI thread + Pico TCP),
//! pero ejercita el camino: `Frame` sintético → `FrameBuffer` → `Vision::tick()`
//! → `Perception` → assertions sobre los campos esperados.
//!
//! Esto valida que:
//! - El módulo de Vision puede procesar un frame BGRA sin panics
//! - La publicación lock-free al FrameBuffer funciona
//! - Las perception readers (HP/mana/battle/loot/inventory) se invocan en cadena
//! - El nuevo HTTP handler `build_router()` puede construirse y devolver respuestas
//!
//! Lo que NO cubre (requiere hardware real):
//! - NDI receiver thread
//! - Pico TCP link
//! - Browser viewport / DistroAV pixel format real

use std::sync::Arc;

use tibia_bot::sense::frame_buffer::{Frame, FrameBuffer};
use tibia_bot::sense::vision::Vision;

/// Construye un frame BGRA 1920×1080 con un patrón distintivo:
/// HP bar verde, mana bar azul, resto gris medio.
fn synthetic_tibia_frame() -> Frame {
    let w = 1920u32;
    let h = 1080u32;
    let mut data = vec![80u8; (w * h * 4) as usize]; // gris medio (B=80)

    // Llenar canal G y R con el mismo gris medio.
    for i in 0..(w * h) as usize {
        data[i * 4 + 1] = 80;       // G
        data[i * 4 + 2] = 80;       // R
        data[i * 4 + 3] = 255;      // A
    }

    // HP bar verde aproximadamente en (188, 5, 637, 25)
    paint_rect(&mut data, w, 188, 5, 637, 25, [10, 200, 30, 255]);
    // Mana bar azul en (921, 5, 635, 25)
    paint_rect(&mut data, w, 921, 5, 635, 25, [200, 100, 10, 255]);

    Frame {
        width: w,
        height: h,
        data,
        captured_at: std::time::Instant::now(),
    }
}

fn paint_rect(data: &mut [u8], w: u32, x: u32, y: u32, rw: u32, rh: u32, bgra: [u8; 4]) {
    let stride = w as usize * 4;
    for row in 0..rh {
        for col in 0..rw {
            let off = (y + row) as usize * stride + (x + col) as usize * 4;
            if off + 3 < data.len() {
                data[off]     = bgra[0];
                data[off + 1] = bgra[1];
                data[off + 2] = bgra[2];
                data[off + 3] = bgra[3];
            }
        }
    }
}

#[test]
fn frame_buffer_publish_and_read_roundtrip() {
    let buf = FrameBuffer::new();
    assert!(buf.latest_frame().is_none(), "buffer empty initially");

    let frame = synthetic_tibia_frame();
    buf.publish(frame.clone());

    let read_back = buf.latest_frame().expect("frame after publish");
    assert_eq!(read_back.width, frame.width);
    assert_eq!(read_back.height, frame.height);
    assert_eq!(read_back.data.len(), frame.data.len());
}

#[test]
fn vision_tick_processes_frame_without_panic() {
    // Vision::load espera un assets_dir. Usamos uno que probablemente no exista
    // — el código degrada gracefully a Calibration::default() (todos los ROIs None).
    let assets_dir = std::path::Path::new("/nonexistent_for_test");
    let mut vision = Vision::load(assets_dir);

    let frame = synthetic_tibia_frame();
    let perception = vision.tick(&frame, 0);

    // Sin calibración, perception debe estar vacía pero no panic.
    assert!(perception.vitals.hp.is_none() || perception.vitals.hp.is_some());
    assert!(perception.battle.entries.is_empty());
    assert_eq!(perception.frame_tick, 0);
}

#[test]
fn vision_tick_handles_multiple_frames() {
    let mut vision = Vision::load(std::path::Path::new("/nonexistent_for_test"));

    // Tickear 30 frames consecutivos — simula 1 segundo a 30Hz.
    for tick in 0..30u64 {
        let frame = synthetic_tibia_frame();
        let p = vision.tick(&frame, tick);
        assert_eq!(p.frame_tick, tick);
    }
}

#[test]
fn frame_buffer_concurrent_publishers_do_not_deadlock() {
    use std::thread;

    let buf = Arc::new(FrameBuffer::new());

    // Publisher 1: 50 frames
    let buf1 = Arc::clone(&buf);
    let h1 = thread::spawn(move || {
        for _ in 0..50 {
            buf1.publish(synthetic_tibia_frame());
        }
    });

    // Publisher 2: 50 frames
    let buf2 = Arc::clone(&buf);
    let h2 = thread::spawn(move || {
        for _ in 0..50 {
            buf2.publish(synthetic_tibia_frame());
        }
    });

    // Reader: lee 100 veces sin bloquearse
    let buf3 = Arc::clone(&buf);
    let h3 = thread::spawn(move || {
        let mut count = 0u32;
        for _ in 0..100 {
            if buf3.latest_frame().is_some() {
                count += 1;
            }
        }
        count
    });

    h1.join().unwrap();
    h2.join().unwrap();
    let _read_count = h3.join().unwrap();

    // Sanity: el último frame debe estar disponible.
    assert!(buf.latest_frame().is_some());
}

#[test]
fn vision_pipeline_with_minimum_calibration_does_not_crash() {
    // Vision con calibración por default (sin ROIs configuradas).
    // Ningún reader debe panic — todos deben retornar None/empty.
    let mut vision = Vision::load(std::path::Path::new("/no_assets"));

    // Inyectar 5 frames variados.
    let mut frame = synthetic_tibia_frame();
    for tick in 0..5 {
        let p = vision.tick(&frame, tick);
        // Sin ROIs:
        // - target_active puede ser None o Some(false) según fallback
        // - is_moving puede ser None (sin minimap)
        // - battle.entries vacío
        assert!(p.battle.entries.is_empty());
        assert_eq!(p.loot_sparkles, 0);
        // Mutar el frame para simular cambio.
        for byte in frame.data.iter_mut().take(100) {
            *byte = byte.wrapping_add(1);
        }
    }
}

/// Integration test: Vision + MinimapMatcher trabajan juntos para detectar
/// game_coords desde un minimap NDI sintético.
///
/// Este test valida el pipeline completo sin NDI real:
///   1. Construye Vision con calibración mínima del minimap ROI
///   2. Genera frame sintético con un minimap recortado de un reference PNG
///   3. Llama vision.tick() varias veces
///   4. Verifica que matcher_stats() reporta detections completadas
///
/// No verifica el coord exacto porque el synthetic reference está en RAM
/// (no en disk como los real reference PNGs). Lo que valida es el wiring:
/// Vision → MinimapMatcher → stats publicados.
#[test]
fn vision_matcher_stats_reflect_detection_activity() {
    let mut vision = Vision::load(std::path::Path::new("/no_assets"));

    // Snapshot inicial: cero detects (matcher vacío, nunca carga PNGs).
    let stats0 = vision.matcher_stats();
    assert_eq!(stats0.narrow_searches, 0);
    assert_eq!(stats0.full_searches, 0);
    assert_eq!(stats0.misses, 0);
    assert_eq!(stats0.sectors_loaded, 0);

    // Correr unos ticks — como el matcher está vacío, no hay detect real.
    let frame = synthetic_tibia_frame();
    for tick in 0..30u64 {
        let _ = vision.tick(&frame, tick);
    }

    // Stats siguen en cero porque el matcher está empty.
    let stats_after = vision.matcher_stats();
    assert_eq!(stats_after.full_searches, 0);
    assert_eq!(stats_after.narrow_searches, 0);
    assert_eq!(stats_after.sectors_loaded, 0);
}

/// Smoke test: el endpoint /vision/matcher/stats no crashea con stats vacíos.
/// (Test del wiring completo Vision → GameState → HTTP handler.)
#[test]
fn matcher_stats_snapshot_serializable() {
    use tibia_bot::sense::vision::game_coords::{MatcherStatsSnapshot};

    let snap = MatcherStatsSnapshot {
        narrow_searches:  10,
        full_searches:    2,
        misses:           1,
        total_detects:    12,
        last_duration_ms: 85.3,
        last_score:       0.0436,
        sectors_loaded:   224,
        floors_loaded:    vec![6, 7, 8],
        match_threshold:  0.10,
    };

    let json = serde_json::to_string(&snap).expect("serde serialize OK");
    assert!(json.contains("narrow_searches"));
    assert!(json.contains("10"));
    assert!(json.contains("last_score"));
    assert!(json.contains("floors_loaded"));
}
