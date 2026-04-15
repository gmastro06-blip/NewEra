//! reaction.rs — Gate de tiempo de reacción humano.
//!
//! Un humano no puede reaccionar a un evento visual en <100ms. El bot actual
//! puede detectar un enemigo o HP crítico en el frame N y emitir el comando
//! en el frame N+1 (≤33ms), lo cual es **imposible** para un humano y por
//! lo tanto una firma trivial de detectabilidad.
//!
//! `ReactionGate` añade un delay "armado" la **primera vez** que una
//! condición se vuelve verdadera. Una vez armado, el gate permanece abierto
//! hasta que la condición se vuelva falsa de nuevo (reset al "no threat").
//!
//! ## Ejemplo
//!
//! ```ignore
//! let mut gate = ReactionGate::new("hp_critical", 180.0, 40.0, 30);
//! // Tick 0: HP OK, no acción.
//! gate.update(false, 0);
//! assert!(!gate.is_armed(0));
//! // Tick 100: HP crítico detectado → gate se "arma" con reaction delay.
//! gate.update(true, 100);
//! // Durante los ticks siguientes (hasta 100 + delay), el gate NO está abierto.
//! assert!(!gate.is_open(100));  // delay acaba de empezar
//! // Tras el reaction delay, el gate se abre.
//! assert!(gate.is_open(100 + delay));
//! ```
//!
//! El reaction delay **solo** aplica a la primera emisión tras una nueva
//! detección; los reenvíos dentro del mismo "evento" usan los cooldowns
//! normales de la FSM.

use crate::safety::timing::sample_gauss_ticks;

/// Estado interno del gate respecto a una condición booleana.
#[derive(Debug, Clone)]
pub struct ReactionGate {
    /// Nombre para logs.
    name:      String,
    /// Parámetros del muestreo gaussiano (ms, ms).
    mean_ms:   f64,
    stddev_ms: f64,
    /// FPS del game loop, para convertir ms → ticks.
    fps:       u32,
    /// Tick a partir del cual el gate está "abierto". `None` = nunca armado
    /// o condición falsa actualmente.
    open_at:   Option<u64>,
    /// Estado de la condición en el tick anterior (para detectar flanco).
    was_true:  bool,
}

impl ReactionGate {
    /// Crea un gate para una condición. `mean_ms` típicamente 180, `stddev_ms` 40.
    pub fn new(name: impl Into<String>, mean_ms: f64, stddev_ms: f64, fps: u32) -> Self {
        Self {
            name: name.into(),
            mean_ms,
            stddev_ms,
            fps,
            open_at: None,
            was_true: false,
        }
    }

    /// Actualiza el estado con el valor actual de la condición en `tick`.
    /// Detecta el flanco `false → true` y arma el gate con un reaction delay.
    /// Cuando la condición vuelve a `false`, resetea el gate.
    pub fn update(&mut self, is_true: bool, tick: u64) {
        if is_true && !self.was_true {
            // Flanco de subida: aparece la amenaza → muestreo del delay.
            let delay = sample_gauss_ticks(self.mean_ms, self.stddev_ms, self.fps);
            self.open_at = Some(tick + delay);
            tracing::debug!(
                "ReactionGate[{}] armed: tick={} open_at={} (+{} ticks)",
                self.name, tick, tick + delay, delay
            );
        } else if !is_true {
            // Amenaza desaparece → resetear para que el próximo flanco
            // vuelva a samplear un reaction delay nuevo.
            self.open_at = None;
        }
        self.was_true = is_true;
    }

    /// ¿El gate está abierto (o nunca se armó)? Solo se puede emitir acción
    /// a través de este gate cuando `is_open` es `true`.
    ///
    /// Si el gate nunca se armó (condición siempre false), retorna `false`.
    /// Una vez armado y pasado el delay, retorna `true`.
    pub fn is_open(&self, tick: u64) -> bool {
        match self.open_at {
            None           => false,
            Some(open_at)  => tick >= open_at,
        }
    }

    /// ¿El gate está armado pero aún dentro del reaction delay?
    #[allow(dead_code)] // extension point: diagnostics
    pub fn is_armed(&self, tick: u64) -> bool {
        match self.open_at {
            None          => false,
            Some(open_at) => tick < open_at,
        }
    }

    /// Fuerza el reset del gate (p.ej. al recibir PauseRequested).
    #[allow(dead_code)] // extension point
    pub fn reset(&mut self) {
        self.open_at = None;
        self.was_true = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deterministic_gate() -> ReactionGate {
        // stddev=0 → el delay es exactamente mean_ms convertido a ticks.
        // mean=200ms @ 30fps = 6 ticks.
        ReactionGate::new("test", 200.0, 0.0, 30)
    }

    #[test]
    fn fresh_gate_is_closed() {
        let g = deterministic_gate();
        assert!(!g.is_open(0));
        assert!(!g.is_armed(0));
    }

    #[test]
    fn rising_edge_arms_the_gate() {
        let mut g = deterministic_gate();
        g.update(true, 100);
        // Armed: aún en el delay.
        assert!(g.is_armed(100));
        assert!(!g.is_open(100));
    }

    #[test]
    fn gate_opens_after_reaction_delay() {
        let mut g = deterministic_gate();
        g.update(true, 100);
        // 200ms @ 30fps = 6 ticks.
        assert!(!g.is_open(105));  // tick 5 después, aún no.
        assert!(g.is_open(106));   // tick 6 después, abierto.
        assert!(g.is_open(200));   // sigue abierto siempre que la condición dure.
    }

    #[test]
    fn falling_edge_resets_gate() {
        let mut g = deterministic_gate();
        g.update(true, 100);
        g.update(true, 106); // abierto
        assert!(g.is_open(106));
        // Condición desaparece.
        g.update(false, 110);
        assert!(!g.is_open(110));
        assert!(!g.is_armed(110));
        // Nuevo rising edge → se vuelve a armar desde cero.
        g.update(true, 200);
        assert!(!g.is_open(200));
        assert!(g.is_open(206));
    }

    #[test]
    fn continuously_true_keeps_gate_open_once_delayed() {
        let mut g = deterministic_gate();
        g.update(true, 0);
        for t in 1..=5 {
            g.update(true, t);
            assert!(!g.is_open(t));
        }
        // tick 6: abierto.
        g.update(true, 6);
        assert!(g.is_open(6));
        // Siguientes ticks con la condición verdadera → sigue abierto.
        for t in 7..30 {
            g.update(true, t);
            assert!(g.is_open(t));
        }
    }

    #[test]
    fn manual_reset_clears_state() {
        let mut g = deterministic_gate();
        g.update(true, 10);
        g.update(true, 20); // abierto
        assert!(g.is_open(20));
        g.reset();
        assert!(!g.is_open(20));
        assert!(!g.is_armed(20));
    }
}
