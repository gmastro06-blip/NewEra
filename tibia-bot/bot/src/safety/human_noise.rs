//! human_noise.rs — Emisor ocasional de "ruido humano".
//!
//! Los bots solo emiten inputs con propósito. Los humanos, además de los
//! inputs útiles, emiten teclas "random" para:
//! - Ver los stats del personaje (Ctrl+Q, Esc, etc).
//! - Abrir/cerrar el chat o el inventario.
//! - Rotar la cámara o mirar alrededor.
//! - Click sin propósito en una zona random del viewport.
//!
//! `HumanNoise` mantiene un timer con intervalo gaussiano. Cuando dispara,
//! retorna una tecla aleatoria de una lista configurable (el usuario elige
//! qué teclas son "seguras" de enviar — típicamente teclas que abren/cierran
//! UI sin efectos side-effect en el mundo: no chat-enter, no spell keys).
//!
//! **Importante para detectabilidad**: si siempre emitiera la misma tecla a
//! intervalos fijos, sería un signature obvio. Por eso:
//! - **Intervalo gaussiano** con σ alto (coeficiente de variación ~50%).
//! - **Selección aleatoria** de la tecla entre las permitidas.

use std::time::{Duration, Instant};

use rand::seq::SliceRandom;

use crate::safety::timing::GaussSampler;

pub struct HumanNoise {
    /// Teclas permitidas (hidcodes). Si vacío, el módulo es no-op.
    keys:    Vec<u8>,
    /// Distribución del intervalo entre emisiones, en segundos.
    interval_sampler: GaussSampler,
    /// Próximo momento en el que se emite una tecla.
    next_at: Instant,
    /// Contador de emisiones (para logs / tests).
    emitted: u64,
}

impl HumanNoise {
    /// Crea un emisor con las teclas dadas y un intervalo medio/stddev en segundos.
    pub fn new(keys: Vec<u8>, interval_mean_s: f64, interval_std_s: f64, now: Instant) -> Self {
        let interval_sampler = GaussSampler::new(interval_mean_s, interval_std_s);
        let next_at = schedule_next(&interval_sampler, now);
        Self { keys, interval_sampler, next_at, emitted: 0 }
    }

    /// Comprueba si es hora de emitir ruido. Retorna `Some(hidcode)` si sí.
    /// Re-programa el próximo disparo tras cada emisión.
    pub fn tick(&mut self, now: Instant) -> Option<u8> {
        if self.keys.is_empty() {
            return None;
        }
        if now < self.next_at {
            return None;
        }
        let mut rng = rand::thread_rng();
        let choice = self.keys.choose(&mut rng).copied();
        if choice.is_some() {
            self.emitted += 1;
        }
        self.next_at = schedule_next(&self.interval_sampler, now);
        choice
    }

    #[allow(dead_code)] // extension point: diagnostics
    pub fn emitted_count(&self) -> u64 { self.emitted }
}

fn schedule_next(sampler: &GaussSampler, now: Instant) -> Instant {
    let mut rng = rand::thread_rng();
    let secs = sampler.sample_clamped(&mut rng, 5.0); // min 5s entre emisiones
    now + Duration::from_secs_f64(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn empty_keys_never_emits() {
        let now = Instant::now();
        let mut n = HumanNoise::new(vec![], 10.0, 0.0, now);
        assert_eq!(n.tick(now + Duration::from_secs(1000)), None);
        assert_eq!(n.emitted_count(), 0);
    }

    #[test]
    fn no_emit_before_interval() {
        let now = Instant::now();
        let mut n = HumanNoise::new(vec![0x3A], 10.0, 0.0, now);
        // Interval = 10s determinista; antes de t=10 no emite.
        assert_eq!(n.tick(now + Duration::from_secs(1)), None);
        assert_eq!(n.tick(now + Duration::from_secs(9)), None);
    }

    #[test]
    fn emits_at_scheduled_time() {
        let now = Instant::now();
        let mut n = HumanNoise::new(vec![0x3A], 10.0, 0.0, now);
        // En t=10, debe emitir.
        let at_10 = now + Duration::from_secs(10);
        assert_eq!(n.tick(at_10), Some(0x3A));
        assert_eq!(n.emitted_count(), 1);
    }

    #[test]
    fn reschedules_after_emit() {
        let now = Instant::now();
        let mut n = HumanNoise::new(vec![0x3A], 10.0, 0.0, now);
        // Emit en t=10. Siguiente scheduled en t=20.
        n.tick(now + Duration::from_secs(10));
        // En t=15, no emite (reprogramado a 20).
        assert_eq!(n.tick(now + Duration::from_secs(15)), None);
        // En t=20, emite.
        assert_eq!(n.tick(now + Duration::from_secs(20)), Some(0x3A));
    }

    #[test]
    fn picks_varying_keys_over_time() {
        // Muchas emisiones → debería tocar varias de la lista (con alta prob).
        let now = Instant::now();
        let mut n = HumanNoise::new(vec![0x3A, 0x3B, 0x3C, 0x3D], 1.0, 0.0, now);
        let mut seen = HashSet::new();
        let mut t = now;
        for _ in 0..100 {
            t += Duration::from_secs(1);
            if let Some(k) = n.tick(t) {
                seen.insert(k);
            }
        }
        // Con 100 emits y 4 keys uniformes, debería haber visto las 4.
        assert_eq!(seen.len(), 4);
    }
}
