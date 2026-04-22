//! histogram.rs — LatencyHistogram con 16 buckets exponenciales y counters atómicos.
//!
//! Diseño:
//! - Buckets exponenciales sobre base 500 µs. Cubre [0..512 ms].
//! - bucket 0:   [0,    500)  µs
//! - bucket 1:   [500,  1000) µs
//! - bucket 2:   [1000, 2000) µs
//! - ...
//! - bucket 15:  [16M,  ∞)    µs (overflow)
//!
//! El bucket 0 atrapa todo lo <500 µs (filter_us, fsm_us típicos). Para
//! etapas que necesitan mayor resolución sub-ms, considerar histograma
//! con base más fina en versiones futuras (p.ej. base 50 µs).
//!
//! Memoria: 16 × 8 (buckets) + 8 (sum) + 8 (count) + 8 (max) = 152 bytes.
//! Hot path: ~30 ns por record_us (1 fetch_add a bucket, 1 a sum, 1 a count,
//! 1 CAS al max si excede).

use std::sync::atomic::{AtomicU64, Ordering};

/// Histograma de latencias con buckets fijos. Lock-free.
#[derive(Debug)]
pub struct LatencyHistogram {
    buckets: [AtomicU64; 16],
    sum_us:  AtomicU64,
    count:   AtomicU64,
    max_us:  AtomicU64,
}

impl Default for LatencyHistogram {
    fn default() -> Self { Self::new() }
}

impl LatencyHistogram {
    pub const fn new() -> Self {
        // const-fn array init de AtomicU64 requiere un poco de gimnasia.
        // Usamos macro-style para desplegar 16 atómicos a 0.
        Self {
            buckets: [
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
            ],
            sum_us:  AtomicU64::new(0),
            count:   AtomicU64::new(0),
            max_us:  AtomicU64::new(0),
        }
    }

    /// Hot path. Costo: ~30 ns en x86_64 release sin contención.
    #[inline]
    pub fn record_us(&self, us: u32) {
        let idx = bucket_idx(us);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us as u64, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        // CAS update del max — solo loopea si otro thread escribió mientras.
        let new = us as u64;
        let mut prev = self.max_us.load(Ordering::Relaxed);
        while new > prev {
            match self.max_us.compare_exchange_weak(
                prev, new, Ordering::Relaxed, Ordering::Relaxed,
            ) {
                Ok(_)  => break,
                Err(p) => prev = p,
            }
        }
    }

    pub fn count(&self) -> u64 { self.count.load(Ordering::Relaxed) }

    pub fn mean_us(&self) -> u32 {
        let c = self.count.load(Ordering::Relaxed);
        if c == 0 { return 0; }
        (self.sum_us.load(Ordering::Relaxed) / c) as u32
    }

    pub fn max_us(&self) -> u32 {
        self.max_us.load(Ordering::Relaxed) as u32
    }

    /// Estima percentil [0.0..1.0] por interpolación lineal dentro del bucket
    /// que contiene el rank objetivo. Error ≤ ancho del bucket.
    /// Cold path — solo consumido por HTTP / Prometheus expose.
    pub fn percentile(&self, p: f32) -> u32 {
        let p = p.clamp(0.0, 1.0);
        let total = self.count.load(Ordering::Relaxed);
        if total == 0 { return 0; }
        let target = ((total as f32 * p).ceil() as u64).max(1);

        let mut acc = 0u64;
        for (idx, b) in self.buckets.iter().enumerate() {
            let c = b.load(Ordering::Relaxed);
            if acc + c >= target {
                // El target cae en este bucket.
                let (lo, hi) = bucket_bounds_us(idx);
                if c == 0 { return lo; }
                let pos_in_bucket = target - acc;
                // Usamos (pos - 0.5) / count para que un sample único caiga
                // en el midpoint del bucket en vez de en el límite superior.
                // Con c samples espaciados: posición k → (k - 0.5) / c.
                let frac = (pos_in_bucket as f32 - 0.5) / c as f32;
                let frac = frac.clamp(0.0, 1.0);
                let estimated = lo as f32 + (hi.saturating_sub(lo)) as f32 * frac;
                return estimated as u32;
            }
            acc += c;
        }
        // Sería raro llegar aquí (target > total). Devolver max conocido.
        self.max_us()
    }

    /// Snapshot inmutable para serialización.
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut buckets = [0u64; 16];
        for (i, b) in self.buckets.iter().enumerate() {
            buckets[i] = b.load(Ordering::Relaxed);
        }
        HistogramSnapshot {
            buckets,
            sum_us: self.sum_us.load(Ordering::Relaxed),
            count: self.count.load(Ordering::Relaxed),
            max_us: self.max_us.load(Ordering::Relaxed) as u32,
            mean_us: self.mean_us(),
            p50: self.percentile(0.50),
            p95: self.percentile(0.95),
            p99: self.percentile(0.99),
        }
    }
}

/// Snapshot serializable del histograma (para HTTP JSON / JSONL).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct HistogramSnapshot {
    pub buckets:  [u64; 16],
    pub sum_us:   u64,
    pub count:    u64,
    pub max_us:   u32,
    pub mean_us:  u32,
    pub p50:      u32,
    pub p95:      u32,
    pub p99:      u32,
}

/// Percentiles agrupados — útil para responses HTTP cortas.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Percentiles {
    pub p50: u32,
    pub p95: u32,
    pub p99: u32,
    pub mean: u32,
    pub max: u32,
}

impl From<&LatencyHistogram> for Percentiles {
    fn from(h: &LatencyHistogram) -> Self {
        Self {
            p50: h.percentile(0.50),
            p95: h.percentile(0.95),
            p99: h.percentile(0.99),
            mean: h.mean_us(),
            max: h.max_us(),
        }
    }
}

// ── Bucket helpers ──────────────────────────────────────────────────────

/// Mapea un valor µs a su bucket. Bucket 0 cubre [0, 500), bucket k cubre
/// [500 * 2^(k-1), 500 * 2^k).
#[inline]
fn bucket_idx(us: u32) -> usize {
    if us < 500 { return 0; }
    // Bucket k: 500 * 2^(k-1) <= us < 500 * 2^k
    // → log2(us / 500) + 1 = k
    let q = us / 500;
    // 32 - leading_zeros(q) es ⌈log2(q+1)⌉ esencialmente.
    // Para q=1 → 1, q=2..3 → 2, q=4..7 → 3, ...
    let k = 32 - q.leading_zeros() as usize;
    k.min(15)
}

/// Bounds [lo, hi) en µs del bucket idx. hi=u32::MAX para idx=15 (overflow).
fn bucket_bounds_us(idx: usize) -> (u32, u32) {
    if idx == 0 { return (0, 500); }
    if idx >= 15 {
        return (500u32.saturating_mul(1 << 14), u32::MAX);
    }
    let lo = 500u32.saturating_mul(1 << (idx - 1));
    let hi = 500u32.saturating_mul(1 << idx);
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_idx_known_values() {
        assert_eq!(bucket_idx(0),     0);
        assert_eq!(bucket_idx(100),   0);
        assert_eq!(bucket_idx(499),   0);
        assert_eq!(bucket_idx(500),   1);
        assert_eq!(bucket_idx(999),   1);
        assert_eq!(bucket_idx(1000),  2);
        assert_eq!(bucket_idx(1999),  2);
        assert_eq!(bucket_idx(2000),  3);
        assert_eq!(bucket_idx(4000),  4);
        assert_eq!(bucket_idx(8000),  5);
        assert_eq!(bucket_idx(16_000), 6);
        assert_eq!(bucket_idx(32_000), 7);
        // Overflow → cap a 15.
        assert_eq!(bucket_idx(u32::MAX), 15);
    }

    #[test]
    fn bucket_bounds_consistent() {
        for i in 1..15 {
            let (lo, hi) = bucket_bounds_us(i);
            assert!(lo < hi, "bucket {} bounds inconsistentes: [{}, {})", i, lo, hi);
            // El bucket idx debería ser i para cualquier valor en el rango.
            assert_eq!(bucket_idx(lo), i);
            assert_eq!(bucket_idx(hi - 1), i);
        }
        // Bucket 15 = overflow.
        let (lo, hi) = bucket_bounds_us(15);
        assert_eq!(hi, u32::MAX);
        assert_eq!(bucket_idx(lo), 15);
    }

    #[test]
    fn empty_histogram_zero_percentiles() {
        let h = LatencyHistogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.mean_us(), 0);
        assert_eq!(h.percentile(0.50), 0);
        assert_eq!(h.percentile(0.95), 0);
    }

    #[test]
    fn single_sample_percentiles() {
        let h = LatencyHistogram::new();
        h.record_us(1500);
        assert_eq!(h.count(), 1);
        assert_eq!(h.mean_us(), 1500);
        assert_eq!(h.max_us(), 1500);
        // 1500 cae en bucket 2 [1000, 2000). Percentil cualquiera devuelve
        // valor interpolado dentro del bucket.
        let p50 = h.percentile(0.50);
        assert!(p50 >= 1000 && p50 < 2000, "p50={}", p50);
    }

    #[test]
    fn many_samples_in_one_bucket_concentrated_percentile() {
        let h = LatencyHistogram::new();
        for _ in 0..1000 { h.record_us(700); } // bucket 1 [500, 1000)
        assert_eq!(h.count(), 1000);
        assert_eq!(h.mean_us(), 700);
        // Todos en bucket 1 → p50 y p95 dentro de [500, 1000).
        let p95 = h.percentile(0.95);
        assert!(p95 >= 500 && p95 < 1000, "p95={}", p95);
    }

    #[test]
    fn bimodal_distribution_percentile_separates() {
        let h = LatencyHistogram::new();
        // 90% rápidos (~700 µs), 10% lentos (~16 ms).
        for _ in 0..900 { h.record_us(700); }
        for _ in 0..100 { h.record_us(16_000); } // bucket 6 [16000, 32000)
        // p50 debería caer en el grupo rápido.
        let p50 = h.percentile(0.50);
        assert!(p50 < 1000, "p50 debe estar en grupo rápido, got {}", p50);
        // p95 cae en el grupo lento (10% son ≥ 16 ms; p95 está dentro).
        let p95 = h.percentile(0.95);
        assert!(p95 >= 16_000, "p95 debe estar en grupo lento, got {}", p95);
    }

    #[test]
    fn max_tracking_updates_correctly() {
        let h = LatencyHistogram::new();
        h.record_us(100);
        assert_eq!(h.max_us(), 100);
        h.record_us(50);
        assert_eq!(h.max_us(), 100); // No baja
        h.record_us(5000);
        assert_eq!(h.max_us(), 5000);
    }

    #[test]
    fn snapshot_consistent_with_individual_accessors() {
        let h = LatencyHistogram::new();
        for v in [100u32, 200, 800, 1500, 3000, 8000].iter() {
            h.record_us(*v);
        }
        let snap = h.snapshot();
        assert_eq!(snap.count, 6);
        assert_eq!(snap.max_us, 8000);
        assert_eq!(snap.mean_us, h.mean_us());
        assert_eq!(snap.p50, h.percentile(0.50));
        assert_eq!(snap.p95, h.percentile(0.95));
    }

    #[test]
    fn percentiles_struct_from_histogram() {
        let h = LatencyHistogram::new();
        h.record_us(1000);
        h.record_us(2000);
        let p = Percentiles::from(&h);
        assert_eq!(p.max, 2000);
        assert_eq!(p.mean, 1500);
    }

    #[test]
    fn percentile_clamps_out_of_range_input() {
        let h = LatencyHistogram::new();
        h.record_us(1000);
        // Inputs fuera de [0,1] no panican.
        assert_eq!(h.percentile(-0.5), h.percentile(0.0));
        assert_eq!(h.percentile(1.5), h.percentile(1.0));
    }

    #[test]
    fn overflow_samples_land_in_top_bucket() {
        let h = LatencyHistogram::new();
        h.record_us(u32::MAX);
        let snap = h.snapshot();
        assert_eq!(snap.buckets[15], 1);
        assert_eq!(snap.max_us, u32::MAX);
    }
}
