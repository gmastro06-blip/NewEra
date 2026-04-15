//! breaks.rs — Scheduler multi-nivel de pausas tipo humano.
//!
//! Las sesiones humanas no son "90 minutos ON, 10 minutos OFF". Son caóticas
//! con tres escalas de tiempo:
//!
//! - **Micro AFKs** cada ~5 min, duran ~30s (mirar el móvil, estirarse).
//! - **Medium breaks** cada ~30 min, duran ~3 min (ir al baño, bebida).
//! - **Long breaks** cada ~2-4 h, duran ~15-45 min (comida, pausa larga).
//!
//! `BreakScheduler` mantiene tres timers independientes, cada uno con su
//! propia distribución. En cada tick comprueba si alguno "dispara" y
//! retorna la duración de la pausa. El `BotLoop` la aplica como
//! `safety_pause_reason = "break:<kind>"`.
//!
//! Los intervalos y duraciones usan muestreo gaussiano con clamping 3σ.

use std::time::{Duration, Instant};

use crate::safety::timing::GaussSampler;

/// Niveles de pausa. El kind se propaga a `safety_pause_reason` en el status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakKind {
    Micro,
    Medium,
    Long,
}

impl BreakKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BreakKind::Micro  => "break:micro",
            BreakKind::Medium => "break:medium",
            BreakKind::Long   => "break:long",
        }
    }
}

/// Un nivel de pausa con sus distribuciones de intervalo y duración.
struct BreakLevel {
    kind:            BreakKind,
    interval_sampler: GaussSampler, // en segundos
    duration_sampler: GaussSampler, // en segundos
    /// Próximo momento en el que se dispara.
    next_at:         Instant,
    /// Si está actualmente activo (bot pausado), hasta cuándo.
    active_until:    Option<Instant>,
}

impl BreakLevel {
    fn new(
        kind: BreakKind,
        interval_mean_s: f64, interval_std_s: f64,
        duration_mean_s: f64, duration_std_s: f64,
        now: Instant,
    ) -> Self {
        let interval_sampler = GaussSampler::new(interval_mean_s, interval_std_s);
        let duration_sampler = GaussSampler::new(duration_mean_s, duration_std_s);
        let next_at = schedule_next(&interval_sampler, now);
        Self {
            kind,
            interval_sampler,
            duration_sampler,
            next_at,
            active_until: None,
        }
    }
}

fn schedule_next(sampler: &GaussSampler, now: Instant) -> Instant {
    let mut rng = rand::thread_rng();
    let secs = sampler.sample_clamped(&mut rng, 1.0);
    now + Duration::from_secs_f64(secs)
}

/// Resultado del tick del scheduler para el BotLoop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakStatus {
    /// No hay break activo ni por empezar. Bot puede operar normalmente.
    None,
    /// Se acaba de disparar un break nuevo — pausar ahora.
    Started(BreakKind),
    /// Hay un break activo que sigue vigente.
    Active(BreakKind),
    /// Acaba de terminar un break — el bot puede reanudar.
    Ended(BreakKind),
}

pub struct BreakScheduler {
    levels: Vec<BreakLevel>,
}

impl BreakScheduler {
    /// Construye un scheduler con 3 niveles estándar.
    /// Los parámetros son las medias en segundos y sus desviaciones.
    pub fn new_standard(now: Instant) -> Self {
        Self {
            levels: vec![
                // Micro: cada ~5 min (300s ± 120s), dura ~30s ± 15s.
                BreakLevel::new(BreakKind::Micro,  300.0, 120.0, 30.0, 15.0, now),
                // Medium: cada ~30 min (1800s ± 600s), dura ~180s ± 60s.
                BreakLevel::new(BreakKind::Medium, 1800.0, 600.0, 180.0, 60.0, now),
                // Long: cada ~3h (10_800s ± 3600s), dura ~1800s ± 600s (30min).
                BreakLevel::new(BreakKind::Long,   10_800.0, 3600.0, 1800.0, 600.0, now),
            ],
        }
    }

    /// Construye con valores custom (útil para tests).
    #[allow(dead_code)] // extension point: custom break schedules
    pub fn with_levels(levels: Vec<(BreakKind, f64, f64, f64, f64)>, now: Instant) -> Self {
        Self {
            levels: levels.into_iter()
                .map(|(k, im, is, dm, ds)| BreakLevel::new(k, im, is, dm, ds, now))
                .collect(),
        }
    }

    /// Se llama una vez por tick. Decide si disparar, mantener, o terminar
    /// un break. Si hay varios niveles simultáneos, retorna el más "grande"
    /// (Long > Medium > Micro).
    pub fn tick(&mut self, now: Instant) -> BreakStatus {
        let mut ended: Option<BreakKind> = None;
        let mut active: Option<BreakKind> = None;
        let mut started: Option<BreakKind> = None;

        for level in self.levels.iter_mut() {
            // 1. ¿Break activo expirado?
            if let Some(until) = level.active_until {
                if now >= until {
                    ended = Some(level.kind);
                    level.active_until = None;
                    // Reprogramar próximo disparo desde ahora.
                    level.next_at = schedule_next(&level.interval_sampler, now);
                } else {
                    active = Some(level.kind);
                }
            }

            // 2. ¿Es hora de disparar uno nuevo? (solo si no hay activo de este nivel)
            if level.active_until.is_none() && now >= level.next_at {
                let mut rng = rand::thread_rng();
                let dur_s = level.duration_sampler.sample_clamped(&mut rng, 1.0);
                let until = now + Duration::from_secs_f64(dur_s);
                level.active_until = Some(until);
                started = Some(level.kind);
                tracing::info!(
                    "BreakScheduler: iniciando break {:?} de {:.1}s",
                    level.kind, dur_s
                );
            }
        }

        // Prioridad: Started > Active > Ended > None.
        // Si un started y un ended ocurren en el mismo tick (raro), started gana.
        if let Some(k) = started { return BreakStatus::Started(k); }
        if let Some(k) = active  { return BreakStatus::Active(k);  }
        if let Some(k) = ended   { return BreakStatus::Ended(k);   }
        BreakStatus::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_break_before_interval() {
        // stddev=0 → determinista; micro cada 10s, duración 2s.
        let now = Instant::now();
        let mut sched = BreakScheduler::with_levels(
            vec![(BreakKind::Micro, 10.0, 0.0, 2.0, 0.0)],
            now,
        );
        assert_eq!(sched.tick(now), BreakStatus::None);
        assert_eq!(sched.tick(now + Duration::from_secs(5)), BreakStatus::None);
    }

    #[test]
    fn started_then_active_then_ended() {
        let now = Instant::now();
        let mut sched = BreakScheduler::with_levels(
            vec![(BreakKind::Micro, 10.0, 0.0, 2.0, 0.0)],
            now,
        );
        // En t=10, se dispara el break.
        let at_10 = now + Duration::from_secs(10);
        assert_eq!(sched.tick(at_10), BreakStatus::Started(BreakKind::Micro));
        // En t=11, sigue activo.
        let at_11 = now + Duration::from_secs(11);
        assert_eq!(sched.tick(at_11), BreakStatus::Active(BreakKind::Micro));
        // En t=12, termina (duration 2s, started at 10).
        let at_12 = now + Duration::from_secs(12);
        assert_eq!(sched.tick(at_12), BreakStatus::Ended(BreakKind::Micro));
        // En t=12 de nuevo: None (el ended ya se reportó y se reprogramó desde t=12).
        assert_eq!(sched.tick(at_12), BreakStatus::None);
    }

    #[test]
    fn next_break_scheduled_after_ended() {
        let now = Instant::now();
        let mut sched = BreakScheduler::with_levels(
            vec![(BreakKind::Micro, 10.0, 0.0, 2.0, 0.0)],
            now,
        );
        // Primer ciclo: disparar + terminar.
        sched.tick(now + Duration::from_secs(10));
        sched.tick(now + Duration::from_secs(12));
        // Próximo disparo en t=12+10=22.
        assert_eq!(sched.tick(now + Duration::from_secs(21)), BreakStatus::None);
        assert_eq!(
            sched.tick(now + Duration::from_secs(22)),
            BreakStatus::Started(BreakKind::Micro)
        );
    }

    #[test]
    fn break_kind_strings_are_stable() {
        assert_eq!(BreakKind::Micro.as_str(), "break:micro");
        assert_eq!(BreakKind::Medium.as_str(), "break:medium");
        assert_eq!(BreakKind::Long.as_str(), "break:long");
    }
}
