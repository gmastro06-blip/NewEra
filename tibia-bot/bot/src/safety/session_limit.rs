//! session_limit.rs — Cap duro de duración de sesión.
//!
//! **Motivación:** sesiones continuas de farmeo sin interrupción son una
//! señal estadística clásica para detección de bots (jugadores humanos
//! hacen pausas impredecibles). Este módulo define un tope absoluto de
//! runtime: tras N horas desde el arranque, el bot pausa con
//! `safety_pause_reason = "session:max_duration_reached"`.
//!
//! A diferencia de `breaks.rs` (pausas cortas intermitentes estilo AFK),
//! aquí es una terminación "blanda": un único disparo al pasar el umbral.
//! El operador decide si reanudar manualmente tras revisar que todo esté
//! OK, o dejar la sesión cerrada hasta el día siguiente.
//!
//! Implementación trivial: contar ticks desde el start y comparar contra
//! el cap convertido a ticks. Usar `u64` evita overflow aun con caps
//! absurdamente grandes.

/// Estado del cap de sesión. Se instancia en el arranque del game loop
/// con el tick actual como baseline y se consulta cada tick.
pub struct SessionLimit {
    /// Máximo de ticks permitidos en la sesión (derivado de horas × fps × 3600).
    max_ticks: u64,
    /// Tick en el que se inició la sesión (baseline para elapsed).
    start_tick: u64,
}

impl SessionLimit {
    /// Construye un `SessionLimit` si está habilitado. Retorna `None` si
    /// `max_hours <= 0.0`, señalando que el feature está deshabilitado.
    ///
    /// - `max_hours`: cap en horas. `0.0` o negativo → disabled.
    /// - `fps`: ticks por segundo del game loop (30 en NewEra).
    /// - `start_tick`: tick actual en el momento de construcción.
    pub fn new(max_hours: f64, fps: u32, start_tick: u64) -> Option<Self> {
        if max_hours <= 0.0 {
            return None;
        }
        let max_ticks = (max_hours * 3600.0 * fps as f64) as u64;
        Some(Self { max_ticks, start_tick })
    }

    /// Retorna true si la sesión ya excedió el cap. Usa `saturating_sub`
    /// para blindar contra la posibilidad (improbable) de que `now_tick`
    /// sea menor que `start_tick` por reinicio del contador.
    pub fn is_expired(&self, now_tick: u64) -> bool {
        now_tick.saturating_sub(self.start_tick) >= self.max_ticks
    }

    /// Horas transcurridas desde el arranque. Útil para logging cuando
    /// se dispara el cap.
    pub fn elapsed_hours(&self, now_tick: u64, fps: u32) -> f64 {
        let elapsed_ticks = now_tick.saturating_sub(self.start_tick);
        elapsed_ticks as f64 / (fps as f64 * 3600.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_max_hours_zero() {
        assert!(SessionLimit::new(0.0, 30, 0).is_none());
    }

    #[test]
    fn disabled_when_max_hours_negative() {
        assert!(SessionLimit::new(-1.0, 30, 0).is_none());
    }

    #[test]
    fn computes_max_ticks_from_hours_and_fps() {
        // 6h @ 30fps = 6 * 3600 * 30 = 648_000 ticks
        let sl = SessionLimit::new(6.0, 30, 0).expect("debería estar habilitado");
        assert_eq!(sl.max_ticks, 648_000);
        assert_eq!(sl.start_tick, 0);
    }

    #[test]
    fn not_expired_before_threshold() {
        let sl = SessionLimit::new(6.0, 30, 0).unwrap();
        assert!(!sl.is_expired(0));
        assert!(!sl.is_expired(1));
        assert!(!sl.is_expired(647_999));
    }

    #[test]
    fn expired_at_and_after_threshold() {
        let sl = SessionLimit::new(6.0, 30, 0).unwrap();
        assert!(sl.is_expired(648_000));
        assert!(sl.is_expired(648_001));
        assert!(sl.is_expired(u64::MAX));
    }

    #[test]
    fn elapsed_hours_reports_correctly() {
        let sl = SessionLimit::new(6.0, 30, 0).unwrap();
        // 648_000 ticks / (30 * 3600) = 6.0 exacto
        assert!((sl.elapsed_hours(648_000, 30) - 6.0).abs() < 0.01);
        // Mid-session
        assert!((sl.elapsed_hours(324_000, 30) - 3.0).abs() < 0.01);
        assert!((sl.elapsed_hours(0, 30) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn respects_nonzero_start_tick() {
        // Baseline en tick 1000 — el cap se cuenta desde ahí.
        let sl = SessionLimit::new(6.0, 30, 1_000).unwrap();
        assert!(!sl.is_expired(1_000));
        assert!(!sl.is_expired(648_999));
        assert!(sl.is_expired(649_000));
    }

    #[test]
    fn saturating_sub_protects_against_reverse_time() {
        // Si `now_tick < start_tick` (improbable pero defensivo), no explota.
        let sl = SessionLimit::new(6.0, 30, 1_000).unwrap();
        assert!(!sl.is_expired(0));
        assert!((sl.elapsed_hours(0, 30) - 0.0).abs() < 1e-9);
    }
}
