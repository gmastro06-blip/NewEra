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
    /// Margen (en ticks) antes del hard cap durante el cual
    /// `is_warning` retorna true. `0` = feature deshabilitada: solo
    /// dispara `is_expired` (comportamiento legacy).
    warning_margin_ticks: u64,
}

impl SessionLimit {
    /// Construye un `SessionLimit` si está habilitado. Retorna `None` si
    /// `max_hours <= 0.0`, señalando que el feature está deshabilitado.
    ///
    /// - `max_hours`: cap en horas. `0.0` o negativo → disabled.
    /// - `fps`: ticks por segundo del game loop (30 en NewEra).
    /// - `start_tick`: tick actual en el momento de construcción.
    ///
    /// Queda construido SIN warning (margin=0). Para habilitar el
    /// graceful-refill usar `with_warning_min` encadenado.
    pub fn new(max_hours: f64, fps: u32, start_tick: u64) -> Option<Self> {
        if max_hours <= 0.0 {
            return None;
        }
        let max_ticks = (max_hours * 3600.0 * fps as f64) as u64;
        Some(Self { max_ticks, start_tick, warning_margin_ticks: 0 })
    }

    /// Builder: fija el margen de warning en minutos. `warning_min <= 0.0`
    /// deja el margen en 0 (feature deshabilitada). El margen se acota al
    /// max_ticks para que `is_warning` nunca dispare antes del start.
    pub fn with_warning_min(mut self, warning_min: f64, fps: u32) -> Self {
        if warning_min > 0.0 {
            let ticks = (warning_min * 60.0 * fps as f64) as u64;
            // Cap defensivo: si el warning > cap, no tiene sentido disparar
            // desde el tick 0 — truncamos al max_ticks.
            self.warning_margin_ticks = ticks.min(self.max_ticks);
        }
        self
    }

    /// Retorna true si la sesión ya excedió el cap. Usa `saturating_sub`
    /// para blindar contra la posibilidad (improbable) de que `now_tick`
    /// sea menor que `start_tick` por reinicio del contador.
    pub fn is_expired(&self, now_tick: u64) -> bool {
        now_tick.saturating_sub(self.start_tick) >= self.max_ticks
    }

    /// Retorna true cuando el runtime entra en la ventana de warning
    /// (últimos `warning_margin_ticks` antes del cap). Con `margin = 0`
    /// (default) siempre retorna false — el graceful refill queda opt-in.
    ///
    /// Una vez disparado, sigue retornando true hasta que se alcance
    /// el cap; esa persistencia la filtra el llamante (p.ej. un flag
    /// `session_warning_active`).
    pub fn is_warning(&self, now_tick: u64) -> bool {
        if self.warning_margin_ticks == 0 {
            return false;
        }
        let elapsed = now_tick.saturating_sub(self.start_tick);
        // threshold = max_ticks - warning_margin_ticks. Usamos saturating_sub
        // por si warning_margin_ticks > max_ticks (no debería ocurrir tras
        // el cap en `with_warning_min`, pero es barato defenderse).
        let threshold = self.max_ticks.saturating_sub(self.warning_margin_ticks);
        elapsed >= threshold
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

    // ── is_warning (graceful refill) ─────────────────────────────────

    #[test]
    fn warning_disabled_by_default_backward_compat() {
        // Un SessionLimit construido sin with_warning_min NUNCA debe
        // disparar is_warning, aun en el último tick antes del cap.
        let sl = SessionLimit::new(6.0, 30, 0).unwrap();
        assert!(!sl.is_warning(0));
        assert!(!sl.is_warning(100));
        assert!(!sl.is_warning(647_999)); // 1 tick antes del cap
        assert!(!sl.is_warning(648_000)); // cap exacto
        assert!(!sl.is_warning(u64::MAX));
    }

    #[test]
    fn warning_disabled_when_margin_zero() {
        // Llamar with_warning_min(0.0) es equivalente a no llamarlo.
        let sl = SessionLimit::new(6.0, 30, 0).unwrap().with_warning_min(0.0, 30);
        assert!(!sl.is_warning(647_999));
    }

    #[test]
    fn warning_disabled_when_margin_negative() {
        // Negativos se tratan como 0 (disabled).
        let sl = SessionLimit::new(6.0, 30, 0).unwrap().with_warning_min(-5.0, 30);
        assert!(!sl.is_warning(647_999));
    }

    #[test]
    fn warning_fires_inside_margin_window() {
        // max=6h, warning=5min @ 30fps.
        // max_ticks = 6 * 3600 * 30 = 648_000
        // warning_margin_ticks = 5 * 60 * 30 = 9_000
        // threshold = 648_000 - 9_000 = 639_000
        let sl = SessionLimit::new(6.0, 30, 0).unwrap().with_warning_min(5.0, 30);

        // Antes del threshold → no warning.
        // 5h50m = 5*3600*30 + 50*60*30 = 540_000 + 90_000 = 630_000 ticks.
        assert!(!sl.is_warning(630_000)); // 5h50m → no
        assert!(!sl.is_warning(638_999)); // justo antes → no

        // Dentro de la ventana → warning.
        assert!(sl.is_warning(639_000)); // threshold exacto → sí
        // 5h55m = 5*3600*30 + 55*60*30 = 540_000 + 99_000 = 639_000 ticks.
        assert!(sl.is_warning(639_000)); // 5h55m → sí
        assert!(sl.is_warning(647_999)); // 1 tick antes del cap → sí
        assert!(sl.is_warning(648_000)); // cap exacto → sigue true (operador filtra)
    }

    #[test]
    fn warning_caps_margin_at_max_ticks() {
        // Si el operador configura warning > cap, no debe disparar desde tick 0
        // antes de tener sesión: lo clamp-eamos a max_ticks. El efecto es que
        // is_warning dispara inmediatamente al arrancar — comportamiento
        // razonable aunque el operador haya configurado nonsense.
        let sl = SessionLimit::new(1.0, 30, 0).unwrap().with_warning_min(120.0, 30); // 2h margin en cap de 1h
        assert!(sl.is_warning(0)); // elapsed >= (max - max) = 0
        assert!(sl.is_warning(1));
    }

    #[test]
    fn warning_respects_nonzero_start_tick() {
        // Con baseline=1_000 y margen=5min, threshold absoluto = 1_000 + 639_000 = 640_000.
        let sl = SessionLimit::new(6.0, 30, 1_000).unwrap().with_warning_min(5.0, 30);
        assert!(!sl.is_warning(1_000)); // arranque
        assert!(!sl.is_warning(639_999)); // antes de la ventana (relativo)
        assert!(sl.is_warning(640_000)); // threshold exacto
        assert!(sl.is_warning(649_000)); // post-cap: sigue true
    }
}
