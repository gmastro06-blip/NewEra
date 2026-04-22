//! window.rs — Rolling window alloc-free.
//!
//! `CircularU32<N>` mantiene los últimos N samples en un array fijo + cursor.
//! Cero allocs, cero locks. Uso típico: jitter (N=30, 1 segundo a 30 Hz),
//! FPS estable (N=300, 10 segundos).
//!
//! Single-writer pattern: el game loop es el único que llama push().
//! Lectores HTTP toman snapshot vía `snapshot()` que copia el array.

/// Buffer circular fijo. Const-generic en N para evitar runtime cap config.
#[derive(Debug, Clone)]
pub struct CircularU32<const N: usize> {
    buf:    [u32; N],
    cursor: usize,
    len:    usize,
}

impl<const N: usize> Default for CircularU32<N> {
    fn default() -> Self { Self::new() }
}

impl<const N: usize> CircularU32<N> {
    pub const fn new() -> Self {
        Self { buf: [0; N], cursor: 0, len: 0 }
    }

    /// Push hot path. ~5 ns en release.
    #[inline]
    pub fn push(&mut self, v: u32) {
        self.buf[self.cursor] = v;
        self.cursor = (self.cursor + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    pub fn len(&self)      -> usize { self.len }
    pub fn capacity(&self) -> usize { N }
    pub fn is_empty(&self) -> bool  { self.len == 0 }

    /// Iter en orden de inserción más reciente al final. No garantiza orden
    /// estricto pero sí cobertura de todos los samples válidos.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.buf.iter().take(self.len).copied()
    }

    pub fn mean(&self) -> u32 {
        if self.len == 0 { return 0; }
        let sum: u64 = self.iter().map(|x| x as u64).sum();
        (sum / self.len as u64) as u32
    }

    /// Standard deviation (uncorrected, n-divisor). Suficiente para jitter
    /// detection — no necesitamos sample stdev (n-1) para n grande.
    pub fn stddev(&self) -> u32 {
        if self.len < 2 { return 0; }
        let mean = self.mean() as i64;
        let var: u64 = self.iter()
            .map(|x| {
                let d = x as i64 - mean;
                (d * d) as u64
            })
            .sum::<u64>() / self.len as u64;
        (var as f64).sqrt() as u32
    }

    pub fn min(&self) -> u32 {
        self.iter().min().unwrap_or(0)
    }

    pub fn max(&self) -> u32 {
        self.iter().max().unwrap_or(0)
    }

    pub fn clear(&mut self) {
        self.cursor = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_window_zero_stats() {
        let w = CircularU32::<10>::new();
        assert_eq!(w.len(), 0);
        assert_eq!(w.mean(), 0);
        assert_eq!(w.stddev(), 0);
        assert_eq!(w.min(), 0);
        assert_eq!(w.max(), 0);
    }

    #[test]
    fn single_sample_stats() {
        let mut w = CircularU32::<10>::new();
        w.push(42);
        assert_eq!(w.len(), 1);
        assert_eq!(w.mean(), 42);
        assert_eq!(w.stddev(), 0); // n=1 → no variance
        assert_eq!(w.min(), 42);
        assert_eq!(w.max(), 42);
    }

    #[test]
    fn fills_to_capacity_and_wraps() {
        let mut w = CircularU32::<3>::new();
        w.push(10); w.push(20); w.push(30);
        assert_eq!(w.len(), 3);
        assert_eq!(w.mean(), 20);
        // Push 4th: evict oldest (10), len stays 3.
        w.push(40);
        assert_eq!(w.len(), 3);
        // Iter cubre los 3 samples actuales (20, 30, 40 en algún orden).
        let sum: u32 = w.iter().sum();
        assert_eq!(sum, 90);
    }

    #[test]
    fn stddev_constant_is_zero() {
        let mut w = CircularU32::<10>::new();
        for _ in 0..10 { w.push(100); }
        assert_eq!(w.stddev(), 0);
    }

    #[test]
    fn stddev_varying_nonzero() {
        let mut w = CircularU32::<5>::new();
        for v in [10, 20, 30, 40, 50].iter() { w.push(*v); }
        assert_eq!(w.mean(), 30);
        // var = ((10-30)^2 + (20-30)^2 + 0 + 100 + 400) / 5 = 1000/5 = 200
        // stddev = sqrt(200) ≈ 14.14 → 14 (truncated by `as u32`)
        let s = w.stddev();
        assert!(s == 14 || s == 15, "stddev got {}", s);
    }

    #[test]
    fn min_max_track_correctly() {
        let mut w = CircularU32::<5>::new();
        for v in [50, 10, 30, 90, 20].iter() { w.push(*v); }
        assert_eq!(w.min(), 10);
        assert_eq!(w.max(), 90);
    }

    #[test]
    fn clear_resets_state() {
        let mut w = CircularU32::<5>::new();
        for _ in 0..5 { w.push(100); }
        w.clear();
        assert_eq!(w.len(), 0);
        assert_eq!(w.mean(), 0);
    }

    #[test]
    fn const_generic_capacity_30() {
        let w: CircularU32<30> = CircularU32::new();
        assert_eq!(w.capacity(), 30);
    }
}
