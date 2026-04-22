//! health/gate.rs — HealthGate: helper consumido por FSM/dispatch.
//!
//! Snapshot-based: el caller lee un Arc<HealthStatus> del HealthSystem
//! (ArcSwap load) y construye un HealthGate temporal. Las decisiones de
//! "permitir esta acción?" son lock-free y O(1).

use super::{DegradationLevel, HealthStatus};

/// Wrapper temporal sobre un snapshot de `HealthStatus` para que el FSM
/// y el dispatch decidan si emitir una acción según el modo de degradación.
///
/// Borrow-only: contiene una referencia al snapshot que se libera cuando
/// el HealthGate sale del scope.
pub struct HealthGate<'a> {
    snapshot: &'a HealthStatus,
}

impl<'a> HealthGate<'a> {
    pub fn new(snapshot: &'a HealthStatus) -> Self {
        Self { snapshot }
    }

    /// Permite acciones de prioridad normal. Bloquea durante Heavy y SafeMode.
    pub fn allow_normal(&self) -> bool {
        matches!(self.snapshot.degraded, None | Some(DegradationLevel::Light))
    }

    /// Permite acciones de emergencia (heal crítico, escape, key F12).
    /// Solo bloquea SafeMode (cuando el bot debe estar 100% pausado).
    pub fn allow_emergency(&self) -> bool {
        !matches!(self.snapshot.degraded, Some(DegradationLevel::SafeMode))
    }

    /// Permite que readers SLOW (inventory, tile-hashing) corran este tick.
    /// Bloqueado en cualquier nivel de degradación.
    pub fn allow_slow_reader(&self) -> bool {
        self.snapshot.degraded.is_none()
    }

    /// Multiplicador de cadencia para readers SLOW. Sin degradation = 1.
    pub fn cadence_multiplier(&self) -> u32 {
        match self.snapshot.degraded {
            None    => 1,
            Some(d) => d.slow_reader_multiplier(),
        }
    }

    /// Acceso directo al snapshot subyacente (para HTTP debug, logs, etc.).
    pub fn snapshot(&self) -> &HealthStatus {
        self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{HealthStatus, Severity};

    fn status_with(degraded: Option<DegradationLevel>) -> HealthStatus {
        HealthStatus {
            overall: if degraded.is_some() { Severity::Warning } else { Severity::Ok },
            score:   if degraded.is_some() { 1 } else { 0 },
            degraded,
            issues:  vec![],
            summary: String::new(),
            tick: 0, frame_seq: 0, generated_at_ms: 0,
        }
    }

    #[test]
    fn ok_state_allows_everything() {
        let s = status_with(None);
        let g = HealthGate::new(&s);
        assert!(g.allow_normal());
        assert!(g.allow_emergency());
        assert!(g.allow_slow_reader());
        assert_eq!(g.cadence_multiplier(), 1);
    }

    #[test]
    fn light_blocks_slow_but_allows_actions() {
        let s = status_with(Some(DegradationLevel::Light));
        let g = HealthGate::new(&s);
        assert!(g.allow_normal());
        assert!(g.allow_emergency());
        assert!(!g.allow_slow_reader());
        assert_eq!(g.cadence_multiplier(), 2);
    }

    #[test]
    fn heavy_blocks_normal_actions_but_allows_emergency() {
        let s = status_with(Some(DegradationLevel::Heavy));
        let g = HealthGate::new(&s);
        assert!(!g.allow_normal());
        assert!(g.allow_emergency());
        assert!(!g.allow_slow_reader());
        assert_eq!(g.cadence_multiplier(), 4);
    }

    #[test]
    fn safe_mode_blocks_everything() {
        let s = status_with(Some(DegradationLevel::SafeMode));
        let g = HealthGate::new(&s);
        assert!(!g.allow_normal());
        assert!(!g.allow_emergency());
        assert!(!g.allow_slow_reader());
        assert_eq!(g.cadence_multiplier(), u32::MAX);
    }
}
