//! rate_limit.rs — Hard cap global de acciones por segundo.
//!
//! Red de seguridad contra bugs del FSM o scripts Lua que podrían producir
//! bursts de acciones (ej: heal spam, attack spam) que son trivialmente
//! detectables. El rate limiter mantiene un conteo por ventana deslizante
//! y **descarta** (no cola) acciones que excedan el cap.
//!
//! Ventana deslizante de 1 segundo — al llamar `allow()`, el limiter
//! cuenta cuántas acciones se emitieron en el último segundo y rechaza
//! si ya se alcanzó el máximo.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    name:     String,
    max_per_sec: u32,
    window:   VecDeque<Instant>,
    dropped:  u64,
}

impl RateLimiter {
    pub fn new(name: impl Into<String>, max_per_sec: u32) -> Self {
        Self {
            name: name.into(),
            max_per_sec,
            window: VecDeque::with_capacity(max_per_sec as usize + 4),
            dropped: 0,
        }
    }

    /// Intenta reservar un "slot" para una nueva acción en `now`.
    /// Retorna `true` si se permite (y se cuenta), `false` si se rechaza.
    pub fn allow(&mut self, now: Instant) -> bool {
        // Limpiar entradas viejas (>1s).
        let cutoff = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
        while let Some(&front) = self.window.front() {
            if front < cutoff {
                self.window.pop_front();
            } else {
                break;
            }
        }

        if self.window.len() < self.max_per_sec as usize {
            self.window.push_back(now);
            true
        } else {
            self.dropped += 1;
            tracing::debug!(
                "RateLimiter[{}] dropped action (total dropped: {})",
                self.name, self.dropped
            );
            false
        }
    }

    pub fn dropped_count(&self) -> u64 { self.dropped }
    #[allow(dead_code)] // extension point: diagnostics
    pub fn current_rate(&self) -> usize { self.window.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_per_sec() {
        let mut rl = RateLimiter::new("test", 5);
        let now = Instant::now();
        for _ in 0..5 {
            assert!(rl.allow(now));
        }
        // La sexta debe ser rechazada dentro del mismo instante.
        assert!(!rl.allow(now));
        assert_eq!(rl.dropped_count(), 1);
    }

    #[test]
    fn expired_entries_are_cleaned() {
        let mut rl = RateLimiter::new("test", 3);
        let t0 = Instant::now();
        assert!(rl.allow(t0));
        assert!(rl.allow(t0));
        assert!(rl.allow(t0));
        assert!(!rl.allow(t0));  // cap en t0

        // Un segundo + un poco más: las 3 entradas originales expiran.
        let t1 = t0 + Duration::from_millis(1100);
        assert!(rl.allow(t1));
        assert!(rl.allow(t1));
        assert!(rl.allow(t1));
        assert!(!rl.allow(t1));  // cap en t1
    }

    #[test]
    fn partial_window_expiry() {
        let mut rl = RateLimiter::new("test", 3);
        let t0 = Instant::now();
        rl.allow(t0);
        rl.allow(t0 + Duration::from_millis(300));
        rl.allow(t0 + Duration::from_millis(600));

        // En t = t0 + 1100 ms, la primera entrada (t0) expira; quedan 2.
        let t_partial = t0 + Duration::from_millis(1100);
        assert!(rl.allow(t_partial));  // permite una 4ta.
        assert!(!rl.allow(t_partial)); // ya en cap.
    }

    #[test]
    fn current_rate_reports_window_size() {
        let mut rl = RateLimiter::new("test", 10);
        let now = Instant::now();
        assert_eq!(rl.current_rate(), 0);
        rl.allow(now);
        rl.allow(now);
        assert_eq!(rl.current_rate(), 2);
    }
}
