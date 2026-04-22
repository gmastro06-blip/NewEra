//! benches/fase1_overhead.rs — valida afirmaciones de overhead de Fase 1+2.
//!
//! Commits previos afirmaron:
//! - Fase 1.4 shift-tolerant matching "25× positions, sigue dentro del budget"
//! - Fase 1.5 region_monitor "~5ms/tick overhead"
//!
//! Este bench MIDE empíricamente esos claims sin depender de un bot live,
//! generando frames sintéticos del mismo tamaño que el real (1920×1080)
//! y ejecutando los paths relevantes.
//!
//! ## Ejecución
//!
//! ```bash
//! cargo bench --bench fase1_overhead
//! ```
//!
//! ## Qué mide
//!
//! - `inventory_match_no_shift`: 16 slots × 5 templates contra slot 32×32 (baseline)
//! - `inventory_match_shift_2px`: 16 slots × 5 templates contra slot 36×36 padded
//!   (con ±2 px tolerance). Debería ser ~5× cost del baseline.
//! - `region_monitor_tick_3regions`: tick() con 3 regiones típicas
//!   (battle_list 171×997, minimap 107×110, viewport 967×719).
//!
//! ## Budget referencia
//!
//! El game loop tiene budget 33.3 ms/tick (30 Hz). Inventory scan corre cada
//! 15 ticks (~500ms) así que puede usar hasta ~30 ms sin overrunning.
//! Region monitor corre cada tick así que debe ser < 5 ms para no dominar.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use image::GrayImage;
use imageproc::template_matching::{match_template, MatchTemplateMethod};

// ── Helpers ────────────────────────────────────────────────────────────

fn make_slot(w: u32, h: u32, seed: u8) -> GrayImage {
    let mut img = GrayImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let v = ((x.wrapping_mul(17) ^ y.wrapping_mul(31)) as u8).wrapping_add(seed);
            img.put_pixel(x, y, image::Luma([v]));
        }
    }
    img
}

// ── Inventory matching: exact vs shift-tolerant ───────────────────────

fn bench_inventory_exact_vs_shift(c: &mut Criterion) {
    // 5 templates 32×24 (post stack count strip).
    let templates: Vec<GrayImage> = (0..5u8).map(|i| make_slot(32, 24, i * 50)).collect();

    // 16 slots como el inventory_backpack_strip. Baseline: slot 32×24 exacto.
    let slots_exact: Vec<GrayImage> = (0..16u8).map(|i| make_slot(32, 24, i * 17)).collect();
    // Fase 1.4: slot 36×28 con +2 px padding cada lado.
    let slots_padded: Vec<GrayImage> = (0..16u8).map(|i| make_slot(36, 28, i * 17)).collect();

    c.bench_function("inventory_match_exact_16slots_5templates", |b| {
        b.iter(|| {
            for slot in &slots_exact {
                for tpl in &templates {
                    let result = match_template(
                        black_box(slot),
                        black_box(tpl),
                        MatchTemplateMethod::CrossCorrelationNormalized,
                    );
                    let _max = result.iter().cloned().fold(f32::MIN, f32::max);
                }
            }
        });
    });

    c.bench_function("inventory_match_shift_2px_16slots_5templates", |b| {
        b.iter(|| {
            for slot in &slots_padded {
                for tpl in &templates {
                    let result = match_template(
                        black_box(slot),
                        black_box(tpl),
                        MatchTemplateMethod::CrossCorrelationNormalized,
                    );
                    let _max = result.iter().cloned().fold(f32::MIN, f32::max);
                }
            }
        });
    });
}

// ── Region monitor tick overhead ──────────────────────────────────────

fn bench_region_monitor_tick(c: &mut Criterion) {
    use std::time::Instant;
    use tibia_bot::sense::frame_buffer::Frame;
    use tibia_bot::sense::vision::calibration::RoiDef;
    use tibia_bot::sense::vision::region_monitor::RegionMonitor;

    // Frame sintético 1920×1080 BGRA.
    let frame = Frame {
        width:       1920,
        height:      1080,
        data:        vec![128u8; 1920 * 1080 * 4],
        captured_at: Instant::now(),
    };

    // 3 regiones como las que wire el game loop.
    let mut monitor = RegionMonitor::new();
    monitor.add_region("battle_list", RoiDef::new(2, 45, 171, 997), 0.05);
    monitor.add_region("minimap",     RoiDef::new(1753, 4, 107, 110), 0.05);
    monitor.add_region("viewport",    RoiDef::new(388, 83, 967, 719), 0.10);

    // Pre-tick para popular el prev_snapshot (primer tick es barato porque
    // no hay diff). Queremos medir el diff steady-state.
    monitor.tick(&frame, 0);

    c.bench_function("region_monitor_tick_3regions_1920x1080", |b| {
        let mut tick = 1u64;
        b.iter(|| {
            let diffs = monitor.tick(black_box(&frame), tick);
            tick += 1;
            black_box(diffs);
        });
    });
}

// ── PerceptionFilter::apply overhead ───────────────────────────────────
//
// Mide el costo real de aplicar el filter sobre un Perception "típico".
// Claim del commit a1e86e6: "<5 µs típico". Este bench lo valida y da
// regression signal si añadimos primitivas costosas.
//
// Escenario: char en hunt activo — vitals presentes, target_active Some,
// is_moving Some(true), 3 enemies en battle list, game_coords Some.

fn bench_perception_filter_apply(c: &mut Criterion) {
    use tibia_bot::sense::filter::PerceptionFilter;
    use tibia_bot::sense::perception::{
        BattleEntry, BattleList, CharVitals, EntryKind, Perception, VitalBar,
    };

    let mk_perception = |hp: f32, mana: f32, n_enemies: usize| Perception {
        vitals: CharVitals {
            hp:   Some(VitalBar { ratio: hp, filled_px: (hp * 100.0) as u32, total_px: 100 }),
            mana: Some(VitalBar { ratio: mana, filled_px: (mana * 100.0) as u32, total_px: 100 }),
        },
        battle: BattleList {
            entries: (0..n_enemies).map(|i| BattleEntry {
                kind: EntryKind::Monster,
                row: i as u8,
                hp_ratio: Some(1.0),
                name: None,
                is_being_attacked: i == 0,
            }).collect(),
            ..Default::default()
        },
        target_active: Some(true),
        is_moving:     Some(true),
        game_coords:   Some((32015, 32212, 7)),
        ..Default::default()
    };

    // Pre-warm el filter con varios apply para que las medianas/votos estén
    // pobladas (representa steady-state del bot, no boot-up).
    let mut filter = PerceptionFilter::new();
    let p_warmup = mk_perception(0.9, 0.8, 3);
    for _ in 0..10 { let _ = filter.apply(&p_warmup); }

    c.bench_function("perception_filter_apply_steady_state", |b| {
        // Alterna ligeramente HP/mana/enemies para ejercitar EMAs/medians.
        let mut tick = 0u32;
        b.iter(|| {
            let hp   = 0.85 + ((tick % 7) as f32) * 0.01;
            let mana = 0.75 + ((tick % 5) as f32) * 0.02;
            let n    = ((tick % 4) + 2) as usize; // 2..5 enemies
            let p    = mk_perception(hp, mana, n);
            let out  = filter.apply(black_box(&p));
            tick += 1;
            black_box(out);
        });
    });
}

criterion_group!(
    benches,
    bench_inventory_exact_vs_shift,
    bench_region_monitor_tick,
    bench_perception_filter_apply,
);
criterion_main!(benches);
