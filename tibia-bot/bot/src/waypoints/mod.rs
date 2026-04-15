//! waypoints/mod.rs — Secuencias temporales de acciones.
//!
//! **Nota sobre el nombre:** en la Fase 3 un "waypoint" es un *step* de una
//! secuencia temporal, no una posición absoluta en el mundo (eso requiere
//! análisis de minimap / OCR, deferido a una fase posterior).
//!
//! Un `Step` consiste en una tecla + duración:
//! - `walk`:   tecla direccional repetida cada `interval_ms` durante `duration_ms`.
//! - `wait`:   `key = ""` — no emite nada, solo deja pasar `duration_ms`.
//! - `hotkey`: `duration_ms = 0` — emite la tecla una vez y avanza inmediatamente.
//!
//! El game loop consulta `WaypointList::tick_action()` una vez por tick:
//! - Retorna `Some(hidcode)` si toca emitir una tecla este tick.
//! - Retorna `None` si toca esperar (mid-step) o la lista está terminada.
//!
//! Cuando la FSM entra en Emergency o Fighting, el step actual se **re-inicia**
//! al volver a Walking — el personaje puede haberse movido durante el combate y
//! reanudar desde la mitad del step daría comportamiento impredecible.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::act::keycode;

/// Un step de la secuencia. Después de cargar desde TOML, `key` se parsea
/// a un código HID y se cachea en `hidcode` para evitar re-parsing cada tick.
#[derive(Debug, Clone)]
pub struct Step {
    /// Etiqueta humana para logs y `/waypoints/status`.
    pub label:       String,
    /// Nombre de la tecla ("Numpad8", "Up", "F5", ""). `""` = wait.
    #[allow(dead_code)] // deserialized from TOML, hidcode is the runtime field
    pub key:         String,
    /// Código HID pre-parseado. `None` para wait steps.
    pub hidcode:     Option<u8>,
    /// Duración total del step en ms. 0 = one-shot (tap + avanzar).
    pub duration_ms: u64,
    /// Cada cuántos ms re-emitir la tecla durante `duration_ms`. 0 = solo al inicio.
    pub interval_ms: u64,
}

/// Copia ligera de los campos del step relevantes para `tick_action`.
/// Evita mantener una referencia inmutable mientras se muta el iterador.
struct StepSnapshot {
    hidcode:     Option<u8>,
    duration_ms: u64,
    interval_ms: u64,
}

impl From<&Step> for StepSnapshot {
    fn from(s: &Step) -> Self {
        Self {
            hidcode:     s.hidcode,
            duration_ms: s.duration_ms,
            interval_ms: s.interval_ms,
        }
    }
}

/// Representación TOML de un step — se convierte a `Step` tras validar la key.
#[derive(Debug, Deserialize)]
struct StepToml {
    #[serde(default)]
    label:       String,
    #[serde(default)]
    key:         String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    interval_ms: u64,
}

/// Representación TOML del archivo de waypoints.
#[derive(Debug, Deserialize)]
struct WaypointFile {
    #[serde(default = "default_loop")]
    #[serde(rename = "loop")]
    loop_:  bool,
    #[serde(default, rename = "step")]
    steps:  Vec<StepToml>,
}

fn default_loop() -> bool { true }

/// Lista de steps + estado del iterador y timing.
///
/// Todo el tiempo se mide en **ticks** — la FSM opera a 30 Hz, así que
/// `duration_ms` se convierte a ticks usando `ms_to_ticks(ms, fps)`.
#[derive(Debug, Clone)]
pub struct WaypointList {
    pub steps:     Vec<Step>,
    pub loop_:     bool,
    /// Índice del step actualmente activo. `None` si la lista terminó (no-loop).
    pub current:   Option<usize>,
    /// Tick en el que empezó el step actual.
    started_tick:  u64,
    /// Último tick en el que se emitió la tecla del step actual.
    last_emit_tick: Option<u64>,
    /// FPS del game loop (para convertir ms↔ticks).
    fps:           u32,
    /// Stuck watchdog — tick en el que el iterador avanzó por última vez a
    /// un step diferente. El `BotLoop` lo compara con el tick actual para
    /// detectar bloqueos.
    last_advance_tick: u64,
    /// Índice del step observado la última vez que se chequeó stuck.
    /// Usado junto con `last_advance_tick` para saber si hubo avance.
    last_seen_idx: Option<usize>,
}

impl WaypointList {
    /// Carga un archivo TOML de waypoints y valida todas las hotkeys.
    /// Falla con error descriptivo si alguna `key` no se puede parsear.
    pub fn load(path: &Path, fps: u32) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("No se pudo leer '{}'", path.display()))?;
        let file: WaypointFile = toml::from_str(&raw)
            .with_context(|| format!("TOML inválido en '{}'", path.display()))?;

        let mut steps = Vec::with_capacity(file.steps.len());
        for (idx, st) in file.steps.into_iter().enumerate() {
            let hidcode = if st.key.is_empty() {
                None
            } else {
                Some(keycode::parse(&st.key)
                    .with_context(|| format!("step[{}] ('{}'): key inválida", idx, st.label))?)
            };
            steps.push(Step {
                label:       st.label,
                key:         st.key,
                hidcode,
                duration_ms: st.duration_ms,
                interval_ms: st.interval_ms,
            });
        }

        if steps.is_empty() {
            anyhow::bail!("Waypoint file '{}' no contiene ningún step", path.display());
        }

        Ok(Self {
            steps,
            loop_: file.loop_,
            current: Some(0),
            started_tick: 0,
            last_emit_tick: None,
            fps,
            last_advance_tick: 0,
            last_seen_idx: Some(0),
        })
    }

    /// Consulta el step activo y decide si emitir una tecla este tick.
    /// Avanza el iterador cuando el step actual expira.
    ///
    /// - `tick`: número de tick actual del game loop.
    /// - Retorna `Some(hidcode)` si toca emitir la tecla ahora.
    /// - Retorna `None` si el step activo está esperando, o la lista terminó.
    pub fn tick_action(&mut self, tick: u64) -> Option<u8> {
        let idx = self.current?;

        // Copiamos los campos del step actual para evitar mantener una
        // referencia inmutable a `self.steps` mientras mutamos `self.advance`.
        let StepSnapshot { hidcode, duration_ms, interval_ms } =
            StepSnapshot::from(&self.steps[idx]);

        let ticks_in_step  = tick.saturating_sub(self.started_tick);
        let duration_ticks = ms_to_ticks(duration_ms, self.fps);

        // ── Caso one-shot (duration_ms = 0) ───────────────────────────────
        if duration_ms == 0 {
            self.advance(tick);
            return hidcode;
        }

        // ── Step expirado ──────────────────────────────────────────────────
        if ticks_in_step >= duration_ticks {
            self.advance(tick);
            // Re-entrar recursivamente para que el siguiente step pueda emitir
            // este mismo tick si es one-shot o tiene su primer emit ahora.
            return self.tick_action(tick);
        }

        // ── Wait step: sin tecla, solo deja pasar el tiempo ───────────────
        let hidcode = hidcode?;

        // ── Primer emit del step ──────────────────────────────────────────
        let Some(last) = self.last_emit_tick else {
            self.last_emit_tick = Some(tick);
            return Some(hidcode);
        };

        // ── Re-emit por interval_ms ───────────────────────────────────────
        if interval_ms == 0 {
            // Sin interval: solo se emite al inicio del step. Ya lo hicimos.
            return None;
        }

        let interval_ticks = ms_to_ticks(interval_ms, self.fps).max(1);
        if tick.saturating_sub(last) >= interval_ticks {
            self.last_emit_tick = Some(tick);
            return Some(hidcode);
        }
        None
    }

    /// Reinicia el step actual (útil tras volver de combate/emergencia).
    /// Si la lista terminó, no hace nada.
    ///
    /// Esto también extiende la "ventana de no-stuck" — un combate largo
    /// seguido de un restart no debe contabilizarse como tiempo atascado.
    pub fn restart_current_step(&mut self, tick: u64) {
        if self.current.is_some() {
            self.started_tick = tick;
            self.last_emit_tick = None;
            self.last_advance_tick = tick;
        }
    }

    /// Avanza al siguiente step, loopeando si `loop_ = true`.
    fn advance(&mut self, tick: u64) {
        let Some(idx) = self.current else { return };
        let next = idx + 1;
        if next < self.steps.len() {
            self.current = Some(next);
        } else if self.loop_ {
            self.current = Some(0);
        } else {
            self.current = None;
        }
        self.started_tick = tick;
        self.last_emit_tick = None;
        // Marca el avance para el stuck watchdog.
        self.last_advance_tick = tick;
        self.last_seen_idx = self.current;
    }

    /// Verifica si la lista lleva demasiado tiempo sin avanzar un step.
    /// Retorna `true` la **primera vez** que se detecta el stuck — llamadas
    /// subsecuentes devuelven `false` hasta que el iterador avance de nuevo
    /// o se llame `reset_stuck_tracker`. El caller (BotLoop) debe decidir
    /// qué hacer (log + pausa + /waypoints/clear).
    ///
    /// `threshold_ticks` = número máximo de ticks sin avance antes de alarmar.
    pub fn tick_stuck_check(&mut self, tick: u64, threshold_ticks: u64) -> bool {
        if self.current.is_none() || threshold_ticks == 0 {
            return false;
        }
        // Si el step actual no coincide con last_seen_idx, hubo avance desde
        // la última consulta — actualizar y no alarmar.
        if self.current != self.last_seen_idx {
            self.last_seen_idx     = self.current;
            self.last_advance_tick = tick;
            return false;
        }
        let stagnant = tick.saturating_sub(self.last_advance_tick);
        if stagnant >= threshold_ticks {
            // Rearmar el tracker para no avisar cada tick tras el warning.
            self.last_advance_tick = tick;
            return true;
        }
        false
    }

    /// Reinicia el tracker del stuck watchdog. Útil tras pausas explícitas
    /// o recargas para no contar el tiempo pausado como "atasco".
    pub fn reset_stuck_tracker(&mut self, tick: u64) {
        self.last_advance_tick = tick;
        self.last_seen_idx     = self.current;
    }

    /// Label + índice del step actualmente activo (para /waypoints/status).
    pub fn current_label(&self) -> Option<(usize, &str)> {
        let idx = self.current?;
        Some((idx, self.steps[idx].label.as_str()))
    }

    /// `true` si la lista tiene un step activo (no ha terminado en modo no-loop).
    pub fn is_running(&self) -> bool {
        self.current.is_some()
    }
}

/// Convierte milisegundos a ticks a la frecuencia dada, redondeando hacia arriba.
/// - `ms_to_ticks(0, 30)` = 0
/// - `ms_to_ticks(33, 30)` = 1 (un tick ≈ 33ms a 30 Hz)
/// - `ms_to_ticks(1000, 30)` = 30
fn ms_to_ticks(ms: u64, fps: u32) -> u64 {
    if ms == 0 || fps == 0 {
        return 0;
    }
    // redondeo hacia arriba: ceil(ms * fps / 1000)
    (ms * fps as u64).div_ceil(1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_walk(label: &str, hidcode: u8, duration_ms: u64, interval_ms: u64) -> Step {
        Step {
            label: label.into(),
            key: "test".into(),
            hidcode: Some(hidcode),
            duration_ms,
            interval_ms,
        }
    }

    fn step_wait(label: &str, duration_ms: u64) -> Step {
        Step {
            label: label.into(),
            key: "".into(),
            hidcode: None,
            duration_ms,
            interval_ms: 0,
        }
    }

    fn step_oneshot(label: &str, hidcode: u8) -> Step {
        Step {
            label: label.into(),
            key: "F1".into(),
            hidcode: Some(hidcode),
            duration_ms: 0,
            interval_ms: 0,
        }
    }

    fn list(steps: Vec<Step>, loop_: bool) -> WaypointList {
        WaypointList {
            steps,
            loop_,
            current: Some(0),
            started_tick: 0,
            last_emit_tick: None,
            fps: 30,
            last_advance_tick: 0,
            last_seen_idx: Some(0),
        }
    }

    #[test]
    fn ms_to_ticks_rounding() {
        assert_eq!(ms_to_ticks(0, 30), 0);
        assert_eq!(ms_to_ticks(33, 30), 1);
        assert_eq!(ms_to_ticks(34, 30), 2);
        assert_eq!(ms_to_ticks(1000, 30), 30);
        assert_eq!(ms_to_ticks(500, 30), 15);
    }

    #[test]
    fn single_walk_step_emits_once_then_waits() {
        // duration=1000ms (30 ticks), interval=0 → emite solo al inicio.
        let mut wl = list(vec![step_walk("N", 0x60, 1000, 0)], false);

        assert_eq!(wl.tick_action(0), Some(0x60));
        assert_eq!(wl.tick_action(1), None);
        assert_eq!(wl.tick_action(15), None);
        assert_eq!(wl.tick_action(29), None);
        // Tick 30: step expiró. Sin loop → None y current = None.
        assert_eq!(wl.tick_action(30), None);
        assert!(!wl.is_running());
    }

    #[test]
    fn walk_with_interval_reemits() {
        // duration=1000ms, interval=250ms → emite cada 8 ticks (250ms≈7.5).
        let mut wl = list(vec![step_walk("N", 0x60, 1000, 250)], false);

        let mut emits_at = vec![];
        for t in 0..30 {
            if let Some(hid) = wl.tick_action(t) {
                assert_eq!(hid, 0x60);
                emits_at.push(t);
            }
        }
        // Primer emit en t=0. Siguientes cada ceil(250*30/1000)=8 ticks.
        // Esperado: 0, 8, 16, 24. Total: 4 emits en un step de 30 ticks.
        assert_eq!(emits_at, vec![0, 8, 16, 24]);
    }

    #[test]
    fn wait_step_emits_nothing() {
        let mut wl = list(vec![step_wait("pause", 500)], false);
        for t in 0..20 {
            assert_eq!(wl.tick_action(t), None);
        }
        // Tras 500ms (15 ticks) debe haber avanzado — pero no hay más steps,
        // así que sigue None y is_running=false.
        assert!(!wl.is_running());
    }

    #[test]
    fn oneshot_step_advances_immediately() {
        let mut wl = list(
            vec![
                step_oneshot("cast heal", 0x3A),
                step_walk("then walk", 0x60, 1000, 0),
            ],
            false,
        );

        // Tick 0: emite el one-shot. El siguiente tick debería estar en el walk step.
        assert_eq!(wl.tick_action(0), Some(0x3A));
        // Tick 1: dentro del walk step (primer emit).
        assert_eq!(wl.tick_action(1), Some(0x60));
        assert_eq!(wl.current_label(), Some((1, "then walk")));
    }

    #[test]
    fn loop_restarts_from_zero() {
        let mut wl = list(vec![step_walk("N", 0x60, 100, 0)], true);
        // 100ms = 3 ticks. Emite en tick 0, expira en tick 3.
        assert_eq!(wl.tick_action(0), Some(0x60));
        assert_eq!(wl.tick_action(1), None);
        assert_eq!(wl.tick_action(2), None);
        // Tick 3: step expiró; por loop, vuelve al inicio y emite de nuevo.
        assert_eq!(wl.tick_action(3), Some(0x60));
        assert!(wl.is_running());
    }

    #[test]
    fn restart_current_step_resets_timing() {
        let mut wl = list(vec![step_walk("N", 0x60, 1000, 250)], false);
        wl.tick_action(0);  // emit at tick 0
        wl.tick_action(8);  // emit at tick 8
        assert_eq!(wl.last_emit_tick, Some(8));

        wl.restart_current_step(100);
        assert_eq!(wl.started_tick, 100);
        assert_eq!(wl.last_emit_tick, None);
        // Después del restart, el primer tick tras 100 debe volver a emitir.
        assert_eq!(wl.tick_action(100), Some(0x60));
    }

    #[test]
    fn oneshot_on_last_step_ends_list_when_not_looping() {
        let mut wl = list(vec![step_oneshot("final", 0x3A)], false);
        assert_eq!(wl.tick_action(0), Some(0x3A));
        assert!(!wl.is_running());
        assert_eq!(wl.tick_action(1), None);
    }

    #[test]
    fn load_rejects_bad_key() {
        use std::io::Write as _;
        let dir = std::env::temp_dir();
        let path = dir.join("tibia_bot_test_waypoints_bad.toml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "loop = false").unwrap();
            writeln!(f, "[[step]]").unwrap();
            writeln!(f, "label = \"bad\"").unwrap();
            writeln!(f, "key = \"FunkyKey\"").unwrap();
            writeln!(f, "duration_ms = 500").unwrap();
        }
        let r = WaypointList::load(&path, 30);
        assert!(r.is_err(), "debería fallar al parsear la key inválida");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stuck_check_fires_after_threshold_without_advance() {
        // Lista de 2 steps muy largos — el iterador nunca avanza dentro
        // de la ventana del watchdog, así que debe alarmar.
        let mut wl = list(
            vec![
                step_walk("N", 0x60, 60_000, 0), // 60s
                step_walk("S", 0x5A, 60_000, 0),
            ],
            true,
        );
        // threshold = 10 ticks. Sin llamar a tick_action (nadie avanza).
        assert!(!wl.tick_stuck_check(1, 10));
        assert!(!wl.tick_stuck_check(5, 10));
        assert!(!wl.tick_stuck_check(9, 10));
        // Tick 10: stagnant = 10 >= 10 → fire.
        assert!(wl.tick_stuck_check(10, 10));
        // Siguiente consulta inmediata: ya se rearmó, no fire de nuevo.
        assert!(!wl.tick_stuck_check(11, 10));
    }

    #[test]
    fn stuck_check_not_fired_when_advancing_normally() {
        // Lista con steps cortos: advance() se llama dentro de la ventana.
        let mut wl = list(vec![step_walk("N", 0x60, 100, 0)], true);
        // 100ms = 3 ticks. Emite, expira, avanza, re-emite...
        for t in 0..30 {
            wl.tick_action(t);
            assert!(!wl.tick_stuck_check(t, 10),
                "tick {}: no debería haber stuck con steps de 100ms", t);
        }
    }

    #[test]
    fn stuck_check_zero_threshold_disabled() {
        let mut wl = list(vec![step_walk("N", 0x60, 60_000, 0)], true);
        // threshold = 0 desactiva el watchdog.
        for t in 0..1000 {
            assert!(!wl.tick_stuck_check(t, 0));
        }
    }

    #[test]
    fn stuck_tracker_reset_extends_window() {
        let mut wl = list(vec![step_walk("N", 0x60, 60_000, 0)], true);
        assert!(!wl.tick_stuck_check(9, 10));
        wl.reset_stuck_tracker(9);
        // Tras reset, empezamos a contar desde 9.
        assert!(!wl.tick_stuck_check(18, 10));
        assert!(wl.tick_stuck_check(19, 10));
    }

    #[test]
    fn restart_current_step_resets_stuck_window() {
        let mut wl = list(vec![step_walk("N", 0x60, 60_000, 0)], true);
        assert!(wl.tick_stuck_check(10, 10));
        // Combate simulado: el loop llama restart_current_step al volver.
        wl.restart_current_step(20);
        // Ahora debe haber otra ventana completa antes de re-alarmar.
        assert!(!wl.tick_stuck_check(29, 10));
        assert!(wl.tick_stuck_check(30, 10));
    }

    #[test]
    fn load_accepts_valid_toml() {
        use std::io::Write as _;
        let dir = std::env::temp_dir();
        let path = dir.join("tibia_bot_test_waypoints_ok.toml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "loop = true").unwrap();
            writeln!(f, "[[step]]").unwrap();
            writeln!(f, "label = \"walk north\"").unwrap();
            writeln!(f, "key = \"Numpad8\"").unwrap();
            writeln!(f, "duration_ms = 2000").unwrap();
            writeln!(f, "interval_ms = 250").unwrap();
            writeln!(f, "[[step]]").unwrap();
            writeln!(f, "label = \"pause\"").unwrap();
            writeln!(f, "duration_ms = 1000").unwrap();
        }
        let wl = WaypointList::load(&path, 30).unwrap();
        assert_eq!(wl.steps.len(), 2);
        assert_eq!(wl.steps[0].label, "walk north");
        assert_eq!(wl.steps[0].hidcode, Some(0x60)); // Numpad8
        assert_eq!(wl.steps[1].hidcode, None);        // wait step
        assert!(wl.loop_);
        let _ = std::fs::remove_file(&path);
    }
}
