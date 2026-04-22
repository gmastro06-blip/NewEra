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

criterion_group!(benches, bench_inventory_exact_vs_shift, bench_region_monitor_tick);
criterion_main!(benches);
