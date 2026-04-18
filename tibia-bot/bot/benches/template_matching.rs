//! benches/template_matching.rs — baseline de performance del UiDetector.
//!
//! Mide el coste real de `imageproc::match_template` sobre los templates
//! reales del proyecto (assets/templates/ui/*.png) con las ROIs configuradas
//! en calibration.toml. Reproduce exactamente el trabajo que el bg_worker
//! hace en producción — sin async overhead, medido al microsegundo.
//!
//! ## Ejecución
//!
//! ```bash
//! # Baseline inicial (antes del split Always/OnDemand)
//! cargo bench --bench template_matching -- --save-baseline pre_split
//!
//! # Tras el cambio
//! cargo bench --bench template_matching -- --baseline pre_split
//! ```
//!
//! ## Qué mide
//!
//! - `template_match_single/<name>`: un solo `match_template` por template
//!   real con su ROI configurada. Útil para ver el coste por-elemento.
//! - `full_cycle_current`: procesa los 4 templates de UI tal como hoy. Es
//!   el cycle_time que el bg_worker experimenta.
//! - `hot_cycle_only`: procesa solo los templates "hot" (npc_trade_bag +
//!   depot_chest). Es el cycle esperado tras el split Always/OnDemand
//!   mientras el cavebot camina sin stow/trade activos.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use image::GrayImage;
use imageproc::template_matching::{match_template, MatchTemplateMethod};
use std::path::PathBuf;

/// Ruta al directorio `assets/` del proyecto, resuelta desde CARGO_MANIFEST_DIR
/// (= tibia-bot/bot/). Los assets viven un nivel arriba.
fn assets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR sin parent")
        .join("assets")
}

/// Frame sintético 1920×1080 RGBA con patrón pseudo-aleatorio determinista
/// (seed fijo). No necesita ser realista — sólo no-uniforme para evitar que
/// el compilador optimice la convolución al trivial case.
fn synth_frame(w: u32, h: u32) -> Vec<u8> {
    let mut data = vec![255u8; (w as usize) * (h as usize) * 4];
    // Patrón determinista: linear-congruential micro-PRNG inline.
    let mut state: u32 = 0x1234_5678;
    for i in 0..(w as usize * h as usize) {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let r = (state >> 16) as u8;
        let g = (state >> 8)  as u8;
        let b =  state        as u8;
        data[i * 4]     = r;
        data[i * 4 + 1] = g;
        data[i * 4 + 2] = b;
        data[i * 4 + 3] = 255;
    }
    data
}

/// Extrae una sub-imagen del frame RGBA y la convierte a luma8.
/// Replica la lógica de `ui_detector::crop_to_gray` para que el bench
/// mida la misma cantidad de trabajo que producción.
fn rgba_crop_to_gray(
    rgba:        &[u8],
    frame_w:     u32,
    roi_x:       u32,
    roi_y:       u32,
    roi_w:       u32,
    roi_h:       u32,
) -> GrayImage {
    let stride = frame_w as usize * 4;
    let mut gray = GrayImage::new(roi_w, roi_h);
    for row in 0..roi_h {
        for col in 0..roi_w {
            let off = (roi_y + row) as usize * stride + (roi_x + col) as usize * 4;
            let r = rgba[off]     as u32;
            let g = rgba[off + 1] as u32;
            let b = rgba[off + 2] as u32;
            let luma = (299 * r + 587 * g + 114 * b) / 1000;
            gray.put_pixel(col, row, image::Luma([luma as u8]));
        }
    }
    gray
}

fn load_template(rel_path: &str) -> Option<GrayImage> {
    let path = assets_dir().join(rel_path);
    match image::open(&path) {
        Ok(img) => Some(img.to_luma8()),
        Err(e)  => {
            eprintln!("bench: skip template {} ({})", path.display(), e);
            None
        }
    }
}

/// Una entrada del set de templates: nombre, path relativo desde `assets/`,
/// ROI configurada `(x, y, w, h)`, y categoría de priority.
struct TemplateSpec {
    name:     &'static str,
    path:     &'static str,
    roi:      (u32, u32, u32, u32),
    priority: Priority,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Priority {
    /// Siempre procesado por el bg_worker (debe ser barato).
    /// Alineado con `UiDetector::classify_template` en bot/src/sense/vision/
    /// ui_detector.rs.
    Always,
    /// Solo procesado cuando el cavebot lo activa via `request_on_demand`.
    /// Los bench labelados `OnDemand` representan el coste que el worker
    /// paga SOLAMENTE mientras el step correspondiente está activo.
    OnDemand,
}

/// Matriz de templates reales del proyecto al estado de 2026-04-18.
/// ROIs tomadas de calibration.toml. Priority alineada con
/// `classify_template()` en ui_detector.rs.
const TEMPLATES: &[TemplateSpec] = &[
    TemplateSpec {
        name:     "npc_trade_bag",
        path:     "templates/ui/npc_trade_bag.png",
        roi:      (0, 250, 180, 100),
        priority: Priority::Always,
    },
    TemplateSpec {
        name:     "depot_chest",
        path:     "templates/ui/depot_chest.png",
        roi:      (1550, 0, 370, 500),
        priority: Priority::OnDemand,
    },
    TemplateSpec {
        name:     "stow_menu",
        path:     "templates/ui/stow_menu.png",
        // 2026-04-18: ROI reducido de (900,0,1020,800) a (1400,50,520,550)
        // tras recortar el template a solo la franja "Stow all items of
        // this type" (215×219 → 210×24). Mantener sync con calibration.toml.
        roi:      (1400, 50, 520, 550),
        priority: Priority::OnDemand,
    },
    // npc_trade tiene template 595×374 > ROI width 420. El bg_worker lo
    // skipea silenciosamente. Lo incluimos igual para que el bench lo
    // detecte y reporte.
    TemplateSpec {
        name:     "npc_trade",
        path:     "templates/ui/npc_trade.png",
        roi:      (50, 25, 420, 650),
        priority: Priority::OnDemand,
    },
];

fn bench_single_templates(c: &mut Criterion) {
    let frame = synth_frame(1920, 1080);
    let mut group = c.benchmark_group("template_match_single");
    // stow_menu es ~segundos; si dejamos el default de 100 samples, el bench
    // solo tarda horas. 10 samples + measurement_time corto es suficiente
    // para los deltas que buscamos (ordenes de magnitud, no %).
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(8));

    for spec in TEMPLATES {
        let Some(tpl) = load_template(spec.path) else { continue; };
        let (x, y, w, h) = spec.roi;
        let search = rgba_crop_to_gray(&frame, 1920, x, y, w, h);

        if search.width() < tpl.width() || search.height() < tpl.height() {
            eprintln!(
                "bench: SKIP '{}' — template {}×{} > ROI {}×{} (mismo skip silencioso que bg_worker)",
                spec.name, tpl.width(), tpl.height(), search.width(), search.height()
            );
            continue;
        }

        let id = BenchmarkId::from_parameter(format!(
            "{} [tpl {}x{} | roi {}x{} | {}]",
            spec.name, tpl.width(), tpl.height(), w, h,
            match spec.priority { Priority::Always => "ALWAYS", Priority::OnDemand => "ON_DEMAND" },
        ));
        group.bench_with_input(id, spec, |b, _| {
            b.iter(|| {
                let _ = match_template(
                    black_box(&search),
                    black_box(&tpl),
                    MatchTemplateMethod::SumOfSquaredErrorsNormalized,
                );
            });
        });
    }
    group.finish();
}

/// Simula un cycle entero del bg_worker: convierte el frame, hace match
/// por cada template, y devuelve un scalar. Lo que el worker actual hace.
fn run_cycle(frame: &[u8], templates: &[(GrayImage, (u32, u32, u32, u32))]) -> usize {
    let mut found = 0usize;
    for (tpl, (x, y, w, h)) in templates {
        let search = rgba_crop_to_gray(frame, 1920, *x, *y, *w, *h);
        if search.width() < tpl.width() || search.height() < tpl.height() {
            continue;
        }
        let res = match_template(&search, tpl, MatchTemplateMethod::SumOfSquaredErrorsNormalized);
        let best = res.iter().cloned().fold(f32::MAX, f32::min);
        if best < 0.15 { found += 1; }
    }
    found
}

fn bench_cycle(c: &mut Criterion) {
    let frame = synth_frame(1920, 1080);

    let mut loaded: Vec<(String, Priority, GrayImage, (u32, u32, u32, u32))> = Vec::new();
    for spec in TEMPLATES {
        if let Some(tpl) = load_template(spec.path) {
            loaded.push((spec.name.to_string(), spec.priority, tpl, spec.roi));
        }
    }

    let all_templates: Vec<_> = loaded.iter().map(|(_, _, t, r)| (t.clone(), *r)).collect();
    let always_only: Vec<_> = loaded.iter()
        .filter(|(_, p, _, _)| *p == Priority::Always)
        .map(|(_, _, t, r)| (t.clone(), *r))
        .collect();

    // Escenarios post-split: Always + un OnDemand específico activado.
    // Simulan el coste del cycle mientras un step concreto del cavebot corre.
    let with_onedemand = |name: &str| -> Vec<(GrayImage, (u32, u32, u32, u32))> {
        loaded.iter()
            .filter(|(n, p, _, _)| *p == Priority::Always || n == name)
            .map(|(_, _, t, r)| (t.clone(), *r))
            .collect()
    };
    let always_plus_stow  = with_onedemand("stow_menu");
    let always_plus_depot = with_onedemand("depot_chest");
    let always_plus_trade = with_onedemand("npc_trade");

    eprintln!(
        "bench: pre_split cycle = {} templates, post_split always_only = {} templates",
        all_templates.len(), always_only.len()
    );

    let mut group = c.benchmark_group("cycle");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(10));

    // Pre-split: lo que el worker procesa HOY cada ciclo (los 4 templates).
    group.bench_function("pre_split_full", |b| {
        b.iter(|| black_box(run_cycle(black_box(&frame), black_box(&all_templates))))
    });

    // Post-split steady state: solo Always. Representa el 99% del tiempo
    // que el bot pasa caminando por el hunt.
    group.bench_function("post_split_steady", |b| {
        b.iter(|| black_box(run_cycle(black_box(&frame), black_box(&always_only))))
    });

    // Post-split mientras un step específico está activo (peak costs).
    group.bench_function("post_split_with_stow",  |b| {
        b.iter(|| black_box(run_cycle(black_box(&frame), black_box(&always_plus_stow))))
    });
    group.bench_function("post_split_with_depot", |b| {
        b.iter(|| black_box(run_cycle(black_box(&frame), black_box(&always_plus_depot))))
    });
    group.bench_function("post_split_with_trade", |b| {
        b.iter(|| black_box(run_cycle(black_box(&frame), black_box(&always_plus_trade))))
    });

    group.finish();
}

criterion_group!(benches, bench_single_templates, bench_cycle);
criterion_main!(benches);
