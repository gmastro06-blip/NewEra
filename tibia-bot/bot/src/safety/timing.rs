//! timing.rs — Muestreo gaussiano para humanización de timing.
//!
//! Los bots son detectables cuando sus intervalos son constantes o siguen
//! distribuciones uniformes obvias. Los humanos tienen timing con varianza
//! aproximadamente **gaussiana** alrededor de una media (Fitts' law etc).
//!
//! Este módulo expone samplers con distribución normal `N(μ, σ)` y wrappers
//! convenientes para tiempo en milisegundos y ticks del game loop.
//!
//! ## Uso típico
//!
//! ```ignore
//! // Cooldown de heal con μ=333ms, σ=83ms, convertido a ticks a 30 Hz.
//! let cd = sample_gauss_ticks(333.0, 83.0, 30);
//! // Pre-send delay antes de un key_tap:
//! let delay_ms = sample_gauss_ms(45.0, 15.0);
//! tokio::time::sleep(Duration::from_millis(delay_ms)).await;
//! ```
//!
//! ## Clamping
//!
//! Los samples se recortan a `[μ - 3σ, μ + 3σ]` para evitar outliers
//! absurdos (pero manteniendo ~99.7% de la distribución). Valores negativos
//! o cero se recortan a un mínimo de `1ms` / `1 tick`.

use rand::rngs::ThreadRng;
use rand_distr::{Distribution, Normal};

/// Sampler gaussiano reusable. Guarda la distribución pre-construida para
/// evitar recrearla en cada llamada.
pub struct GaussSampler {
    distr: Normal<f64>,
    mean:  f64,
    stddev: f64,
}

impl GaussSampler {
    /// Crea un sampler con la media y desviación dadas.
    /// Si `stddev <= 0`, se usa `0.0` (sampler determinista = siempre `mean`).
    pub fn new(mean: f64, stddev: f64) -> Self {
        let safe_std = stddev.max(0.0);
        Self {
            distr: Normal::new(mean, safe_std).unwrap_or_else(|_| Normal::new(mean, 0.0).unwrap()),
            mean,
            stddev: safe_std,
        }
    }

    /// Muestrea un valor, clamped a `[μ - 3σ, μ + 3σ]` y con mínimo `min`.
    pub fn sample_clamped(&self, rng: &mut ThreadRng, min: f64) -> f64 {
        let raw = self.distr.sample(rng);
        let lo  = self.mean - 3.0 * self.stddev;
        let hi  = self.mean + 3.0 * self.stddev;
        raw.clamp(lo, hi).max(min)
    }

    #[allow(dead_code)] // extension point: diagnostics
    pub fn mean(&self) -> f64 { self.mean }
    #[allow(dead_code)]
    pub fn stddev(&self) -> f64 { self.stddev }
}

/// Helper one-shot: muestrea un tiempo en **milisegundos**.
/// Clampeo a `[1, μ + 3σ]` (nunca menos de 1ms).
pub fn sample_gauss_ms(mean_ms: f64, stddev_ms: f64) -> u64 {
    let mut rng = rand::thread_rng();
    let sampler = GaussSampler::new(mean_ms, stddev_ms);
    sampler.sample_clamped(&mut rng, 1.0).round() as u64
}

/// Helper one-shot: muestrea un delay en **ticks** a partir de parámetros en ms.
/// `fps` es la frecuencia del game loop (30 Hz por default).
/// Clampeo a `[1, μ+3σ]` ticks (siempre al menos 1 tick).
pub fn sample_gauss_ticks(mean_ms: f64, stddev_ms: f64, fps: u32) -> u64 {
    let fps_f = fps.max(1) as f64;
    let mean_ticks   = mean_ms   * fps_f / 1000.0;
    let stddev_ticks = stddev_ms * fps_f / 1000.0;
    let mut rng = rand::thread_rng();
    let sampler = GaussSampler::new(mean_ticks, stddev_ticks);
    sampler.sample_clamped(&mut rng, 1.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_mean_of_many_is_close_to_mean() {
        // Ley de grandes números: promedio de muchas muestras → media de la distribución.
        let sampler = GaussSampler::new(100.0, 20.0);
        let mut rng = rand::thread_rng();
        let n = 10_000;
        let sum: f64 = (0..n).map(|_| sampler.sample_clamped(&mut rng, 0.0)).sum();
        let avg = sum / n as f64;
        assert!((avg - 100.0).abs() < 2.0, "avg={} lejos de 100", avg);
    }

    #[test]
    fn sample_respects_3sigma_clamp() {
        let sampler = GaussSampler::new(50.0, 10.0);
        let mut rng = rand::thread_rng();
        for _ in 0..1000 {
            let v = sampler.sample_clamped(&mut rng, 0.0);
            assert!(v >= 20.0 && v <= 80.0, "v={} fuera de [20,80]", v);
        }
    }

    #[test]
    fn sample_respects_min_floor() {
        // Con mean=1, stddev=5 habría muchos valores negativos o 0.
        let sampler = GaussSampler::new(1.0, 5.0);
        let mut rng = rand::thread_rng();
        for _ in 0..1000 {
            let v = sampler.sample_clamped(&mut rng, 1.0);
            assert!(v >= 1.0, "v={} bajo el floor de 1.0", v);
        }
    }

    #[test]
    fn zero_stddev_is_deterministic() {
        let sampler = GaussSampler::new(42.0, 0.0);
        let mut rng = rand::thread_rng();
        for _ in 0..100 {
            assert_eq!(sampler.sample_clamped(&mut rng, 0.0), 42.0);
        }
    }

    #[test]
    fn sample_gauss_ms_returns_reasonable_values() {
        // 100 muestras, todas deben estar en [1, μ+3σ].
        for _ in 0..100 {
            let v = sample_gauss_ms(50.0, 10.0);
            assert!((1..=80).contains(&v), "v={} fuera de rango", v);
        }
    }

    #[test]
    fn sample_gauss_ticks_converts_ms_to_ticks_at_fps() {
        // 1000ms @ 30 Hz = 30 ticks (mean).
        let mut total = 0u64;
        let n = 1000u64;
        for _ in 0..n {
            total += sample_gauss_ticks(1000.0, 0.0, 30);
        }
        let avg = total as f64 / n as f64;
        assert!((avg - 30.0).abs() < 0.01, "avg={} esperaba 30", avg);
    }

    #[test]
    fn negative_stddev_is_coerced_to_zero() {
        let sampler = GaussSampler::new(10.0, -5.0);
        assert_eq!(sampler.stddev(), 0.0);
        let mut rng = rand::thread_rng();
        assert_eq!(sampler.sample_clamped(&mut rng, 0.0), 10.0);
    }
}
