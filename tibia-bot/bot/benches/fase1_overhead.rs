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

// ── Instrumentation: record_tick + record_action_ack overhead ──────────
//
// Mide el costo real del MetricsRegistry hot path. Claim del commit
// 9d13345: "~500 ns/tick estimado" — esto lo valida empíricamente.
// Si supera 1 µs, considerar que las actualizaciones tipo histogram
// son más caras de lo asumido (prob. cache misses por tantos AtomicU64).

fn bench_instrumentation_record_tick(c: &mut Criterion) {
    use tibia_bot::instrumentation::{
        ActionKindTag, MetricsRegistry, ReaderId, TickFlags, TickMetrics,
    };

    let registry = MetricsRegistry::new();

    let mk_tick = |i: u64| {
        let mut per_reader = [0u16; ReaderId::COUNT];
        per_reader[ReaderId::HpMana as usize]    = 100;
        per_reader[ReaderId::Battle as usize]    = 5_000;
        per_reader[ReaderId::Inventory as usize] = if i % 15 == 0 { 8_000 } else { 0 };
        TickMetrics {
            tick: i,
            frame_seq: i,
            ts_unix_ms: 1_745_347_291_000 + i,
            frame_age_us: 80_000 + (i as u32 % 30) * 100,
            acquire_us: 50,
            vision_total_us: 12_000 + (i as u32 % 7) * 200,
            filter_us: 300,
            fsm_us: 200,
            dispatch_us: 100,
            state_write_us: 0,
            tick_total_us: 18_000 + (i as u32 % 11) * 100,
            vision_per_reader_us: per_reader,
            last_action_kind: if i % 10 == 0 { ActionKindTag::Heal } else { ActionKindTag::Key },
            last_action_rtt_us: 0,
            valid_anchors: 2,
            total_anchors: 2,
            anchor_confidence_bp: 10_000,
            vitals_confidence_bp: 9_500,
            target_confidence_bp: 8_000,
            enemies_visible: 3,
            inventory_items: 5,
            flags: if i % 100 == 0 { TickFlags::TICK_OVERRUN } else { TickFlags::NONE },
        }
    };

    // Pre-warm con 100 ticks para que las CircularU32 windows estén pobladas.
    for i in 0..100 { registry.record_tick(mk_tick(i)); }

    c.bench_function("instrumentation_record_tick_steady_state", |b| {
        let mut i = 100u64;
        b.iter(|| {
            let m = mk_tick(i);
            registry.record_tick(black_box(m));
            i += 1;
        });
    });
}

fn bench_instrumentation_record_action_ack(c: &mut Criterion) {
    use tibia_bot::instrumentation::{ActionKindTag, MetricsRegistry};
    let registry = MetricsRegistry::new();
    c.bench_function("instrumentation_record_action_ack", |b| {
        let mut rtt = 1_000u32;
        b.iter(|| {
            registry.record_action_ack(
                black_box(ActionKindTag::Heal),
                black_box(rtt),
                true,
            );
            rtt = rtt.wrapping_add(13);
        });
    });
}

// ── HealthSystem evaluate_tick overhead ────────────────────────────────
//
// Mide el costo de la evaluación per-tick del HealthSystem. Claim del
// commit ee60545: "~1 µs estimado". Validación empírica.

fn bench_health_evaluate_tick(c: &mut Criterion) {
    use tibia_bot::health::{HealthConfig, HealthSystem};
    use tibia_bot::health::system::ExtraInputs;
    use tibia_bot::instrumentation::{
        ActionKindTag, MetricsRegistry, ReaderId, TickFlags, TickMetrics,
    };

    let mut sys = HealthSystem::new(HealthConfig::default());
    let registry = MetricsRegistry::new();

    let mk_tick = |i: u64| {
        let mut per_reader = [0u16; ReaderId::COUNT];
        per_reader[ReaderId::HpMana as usize]    = 100;
        per_reader[ReaderId::Battle as usize]    = 5_000;
        per_reader[ReaderId::Inventory as usize] = if i % 15 == 0 { 8_000 } else { 0 };
        TickMetrics {
            tick: i,
            frame_seq: i,
            ts_unix_ms: 1_745_347_291_000 + i,
            frame_age_us: 80_000,
            acquire_us: 50,
            vision_total_us: 12_000,
            filter_us: 300,
            fsm_us: 200,
            dispatch_us: 100,
            state_write_us: 0,
            tick_total_us: 18_000,
            vision_per_reader_us: per_reader,
            last_action_kind: ActionKindTag::Key,
            last_action_rtt_us: 0,
            valid_anchors: 2,
            total_anchors: 2,
            anchor_confidence_bp: 10_000,
            vitals_confidence_bp: 9_500,
            target_confidence_bp: 8_000,
            enemies_visible: 3,
            inventory_items: 5,
            flags: TickFlags::NONE,
        }
    };

    // Pre-warm con 100 ticks para que las windows estén pobladas.
    for i in 0..100 {
        registry.record_tick(mk_tick(i));
        sys.evaluate_tick(&mk_tick(i), &registry, ExtraInputs::default());
    }

    c.bench_function("health_evaluate_tick_steady_state_ok", |b| {
        let mut i = 100u64;
        b.iter(|| {
            let m = mk_tick(i);
            registry.record_tick(m);
            let s = sys.evaluate_tick(black_box(&m), black_box(&registry),
                                      ExtraInputs::default());
            black_box(s);
            i += 1;
        });
    });

    // Escenario degradado: varios issues activos + composite triggered.
    let mk_degraded_tick = |i: u64| {
        let mut m = mk_tick(i);
        m.frame_age_us = 250_000;     // FrameStale critical
        m.tick_total_us = 60_000;     // TickOverrun critical
        m.vision_total_us = 30_000;   // VisionSlow critical
        m.flags = TickFlags::ANCHOR_LOST | TickFlags::FRAME_STALE;
        m
    };
    c.bench_function("health_evaluate_tick_steady_state_degraded", |b| {
        let mut i = 200u64;
        b.iter(|| {
            let m = mk_degraded_tick(i);
            registry.record_tick(m);
            let s = sys.evaluate_tick(black_box(&m), black_box(&registry),
                                      ExtraInputs { anchor_drift_streak: 10, bridge_rtt_ms: None });
            black_box(s);
            i += 1;
        });
    });
}

criterion_group!(
    benches,
    bench_inventory_exact_vs_shift,
    bench_region_monitor_tick,
    bench_perception_filter_apply,
    bench_instrumentation_record_tick,
    bench_instrumentation_record_action_ack,
    bench_health_evaluate_tick,
);
criterion_main!(benches);
