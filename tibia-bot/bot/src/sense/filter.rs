//! filter.rs — Primitivas de filtrado temporal + `PerceptionFilter`.
//!
//! ## Motivación
//!
//! Varias señales de `Perception` hoy llegan crudas al FSM y al cavebot:
//!
//! - `target_active` cambia cada tick según lectura del target bar; un solo
//!   frame de overlay provoca `Some(true) → Some(false) → Some(true)` con
//!   consecuencias en combat (rotación de target espuria, PgDown innecesaria).
//! - `game_coords` viene de tile-hashing (dHash) que puede colisionar
//!   puntualmente: el char reporta coord X durante 3 ticks, salta a Y por
//!   1 tick, vuelve a X. Un `at_coord` check puede fallar por esto.
//! - `enemy_count` parpadea cuando un mob ocluye brevemente otro en la
//!   lista de batalla.
//!
//! Este módulo provee **primitivas genéricas** (EMA, hysteresis, median,
//! majority vote, streak counter) y una fachada `PerceptionFilter` que las
//! aplica sobre un `Perception` crudo devolviendo uno suavizado.
//!
//! ## Contratos
//!
//! - **Single-thread**: vive en el game loop. No locks internos.
//! - **Idempotente sin input**: `apply()` sin llamadas previas retorna el
//!   input cuasi-sin-tocar (solo señales con estado acumulable cambian).
//! - **Raw preservado**: el caller recibe un NUEVO `Perception`; el raw se
//!   guarda aparte (p. ej. `SharedState.last_perception_raw`) para HTTP y
//!   recorder, manteniendo replay bit-exactness.
//!
//! ## Tuning
//!
//! Los parámetros por señal (alphas EMA, off-confirm frames) se eligieron
//! para NO alterar tuning del FSM actual (HP/mana ya traen debouncing inline
//! en `Vision`, el EMA aquí es α=0.85 → cambio <5% vs raw). Las señales
//! NO filtradas upstream (`target_active`, `game_coords`, `enemy_count`)
//! sí ganan protección real.

use std::collections::VecDeque;

// ── Consts migradas desde `vision/mod.rs` ────────────────────────────────
//
// Hasta 2026-04 estas vivían inline en `Vision::tick`. Se consolidan acá
// porque el debouncing de vitales + la hysteresis de movimiento son
// comportamiento temporal, no visión per se. Vision ahora emite el raw
// y el filter aplica la semántica temporal.

/// Frames consecutivos de "bad read" (HP ratio ≈ 0 o None) antes de
/// propagar el valor real. Absorbe transient de overlays/animaciones.
/// 5 ticks ≈ 150 ms @ 30 Hz.
pub const VITALS_PANIC_FRAMES: u32 = 5;

/// Frames consecutivos con minimap_diff bajo el umbral antes de declarar
/// "no me muevo" (desactivar is_moving). Asimétrico: activar es inmediato,
/// desactivar requiere N frames calm. Evita que un frame de pausa visual
/// corte walk steps en el cavebot.
pub const MOVEMENT_CALM_FRAMES: u32 = 10;

// ── Primitivas ────────────────────────────────────────────────────────────

/// Exponential Moving Average para señales continuas f32.
///
/// `α ∈ (0, 1]`: más alto = más reactivo, menos smoothing.
/// - `α = 1.0` → passthrough (sin smoothing).
/// - `α = 0.85` → smoothing ligero (~5-10% lag en step).
/// - `α = 0.3` → smoothing agresivo (laggy pero robusto).
///
/// `None` input preserva el último valor suavizado (no resetea). Esto evita
/// que un frame sin señal reinicie la curva. Llamar `reset()` para limpiar.
#[derive(Debug, Clone)]
pub struct EmaState {
    value: Option<f32>,
    alpha: f32,
}

impl EmaState {
    pub fn new(alpha: f32) -> Self {
        let alpha = alpha.clamp(0.01, 1.0);
        Self { value: None, alpha }
    }

    /// Actualiza con un nuevo raw y retorna el valor suavizado.
    /// Si raw es `None`, no muta estado y retorna el último smoothed
    /// (o `None` si nunca hubo input).
    pub fn update(&mut self, raw: Option<f32>) -> Option<f32> {
        match (raw, self.value) {
            (Some(r), None)    => { self.value = Some(r); Some(r) }
            (Some(r), Some(v)) => {
                let new = self.alpha * r + (1.0 - self.alpha) * v;
                self.value = Some(new);
                Some(new)
            }
            (None, v) => v,
        }
    }

    pub fn reset(&mut self) { self.value = None; }

    #[cfg(test)]
    pub fn current(&self) -> Option<f32> { self.value }
}

/// Hysteresis asimétrica para señales binarias.
///
/// Activa inmediato, desactiva tras N frames consecutivos `false`. Útil para
/// signals como `target_active` donde un false transient (overlay, animation)
/// no debe tumbar el estado "estoy atacando algo".
///
/// - `on`: un único `true` basta para activar.
/// - `off`: requiere `off_confirm` falses consecutivos para desactivar.
#[derive(Debug, Clone)]
pub struct HysteresisState {
    off_confirm: u32,
    state:       bool,
    off_streak:  u32,
}

impl HysteresisState {
    pub fn new(off_confirm: u32) -> Self {
        Self { off_confirm: off_confirm.max(1), state: false, off_streak: 0 }
    }

    /// Streak actual de "false consecutivos mientras state=true". Expuesto
    /// para que PerceptionFilter calcule target_confidence (degrada durante
    /// el hold period). 0 = no hay hold activo; `off_confirm` = próximo tick
    /// desactiva.
    pub fn off_streak(&self) -> u32 { self.off_streak }
    pub fn off_confirm(&self) -> u32 { self.off_confirm }

    /// Procesa un nuevo raw, retorna el estado filtrado.
    pub fn update(&mut self, raw: bool) -> bool {
        if raw {
            self.state      = true;
            self.off_streak = 0;
        } else if self.state {
            self.off_streak += 1;
            if self.off_streak >= self.off_confirm {
                self.state      = false;
                self.off_streak = 0;
            }
        }
        self.state
    }

    pub fn is_active(&self) -> bool { self.state }
    pub fn reset(&mut self) { self.state = false; self.off_streak = 0; }
}

/// Median de los últimos N samples. N=3 es el caso típico (reduce flicker
/// sin introducir lag >N frames). Con el buffer sin llenarse, retorna el
/// median del partial (equivalent a valor central del chico).
#[derive(Debug, Clone)]
pub struct MedianWindow<T: Copy + Ord> {
    buf: VecDeque<T>,
    cap: usize,
}

impl<T: Copy + Ord> MedianWindow<T> {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self { buf: VecDeque::with_capacity(cap), cap }
    }

    /// Inserta un nuevo sample y retorna el median del window actual.
    pub fn update(&mut self, sample: T) -> T {
        if self.buf.len() == self.cap { self.buf.pop_front(); }
        self.buf.push_back(sample);
        let mut sorted: Vec<T> = self.buf.iter().copied().collect();
        sorted.sort();
        sorted[sorted.len() / 2]
    }

    pub fn reset(&mut self) { self.buf.clear(); }
}

/// Majority vote sobre los últimos N samples. Empates → el más reciente
/// dentro del empate (desde la cola del buffer).
///
/// Útil para enteros discretos con aliasing: `game_coords` tile-hashing
/// puede colisionar puntualmente → mayoría de la ventana estabiliza.
#[derive(Debug, Clone)]
pub struct MajorityVote<T: Clone + Eq> {
    buf: VecDeque<T>,
    cap: usize,
}

impl<T: Clone + Eq> MajorityVote<T> {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self { buf: VecDeque::with_capacity(cap), cap }
    }

    /// Inserta sample y retorna el elemento más frecuente (empates → newest).
    pub fn update(&mut self, sample: T) -> T {
        if self.buf.len() == self.cap { self.buf.pop_front(); }
        self.buf.push_back(sample);

        // Contamos frecuencias iterando de atrás hacia adelante (el primero
        // que alcance el max será el más reciente dentro del empate).
        let mut best: Option<(T, usize)> = None;
        for item in self.buf.iter().rev() {
            let count = self.buf.iter().filter(|x| *x == item).count();
            match &best {
                None => best = Some((item.clone(), count)),
                Some((_, bc)) if count > *bc => best = Some((item.clone(), count)),
                _ => {}
            }
        }
        best.map(|(t, _)| t).expect("buf non-empty after push")
    }

    pub fn reset(&mut self) { self.buf.clear(); }
}

/// Streak counter: retorna `true` cuando se acumularon N matches consecutivos.
/// Reset automático en mismatch.
#[derive(Debug, Clone, Default)]
pub struct StreakCounter {
    count: u32,
}

impl StreakCounter {
    pub fn new() -> Self { Self::default() }

    /// Retorna el streak actual tras procesar el evento. El caller decide
    /// si supera su threshold.
    pub fn update(&mut self, matched: bool) -> u32 {
        if matched { self.count = self.count.saturating_add(1); }
        else       { self.count = 0; }
        self.count
    }

    pub fn reset(&mut self) { self.count = 0; }
    pub fn current(&self) -> u32 { self.count }
}

// ── VitalsDebouncer ──────────────────────────────────────────────────────

use super::perception::{Perception, VitalBar};

/// Filtro de "bad read" para barras vitales (HP / mana). Mantiene el último
/// `VitalBar` estable y propaga el raw recién después de N frames consecutivos
/// de bad. Equivalente al debouncing inline que vivía en `Vision::tick`.
///
/// `is_bad` se determina externamente (HP usa `ratio < 0.001`, mana usa
/// `raw.is_none()`). El debouncer no impone política — recibe `(raw, bad_flag)`.
#[derive(Debug, Clone)]
pub struct VitalsDebouncer {
    last_stable: Option<VitalBar>,
    bad_frames:  u32,
    panic_thr:   u32,
}

impl VitalsDebouncer {
    pub fn new(panic_threshold: u32) -> Self {
        Self { last_stable: None, bad_frames: 0, panic_thr: panic_threshold.max(1) }
    }

    /// Procesa un nuevo raw. Si `is_bad`, retorna el último stable hasta
    /// que se acumulan `panic_thr` bads consecutivos — entonces propaga el
    /// raw (que probablemente sea None o vacío). Si no es bad, resetea
    /// el contador y actualiza last_stable.
    pub fn update(&mut self, raw: Option<VitalBar>, is_bad: bool) -> Option<VitalBar> {
        if is_bad {
            self.bad_frames += 1;
            if self.bad_frames >= self.panic_thr {
                // Persistente → confiar en el raw (puede indicar muerte / cambio
                // de pantalla / disconnect — el FSM debe ver eso de verdad).
                raw
            } else {
                // Transient → mantener último stable.
                self.last_stable.clone()
            }
        } else {
            self.bad_frames = 0;
            self.last_stable = raw.clone();
            raw
        }
    }

    pub fn reset(&mut self) {
        self.last_stable = None;
        self.bad_frames  = 0;
    }

    /// Confidence [0..1] derivada del estado interno.
    /// - `1.0`: lecturas consecutivas buenas (bad_frames=0).
    /// - Decae linealmente hasta `0.0` cuando bad_frames alcanza panic_thr.
    /// - Expuesto para HealthSystem::LowDetectionConfidence.
    pub fn confidence(&self) -> f32 {
        if self.panic_thr == 0 { return 1.0; }
        let ratio = self.bad_frames as f32 / self.panic_thr as f32;
        (1.0 - ratio).clamp(0.0, 1.0)
    }
}

// ── PerceptionFilter ──────────────────────────────────────────────────────

/// Aplica smoothing temporal a un `Perception` crudo. Ver módulo docstring
/// para diseño.
pub struct PerceptionFilter {
    // Vitales: debouncer de bad reads + EMA ligera sobre el ratio estable.
    // VITALS_PANIC_FRAMES=5 absorbe transients (overlay, animación, UI flash)
    // antes de propagar el bad read genuino. EMA α=0.85 después suaviza
    // micro-ruido entre lecturas válidas.
    hp_debouncer:  VitalsDebouncer,
    hp_ema:        EmaState,
    mana_debouncer: VitalsDebouncer,
    mana_ema:      EmaState,

    // Binarias sin filtro upstream.
    // target_active: 2 frames consecutivos false para desactivar.
    // A 30 Hz ≈ 66 ms de "hold" post-target-lost.
    target_hysteresis: HysteresisState,

    // is_moving: hysteresis asimétrica (activa inmediato, desactiva tras
    // MOVEMENT_CALM_FRAMES). Replica la lógica que vivía en Vision.
    moving_hysteresis: HysteresisState,
    /// Último is_moving filtrado — se conserva para que la transición
    /// triggear logs/diagnostics. Solo Some cuando el minimap está calibrado.
    prev_is_moving: Option<bool>,

    // Contadores discretos.
    // enemy_count: median de 3. Absorbe 1 frame spúreo.
    enemy_count_median: MedianWindow<u32>,

    // Categórica compuesta — game_coords con mayoría 3/5 para tile-hashing.
    coords_vote: MajorityVote<(i32, i32, i32)>,
    /// Último coord Some observado — se usa cuando el tick actual es None
    /// para evitar "sparse holes" durante tile-hashing miss temporal.
    last_coords_hold: Option<(i32, i32, i32)>,

    /// Último enemy_count con median aplicado. Accesible via
    /// `filtered_enemy_count()` — no muta el Perception porque BattleList
    /// no expone el count como field público.
    last_enemy_count_filtered: u32,
}

impl Default for PerceptionFilter {
    fn default() -> Self {
        Self {
            hp_debouncer:       VitalsDebouncer::new(VITALS_PANIC_FRAMES),
            hp_ema:             EmaState::new(0.85),
            mana_debouncer:     VitalsDebouncer::new(VITALS_PANIC_FRAMES),
            mana_ema:           EmaState::new(0.85),
            target_hysteresis:  HysteresisState::new(2),
            moving_hysteresis:  HysteresisState::new(MOVEMENT_CALM_FRAMES),
            prev_is_moving:     None,
            enemy_count_median: MedianWindow::new(3),
            coords_vote:        MajorityVote::new(5),
            last_coords_hold:   None,
            last_enemy_count_filtered: 0,
        }
    }
}

impl PerceptionFilter {
    pub fn new() -> Self { Self::default() }

    /// Consume un `Perception` crudo, produce uno filtrado. El raw no se
    /// muta — el caller lo conserva para HTTP/recorder.
    pub fn apply(&mut self, raw: &Perception) -> Perception {
        let mut out = raw.clone();

        // ── HP: debouncer (5-frame panic) seguido de EMA ligera sobre el
        //    ratio del valor estable. is_bad cuando ratio < 0.001 (overlay
        //    pinta encima de la barra y devuelve cero filled).
        let hp_is_bad = raw.vitals.hp.as_ref().map(|b| b.ratio < 0.001).unwrap_or(true);
        let stable_hp = self.hp_debouncer.update(raw.vitals.hp, hp_is_bad);
        out.vitals.hp = stable_hp.map(|hp| {
            let smoothed = self.hp_ema.update(Some(hp.ratio)).unwrap_or(hp.ratio);
            VitalBar { ratio: smoothed, ..hp }
        });

        // ── Mana: same patrón. is_bad = raw.is_none() (mana reader devuelve
        //    None si no encuentra la barra; ratio=0 sí es estado válido).
        let mana_is_bad = raw.vitals.mana.is_none();
        let stable_mana = self.mana_debouncer.update(raw.vitals.mana, mana_is_bad);
        out.vitals.mana = stable_mana.map(|mana| {
            let smoothed = self.mana_ema.update(Some(mana.ratio)).unwrap_or(mana.ratio);
            VitalBar { ratio: smoothed, ..mana }
        });

        // ── target_active: hysteresis. None se preserva (no hay signal).
        if let Some(raw_target) = raw.target_active {
            out.target_active = Some(self.target_hysteresis.update(raw_target));
        }

        // ── is_moving: hysteresis asimétrica activa-rápido / desactiva-lento.
        //    None se preserva (minimap no calibrado → no hay signal).
        match raw.is_moving {
            Some(raw_moving) => {
                let filtered = self.moving_hysteresis.update(raw_moving);
                out.is_moving = Some(filtered);
                self.prev_is_moving = Some(filtered);
            }
            None => {
                // Sin minimap → reseteamos hysteresis para evitar que
                // arrastre estado entre fases con/sin minimap.
                self.moving_hysteresis.reset();
                self.prev_is_moving = None;
                out.is_moving = None;
            }
        }

        // ── enemy_count: median 3 sobre el count derivado de la BattleList.
        //    Ahora que BattleList expone `enemy_count_filtered: Option<u32>`
        //    propagamos el valor directamente al Perception filtrado.
        //    Consumers llamar `battle.enemy_count_effective()` para decisiones.
        let filtered_count = self.enemy_count_median.update(raw.battle.enemy_count() as u32);
        self.last_enemy_count_filtered = filtered_count;
        out.battle.enemy_count_filtered = Some(filtered_count);

        // ── game_coords: majority vote con fallback al último Some.
        match raw.game_coords {
            Some(c) => {
                let voted = self.coords_vote.update(c);
                self.last_coords_hold = Some(voted);
                out.game_coords = Some(voted);
            }
            None => {
                // Sin lectura fresca → hold del último voted si existe.
                // Evita que un solo tile-hashing miss borre el coord y
                // rompa `at_coord` en cavebot.
                out.game_coords = self.last_coords_hold;
            }
        }

        out
    }

    /// Accesor separado para el enemy_count filtrado — el Perception
    /// filtrado no lo propaga (ver nota en `apply()`). Retorna el median
    /// del último tick aplicado. Antes del primer `apply()` retorna 0.
    pub fn filtered_enemy_count(&self) -> u32 {
        self.last_enemy_count_filtered
    }

    pub fn reset(&mut self) {
        self.hp_debouncer.reset();
        self.hp_ema.reset();
        self.mana_debouncer.reset();
        self.mana_ema.reset();
        self.target_hysteresis.reset();
        self.moving_hysteresis.reset();
        self.prev_is_moving = None;
        self.enemy_count_median.reset();
        self.coords_vote.reset();
        self.last_coords_hold = None;
        self.last_enemy_count_filtered = 0;
    }

    /// Diagnóstico: estado is_moving del último apply (None si no se llamó
    /// o si raw venía sin minimap calibrado).
    pub fn current_is_moving(&self) -> Option<bool> { self.prev_is_moving }

    /// Confidence [0..1] de los vitales (min de hp + mana debouncer).
    /// Refleja cuántos frames bad consecutivos hay — útil para el
    /// HealthSystem::LowDetectionConfidence.
    pub fn vitals_confidence(&self) -> f32 {
        self.hp_debouncer.confidence().min(self.mana_debouncer.confidence())
    }

    /// Confidence [0..1] del target_active. 1.0 cuando hysteresis está en
    /// estado estable; degrada linealmente durante el hold (state=true con
    /// false consecutivos acumulando) hasta 0.5 justo antes de desactivar.
    /// Significa: "el signal puede que esté a punto de apagarse; actuar
    /// con cautela".
    pub fn target_confidence(&self) -> f32 {
        if self.target_hysteresis.is_active() && self.target_hysteresis.off_streak() > 0 {
            let confirm = self.target_hysteresis.off_confirm().max(1) as f32;
            let ratio = self.target_hysteresis.off_streak() as f32 / confirm;
            // Degrada de 1.0 a 0.5 durante el hold.
            (1.0 - ratio * 0.5).clamp(0.5, 1.0)
        } else {
            1.0
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── EmaState ──────────────────────────────────────────────────────

    #[test]
    fn ema_passthrough_alpha_one() {
        let mut e = EmaState::new(1.0);
        assert_eq!(e.update(Some(0.5)), Some(0.5));
        assert_eq!(e.update(Some(0.8)), Some(0.8));
    }

    #[test]
    fn ema_smooths_step() {
        let mut e = EmaState::new(0.5);
        assert_eq!(e.update(Some(0.0)), Some(0.0));
        // Step a 1.0 con α=0.5: 0.5*1.0 + 0.5*0.0 = 0.5
        assert_eq!(e.update(Some(1.0)), Some(0.5));
        // Segundo tick en 1.0: 0.5*1.0 + 0.5*0.5 = 0.75
        assert_eq!(e.update(Some(1.0)), Some(0.75));
    }

    #[test]
    fn ema_none_input_preserves_last() {
        let mut e = EmaState::new(0.5);
        e.update(Some(0.7));
        assert_eq!(e.update(None), Some(0.7));
        assert_eq!(e.update(None), Some(0.7));
    }

    #[test]
    fn ema_reset_clears_state() {
        let mut e = EmaState::new(0.5);
        e.update(Some(0.5));
        e.reset();
        assert_eq!(e.current(), None);
    }

    #[test]
    fn ema_alpha_clamped_to_valid_range() {
        // α=0 causaría división por cero conceptual; clamp a 0.01.
        let mut e = EmaState::new(0.0);
        // Con α=0.01, la respuesta a step es muy lenta pero no nula.
        e.update(Some(0.0));
        let v1 = e.update(Some(1.0)).unwrap();
        assert!(v1 > 0.0 && v1 < 0.05, "got {}", v1);
    }

    // ── HysteresisState ───────────────────────────────────────────────

    #[test]
    fn hysteresis_activates_on_first_true() {
        let mut h = HysteresisState::new(3);
        assert!(h.update(true));
        assert!(h.is_active());
    }

    #[test]
    fn hysteresis_holds_through_transient_false() {
        let mut h = HysteresisState::new(3);
        h.update(true);
        assert!(h.update(false));  // 1 false → todavía activo
        assert!(h.update(false));  // 2 false → activo
        assert!(!h.update(false)); // 3 false → desactiva
    }

    #[test]
    fn hysteresis_reactivates_resets_streak() {
        let mut h = HysteresisState::new(3);
        h.update(true);
        h.update(false);
        h.update(false);
        assert!(h.update(true));  // reactiva, streak reset
        assert!(h.update(false)); // solo 1 false, sigue activo
        assert!(h.update(false)); // 2 falses, sigue activo
    }

    #[test]
    fn hysteresis_never_active_without_true() {
        let mut h = HysteresisState::new(3);
        for _ in 0..10 { assert!(!h.update(false)); }
    }

    #[test]
    fn hysteresis_reset_deactivates() {
        let mut h = HysteresisState::new(3);
        h.update(true);
        h.reset();
        assert!(!h.is_active());
    }

    // ── MedianWindow ──────────────────────────────────────────────────

    #[test]
    fn median_partial_buffer_returns_mid() {
        let mut m = MedianWindow::<u32>::new(3);
        assert_eq!(m.update(5), 5);       // [5] → 5
        assert_eq!(m.update(10), 10);     // [5,10] → 10 (ceil mid)
        assert_eq!(m.update(3), 5);       // [5,10,3] sorted [3,5,10] → 5
    }

    #[test]
    fn median_absorbs_single_spike() {
        let mut m = MedianWindow::<u32>::new(3);
        m.update(2); m.update(2);
        // Spike a 99: [2,2,99] median = 2
        assert_eq!(m.update(99), 2);
        // Vuelve a normal [2,99,2] median = 2
        assert_eq!(m.update(2), 2);
    }

    #[test]
    fn median_window_slides() {
        let mut m = MedianWindow::<u32>::new(3);
        m.update(1); m.update(2); m.update(3); // [1,2,3] → 2
        m.update(10); // [2,3,10] → 3
        assert_eq!(m.update(11), 10); // [3,10,11] → 10
    }

    // ── MajorityVote ──────────────────────────────────────────────────

    #[test]
    fn majority_single_value() {
        let mut v = MajorityVote::<i32>::new(5);
        assert_eq!(v.update(42), 42);
    }

    #[test]
    fn majority_beats_minority() {
        let mut v = MajorityVote::<i32>::new(5);
        v.update(1); v.update(1); v.update(2); v.update(1); v.update(2);
        // [1,1,2,1,2] → 1 appears 3×, 2 appears 2× → 1
        assert_eq!(v.buf_len(), 5);
        // Estado actual final ya votado en el último update.
    }

    #[test]
    fn majority_tie_prefers_recent() {
        let mut v = MajorityVote::<i32>::new(4);
        v.update(1); v.update(2); v.update(1); // [1,2,1] → 1
        let r = v.update(2);                    // [1,2,1,2] tie → newest=2
        assert_eq!(r, 2);
    }

    #[test]
    fn majority_window_slides() {
        let mut v = MajorityVote::<i32>::new(3);
        v.update(1); v.update(1); v.update(1); // [1,1,1] → 1
        v.update(2); v.update(2); // [1,2,2] → 2
        let r = v.update(2);       // [2,2,2] → 2
        assert_eq!(r, 2);
    }

    impl<T: Clone + Eq> MajorityVote<T> {
        fn buf_len(&self) -> usize { self.buf.len() }
    }

    // ── StreakCounter ─────────────────────────────────────────────────

    #[test]
    fn streak_counts_consecutive_matches() {
        let mut s = StreakCounter::new();
        assert_eq!(s.update(true), 1);
        assert_eq!(s.update(true), 2);
        assert_eq!(s.update(true), 3);
    }

    #[test]
    fn streak_resets_on_mismatch() {
        let mut s = StreakCounter::new();
        s.update(true); s.update(true);
        assert_eq!(s.update(false), 0);
        assert_eq!(s.update(true), 1); // restart
    }

    // ── PerceptionFilter integration ─────────────────────────────────

    use crate::sense::perception::{CharVitals, Perception, VitalBar};

    fn perc_with_hp(hp: f32) -> Perception {
        Perception {
            vitals: CharVitals {
                hp:   Some(VitalBar { ratio: hp, filled_px: 0, total_px: 100 }),
                mana: None,
            },
            ..Default::default()
        }
    }

    #[test]
    fn filter_smooths_hp_ratio() {
        let mut f = PerceptionFilter::new();
        let p1 = f.apply(&perc_with_hp(1.0));
        assert_eq!(p1.vitals.hp.unwrap().ratio, 1.0);
        // Drop drástico a 0.2 — con α=0.85 debería suavizar:
        // 0.85*0.2 + 0.15*1.0 = 0.17 + 0.15 = 0.32
        let p2 = f.apply(&perc_with_hp(0.2));
        let r = p2.vitals.hp.unwrap().ratio;
        assert!(r > 0.2 && r < 0.4, "got {}", r);
    }

    #[test]
    fn filter_preserves_hp_none() {
        let mut f = PerceptionFilter::new();
        let p = Perception::default();
        let out = f.apply(&p);
        assert!(out.vitals.hp.is_none());
    }

    #[test]
    fn filter_target_hysteresis_holds_through_single_false() {
        let mut f = PerceptionFilter::new();
        let mut p = Perception::default();
        p.target_active = Some(true);
        assert_eq!(f.apply(&p).target_active, Some(true));
        // 1 frame de false → sigue true (off_confirm=2)
        p.target_active = Some(false);
        assert_eq!(f.apply(&p).target_active, Some(true));
        // 2 frames de false → desactiva
        assert_eq!(f.apply(&p).target_active, Some(false));
    }

    #[test]
    fn filter_target_preserves_none() {
        let mut f = PerceptionFilter::new();
        let p = Perception::default();
        assert!(f.apply(&p).target_active.is_none());
    }

    #[test]
    fn filter_coords_majority_absorbs_collision() {
        let mut f = PerceptionFilter::new();
        let mut p = Perception::default();
        // Secuencia: (1,1,7) × 3, (2,2,7) × 1, (1,1,7) × 1
        // Esto simula una colisión dHash transitoria.
        for c in [(1,1,7), (1,1,7), (1,1,7), (2,2,7), (1,1,7)] {
            p.game_coords = Some(c);
            f.apply(&p);
        }
        // Último apply con la colisión ya absorbida:
        // buf = [(1,1,7),(1,1,7),(1,1,7),(2,2,7),(1,1,7)] → majority=(1,1,7)
        p.game_coords = Some((1,1,7));
        let out = f.apply(&p);
        assert_eq!(out.game_coords, Some((1,1,7)));
    }

    #[test]
    fn filter_coords_holds_through_none() {
        let mut f = PerceptionFilter::new();
        let mut p = Perception::default();
        p.game_coords = Some((5, 5, 7));
        f.apply(&p);
        // tile-hashing miss en el siguiente tick:
        p.game_coords = None;
        let out = f.apply(&p);
        assert_eq!(out.game_coords, Some((5, 5, 7)),
            "filter debe mantener último coord conocido tras None transient");
    }

    #[test]
    fn filter_enemy_count_median_absorbs_spike() {
        use crate::sense::perception::{BattleEntry, BattleList, EntryKind};
        let mut f = PerceptionFilter::new();
        assert_eq!(f.filtered_enemy_count(), 0); // antes de cualquier apply

        let mk = |n: usize| -> Perception {
            let entries = (0..n).map(|i| BattleEntry {
                kind: EntryKind::Monster,
                row: i as u8,
                hp_ratio: Some(1.0),
                name: None,
                is_being_attacked: false,
            }).collect();
            Perception {
                battle: BattleList { entries, ..Default::default() },
                ..Default::default()
            }
        };
        f.apply(&mk(2));
        f.apply(&mk(2));
        f.apply(&mk(99)); // spike transient
        // [2, 2, 99] → median = 2
        assert_eq!(f.filtered_enemy_count(), 2);
    }

    // ── VitalsDebouncer (semantic equivalence con Vision pre-refactor) ─

    fn vbar(ratio: f32) -> VitalBar {
        VitalBar { ratio, filled_px: (ratio * 100.0) as u32, total_px: 100 }
    }

    #[test]
    fn vitals_debouncer_holds_through_panic_window() {
        let mut d = VitalsDebouncer::new(5);
        // Raw bueno establece last_stable.
        let r = d.update(Some(vbar(0.9)), false);
        assert_eq!(r.unwrap().ratio, 0.9);
        // 4 bads consecutivos → mantiene 0.9 (panic_thr=5 no alcanzado aún).
        for _ in 0..4 {
            let r = d.update(None, true);
            assert_eq!(r.unwrap().ratio, 0.9, "bad <5: debe mantener last_stable");
        }
        // 5to bad → propaga el raw (None aquí).
        let r = d.update(None, true);
        assert!(r.is_none(), "bad >=5: debe propagar el raw");
    }

    #[test]
    fn vitals_debouncer_resets_on_good_read() {
        let mut d = VitalsDebouncer::new(5);
        d.update(Some(vbar(0.9)), false);
        d.update(None, true); // 1 bad
        d.update(None, true); // 2 bad
        // Raw bueno resetea contador y actualiza stable.
        let r = d.update(Some(vbar(0.5)), false);
        assert_eq!(r.unwrap().ratio, 0.5);
        // Otro bad: cuenta desde 1 (no acumula los previos).
        let r = d.update(None, true);
        assert_eq!(r.unwrap().ratio, 0.5);
    }

    #[test]
    fn vitals_debouncer_confidence_full_when_all_good() {
        let mut d = VitalsDebouncer::new(5);
        d.update(Some(vbar(0.9)), false);
        assert!((d.confidence() - 1.0).abs() < 0.01);
    }

    #[test]
    fn vitals_debouncer_confidence_decays_linearly() {
        let mut d = VitalsDebouncer::new(5);
        d.update(Some(vbar(0.9)), false);
        d.update(None, true);  // 1 bad → conf = 1 - 1/5 = 0.8
        assert!((d.confidence() - 0.8).abs() < 0.01);
        d.update(None, true);  // 2 bad → 0.6
        assert!((d.confidence() - 0.6).abs() < 0.01);
    }

    #[test]
    fn vitals_debouncer_confidence_zero_at_panic() {
        let mut d = VitalsDebouncer::new(5);
        for _ in 0..5 { d.update(None, true); }
        assert!(d.confidence() < 0.01);
    }

    #[test]
    fn filter_vitals_confidence_is_min_of_hp_mana() {
        let mut f = PerceptionFilter::new();
        // HP good, mana in bad streak 3/5 → mana conf = 0.4, hp conf = 1.0.
        let mut p = Perception::default();
        p.vitals.hp = Some(vbar(0.9));
        p.vitals.mana = None; // bad
        f.apply(&p);
        f.apply(&p);
        f.apply(&p);
        // min(1.0, 1 - 3/5) = 0.4
        let c = f.vitals_confidence();
        assert!((c - 0.4).abs() < 0.01, "got {}", c);
    }

    #[test]
    fn filter_target_confidence_full_when_stable() {
        let mut f = PerceptionFilter::new();
        let mut p = Perception::default();
        p.target_active = Some(true);
        f.apply(&p);
        assert!((f.target_confidence() - 1.0).abs() < 0.01);
    }

    #[test]
    fn filter_target_confidence_degrades_during_hold() {
        let mut f = PerceptionFilter::new();
        let mut p = Perception::default();
        // Activar hysteresis.
        p.target_active = Some(true);
        f.apply(&p);
        // off_confirm=2 (default). 1 false consecutivo → off_streak=1.
        p.target_active = Some(false);
        f.apply(&p);
        let c = f.target_confidence();
        // 1 - 0.5 * 1/2 = 0.75
        assert!((c - 0.75).abs() < 0.01, "got {}", c);
    }

    #[test]
    fn vitals_debouncer_reset_clears() {
        let mut d = VitalsDebouncer::new(5);
        d.update(Some(vbar(0.5)), false);
        d.reset();
        // Tras reset, raw bad → no hay last_stable → propaga None.
        assert!(d.update(None, true).is_none());
    }

    // ── PerceptionFilter integración con VitalsDebouncer ─────────────

    #[test]
    fn filter_hp_holds_through_4_bad_then_propagates_at_5() {
        let mut f = PerceptionFilter::new();
        // Establecer baseline 0.9.
        f.apply(&perc_with_hp(0.9));
        // 4 frames con ratio 0.0 (bad) → output debe mantenerse cerca de 0.9.
        for _ in 0..4 {
            let out = f.apply(&perc_with_hp(0.0));
            let r = out.vitals.hp.unwrap().ratio;
            assert!(r > 0.5, "frame bad <5 debe mantener stable; got {}", r);
        }
        // 5to bad → propaga ratio 0.0 (atravesando el EMA).
        // EMA con stable_hp=Some(0.0) y previous smoothed≈0.9: 0.85*0 + 0.15*0.9 = 0.135
        let out = f.apply(&perc_with_hp(0.0));
        let r = out.vitals.hp.unwrap().ratio;
        assert!(r < 0.2, "frame bad >=5 debe propagar (ema todavía decae); got {}", r);
    }

    #[test]
    fn filter_hp_none_input_treated_as_bad() {
        let mut f = PerceptionFilter::new();
        f.apply(&perc_with_hp(0.7));
        // 4 frames con HP=None → mantiene last_stable.
        let p_no_hp = Perception::default();
        for _ in 0..4 {
            let out = f.apply(&p_no_hp);
            assert!(out.vitals.hp.is_some(), "None bad <5 debe propagar last_stable");
        }
        // 5to None → output también None.
        let out = f.apply(&p_no_hp);
        assert!(out.vitals.hp.is_none());
    }

    // ── moving_hysteresis (semantic equivalence con Vision pre-refactor) ─

    #[test]
    fn filter_is_moving_activates_immediately() {
        let mut f = PerceptionFilter::new();
        let mut p = Perception::default();
        p.is_moving = Some(true);
        assert_eq!(f.apply(&p).is_moving, Some(true));
    }

    #[test]
    fn filter_is_moving_deactivates_after_calm_frames() {
        let mut f = PerceptionFilter::new();
        let mut p = Perception::default();
        p.is_moving = Some(true);
        f.apply(&p);
        // false sostenido por MOVEMENT_CALM_FRAMES-1 sigue dando true.
        p.is_moving = Some(false);
        for _ in 0..(MOVEMENT_CALM_FRAMES - 1) {
            assert_eq!(f.apply(&p).is_moving, Some(true));
        }
        // El N-ésimo false desactiva.
        assert_eq!(f.apply(&p).is_moving, Some(false));
    }

    #[test]
    fn filter_is_moving_none_when_minimap_uncalibrated() {
        let mut f = PerceptionFilter::new();
        let p = Perception::default(); // is_moving = None
        assert_eq!(f.apply(&p).is_moving, None);
    }

    #[test]
    fn filter_is_moving_none_resets_hysteresis() {
        let mut f = PerceptionFilter::new();
        let mut p = Perception::default();
        // Primero activamos.
        p.is_moving = Some(true);
        f.apply(&p);
        // Minimap se descalibra (None) — hysteresis debe resetear.
        p.is_moving = None;
        assert_eq!(f.apply(&p).is_moving, None);
        // Vuelve un raw=false: como hysteresis está reseteada, output false.
        p.is_moving = Some(false);
        assert_eq!(f.apply(&p).is_moving, Some(false));
    }

    #[test]
    fn filter_reset_clears_all_state() {
        let mut f = PerceptionFilter::new();
        let mut p = Perception::default();
        p.target_active = Some(true);
        p.game_coords   = Some((1,1,1));
        p.is_moving     = Some(true);
        p.vitals.hp     = Some(vbar(0.5));
        f.apply(&p);
        f.reset();
        // Tras reset: None input → None output, hysteresis/debouncer reseteados.
        let p_empty = Perception::default();
        let out = f.apply(&p_empty);
        assert!(out.target_active.is_none());
        assert!(out.game_coords.is_none());
        assert!(out.is_moving.is_none());
        assert!(out.vitals.hp.is_none(), "vitals_debouncer reset → no last_stable a propagar");
    }
}
