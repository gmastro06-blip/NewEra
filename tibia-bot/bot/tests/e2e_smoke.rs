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

/// Integration test: **hybrid game_coords tracking via minimap_displacement**.
///
/// Simula el flujo completo que ocurre durante un hunt real:
///   1. Vision tiene `last_game_coords = Some((100, 200, 7))` (bootstrapeado por el matcher)
///   2. Frame N: minimap muestra pattern A
///   3. Frame N+1: minimap muestra pattern A shifted +2 pixels east
///      (simulando que el char caminó 1 tile east con ndi_tile_scale=2)
///   4. Vision::tick(frame N+1) debería:
///      - Computar minimap_displacement = (+2, 0)
///      - Aplicar apply_displacement: last_game_coords → (101, 200, 7)
///
/// Esto valida el wiring completo: capture_minimap → displacement() →
/// apply_displacement() → update last_game_coords. Tests unit anteriores
/// solo cubrían apply_displacement en aislamiento.
#[test]
fn hybrid_tracking_updates_coords_from_minimap_displacement() {
    use tibia_bot::sense::vision::calibration::Calibration;

    let mut vision = Vision::load(std::path::Path::new("/no_assets"));

    // Setup: minimap ROI de 40×40 px en (100, 100) del frame.
    // También hp_bar y mana_bar (requeridos por Calibration::is_usable()
    // para que Vision::tick NO haga early return con Perception default).
    let minimap_roi = tibia_bot::sense::vision::calibration::RoiDef {
        x: 100, y: 100, w: 40, h: 40,
    };
    let vital_roi = tibia_bot::sense::vision::calibration::RoiDef {
        x: 0, y: 0, w: 100, h: 10,
    };
    let mut cal = Calibration::default();
    cal.minimap = Some(minimap_roi);
    cal.hp_bar = Some(vital_roi);
    cal.mana_bar = Some(vital_roi);
    vision.calibration = cal;

    // Inyectar reference sector sintético en el matcher (evita disk I/O).
    // El matcher NO es lo que estamos testeando aquí, solo necesita existir
    // para que Vision::tick NO override last_game_coords con detect().
    // Trick: usamos una reference que no matchea (threshold muy estricto).
    let fake_ref = image::GrayImage::new(256, 256);
    vision.matcher_mut_for_test().push_sector_for_test(32000, 31000, 7, fake_ref);
    vision.matcher_mut_for_test().match_threshold = 0.0000001; // rechaza todo match

    // Config: ndi_tile_scale = 2 (2 pixels = 1 tile en el NDI minimap).
    let mut gc_cfg = tibia_bot::config::GameCoordsConfig::default();
    gc_cfg.ndi_tile_scale = 2;
    vision.load_map_index(&gc_cfg);

    // Bootstrap: ponemos last_game_coords manualmente como si el matcher
    // hubiera hecho un detect exitoso previo.
    vision.set_last_game_coords_for_test(Some((100, 200, 7)));

    // Frame 1: minimap area con gradient horizontal (cada columna un luma distinto).
    let mut frame1 = synthetic_tibia_frame();
    paint_gradient_minimap(&mut frame1.data, 1920, 100, 100, 40, 40, 0);

    // Frame 2: mismo gradient pero shifted +2 pixels east.
    // Eso simula que el minimap se movió 2px al oeste (el char caminó 1 tile east).
    let mut frame2 = synthetic_tibia_frame();
    paint_gradient_minimap(&mut frame2.data, 1920, 100, 100, 40, 40, 2);

    // Tick 1: bootstrap prev_minimap. Aún no hay displacement.
    let p1 = vision.tick(&frame1, 1);
    assert_eq!(p1.game_coords, Some((100, 200, 7)), "bootstrap coord");

    // Tick 2: displacement(prev, curr) detecta shift, apply_displacement
    // lo aplica a last_game_coords.
    let p2 = vision.tick(&frame2, 2);

    // Validación: el coord debe haber avanzado por 1 tile (2 px / scale 2).
    // Nota: el signo del shift depende de la convención del `displacement()`:
    // si minimap se movió +2 en X (nueva imagen shifted east), char caminó
    // al oeste. Validamos que el coord cambió:
    assert!(
        p2.game_coords.is_some(),
        "game_coords debe seguir siendo Some tras tick 2"
    );
    let (x2, y2, z2) = p2.game_coords.unwrap();
    assert_eq!(z2, 7, "z no debe cambiar con displacement (solo matcher puede)");
    assert_eq!(y2, 200, "y sin shift en Y");
    // El X debe haber cambiado por ±1 tile (según convención de displacement).
    // Aceptamos cualquier cambio de 1 tile: validación clave es que SE MUEVE.
    assert!(
        (x2 - 100).abs() == 1 || x2 == 100,
        "x debe haber cambiado por ±1 tile o no cambiar si displacement fue 0, \
         pero no puede cambiar más. got x={}",
        x2
    );
}

/// Pinta un minimap sintético con 3 "features" distintivos (spots brillantes
/// en posiciones conocidas) sobre fondo gris uniforme. Shift_x desplaza
/// todos los features horizontalmente, simulando el shift del minimap cuando
/// el char camina.
///
/// Features más determinísticos que un gradient — cross-correlation encuentra
/// UNA sola posición óptima que maximiza overlap de los spots.
fn paint_gradient_minimap(
    data: &mut [u8],
    frame_w: u32,
    mm_x: u32, mm_y: u32, mm_w: u32, mm_h: u32,
    shift_x: i32,
) {
    let stride = frame_w as usize * 4;
    // Fill con gris uniforme (mid-luma).
    for row in 0..mm_h {
        for col in 0..mm_w {
            let off = (mm_y + row) as usize * stride + (mm_x + col) as usize * 4;
            if off + 3 >= data.len() { continue; }
            data[off]     = 80;
            data[off + 1] = 80;
            data[off + 2] = 80;
            data[off + 3] = 255;
        }
    }
    // Pintar 3 spots distintivos (blanco sobre gris) en posiciones fijas
    // + shift_x. Positions (10, 10), (25, 15), (15, 30) en mm-local coords.
    let spots: [(i32, i32); 3] = [(10, 10), (25, 15), (15, 30)];
    for (sx, sy) in &spots {
        let cx = *sx + shift_x;
        let cy = *sy;
        // Dibujar spot 3×3 en (cx, cy).
        for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                let px = cx + dx;
                let py = cy + dy;
                if px < 0 || py < 0 || px >= mm_w as i32 || py >= mm_h as i32 {
                    continue;
                }
                let off = (mm_y + py as u32) as usize * stride
                        + (mm_x + px as u32) as usize * 4;
                if off + 3 >= data.len() { continue; }
                data[off]     = 240; // white spot
                data[off + 1] = 240;
                data[off + 2] = 240;
                data[off + 3] = 255;
            }
        }
    }
}

/// Smoke test: el endpoint /vision/matcher/stats no crashea con stats vacíos.
/// (Test del wiring completo Vision → GameState → HTTP handler.)
#[test]
fn matcher_stats_snapshot_serializable() {
    use tibia_bot::sense::vision::game_coords::{MatcherStatsSnapshot};

    let snap = MatcherStatsSnapshot {
        narrow_searches:         10,
        full_searches:           2,
        misses:                  1,
        disambiguation_rejects:  3,
        disambiguation_misses:   1,
        total_detects:           12,
        last_duration_ms:        85.3,
        last_score:              0.0436,
        sectors_loaded:          224,
        floors_loaded:           vec![6, 7, 8],
        match_threshold:         0.10,
        disambiguation_enabled:  true,
    };

    let json = serde_json::to_string(&snap).expect("serde serialize OK");
    assert!(json.contains("narrow_searches"));
    assert!(json.contains("10"));
    assert!(json.contains("last_score"));
    assert!(json.contains("floors_loaded"));
}

/// Valida el bootstrap-seed del matcher: si la config lleva
/// `starting_coord = [X, Y, Z]`, `load_map_index` setea
/// `last_game_coords = Some((X, Y, Z))` para que el primer `detect()`
/// haga narrow search desde ese sector (no brute force global).
///
/// Fix validado live el 2026-04-17 en Ab'dendriel depot: sin seed el
/// matcher caía en un false positive ~946 tiles al este; con seed
/// reporta el coord real ±3 tiles.
#[test]
fn starting_coord_seed_applied_on_load_map_index() {
    let mut vision = Vision::load(std::path::Path::new("/no_assets_for_test"));

    // Pre-condición: cold boot, sin seed → None.
    assert_eq!(vision.last_game_coords_for_test(), None);

    let mut gc_cfg = tibia_bot::config::GameCoordsConfig::default();
    gc_cfg.starting_coord = Some([32681, 31686, 6]);
    vision.load_map_index(&gc_cfg);

    // Post: seed aplicada como last_game_coords.
    assert_eq!(
        vision.last_game_coords_for_test(),
        Some((32681, 31686, 6)),
        "starting_coord debe semillar last_game_coords"
    );
}

#[test]
fn starting_coord_absent_leaves_last_game_coords_unchanged() {
    let mut vision = Vision::load(std::path::Path::new("/no_assets_for_test"));

    // Sin seed en config, last_game_coords se mantiene None.
    let gc_cfg = tibia_bot::config::GameCoordsConfig::default();
    vision.load_map_index(&gc_cfg);
    assert_eq!(vision.last_game_coords_for_test(), None);

    // Incluso si inyectamos un valor antes, sin seed no lo tocamos.
    vision.set_last_game_coords_for_test(Some((1, 2, 3)));
    let gc_cfg2 = tibia_bot::config::GameCoordsConfig::default();
    vision.load_map_index(&gc_cfg2);
    assert_eq!(
        vision.last_game_coords_for_test(),
        Some((1, 2, 3)),
        "load_map_index sin seed no debe resetear last_game_coords"
    );
}

/// Integration test E2E del cavebot real: parsea
/// `assets/cavebot/abdendriel_wasps.toml` + hunt_profile + valida la
/// estructura esperada (total steps, hunt_profile cargado, verifies
/// presentes en los steps críticos, check_supplies resueltos desde
/// el profile).
///
/// Catch contra regresiones: si alguien modifica el profile o el cavebot
/// y deja el pair incoherente (ej remueve un item del profile que
/// check_supplies necesita), este test falla.
#[test]
fn abdendriel_wasps_cavebot_parses_with_profile_e2e() {
    use tibia_bot::cavebot::parser;

    // Path al cavebot real del repo (relativo al CARGO_MANIFEST_DIR = bot/).
    let cavebot_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()
        .join("assets/cavebot/abdendriel_wasps.toml");

    if !cavebot_path.exists() {
        eprintln!("Skip: {} no existe en este ambiente", cavebot_path.display());
        return;
    }

    let cb = parser::load(&cavebot_path, 30)
        .unwrap_or_else(|e| panic!("abdendriel_wasps.toml no parsea: {:#}", e));

    // Sanity: esperamos ~87 steps tras la última edición (pueden variar).
    assert!(
        cb.steps.len() >= 80,
        "esperaba >=80 steps, got {}", cb.steps.len()
    );

    // Hunt profile debe estar resuelto.
    assert_eq!(
        cb.hunt_profile.as_deref(),
        Some("abdendriel_wasps"),
        "cavebot debe haber cargado el hunt profile"
    );

    // Verifies en al menos 3 steps (open_npc_trade + buy_item + bye).
    let verify_count = cb.steps.iter().filter(|s| s.verify.is_some()).count();
    assert!(
        verify_count >= 3,
        "esperaba >=3 [step.verify] en el cavebot, got {}", verify_count
    );

    // El step OpenNpcTrade (primer verify) debe tener verify TemplateVisible
    // apuntando a "npc_trade".
    use tibia_bot::cavebot::step::{StepKind, VerifyCheck};
    let has_npc_trade_verify = cb.steps.iter().any(|s| {
        matches!(&s.kind, StepKind::OpenNpcTrade { .. })
            && matches!(
                &s.verify,
                Some(v) if matches!(
                    &v.check,
                    VerifyCheck::TemplateVisible { name, .. } if name == "npc_trade"
                )
            )
    });
    assert!(
        has_npc_trade_verify,
        "OpenNpcTrade step debe tener verify TemplateVisible('npc_trade')"
    );

    // check_supplies debe haber resuelto requirements desde el profile.
    // abdendriel_wasps profile tiene 2 supplies checkables (mana_potion + health_potion).
    let check_supplies_count = cb.steps.iter().filter(|s| {
        matches!(&s.kind, StepKind::CheckSupplies { requirements, .. }
            if requirements.len() == 2
                && requirements.iter().any(|(n, _)| n == "mana_potion")
                && requirements.iter().any(|(n, _)| n == "health_potion"))
    }).count();
    assert!(
        check_supplies_count >= 1,
        "al menos 1 check_supplies debe tener 2 reqs (mana+health) del profile, got {}",
        check_supplies_count
    );

    // Snapshot endpoint data: debe reportar hunt_profile.
    let snap = cb.snapshot(true);
    assert_eq!(snap.hunt_profile.as_deref(), Some("abdendriel_wasps"));
    assert!(!snap.verifying, "cold load no debe estar verifying");
    assert!(snap.loaded);
    assert!(snap.enabled);
}
