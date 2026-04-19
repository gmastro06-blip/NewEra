//! fixture_vision — smoke tests de la pipeline de vision contra frames
//! NDI reales capturados (test_frames/*.png).
//!
//! **Qué hace**: por cada PNG en `test_frames/`, construye un `Frame` RGBA,
//! invoca `Vision::tick()` con la calibración del proyecto, y verifica
//! invariantes básicos sobre el `Perception` resultante.
//!
//! **Qué NO hace**: assertions exactas tipo "HP = 82%". Esas requieren
//! calibración manual por frame y son frágiles ante cambios de tema/UI del
//! cliente Tibia. Este test valida que la pipeline:
//!
//! 1. Nunca panica en frames reales (captura regresiones donde un nuevo
//!    detector crashea en UI edge cases).
//! 2. Retorna valores coherentes (ratio ∈ [0,1], counters >= 0, etc.).
//! 3. Al menos SOME frames producen detecciones non-None (asegura que
//!    la calibración sigue emparejada con los frames de referencia).
//!
//! Ejecución: `cargo test --release --test fixture_vision`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tibia_bot::sense::frame_buffer::Frame;
use tibia_bot::sense::vision::Vision;

/// Root del workspace (parent de `bot/`).
fn workspace_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR sin parent")
        .to_path_buf()
}

/// Lee un PNG y lo convierte a un `Frame` RGBA listo para la pipeline.
/// Los frames del proyecto son mayoritariamente 1920×1080 RGBA 8-bit;
/// frames con otras dimensiones se aceptan tal cual (la calibración puede
/// dar ROIs fuera de rango y quedarse en defaults).
fn load_frame(path: &Path) -> Frame {
    let img = image::open(path)
        .unwrap_or_else(|e| panic!("no se pudo abrir '{}': {}", path.display(), e));
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Frame {
        width:       w,
        height:      h,
        data:        rgba.into_raw(),
        captured_at: Instant::now(),
    }
}

/// Lista los PNGs en `test_frames/`. Deja fuera cualquier otro archivo.
fn test_frames() -> Vec<PathBuf> {
    let dir = workspace_dir().join("test_frames");
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {}", dir.display(), e));
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("png"))
        .collect();
    paths.sort();
    paths
}

/// Construye una Vision cargando la calibración y assets reales del proyecto.
fn vision_with_project_calibration() -> Vision {
    let assets = workspace_dir().join("assets");
    Vision::load(&assets)
}

/// Invariantes que TODO `Perception` debe satisfacer, independientemente
/// del contenido del frame.
fn assert_perception_invariants(
    p: &tibia_bot::sense::perception::Perception,
    frame_name: &str,
) {
    // frame_tick es u64, siempre >= 0 (trivial). Pasamos tick=0 en el call.
    assert_eq!(p.frame_tick, 0, "{}: frame_tick debería ser 0", frame_name);

    // HP/mana ratio ∈ [0.0, 1.0] si presentes.
    if let Some(bar) = p.vitals.hp {
        assert!(
            bar.ratio >= 0.0 && bar.ratio <= 1.0,
            "{}: hp.ratio fuera de rango: {}", frame_name, bar.ratio
        );
    }
    if let Some(bar) = p.vitals.mana {
        assert!(
            bar.ratio >= 0.0 && bar.ratio <= 1.0,
            "{}: mana.ratio fuera de rango: {}", frame_name, bar.ratio
        );
    }

    // Battle list: el número de enemigos no puede ser negativo (trivial
    // porque es usize/u32) pero verificamos que los slots reportados son
    // coherentes — si hay enemies, battle.enemy_count() > 0.
    if p.battle.has_enemies() {
        assert!(
            p.battle.enemy_count() > 0,
            "{}: has_enemies()=true pero enemy_count()=0", frame_name
        );
    }

    // Si hay game_coords, z debe estar en rango razonable de Tibia
    // (floors 0-15 para el mapa público).
    if let Some((_x, _y, z)) = p.game_coords {
        assert!(
            (0..=15).contains(&z),
            "{}: z={} fuera de rango Tibia [0, 15]", frame_name, z
        );
    }
}

#[test]
fn pipeline_never_panics_on_any_test_frame() {
    let mut vision = vision_with_project_calibration();
    let frames = test_frames();
    assert!(!frames.is_empty(), "test_frames/ no tiene PNGs");

    for path in &frames {
        let frame = load_frame(path);
        // El test REAL es este — si el .tick() panica, el test falla aquí.
        let perception = vision.tick(&frame, 0);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        assert_perception_invariants(&perception, name);
    }

    println!("validados {} frames sin panic", frames.len());
}

#[test]
fn at_least_some_frames_produce_non_default_perception() {
    // Sanity check: si la calibración + templates están alineados con los
    // frames de referencia, al menos ALGUNOS frames deben producir una
    // perception con HP/mana/battle no-default. Si TODOS los frames dan
    // Perception::default()-like, hay un mismatch de calibración (los
    // ROIs apuntan a zonas vacías, los templates están mal, etc.).
    //
    // Threshold bajo (≥1 frame con hp detectado, ≥1 con mana) porque
    // algunos frames del set son de menús/char_select sin HP bar visible.
    let mut vision = vision_with_project_calibration();
    let frames = test_frames();

    let mut hp_detected    = 0usize;
    let mut mana_detected  = 0usize;
    let mut coords_detected = 0usize;

    for path in &frames {
        let frame = load_frame(path);
        let p = vision.tick(&frame, 0);
        if p.vitals.hp.is_some()   { hp_detected    += 1; }
        if p.vitals.mana.is_some() { mana_detected  += 1; }
        if p.game_coords.is_some() { coords_detected += 1; }
    }

    // Threshold soft: al menos UN frame de los 19 debe dar HP. Si cero,
    // la ROI hp_bar está rota o los frames no contienen la UI de Tibia.
    assert!(
        hp_detected > 0,
        "ningún frame produjo hp detection ({}/{}) — calibración rota?",
        hp_detected, frames.len()
    );
    assert!(
        mana_detected > 0,
        "ningún frame produjo mana detection ({}/{}) — calibración rota?",
        mana_detected, frames.len()
    );
    // game_coords es opcional en frames sin minimap (menús) — no asserteamos
    // threshold mínimo, solo logueamos.
    println!(
        "frames con detections: hp={}/{}, mana={}/{}, coords={}/{}",
        hp_detected, frames.len(),
        mana_detected, frames.len(),
        coords_detected, frames.len(),
    );
}

#[test]
fn pipeline_tick_time_within_budget() {
    // El game loop tiene 33ms/tick budget. Vision.tick() es la parte
    // más cara y debe quedar << 30ms en frames típicos (1920×1080).
    //
    // El primer tick incluye setup caches (anchors, etc) así que medimos
    // desde el segundo tick. 50 ms es umbral holgado — el objetivo real
    // es ~5-15 ms con los optimizaciones del Sprint 2.
    let mut vision = vision_with_project_calibration();
    let frames = test_frames();
    let warmup = &frames[0];
    let frame = load_frame(warmup);

    // Warmup (descarta primer tick).
    let _ = vision.tick(&frame, 0);

    // Mide tick #2.
    let start = Instant::now();
    let _ = vision.tick(&frame, 1);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 50,
        "vision.tick() demoró {:?} — presupuesto 50 ms. Regresión de perf?",
        elapsed
    );
    println!("vision.tick() warm: {:?}", elapsed);
}
