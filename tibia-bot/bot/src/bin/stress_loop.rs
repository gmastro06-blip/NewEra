//! stress_loop — Benchmark de estabilidad de sesión larga del Vision pipeline.
//!
//! Simula una sesión de N iteraciones corriendo `Vision::tick()` con un
//! frame sintético. Mide proc time per iteration y detecta degradación
//! (drift) comparando p95 de las primeras 10% vs últimas 10% de muestras.
//!
//! **No requiere Tibia, NDI, Pico ni bridge.** Es un test aislado del
//! Vision pipeline para validar que no hay memory leaks ni slow paths
//! que se acumulen con el tiempo.
//!
//! ## Uso
//!
//! ```bash
//! # Default: 10_000 iterations ≈ 5.5 min at 30 Hz
//! cargo run --release --bin stress_loop
//!
//! # Sesión de 2h: 216000 iterations (30Hz * 60s * 120min)
//! cargo run --release --bin stress_loop -- --iterations 216000
//!
//! # Custom assets
//! cargo run --release --bin stress_loop -- --assets assets --iterations 10000
//! ```
//!
//! ## Criterio de éxito
//!
//! - p95 proc time del último 10% ≤ p95 del primer 10% × 1.10 (drift ≤ 10%)
//! - p99 absoluto ≤ 30 ms
//! - Ninguna iteración excede 100 ms
//!
//! Exit code: 0 si estable, 1 si degradación detectada.

use std::path::PathBuf;
use std::time::Instant;

use tibia_bot::sense::frame_buffer::Frame;
use tibia_bot::sense::vision::Vision;

const DEFAULT_ITERATIONS: usize = 10_000;
const FRAME_WIDTH: u32 = 1920;
const FRAME_HEIGHT: u32 = 1080;
const MAX_PROC_MS: f64 = 100.0;
const P99_THRESHOLD_MS: f64 = 30.0;
const DRIFT_TOLERANCE: f64 = 1.10; // 10% worse allowed

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let iterations = arg_value(&args, "--iterations")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_ITERATIONS);

    let assets = arg_value(&args, "--assets")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets"));

    println!("stress_loop — Vision pipeline stability test");
    println!("  iterations: {}", iterations);
    println!("  assets:     {}", assets.display());
    println!();

    if !assets.exists() {
        anyhow::bail!("assets dir not found: {}", assets.display());
    }

    // Construir Vision.
    println!("Loading Vision from {}...", assets.display());
    let load_start = Instant::now();
    let mut vision = Vision::load(&assets);
    println!("  loaded in {:?}", load_start.elapsed());
    println!();

    // Generar un frame sintético en memoria (RGBA gris neutro).
    let frame = make_synthetic_frame(FRAME_WIDTH, FRAME_HEIGHT);
    println!("Frame: {}×{} ({} bytes)", frame.width, frame.height, frame.data.len());
    println!();

    // Correr N iteraciones midiendo proc time.
    println!("Running {} iterations...", iterations);
    let mut samples: Vec<f64> = Vec::with_capacity(iterations);
    let overall_start = Instant::now();
    let mut last_progress = Instant::now();

    for i in 0..iterations {
        let t0 = Instant::now();
        let _perception = vision.tick(&frame, i as u64);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        samples.push(elapsed_ms);

        // Progress cada 3s.
        if last_progress.elapsed().as_secs() >= 3 {
            let pct = (i + 1) as f64 / iterations as f64 * 100.0;
            print!("\r  [{:5.1}%] iter {}/{} last={:.2}ms", pct, i + 1, iterations, elapsed_ms);
            std::io::Write::flush(&mut std::io::stdout()).ok();
            last_progress = Instant::now();
        }
    }
    let total_elapsed = overall_start.elapsed();
    println!("\r  [100.0%] completed in {:?}                               ", total_elapsed);
    println!();

    // Análisis.
    let report = analyze(&samples);
    report.print();

    // Pass/fail.
    let mut failed = false;
    if report.p99 > P99_THRESHOLD_MS {
        eprintln!("FAIL: p99 {:.2}ms > threshold {:.2}ms", report.p99, P99_THRESHOLD_MS);
        failed = true;
    }
    if report.max > MAX_PROC_MS {
        eprintln!("FAIL: max {:.2}ms > threshold {:.2}ms", report.max, MAX_PROC_MS);
        failed = true;
    }
    if report.drift_ratio > DRIFT_TOLERANCE {
        eprintln!(
            "FAIL: drift detected — tail p95 {:.2}ms > head p95 {:.2}ms × {:.2}",
            report.tail_p95, report.head_p95, DRIFT_TOLERANCE
        );
        failed = true;
    }

    if failed {
        println!();
        println!("RESULT: degradation detected");
        std::process::exit(1);
    }

    println!();
    println!("RESULT: stable ✓");
    Ok(())
}

struct Report {
    count:        usize,
    total_ms:     f64,
    mean:         f64,
    p50:          f64,
    p95:          f64,
    p99:          f64,
    max:          f64,
    head_p95:     f64,
    tail_p95:     f64,
    drift_ratio:  f64,
}

impl Report {
    fn print(&self) {
        println!("Results:");
        println!("  iterations:    {}", self.count);
        println!("  total_time:    {:.2}s", self.total_ms / 1000.0);
        println!("  mean:          {:.3} ms", self.mean);
        println!("  p50:           {:.3} ms", self.p50);
        println!("  p95:           {:.3} ms", self.p95);
        println!("  p99:           {:.3} ms", self.p99);
        println!("  max:           {:.3} ms", self.max);
        println!();
        println!("Drift analysis (first 10% vs last 10%):");
        println!("  head p95:      {:.3} ms", self.head_p95);
        println!("  tail p95:      {:.3} ms", self.tail_p95);
        println!("  ratio:         {:.3} (tail/head)", self.drift_ratio);
        println!();
    }
}

fn analyze(samples: &[f64]) -> Report {
    let count = samples.len();
    let total_ms: f64 = samples.iter().sum();
    let mean = total_ms / count as f64;

    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = percentile(&sorted, 0.50);
    let p95 = percentile(&sorted, 0.95);
    let p99 = percentile(&sorted, 0.99);
    let max = *sorted.last().unwrap_or(&0.0);

    // Drift: comparar p95 del primer 10% vs último 10%.
    let chunk = (count / 10).max(1);
    let head: Vec<f64> = samples[..chunk].to_vec();
    let tail: Vec<f64> = samples[count - chunk..].to_vec();
    let mut head_sorted = head.clone();
    head_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut tail_sorted = tail.clone();
    tail_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let head_p95 = percentile(&head_sorted, 0.95);
    let tail_p95 = percentile(&tail_sorted, 0.95);
    let drift_ratio = if head_p95 > 0.0 { tail_p95 / head_p95 } else { 1.0 };

    Report {
        count,
        total_ms,
        mean,
        p50,
        p95,
        p99,
        max,
        head_p95,
        tail_p95,
        drift_ratio,
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn make_synthetic_frame(w: u32, h: u32) -> Frame {
    // RGBA gris uniforme (128, 128, 128, 255). Es suficiente para ejercitar
    // el pipeline sin que ningún template matchee — el costo del matching
    // depende del tamaño del frame, no del contenido.
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        data.push(128u8); // R
        data.push(128u8); // G
        data.push(128u8); // B
        data.push(255u8); // A
    }
    Frame {
        width: w,
        height: h,
        data,
        captured_at: Instant::now(),
    }
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
