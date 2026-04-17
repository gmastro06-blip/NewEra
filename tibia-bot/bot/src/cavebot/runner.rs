//! runner.rs — Ejecución del cavebot tick a tick.
//!
//! Mantiene el estado del iterador (step actual, timers, counters) y
//! dispatcha cada tick basado en el `StepKind` del step activo.
//!
//! ## Flujo de un tick
//!
//! 1. `BotLoop` construye un `TickContext` con HP/mana/kills/in_combat
//! 2. Llama `cavebot.tick(&mut ctx)` → `CavebotAction`
//! 3. Si el step actual es `Walk/Hotkey/Loot` → emit
//! 4. Si el step es `Wait/Stand` → None + actualizar timers
//! 5. Si el step es `Label/Goto/GotoIf` → avanzar/saltar sin emit (y re-ejecutar)
//!
//! ## Integración con el FSM
//!
//! El cavebot **no sabe nada** del FSM. Solo ejecuta steps y devuelve
//! `CavebotAction`. El loop decide qué hacer con el resultado.
//!
//! Cuando el FSM pasa a Fighting/Emergency, el loop **NO llama a tick()**
//! hasta que vuelva a Walking. Esto congela los timers del step actual,
//! que se restartearán al volver (ver `restart_current_step`).

use crate::cavebot::step::{Step, StepKind, StandUntil, VerifyCheck, VerifyFailAction};

/// Ticks consecutivos sin movimiento antes de declarar stuck y avanzar el step.
/// 60 ticks @ 30Hz = 2 segundos empujando contra una pared → abandonar dirección.
const STUCK_THRESHOLD_TICKS: u64 = 60;

/// Contexto pasado al cavebot cada tick para evaluar condiciones.
#[derive(Debug, Clone, Default)]
pub struct TickContext {
    /// Tick del game loop.
    pub tick: u64,
    /// HP ratio [0.0..1.0] o None si la visión no lo lee.
    pub hp_ratio: Option<f32>,
    /// Mana ratio [0.0..1.0] o None.
    pub mana_ratio: Option<f32>,
    /// Total de kills confirmados desde el inicio de la sesión.
    pub total_kills: u64,
    /// Ticks transcurridos dentro del step actual (0 al entrar).
    pub ticks_in_current_step: u64,
    /// ¿Hay combate en este frame? (battle list tiene entries).
    pub in_combat: bool,
    /// Último tick en el que el detector vio "actividad" (cambio en HP o en
    /// battle list). Usado por `SkipIfBlocked` para medir stuck.
    pub last_activity_tick: u64,
    /// Nombres de templates de UI visibles en el frame actual.
    /// Usado por `Condition::UiVisible` en GotoIf/Stand.
    pub ui_matches: Vec<String>,
    /// `Some(true)` = char se movió. `Some(false)` = sin movimiento.
    /// `None` = minimap no calibrado — stuck detection deshabilitado.
    pub is_moving: Option<bool>,
    /// Cantidad de enemigos (Monster) en el battle list.
    pub enemy_count: u32,
    /// Píxeles de sparkle de loot detectados en el viewport.
    pub loot_sparkles: u32,
    /// Centro del minimap en coordenadas del viewport (ajustado por anchor tracker).
    /// Reservado para uso futuro (spatial navigation).
    #[allow(dead_code)]
    pub minimap_center: Option<(i32, i32)>,
    /// Desplazamiento del minimap en píxeles: (dx, dy).
    /// +dx = derecha, +dy = abajo. `None` si no hubo movimiento o no calibrado.
    pub minimap_displacement: Option<(i32, i32)>,
    /// Coordenadas absolutas (x, y, z) del personaje por tile-hashing.
    /// `None` si no hay map index o no hubo match.
    pub game_coords: Option<(i32, i32, i32)>,
    /// Conteo de items detectados en inventario por template matching.
    /// Key = nombre del template (sin .png), value = número de slots que matchean.
    /// Vacío si inventory vision no está calibrada.
    pub inventory_counts: std::collections::HashMap<String, u32>,
    /// Suma de unidades por item leído via OCR del stack count (M1).
    /// Si los digit templates no están cargados, este map suele coincidir
    /// con `inventory_counts` (1 unit per slot).
    pub inventory_stacks: std::collections::HashMap<String, u32>,
}

/// State while the runner is polling the current step's postcondition.
/// Set when `advance()` is called on a step with `verify: Some(...)`.
/// Cleared by `do_advance()` when verify passes or on_fail is applied.
#[derive(Debug, Clone)]
struct VerifyingState {
    /// Tick when verify started (when the step would have advanced).
    started_tick: u64,
    /// Max ticks before timeout.
    timeout_ticks: u64,
}

/// Acción que el cavebot pide al BotLoop este tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CavebotAction {
    /// No emitir nada (waiting, standing, step resuelto internamente).
    Idle,
    /// Tap de una tecla (walk direccional, hotkey).
    KeyTap(u8),
    /// Click en coordenada del viewport (para loot).
    Click { vx: i32, vy: i32 },
    /// Right-click en coordenada del viewport (context menu de depot/items).
    RightClick { vx: i32, vy: i32 },
    /// Tipear una frase en el chat del juego (Fase D). El loop la encola en
    /// el typing buffer y la procesa carácter a carácter paceado.
    Say(String),
    /// El cavebot terminó (lista no-loop completada). El loop puede
    /// desactivarlo y caer a Idle del FSM.
    Finished,
    /// El cavebot detectó una condición inválida que no puede auto-resolver
    /// (e.g. char en piso equivocado tras un Node). El loop debe setear
    /// `shared_state.is_paused = true` con `safety_pause_reason = reason`.
    ///
    /// Previene cadenas de acciones inútiles (clicks fantasma en piso
    /// equivocado, drags imposibles) cuando la navegación rompe invariantes.
    SafetyPause { reason: String },
}

/// Parámetros tunables de navegación por nodos.
/// Todos tienen defaults sensibles; se pueden overridear desde `[cavebot]` en config.
#[derive(Debug, Clone)]
pub struct NodeTuning {
    pub pixels_per_tile:        i32,
    pub displacement_tolerance: i32,
    pub arrived_idle_ticks:     u32,
    pub reclick_idle_ticks:     u32,
    pub max_reclicks:           u8,
    pub fallback_stop_ticks:    u32,
    pub sensor_degraded_ticks:  u32,
    pub timeout_ticks:          u64,
    pub initial_click_wait:     u32,
    pub initial_click_retries:  u8,
}

impl Default for NodeTuning {
    fn default() -> Self {
        Self {
            pixels_per_tile:        2,
            displacement_tolerance: 4,
            arrived_idle_ticks:     10,
            reclick_idle_ticks:     60,
            max_reclicks:           3,
            fallback_stop_ticks:    90,
            sensor_degraded_ticks:  90,
            timeout_ticks:          900,
            initial_click_wait:     30,
            initial_click_retries:  2,
        }
    }
}

impl NodeTuning {
    /// Construye desde CavebotConfig, usando defaults para campos no especificados.
    pub fn from_config(cfg: &crate::config::CavebotConfig) -> Self {
        let d = Self::default();
        Self {
            pixels_per_tile:        cfg.pixels_per_tile.unwrap_or(d.pixels_per_tile),
            displacement_tolerance: cfg.displacement_tolerance.unwrap_or(d.displacement_tolerance),
            arrived_idle_ticks:     cfg.arrived_idle_ticks.unwrap_or(d.arrived_idle_ticks),
            reclick_idle_ticks:     cfg.reclick_idle_ticks.unwrap_or(d.reclick_idle_ticks),
            max_reclicks:           cfg.max_reclicks.unwrap_or(d.max_reclicks),
            timeout_ticks:          cfg.timeout_ticks.unwrap_or(d.timeout_ticks),
            ..d
        }
    }
}

/// El cavebot en sí — lista de steps + estado del iterador.
#[derive(Debug, Clone)]
pub struct Cavebot {
    /// Steps pre-resueltos (labels convertidos a índices).
    pub steps: Vec<Step>,
    /// `true` si al terminar la lista debe volver al inicio.
    pub loop_: bool,
    /// Nombre del hunt profile declarado en `[cavebot].hunt_profile` del TOML
    /// (si cargó correctamente). Expuesto en `/cavebot/status` para
    /// observability — ayuda a confirmar que el profile correcto está activo
    /// antes de una sesión live.
    pub hunt_profile: Option<String>,
    /// Index del step activo. `None` = terminado (solo relevante si `!loop_`).
    pub current: Option<usize>,
    /// Tick en el que se entró al step actual.
    started_tick: u64,
    /// Último tick en el que se emitió la tecla del step Walk actual.
    last_emit_tick: Option<u64>,
    /// Kills registrados al entrar al step actual (para `StandUntil::MobsKilled`
    /// y `KillsSinceLabel`).
    kills_at_step_start: u64,
    /// FPS del game loop para conversión ms↔ticks.
    fps: u32,
    /// Contador de retries internos del step Loot (para emitir N clicks).
    loot_clicks_done: u8,
    /// Índice de la frase actual en un NpcDialog. Se resetea al salir del step.
    npc_phrase_idx: usize,
    /// Tick en que se empezó la espera post-tipeo del NpcDialog (si aplica).
    /// `None` = aún tipeando frases.
    npc_wait_start: Option<u64>,
    // ── Estado de OpenNpcTrade step ──────────────────────────────────
    /// Fase del OpenNpcTrade: 0=tipeando greeting_phrases, 1=wait_button_ms,
    /// 2=click botón bag, 3=wait 500ms post-click.
    open_trade_phase: u8,
    /// Índice de la frase actual en greeting_phrases.
    open_trade_phrase_idx: usize,
    /// Tick en que se empezó la fase actual (wait o post-click).
    open_trade_phase_start: u64,
    /// Ticks consecutivos donde `ctx.is_moving == false` durante un Walk.
    /// Se resetea en 0 al entrar a cada step. Si supera STUCK_THRESHOLD_TICKS
    /// durante un Walk activo, el bot avanza al siguiente step (da por perdida
    /// la dirección actual — mejor continuar la ruta que quedarse empujando).
    stuck_walk_ticks: u64,
    /// Flag lazy: advance/jump_to no tienen ctx, así que marcamos que el
    /// próximo tick de Stand debe capturar `kills_at_step_start = ctx.total_kills`.
    needs_kills_baseline: bool,
    /// Coordenadas del último Node procesado (para calcular offset en minimap).
    /// Se preserva entre advance/jump_to — es acumulativo.
    prev_node: Option<(i32, i32, i32)>,
    /// Fase de ejecución del Node actual: 0=setup+click, 1=esperando llegada.
    node_phase: u8,
    /// Tick de inicio de la fase actual (para timeout).
    node_phase_start_tick: u64,
    /// true si is_moving fue Some(true) en algún momento durante fase 1.
    node_saw_moving: bool,
    /// Accumulated character displacement (negado del camera shift).
    /// +dx = personaje se movió a la derecha, +dy = abajo.
    node_accum_dx: i32,
    node_accum_dy: i32,
    /// Expected character displacement para este Node.
    node_expect_dx: i32,
    node_expect_dy: i32,
    /// true si algún displacement != 0 fue observado. Si false, fallback a is_moving.
    node_saw_displacement: bool,
    /// Ticks consecutivos con is_moving == Some(false).
    node_idle_ticks: u32,
    /// Ticks consecutivos con is_moving=None (sensor-degraded).
    node_none_moving_ticks: u32,
    /// Re-clicks emitidos para este node (capped a max_reclicks).
    node_reclick_count: u8,
    /// Retries del click inicial de phase 0 (F6).
    node_initial_click_retries: u8,
    /// Ticks esperando que tile-hashing produzca un `game_coords` válido
    /// para semillar `prev_node` en el primer Node después de activar el
    /// cavebot. Ver `tick_node` Fase 0 para el fallback tras timeout.
    node_seed_wait: u64,
    /// Parámetros tunables de navegación.
    tuning: NodeTuning,
    // ── Estado de Deposit step ───────────────────────────────────────
    /// 0=right-click chest, 1=wait menu, 2=left-click stow, 3=wait process
    deposit_phase: u8,
    /// Tick en el que se entró a la fase actual de deposit.
    deposit_phase_start: u64,
    // ── Estado de BuyItem step ───────────────────────────────────────
    //
    // Legacy flow (sin amount fields):
    //   phase 0 = select item click
    //   phase 1 = loop N clicks de confirm (buy_clicks_done)
    //
    // Amount flow (con amount_vx/vy):
    //   phase 0 = click item (select)
    //   phase 1 = wait 200ms → click amount field (focus)
    //   phase 2 = wait 150ms → entrar loop de dígitos
    //   phase 3 = tipear un dígito (incrementa buy_amount_digit_idx)
    //   phase 4 = wait spacing_ms/2 entre dígitos (o post-last 150ms)
    //   phase 5 = click confirm (1 solo)
    //   phase 6 = wait spacing_ms post-confirm → advance
    //
    /// Fase del flujo. Semántica distinta en legacy vs amount (ver arriba).
    buy_phase: u8,
    /// Clicks de confirm ya emitidos (solo usado en legacy flow).
    buy_clicks_done: u32,
    /// Tick del último click (para spacing en legacy) o del último cambio
    /// de fase (para timers en amount flow).
    buy_last_click_tick: u64,
    /// Índice del próximo dígito a tipear en el amount flow. Se resetea
    /// al entrar al step y avanza cada vez que se emite un KeyTap de dígito.
    buy_amount_digit_idx: usize,
    // ── Estado de StowAllItems step (iterativo per-item) ────────────
    /// 0=right-click slot 0, 1=wait menu, 2=click "Stow all items of this type", 3=wait process + next iter
    stow_phase: u8,
    /// Tick de entrada a la fase actual.
    stow_phase_start: u64,
    /// Iteraciones completadas (stow clicks emitidos).
    stow_iterations_done: u8,
    /// Snapshot del `inventory_counts` al entrar al step (iter 1, phase 0).
    /// Usado por la detección de "stash full" para comparar cambios entre
    /// iteraciones. `None` = baseline todavía no capturado (reset al entrar
    /// a un step nuevo, tras completar o tras abortar).
    stow_baseline_counts: Option<std::collections::HashMap<String, u32>>,
    /// Contador de iteraciones consecutivas sin cambio en `inventory_counts`
    /// vs el baseline. Si alcanza 2 → emit SafetyPause con reason explícita
    /// (stash lleno o no hay más stackables). Reset cuando hay drop en
    /// cualquier item del baseline.
    stow_stale_iters: u8,
    // ── Estado de TypeInField step ───────────────────────────────────
    /// Fase del TypeInField:
    ///   0 = click al field_vx/vy + transicionar a wait
    ///   1 = wait wait_after_click_ms (focus del input)
    ///   2 = loop: tap HID del char actual + wait char_spacing_ms
    ///   3 = wait wait_after_type_ms (la UI aplica el filtro)
    ///   4 = advance
    type_field_phase: u8,
    /// Índice del próximo caracter a tipear dentro de `text`. Avanza al
    /// emitir cada `KeyTap` y también al skipear chars no mapeables.
    type_field_char_idx: usize,
    /// Tick en el que se entró a la fase actual, usado para medir los
    /// timers de `wait_after_click_ms`, `char_spacing_ms` y
    /// `wait_after_type_ms`.
    type_field_phase_start: u64,
    /// Set to Some when the runner is polling a postcondition. While Some,
    /// tick() evaluates the check instead of dispatching the step.
    verifying: Option<VerifyingState>,
    /// Snapshot of ctx.inventory_stacks at step entry. Used by
    /// VerifyCheck::InventoryDelta. Captured lazily when `needs_inventory_snapshot`
    /// is true.
    inventory_at_step_start: std::collections::HashMap<String, u32>,
    /// Set to true when advance/jump_to transitions to a new step. The top
    /// of tick() snapshots the current inventory stacks lazily (it needs a
    /// ctx reference).
    needs_inventory_snapshot: bool,
}

impl Cavebot {
    /// Crea un cavebot con tuning por defecto (convenience para tests).
    #[allow(dead_code)]
    pub fn new(steps: Vec<Step>, loop_: bool, fps: u32) -> Self {
        Self::with_tuning(steps, loop_, fps, NodeTuning::default())
    }

    /// Crea un cavebot con tuning custom (desde config).
    pub fn with_tuning(steps: Vec<Step>, loop_: bool, fps: u32, tuning: NodeTuning) -> Self {
        Self::with_tuning_and_profile(steps, loop_, fps, tuning, None)
    }

    /// Versión completa de `with_tuning` que también acepta el nombre del
    /// hunt profile (si el TOML lo declaró). Usada por el parser; los tests
    /// pueden seguir usando `with_tuning` sin profile.
    pub fn with_tuning_and_profile(
        steps:        Vec<Step>,
        loop_:        bool,
        fps:          u32,
        tuning:       NodeTuning,
        hunt_profile: Option<String>,
    ) -> Self {
        let current = if steps.is_empty() { None } else { Some(0) };
        Self {
            steps,
            loop_,
            hunt_profile,
            current,
            started_tick: 0,
            last_emit_tick: None,
            kills_at_step_start: 0,
            fps,
            loot_clicks_done: 0,
            npc_phrase_idx: 0,
            npc_wait_start: None,
            open_trade_phase: 0,
            open_trade_phrase_idx: 0,
            open_trade_phase_start: 0,
            stuck_walk_ticks: 0,
            needs_kills_baseline: true,
            prev_node: None,
            node_phase: 0,
            node_phase_start_tick: 0,
            node_saw_moving: false,
            node_accum_dx: 0,
            node_accum_dy: 0,
            node_expect_dx: 0,
            node_expect_dy: 0,
            node_saw_displacement: false,
            node_idle_ticks: 0,
            node_none_moving_ticks: 0,
            node_reclick_count: 0,
            node_initial_click_retries: 0,
            node_seed_wait: 0,
            tuning,
            deposit_phase: 0,
            deposit_phase_start: 0,
            stow_phase: 0,
            stow_phase_start: 0,
            stow_iterations_done: 0,
            stow_baseline_counts: None,
            stow_stale_iters: 0,
            buy_phase: 0,
            buy_clicks_done: 0,
            buy_last_click_tick: 0,
            buy_amount_digit_idx: 0,
            type_field_phase: 0,
            type_field_char_idx: 0,
            type_field_phase_start: 0,
            verifying: None,
            inventory_at_step_start: std::collections::HashMap::new(),
            needs_inventory_snapshot: true,
        }
    }

    /// Snapshot ligero para exponer en HTTP `/cavebot/status`.
    pub fn snapshot(&self, enabled: bool) -> crate::core::state::CavebotSnapshot {
        let (label, kind) = match self.current {
            Some(idx) => {
                let s = &self.steps[idx];
                let kind_str = format!("{:?}", s.kind);
                let kind_short = kind_str.split('{').next().unwrap_or("").trim().to_string();
                (s.label.clone(), kind_short)
            }
            None => (None, String::new()),
        };
        crate::core::state::CavebotSnapshot {
            loaded:        !self.steps.is_empty(),
            enabled,
            total_steps:   self.steps.len(),
            current_index: self.current,
            current_label: label,
            current_kind:  kind,
            loop_:         self.loop_,
            hunt_profile:  self.hunt_profile.clone(),
            verifying:     self.verifying.is_some(),
        }
    }

    /// ¿El cavebot tiene steps cargados y un step activo?
    pub fn is_running(&self) -> bool {
        self.current.is_some()
    }

    /// Ejecuta un tick del cavebot con el contexto dado.
    /// Devuelve la acción a dispatchar.
    ///
    /// Este método es idempotente si se le pasa el mismo ctx varias veces,
    /// EXCEPTO cuando avanza el step (entonces mutates).
    pub fn tick(&mut self, ctx: &mut TickContext) -> CavebotAction {
        // Lazy snapshot at step entry (for VerifyCheck::InventoryDelta).
        if self.needs_inventory_snapshot {
            self.inventory_at_step_start = ctx.inventory_stacks.clone();
            self.needs_inventory_snapshot = false;
        }

        // Protección contra loops infinitos de Goto/Label/GotoIf sin Wait/Walk.
        // Si más de N jumps ocurren en un mismo tick, pausamos.
        let mut max_iters = 64;
        loop {
            if max_iters == 0 {
                tracing::warn!("Cavebot: más de 64 saltos en un tick — posible loop infinito sin steps emisores. Pausando.");
                self.current = None;
                return CavebotAction::Idle;
            }
            max_iters -= 1;

            let Some(idx) = self.current else {
                return CavebotAction::Finished;
            };

            // Verify intercept — if we're polling a postcondition, evaluate it.
            if let Some(verifying) = self.verifying.clone() {
                // Safety: if verifying is Some, the current step MUST have a verify clause.
                let verify = match self.steps[idx].verify.clone() {
                    Some(v) => v,
                    None => {
                        tracing::error!(
                            "Cavebot step[{}]: verifying=Some but step.verify=None. Clearing.",
                            idx
                        );
                        self.verifying = None;
                        continue;
                    }
                };
                let elapsed = ctx.tick.saturating_sub(verifying.started_tick);
                if self.evaluate_verify(&verify.check, ctx) {
                    let elapsed_ms = (elapsed as u64) * 1000 / self.fps as u64;
                    tracing::info!(
                        "Cavebot step[{}]={:?}: verify PASS in {}ms",
                        idx, self.steps[idx].label, elapsed_ms
                    );
                    self.verifying = None;
                    self.do_advance(ctx.tick);
                    continue;
                }
                if elapsed >= verifying.timeout_ticks {
                    let label = self.steps[idx].label.clone();
                    tracing::warn!(
                        "Cavebot step[{}]={:?}: verify TIMEOUT ({}ms), on_fail={:?}",
                        idx, label, verify.timeout_ms, verify.on_fail
                    );
                    match verify.on_fail {
                        VerifyFailAction::SafetyPause => {
                            self.verifying = None;
                            let reason = format!(
                                "verify_failed: step[{}]={:?} check={:?} timeout={}ms",
                                idx, label, verify.check, verify.timeout_ms
                            );
                            return CavebotAction::SafetyPause { reason };
                        }
                        VerifyFailAction::Advance => {
                            self.verifying = None;
                            self.do_advance(ctx.tick);
                            continue;
                        }
                        VerifyFailAction::GotoLabel { target_idx, .. } => {
                            self.verifying = None;
                            self.jump_to(target_idx, ctx.tick);
                            continue;
                        }
                    }
                }
                // Still waiting — emit Idle.
                return CavebotAction::Idle;
            }

            // Clonamos el kind porque algunas ramas mutan self (advance).
            let kind = self.steps[idx].kind.clone();
            let ticks_in_step = ctx.tick.saturating_sub(self.started_tick);
            ctx.ticks_in_current_step = ticks_in_step;

            match kind {
                // ── Walk ───────────────────────────────────────────────
                StepKind::Walk { hidcode, duration_ms, interval_ms } => {
                    match self.tick_walk(ctx, idx, ticks_in_step, hidcode, duration_ms, interval_ms, true) {
                        Some(action) => return action,
                        None => { self.advance(ctx.tick); continue; }
                    }
                }

                // ── Wait ───────────────────────────────────────────────
                StepKind::Wait { duration_ms } => {
                    let duration_ticks = ms_to_ticks(duration_ms, self.fps);
                    if ticks_in_step >= duration_ticks {
                        self.advance(ctx.tick);
                        continue;
                    }
                    return CavebotAction::Idle;
                }

                // ── Hotkey ─────────────────────────────────────────────
                StepKind::Hotkey { hidcode } => {
                    // Emite una vez y avanza.
                    self.advance(ctx.tick);
                    return CavebotAction::KeyTap(hidcode);
                }

                // ── Stand ──────────────────────────────────────────────
                StepKind::Stand { until, max_wait_ms } => {
                    // Lazy init: capturar baseline de kills al entrar al step.
                    if self.needs_kills_baseline {
                        self.kills_at_step_start = ctx.total_kills;
                        self.needs_kills_baseline = false;
                    }
                    let max_ticks = ms_to_ticks(max_wait_ms, self.fps);
                    let timeout = max_wait_ms > 0 && ticks_in_step >= max_ticks;
                    let done = match &until {
                        StandUntil::MobsKilled(n) => {
                            ctx.total_kills.saturating_sub(self.kills_at_step_start) >= *n as u64
                        }
                        StandUntil::HpFull => ctx.hp_ratio.map(|r| r >= 0.95).unwrap_or(false),
                        StandUntil::ManaFull => ctx.mana_ratio.map(|r| r >= 0.95).unwrap_or(false),
                        StandUntil::TimerMs(ms) => {
                            let t = ms_to_ticks(*ms, self.fps);
                            ticks_in_step >= t
                        }
                        StandUntil::NoCombat => !ctx.in_combat,
                        StandUntil::EnemiesGte(n) => ctx.enemy_count >= *n,
                        StandUntil::ReachedCoord(x, y, z) => {
                            ctx.game_coords.map(|(gx, gy, gz)| gx == *x && gy == *y && gz == *z).unwrap_or(false)
                        }
                    };
                    if done || timeout {
                        if timeout && !done {
                            tracing::warn!(
                                "Cavebot Stand timeout ({}ms) sin cumplir {:?}", max_wait_ms, until
                            );
                        }
                        self.advance(ctx.tick);
                        continue;
                    }
                    return CavebotAction::Idle;
                }

                // ── Label ──────────────────────────────────────────────
                StepKind::Label => {
                    // Marcador — no hace nada, avanza.
                    self.advance(ctx.tick);
                    continue;
                }

                // ── Goto ───────────────────────────────────────────────
                StepKind::Goto { target_idx, .. } => {
                    self.jump_to(target_idx, ctx.tick);
                    continue;
                }

                // ── GotoIf ─────────────────────────────────────────────
                StepKind::GotoIf { target_idx, condition, .. } => {
                    if condition.eval(ctx) {
                        self.jump_to(target_idx, ctx.tick);
                    } else {
                        self.advance(ctx.tick);
                    }
                    continue;
                }

                // ── Loot ───────────────────────────────────────────────
                StepKind::Loot { vx, vy, retry_count } => {
                    if self.loot_clicks_done >= retry_count {
                        self.loot_clicks_done = 0;
                        self.advance(ctx.tick);
                        continue;
                    }
                    // Emitimos un click cada ~6 ticks (200ms) para que el
                    // cliente tenga tiempo de procesar y abrir corpse window.
                    let last = self.last_emit_tick.unwrap_or(0);
                    let ready = self.loot_clicks_done == 0
                        || ctx.tick.saturating_sub(last) >= 6;
                    if ready {
                        self.last_emit_tick = Some(ctx.tick);
                        self.loot_clicks_done += 1;
                        return CavebotAction::Click { vx, vy };
                    }
                    return CavebotAction::Idle;
                }

                // ── NpcDialog ──────────────────────────────────────────
                StepKind::NpcDialog { phrases, wait_prompt_ms } => {
                    // Si aún quedan frases por emitir, devolver la próxima.
                    if self.npc_phrase_idx < phrases.len() {
                        let phrase = phrases[self.npc_phrase_idx].clone();
                        self.npc_phrase_idx += 1;
                        return CavebotAction::Say(phrase);
                    }
                    // Todas las frases encoladas — esperar la ventana de prompt.
                    let wait_ticks = ms_to_ticks(wait_prompt_ms, self.fps);
                    match self.npc_wait_start {
                        None => {
                            self.npc_wait_start = Some(ctx.tick);
                            if wait_ticks == 0 {
                                // Sin wait — avanzar inmediatamente.
                                self.advance(ctx.tick);
                                continue;
                            }
                            return CavebotAction::Idle;
                        }
                        Some(start) => {
                            if ctx.tick.saturating_sub(start) >= wait_ticks {
                                self.advance(ctx.tick);
                                continue;
                            }
                            return CavebotAction::Idle;
                        }
                    }
                }

                // ── OpenNpcTrade ───────────────────────────────────────
                //
                // Saludo via chat + click en el botón de bag del greeting
                // window. Fases:
                //   0: tipeando `greeting_phrases` (una por tick vía Say)
                //   1: wait `wait_button_ms` para que renderice el greeting
                //   2: emitir Click en (bag_button_vx, bag_button_vy)
                //   3: wait ~500ms post-click para que abra la trade window
                //
                // Usado por NPCs de Tibia 12 cuyo greeting expone un icono
                // de bag (alternativa a decir "trade" / "potions" / etc).
                StepKind::OpenNpcTrade {
                    greeting_phrases, bag_button_vx, bag_button_vy, wait_button_ms,
                } => {
                    // Ticks de espera post-click — hardcoded 500ms para que
                    // la trade window termine de renderizarse antes del
                    // siguiente step (típicamente un BuyItem).
                    const POST_CLICK_WAIT_MS: u64 = 500;
                    match self.open_trade_phase {
                        0 => {
                            // Fase 0: tipear greeting_phrases una por tick.
                            if self.open_trade_phrase_idx < greeting_phrases.len() {
                                let phrase = greeting_phrases[self.open_trade_phrase_idx].clone();
                                self.open_trade_phrase_idx += 1;
                                return CavebotAction::Say(phrase);
                            }
                            // Todas las frases emitidas → pasar a fase 1.
                            self.open_trade_phase = 1;
                            self.open_trade_phase_start = ctx.tick;
                            tracing::info!(
                                "OpenNpcTrade[{}] phase 1: esperando {}ms para render del greeting window",
                                idx, wait_button_ms
                            );
                            // Si wait_button_ms == 0, fall-through al próximo
                            // tick evitaría esperar — devolvemos Idle para
                            // chequear el timer en el siguiente tick.
                            return CavebotAction::Idle;
                        }
                        1 => {
                            // Fase 1: esperar wait_button_ms para que Tibia
                            // renderice el greeting window con el bag button.
                            let wait_ticks = ms_to_ticks(wait_button_ms, self.fps);
                            if ctx.tick.saturating_sub(self.open_trade_phase_start) >= wait_ticks {
                                self.open_trade_phase = 2;
                                self.open_trade_phase_start = ctx.tick;
                                tracing::info!(
                                    "OpenNpcTrade[{}] phase 2: click bag button at ({}, {}). \
                                     If wrong position, edit script: bag_button_vx={}, bag_button_vy={}",
                                    idx, bag_button_vx, bag_button_vy, bag_button_vx, bag_button_vy
                                );
                                return CavebotAction::Click {
                                    vx: bag_button_vx, vy: bag_button_vy,
                                };
                            }
                            return CavebotAction::Idle;
                        }
                        2 => {
                            // Fase 2: click emitido, esperar POST_CLICK_WAIT_MS
                            // para que la trade window abra antes de avanzar.
                            let wait_ticks = ms_to_ticks(POST_CLICK_WAIT_MS, self.fps);
                            if ctx.tick.saturating_sub(self.open_trade_phase_start) >= wait_ticks {
                                tracing::info!(
                                    "OpenNpcTrade[{}] complete, advancing", idx
                                );
                                self.advance(ctx.tick);
                                continue;
                            }
                            return CavebotAction::Idle;
                        }
                        _ => {
                            tracing::warn!(
                                "OpenNpcTrade[{}] invalid phase {}, resetting",
                                idx, self.open_trade_phase
                            );
                            self.open_trade_phase = 0;
                            self.open_trade_phrase_idx = 0;
                            self.advance(ctx.tick);
                            continue;
                        }
                    }
                }

                // ── Rope ───────────────────────────────────────────────
                StepKind::Rope { hidcode } => {
                    self.advance(ctx.tick);
                    return CavebotAction::KeyTap(hidcode);
                }

                // ── Ladder ────────────────────────────────────────────
                StepKind::Ladder { vx, vy } => {
                    self.advance(ctx.tick);
                    return CavebotAction::Click { vx, vy };
                }

                // ── Node ───────────────────────────────────────────────
                StepKind::Node { x, y, z, max_wait_ms } => {
                    match self.tick_node(ctx, ticks_in_step, x, y, z, max_wait_ms) {
                        Some(action) => return action,
                        None => { self.advance(ctx.tick); continue; }
                    }
                }

                // ── Deposit (right-click → wait → left-click stow) ────
                StepKind::Deposit { chest_vx, chest_vy, stow_vx, stow_vy, menu_wait_ms, process_ms } => {
                    let menu_ticks = ms_to_ticks(menu_wait_ms, self.fps);
                    let process_ticks = ms_to_ticks(process_ms, self.fps);
                    match self.deposit_phase {
                        0 => {
                            // Phase 0: right-click chest. Logear coords para calibración.
                            tracing::info!(
                                "Deposit[{}] phase 0: right-click chest at ({}, {}). If wrong position, edit script: chest_vx={}, chest_vy={}",
                                idx, chest_vx, chest_vy, chest_vx, chest_vy
                            );
                            self.deposit_phase = 1;
                            self.deposit_phase_start = ctx.tick;
                            return CavebotAction::RightClick { vx: chest_vx, vy: chest_vy };
                        }
                        1 => {
                            // Phase 1: esperar menu.
                            if ctx.tick.saturating_sub(self.deposit_phase_start) >= menu_ticks {
                                tracing::info!(
                                    "Deposit[{}] phase 2: click 'Stow all' at ({}, {}). If wrong position, edit script: stow_vx={}, stow_vy={}",
                                    idx, stow_vx, stow_vy, stow_vx, stow_vy
                                );
                                self.deposit_phase = 2;
                                self.deposit_phase_start = ctx.tick;
                                return CavebotAction::Click { vx: stow_vx, vy: stow_vy };
                            }
                            return CavebotAction::Idle;
                        }
                        2 => {
                            // Phase 2: wait process.
                            if ctx.tick.saturating_sub(self.deposit_phase_start) >= process_ticks {
                                tracing::info!("Deposit[{}] complete, advancing", idx);
                                self.deposit_phase = 0;
                                self.advance(ctx.tick);
                                continue;
                            }
                            return CavebotAction::Idle;
                        }
                        _ => {
                            tracing::warn!("Deposit[{}] invalid phase {}, resetting", idx, self.deposit_phase);
                            self.deposit_phase = 0;
                            self.advance(ctx.tick);
                            continue;
                        }
                    }
                }

                // ── StowAllItems (iterativo per-item, Tibia 12 Supply Stash)
                //
                // Por cada iteración:
                //   phase 0: right-click slot 0 del bag
                //   phase 1: wait menu_wait_ms → click en "Stow all items of this type"
                //   phase 2: wait stow_process_ms → incrementa iter, vuelve a phase 0
                //
                // Termina cuando stow_iterations_done >= max_iterations.
                // Si los items del bag se acaban antes, los clicks emitidos no
                // tienen efecto (menu sin opción Stow para non-stackables), pero
                // no causan daño.
                //
                // Ver step.rs StepKind::StowAllItems para calibración.
                StepKind::StowAllItems {
                    slot_vx, slot_vy,
                    menu_offset_x, menu_offset_y,
                    menu_wait_ms, stow_process_ms, max_iterations,
                } => {
                    // Exit condition: ya hicimos todas las iteraciones.
                    if self.stow_iterations_done >= max_iterations {
                        tracing::info!(
                            "StowAllItems[{}] complete after {} iterations, advancing",
                            idx, self.stow_iterations_done
                        );
                        self.stow_phase = 0;
                        self.stow_iterations_done = 0;
                        self.stow_baseline_counts = None;
                        self.stow_stale_iters = 0;
                        self.advance(ctx.tick);
                        continue;
                    }

                    let menu_ticks = ms_to_ticks(menu_wait_ms, self.fps);
                    let process_ticks = ms_to_ticks(stow_process_ms, self.fps);
                    match self.stow_phase {
                        0 => {
                            // Phase 0: right-click al slot 0 del bag.
                            // Al entrar al step (iter 1, baseline aún no capturado),
                            // snapshot de inventory_counts para la detección de
                            // "stash lleno" (Task 2.3). Solo 1 clone por step.
                            if self.stow_iterations_done == 0 && self.stow_baseline_counts.is_none() {
                                self.stow_baseline_counts = Some(ctx.inventory_counts.clone());
                                self.stow_stale_iters = 0;
                            }
                            tracing::info!(
                                "StowAllItems[{}] iter {}/{}: right-click bag slot at ({}, {})",
                                idx, self.stow_iterations_done + 1, max_iterations, slot_vx, slot_vy
                            );
                            self.stow_phase = 1;
                            self.stow_phase_start = ctx.tick;
                            return CavebotAction::RightClick { vx: slot_vx, vy: slot_vy };
                        }
                        1 => {
                            // Phase 1: esperar menu → click "Stow all items of this type".
                            if ctx.tick.saturating_sub(self.stow_phase_start) >= menu_ticks {
                                let menu_vx = slot_vx + menu_offset_x;
                                let menu_vy = slot_vy + menu_offset_y;
                                tracing::info!(
                                    "StowAllItems[{}] iter {}: click 'Stow all items of this type' at ({}, {})",
                                    idx, self.stow_iterations_done + 1, menu_vx, menu_vy
                                );
                                self.stow_phase = 2;
                                self.stow_phase_start = ctx.tick;
                                return CavebotAction::Click { vx: menu_vx, vy: menu_vy };
                            }
                            return CavebotAction::Idle;
                        }
                        2 => {
                            // Phase 2: wait process → siguiente iteración.
                            if ctx.tick.saturating_sub(self.stow_phase_start) >= process_ticks {
                                self.stow_iterations_done += 1;

                                // Stash-full detection (Task 2.3): comparar el
                                // inventory actual contra el baseline. Si NINGÚN
                                // item del baseline bajó su count, esta iter no
                                // tuvo efecto. Tras 2 iters stale consecutivos,
                                // emitir SafetyPause.
                                if let Some(baseline) = self.stow_baseline_counts.as_ref() {
                                    let any_drop = baseline.iter().any(|(name, &base_count)| {
                                        let cur = ctx.inventory_counts.get(name).copied().unwrap_or(0);
                                        cur < base_count
                                    });
                                    if any_drop {
                                        // Progreso real → reset stale + refrescar baseline
                                        // al estado actual para la próxima iter.
                                        self.stow_stale_iters = 0;
                                        self.stow_baseline_counts = Some(ctx.inventory_counts.clone());
                                    } else {
                                        self.stow_stale_iters = self.stow_stale_iters.saturating_add(1);
                                        if self.stow_stale_iters >= 2 {
                                            let reason = format!(
                                                "stow:stash_full_or_no_stackables: {} iters sin cambio en inventory. \
                                                 baseline={} current={} — revisar si Supply Stash está lleno o \
                                                 si el bag no tiene más items stackables.",
                                                self.stow_stale_iters,
                                                fmt_counts_summary(baseline),
                                                fmt_counts_summary(&ctx.inventory_counts),
                                            );
                                            tracing::error!("Cavebot: {}", reason);
                                            self.stow_phase = 0;
                                            self.stow_iterations_done = 0;
                                            self.stow_baseline_counts = None;
                                            self.stow_stale_iters = 0;
                                            return CavebotAction::SafetyPause { reason };
                                        }
                                    }
                                }

                                self.stow_phase = 0;
                                // NO advance — volvemos a phase 0 para el siguiente iter.
                                // La check de max_iterations arriba controla cuándo salir.
                                continue;
                            }
                            return CavebotAction::Idle;
                        }
                        _ => {
                            tracing::warn!("StowAllItems[{}] invalid phase {}, resetting", idx, self.stow_phase);
                            self.stow_phase = 0;
                            self.stow_iterations_done = 0;
                            self.stow_baseline_counts = None;
                            self.stow_stale_iters = 0;
                            self.advance(ctx.tick);
                            continue;
                        }
                    }
                }

                // ── BuyItem (dos flujos según amount_vx/vy) ──────────
                //
                // Ver `StepKind::BuyItem` en step.rs para la semántica
                // completa de cada fase. Resumen:
                //
                // Legacy (amount_* = None):
                //   phase 0 = click item → phase 1
                //   phase 1 = loop N clicks confirm con spacing_ms
                //
                // Amount flow (ambos amount_* = Some):
                //   phase 0 = click item → phase 1 (+ 200ms)
                //   phase 1 = wait 200ms → click amount field → phase 2 (+ 150ms)
                //   phase 2 = wait 150ms → phase 3 (tipear dígitos)
                //   phase 3 = emitir KeyTap del dígito actual → phase 4
                //   phase 4 = wait spacing_ms/2 entre dígitos, o 150ms post-last
                //   phase 5 = click confirm (1 sola vez) → phase 6
                //   phase 6 = wait spacing_ms → advance
                StepKind::BuyItem {
                    item_vx, item_vy,
                    amount_vx, amount_vy,
                    confirm_vx, confirm_vy,
                    quantity, spacing_ms,
                } => {
                    let spacing_ticks = ms_to_ticks(spacing_ms, self.fps).max(1);
                    // Waits hardcoded del flujo Amount (ver docs en step.rs).
                    const POST_ITEM_CLICK_MS: u64 = 200;
                    const POST_AMOUNT_CLICK_MS: u64 = 150;
                    const POST_DIGITS_MS: u64 = 150;

                    // Decidir el flujo en base a si ambos amount_* están Some.
                    // Un solo Some ya fue rechazado por el parser con error
                    // explícito, así que aquí es un match binario limpio.
                    let use_amount_flow = amount_vx.is_some() && amount_vy.is_some();

                    if !use_amount_flow {
                        // ── Flujo legacy: N clicks de confirm ─────────
                        match self.buy_phase {
                            0 => {
                                tracing::info!(
                                    "BuyItem[{}] legacy phase 0: select item at ({}, {}), quantity={}. If wrong position, edit script: item_vx={}, item_vy={}",
                                    idx, item_vx, item_vy, quantity, item_vx, item_vy
                                );
                                self.buy_phase = 1;
                                self.buy_last_click_tick = ctx.tick;
                                self.buy_clicks_done = 0;
                                return CavebotAction::Click { vx: item_vx, vy: item_vy };
                            }
                            1 => {
                                if self.buy_clicks_done >= quantity {
                                    tracing::info!(
                                        "BuyItem[{}] legacy complete: {} clicks done, advancing",
                                        idx, self.buy_clicks_done
                                    );
                                    self.buy_phase = 0;
                                    self.buy_clicks_done = 0;
                                    self.advance(ctx.tick);
                                    continue;
                                }
                                let elapsed = ctx.tick.saturating_sub(self.buy_last_click_tick);
                                if elapsed >= spacing_ticks {
                                    self.buy_clicks_done += 1;
                                    if self.buy_clicks_done == 1 {
                                        tracing::info!(
                                            "BuyItem[{}] legacy phase 1: confirm at ({}, {}) × {}. If wrong, edit: confirm_vx={}, confirm_vy={}",
                                            idx, confirm_vx, confirm_vy, quantity, confirm_vx, confirm_vy
                                        );
                                    }
                                    self.buy_last_click_tick = ctx.tick;
                                    return CavebotAction::Click { vx: confirm_vx, vy: confirm_vy };
                                }
                                return CavebotAction::Idle;
                            }
                            _ => {
                                tracing::warn!("BuyItem[{}] legacy invalid phase {}, resetting", idx, self.buy_phase);
                                self.buy_phase = 0;
                                self.advance(ctx.tick);
                                continue;
                            }
                        }
                    }

                    // ── Flujo Amount (Tibia 12): tipear dígitos + 1 click ─
                    //
                    // Los amount_* son Some por el check de arriba; unwrap
                    // es seguro en este branch.
                    let amt_vx = amount_vx.expect("amount_vx guarded by use_amount_flow");
                    let amt_vy = amount_vy.expect("amount_vy guarded by use_amount_flow");
                    let qty_str = quantity.to_string();

                    let post_item_ticks   = ms_to_ticks(POST_ITEM_CLICK_MS, self.fps).max(1);
                    let post_amount_ticks = ms_to_ticks(POST_AMOUNT_CLICK_MS, self.fps).max(1);
                    let post_digits_ticks = ms_to_ticks(POST_DIGITS_MS, self.fps).max(1);
                    let inter_digit_ticks = ms_to_ticks(spacing_ms / 2, self.fps).max(1);

                    match self.buy_phase {
                        0 => {
                            // Phase 0: click item (select row).
                            tracing::info!(
                                "BuyItem[{}] amount phase 0: select item at ({}, {}), quantity={}. amount_field=({}, {})",
                                idx, item_vx, item_vy, quantity, amt_vx, amt_vy
                            );
                            self.buy_phase = 1;
                            self.buy_last_click_tick = ctx.tick;
                            self.buy_amount_digit_idx = 0;
                            return CavebotAction::Click { vx: item_vx, vy: item_vy };
                        }
                        1 => {
                            // Phase 1: esperar 200ms → click amount field.
                            if ctx.tick.saturating_sub(self.buy_last_click_tick) < post_item_ticks {
                                return CavebotAction::Idle;
                            }
                            tracing::info!(
                                "BuyItem[{}] amount phase 1: click amount field at ({}, {})",
                                idx, amt_vx, amt_vy
                            );
                            self.buy_phase = 2;
                            self.buy_last_click_tick = ctx.tick;
                            return CavebotAction::Click { vx: amt_vx, vy: amt_vy };
                        }
                        2 => {
                            // Phase 2: esperar 150ms → entrar loop de dígitos.
                            if ctx.tick.saturating_sub(self.buy_last_click_tick) < post_amount_ticks {
                                return CavebotAction::Idle;
                            }
                            self.buy_phase = 3;
                            self.buy_last_click_tick = ctx.tick;
                            continue; // re-entrar en phase 3 en este mismo tick
                        }
                        3 => {
                            // Phase 3: emitir KeyTap del dígito actual.
                            if self.buy_amount_digit_idx >= qty_str.len() {
                                // Ya no quedan dígitos → pasar a wait post-digits.
                                self.buy_phase = 4;
                                self.buy_last_click_tick = ctx.tick;
                                return CavebotAction::Idle;
                            }
                            let ch = qty_str.as_bytes()[self.buy_amount_digit_idx] as char;
                            // Convertir '0'-'9' a HID code via helper compartido.
                            let hid = crate::act::keycode::ascii_to_hid(ch)
                                .expect("digit '0'-'9' siempre convertible a HID");
                            tracing::info!(
                                "BuyItem[{}] amount phase 3: typing digit '{}' (HID 0x{:02X}), idx {}/{}",
                                idx, ch, hid, self.buy_amount_digit_idx + 1, qty_str.len()
                            );
                            self.buy_amount_digit_idx += 1;
                            self.buy_phase = 4;
                            self.buy_last_click_tick = ctx.tick;
                            return CavebotAction::KeyTap(hid);
                        }
                        4 => {
                            // Phase 4: spacing entre dígitos o post-last.
                            let more_digits = self.buy_amount_digit_idx < qty_str.len();
                            let wait_ticks = if more_digits {
                                inter_digit_ticks
                            } else {
                                post_digits_ticks
                            };
                            if ctx.tick.saturating_sub(self.buy_last_click_tick) < wait_ticks {
                                return CavebotAction::Idle;
                            }
                            if more_digits {
                                // Volver a phase 3 para el siguiente dígito.
                                self.buy_phase = 3;
                                continue;
                            }
                            // Todos los dígitos tipeados + wait post-digits
                            // cumplido → click de confirm.
                            self.buy_phase = 5;
                            continue;
                        }
                        5 => {
                            // Phase 5: click confirm (1 sola vez).
                            tracing::info!(
                                "BuyItem[{}] amount phase 5: single confirm click at ({}, {}) for quantity={}",
                                idx, confirm_vx, confirm_vy, quantity
                            );
                            self.buy_phase = 6;
                            self.buy_last_click_tick = ctx.tick;
                            return CavebotAction::Click { vx: confirm_vx, vy: confirm_vy };
                        }
                        6 => {
                            // Phase 6: wait spacing_ms post-confirm → advance.
                            if ctx.tick.saturating_sub(self.buy_last_click_tick) < spacing_ticks {
                                return CavebotAction::Idle;
                            }
                            tracing::info!(
                                "BuyItem[{}] amount complete: typed '{}' + 1 confirm click, advancing",
                                idx, qty_str
                            );
                            self.buy_phase = 0;
                            self.buy_amount_digit_idx = 0;
                            self.advance(ctx.tick);
                            continue;
                        }
                        _ => {
                            tracing::warn!("BuyItem[{}] amount invalid phase {}, resetting", idx, self.buy_phase);
                            self.buy_phase = 0;
                            self.buy_amount_digit_idx = 0;
                            self.advance(ctx.tick);
                            continue;
                        }
                    }
                }

                // ── CheckSupplies (verifica HasItem en lote) ──────────
                StepKind::CheckSupplies { requirements, on_fail_label, on_fail_idx } => {
                    let mut all_ok = true;
                    let mut missing: Vec<String> = Vec::new();
                    for (name, min_count) in &requirements {
                        let have = ctx.inventory_counts.get(name.as_str()).copied().unwrap_or(0);
                        if have < *min_count {
                            all_ok = false;
                            missing.push(format!("{}: have {}, need {}", name, have, min_count));
                        }
                    }
                    if all_ok {
                        tracing::info!("CheckSupplies[{}] passed: all requirements met", idx);
                        self.advance(ctx.tick);
                        continue;
                    }
                    // Falta al menos un item → salta al label on_fail.
                    tracing::warn!(
                        "CheckSupplies[{}] failed, jumping to '{}': {}",
                        idx, on_fail_label, missing.join("; ")
                    );
                    self.jump_to(on_fail_idx, ctx.tick);
                    continue;
                }

                // ── TypeInField ───────────────────────────────────────
                //
                // Click al field + tipear `text` char a char + wait final +
                // advance. Pensado para el search field de la trade window
                // de Tibia 12 (a diferencia del chat, no wrapea con Enter).
                //
                // Fases (ver doc de `StepKind::TypeInField`):
                //   0 = emit Click en (field_vx, field_vy)
                //   1 = wait wait_after_click_ms
                //   2 = loop: emit KeyTap del char actual + wait char_spacing_ms
                //   3 = wait wait_after_type_ms
                //   4 = advance (handled by fall-through a phase 3 terminando)
                //
                // Chars no-mapeables (mayúsculas, símbolos) → log warn y skip.
                StepKind::TypeInField {
                    field_vx, field_vy, text,
                    wait_after_click_ms, wait_after_type_ms, char_spacing_ms,
                } => {
                    match self.type_field_phase {
                        0 => {
                            // Phase 0: click al field para que tome focus.
                            tracing::info!(
                                "TypeInField[{}] phase 0: click field at ({}, {}), will type '{}' ({} chars)",
                                idx, field_vx, field_vy, text, text.chars().count()
                            );
                            self.type_field_phase = 1;
                            self.type_field_phase_start = ctx.tick;
                            self.type_field_char_idx = 0;
                            return CavebotAction::Click { vx: field_vx, vy: field_vy };
                        }
                        1 => {
                            // Phase 1: wait wait_after_click_ms.
                            let wait_ticks = ms_to_ticks(wait_after_click_ms, self.fps);
                            if ctx.tick.saturating_sub(self.type_field_phase_start) >= wait_ticks {
                                self.type_field_phase = 2;
                                self.type_field_phase_start = ctx.tick;
                                continue; // re-entrar en phase 2 este mismo tick
                            }
                            return CavebotAction::Idle;
                        }
                        2 => {
                            // Phase 2: tipear chars uno a uno. Skipea los
                            // no-mapeables con warn.
                            let text_bytes = text.as_bytes();
                            // Skip chars no-mapeables hasta encontrar uno válido o fin de string.
                            while self.type_field_char_idx < text_bytes.len() {
                                let ch = text_bytes[self.type_field_char_idx] as char;
                                if let Some(hid) = crate::act::keycode::ascii_to_hid(ch) {
                                    // Respetar spacing: solo emitir si ya pasaron
                                    // char_spacing_ms desde el último tap. El primer
                                    // tap (char_idx=0) emite inmediatamente porque
                                    // el phase_start se seteó al entrar a phase 2.
                                    let spacing_ticks = ms_to_ticks(char_spacing_ms, self.fps);
                                    let first_char = self.type_field_char_idx == 0;
                                    let elapsed = ctx.tick.saturating_sub(self.type_field_phase_start);
                                    if !first_char && elapsed < spacing_ticks {
                                        return CavebotAction::Idle;
                                    }
                                    tracing::debug!(
                                        "TypeInField[{}] phase 2: tap '{}' (HID 0x{:02X}), idx {}/{}",
                                        idx, ch, hid,
                                        self.type_field_char_idx + 1, text_bytes.len()
                                    );
                                    self.type_field_char_idx += 1;
                                    self.type_field_phase_start = ctx.tick;
                                    return CavebotAction::KeyTap(hid);
                                }
                                // No-mapeable: warn + skip SIN consumir tick
                                // para que no degrademos la cadencia.
                                tracing::warn!(
                                    "TypeInField[{}]: char '{}' (idx {}) no mapeable a HID — skipping",
                                    idx, ch, self.type_field_char_idx
                                );
                                self.type_field_char_idx += 1;
                            }
                            // Todos los chars procesados → pasar a wait_after_type.
                            self.type_field_phase = 3;
                            self.type_field_phase_start = ctx.tick;
                            return CavebotAction::Idle;
                        }
                        3 => {
                            // Phase 3: wait wait_after_type_ms → advance.
                            let wait_ticks = ms_to_ticks(wait_after_type_ms, self.fps);
                            if ctx.tick.saturating_sub(self.type_field_phase_start) >= wait_ticks {
                                tracing::info!("TypeInField[{}] complete, advancing", idx);
                                self.type_field_phase = 0;
                                self.type_field_char_idx = 0;
                                self.advance(ctx.tick);
                                continue;
                            }
                            return CavebotAction::Idle;
                        }
                        _ => {
                            tracing::warn!(
                                "TypeInField[{}] invalid phase {}, resetting",
                                idx, self.type_field_phase
                            );
                            self.type_field_phase = 0;
                            self.type_field_char_idx = 0;
                            self.advance(ctx.tick);
                            continue;
                        }
                    }
                }

                // ── SkipIfBlocked ──────────────────────────────────────
                StepKind::SkipIfBlocked { inner, max_wait_ms } => {
                    // Watchdog: si no hubo actividad en `max_wait_ms`, avanzar.
                    let ticks_since_activity = ctx.tick.saturating_sub(ctx.last_activity_tick);
                    let max_ticks = ms_to_ticks(max_wait_ms, self.fps);
                    if ticks_since_activity >= max_ticks {
                        tracing::warn!(
                            "SkipIfBlocked: {} ticks sin actividad en step {} — saltando",
                            ticks_since_activity, idx
                        );
                        self.advance(ctx.tick);
                        continue;
                    }
                    // Si no está bloqueado, comportarse como el inner step.
                    // Stuck detection del Walk se deshabilita — SkipIfBlocked
                    // usa su propio watchdog basado en last_activity_tick.
                    match *inner {
                        StepKind::Walk { hidcode, duration_ms, interval_ms } => {
                            match self.tick_walk(ctx, idx, ticks_in_step, hidcode, duration_ms, interval_ms, false) {
                                Some(action) => return action,
                                None => { self.advance(ctx.tick); continue; }
                            }
                        }
                        _ => {
                            tracing::warn!("SkipIfBlocked: inner no soportado, saltando");
                            self.advance(ctx.tick);
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// Evaluate a VerifyCheck against the current context. Returns true if
    /// the postcondition is satisfied.
    ///
    /// For TemplateVisible/Absent, uses `ctx.ui_matches` which is the cached
    /// async result from UiDetector (up to ~500ms stale). That staleness is
    /// acceptable because verify timeouts default to 3000ms.
    fn evaluate_verify(&self, check: &VerifyCheck, ctx: &TickContext) -> bool {
        match check {
            VerifyCheck::TemplateVisible { name, roi: _ } => {
                ctx.ui_matches.iter().any(|m| m == name)
            }
            VerifyCheck::TemplateAbsent { name, roi: _ } => {
                !ctx.ui_matches.iter().any(|m| m == name)
            }
            VerifyCheck::ConditionMet(cond) => cond.eval(ctx),
            VerifyCheck::InventoryDelta { item, min_abs_delta, require_positive } => {
                let start = self.inventory_at_step_start.get(item).copied().unwrap_or(0) as i64;
                let now = ctx.inventory_stacks.get(item).copied().unwrap_or(0) as i64;
                let delta = now - start;
                if *require_positive {
                    delta >= *min_abs_delta as i64
                } else {
                    delta.unsigned_abs() >= *min_abs_delta as u64
                }
            }
        }
    }

    /// Avanza al siguiente step. Loopea si `loop_`, termina si no.
    ///
    /// Wrapper que enruta por verify: si el step actual tiene un `StepVerify`
    /// y no estamos ya en modo verifying, entra a verify mode en lugar de
    /// avanzar. Los call sites existentes invocan este wrapper sin cambios.
    fn advance(&mut self, tick: u64) {
        let Some(idx) = self.current else {
            self.do_advance(tick);
            return;
        };
        // If already verifying, this is a PASS — fall through to do_advance.
        if self.verifying.is_some() {
            self.do_advance(tick);
            return;
        }
        // If step has verify, enter verify mode instead of advancing.
        if let Some(verify) = self.steps[idx].verify.clone() {
            let timeout_ticks = ms_to_ticks(verify.timeout_ms, self.fps);
            let label = self.steps[idx].label.clone();
            tracing::info!(
                "Cavebot step[{}]={:?}: entering verify check={:?} timeout_ms={}",
                idx, label, verify.check, verify.timeout_ms
            );
            self.verifying = Some(VerifyingState {
                started_tick: tick,
                timeout_ticks,
            });
            return;
        }
        self.do_advance(tick);
    }

    /// Low-level advance (sin verify interception). Mueve `current` al siguiente
    /// step y resetea todo el estado per-step. Llamado por el wrapper `advance`
    /// cuando no hay verify o tras un verify PASS.
    fn do_advance(&mut self, tick: u64) {
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
        self.loot_clicks_done = 0;
        self.npc_phrase_idx = 0;
        self.npc_wait_start = None;
        self.open_trade_phase = 0;
        self.open_trade_phrase_idx = 0;
        self.open_trade_phase_start = 0;
        self.stuck_walk_ticks = 0;
        self.needs_kills_baseline = true;
        self.deposit_phase = 0;
        self.deposit_phase_start = 0;
        self.buy_phase = 0;
        self.buy_clicks_done = 0;
        self.buy_last_click_tick = 0;
        self.buy_amount_digit_idx = 0;
        self.stow_phase = 0;
        self.stow_phase_start = 0;
        self.stow_iterations_done = 0;
        self.stow_baseline_counts = None;
        self.stow_stale_iters = 0;
        self.type_field_phase = 0;
        self.type_field_char_idx = 0;
        self.type_field_phase_start = 0;
        self.reset_node_state();
        self.verifying = None;
        self.needs_inventory_snapshot = true;
    }

    /// Salta a un índice específico (Goto/GotoIf).
    /// Busca un label por nombre. Retorna el índice del step Label si existe.
    /// Usado por el hot-reload para preservar posición cuando se recarga
    /// un script modificado con el mismo label.
    pub fn find_label(&self, name: &str) -> Option<usize> {
        self.steps.iter().position(|s| {
            matches!(s.kind, StepKind::Label) && s.label.as_deref() == Some(name)
        })
    }

    /// Retorna el nombre del label más reciente en o antes del step actual.
    /// Busca hacia atrás desde `current` hasta encontrar un Step con
    /// `StepKind::Label` y un nombre. Retorna None si no hay labels previos.
    pub fn current_label_name(&self) -> Option<String> {
        let idx = self.current?;
        for i in (0..=idx).rev() {
            let s = self.steps.get(i)?;
            if matches!(s.kind, StepKind::Label) {
                return s.label.clone();
            }
        }
        None
    }

    /// Salto público a un label por nombre. Reinicia timers del step.
    /// Retorna `true` si el label existía y el salto se ejecutó.
    #[allow(dead_code)] // user of hot-reload in loop_.rs
    pub fn jump_to_label(&mut self, name: &str, tick: u64) -> bool {
        if let Some(idx) = self.find_label(name) {
            self.jump_to(idx, tick);
            true
        } else {
            false
        }
    }

    fn jump_to(&mut self, target_idx: usize, tick: u64) {
        if target_idx < self.steps.len() {
            self.current = Some(target_idx);
            self.started_tick = tick;
            self.last_emit_tick = None;
            self.loot_clicks_done = 0;
            self.npc_phrase_idx = 0;
            self.npc_wait_start = None;
            self.open_trade_phase = 0;
            self.open_trade_phrase_idx = 0;
            self.open_trade_phase_start = 0;
            self.stuck_walk_ticks = 0;
            self.deposit_phase = 0;
            self.deposit_phase_start = 0;
            self.buy_phase = 0;
            self.buy_clicks_done = 0;
            self.buy_last_click_tick = 0;
            self.buy_amount_digit_idx = 0;
            self.stow_phase = 0;
            self.stow_phase_start = 0;
            self.stow_iterations_done = 0;
            self.stow_baseline_counts = None;
            self.stow_stale_iters = 0;
            self.type_field_phase = 0;
            self.type_field_char_idx = 0;
            self.type_field_phase_start = 0;
            self.needs_kills_baseline = true;
            self.reset_node_state();
            self.verifying = None;
            self.needs_inventory_snapshot = true;
        } else {
            tracing::warn!(
                "Cavebot::jump_to: target_idx={} out of bounds (len={}) — terminando lista",
                target_idx, self.steps.len()
            );
            self.current = None;
        }
    }

    // Todas las constantes de Node están en self.tuning (NodeTuning).

    /// Reset completo de estado de Node.
    fn reset_node_state(&mut self) {
        self.node_phase = 0;
        self.node_phase_start_tick = 0;
        self.node_saw_moving = false;
        self.node_accum_dx = 0;
        self.node_accum_dy = 0;
        self.node_expect_dx = 0;
        self.node_expect_dy = 0;
        self.node_saw_displacement = false;
        self.node_idle_ticks = 0;
        self.node_none_moving_ticks = 0;
        self.node_reclick_count = 0;
        self.node_initial_click_retries = 0;
    }

    /// Estima la posición actual del personaje usando displacement acumulado.
    /// Usado cuando timeout/max-reclicks para no asumir que llegó al destino.
    fn estimated_position(&self) -> Option<(i32, i32, i32)> {
        let prev = self.prev_node?;
        let tiles_dx = self.node_accum_dx / self.tuning.pixels_per_tile;
        let tiles_dy = self.node_accum_dy / self.tuning.pixels_per_tile;
        Some((prev.0 + tiles_dx, prev.1 + tiles_dy, prev.2))
    }

    /// Valida que el z real del char (via tile-hashing) coincide con el z del
    /// target del node. Si no coincide, emite `SafetyPause` para que el FSM
    /// pause el bot y evite cadenas de acciones inútiles en piso equivocado
    /// (ej stow clicks fantasma en z=7 cuando el depot está en z=6).
    ///
    /// Si `game_coords` no está disponible (map index no cargado / matcher
    /// degradado), no bloquea — retorna None.
    fn validate_z_arrival(&self, ctx: &TickContext, x: i32, y: i32, z: i32) -> Option<CavebotAction> {
        let (rx, ry, rz) = ctx.game_coords?;
        if rz == z {
            return None;
        }
        let reason = format!(
            "node_z_mismatch: target=({},{},{}) real=({},{},{}) — char en piso equivocado \
             tras navegación. Revisar ladders/ropes/pathfinding. Cavebot pausado para evitar \
             cadena de acciones inválidas.",
            x, y, z, rx, ry, rz
        );
        tracing::error!("Cavebot: {}", reason);
        Some(CavebotAction::SafetyPause { reason })
    }

    /// Lógica de Node: click en minimap para auto-walk con pathfinding A*.
    /// Detección de llegada por displacement acumulado + re-click automático.
    fn tick_node(
        &mut self,
        ctx: &TickContext,
        _ticks_in_step: u64,
        x: i32, y: i32, z: i32,
        _max_wait_ms: u64,
    ) -> Option<CavebotAction> {
        // ── Fase 0: setup + click ───────────────────────────────────
        if self.node_phase == 0 {
            let prev = match self.prev_node {
                Some(p) => p,
                None => {
                    // Primer Node de esta activación: intentamos semillar
                    // `prev_node` desde la posición real del char via
                    // tile-hashing. Permite arrancar el cavebot desde
                    // cualquier lugar — el bot camina hacia el target en
                    // vez de asumir que ya está allí.
                    //
                    // Si tile-hashing aún no produjo match (corre cada ~15
                    // frames), esperamos devolviendo Idle (sin advance).
                    // Tras `SEED_WAIT_TICKS` caemos al comportamiento legacy
                    // (registrar target como baseline + advance) para
                    // evitar quedar bloqueado si el map index no está cargado.
                    const SEED_WAIT_TICKS: u64 = 60; // ~2s @ 30Hz
                    if let Some(seed) = ctx.game_coords {
                        tracing::info!(
                            "Node: semillando prev_node desde game_coords real ({},{},{}) — \
                             target ({},{},{})",
                            seed.0, seed.1, seed.2, x, y, z
                        );
                        self.prev_node = Some(seed);
                        self.node_seed_wait = 0;
                        // Idle: nos quedamos en este step en phase 0, próximo
                        // tick re-entra con prev_node Some y computa dx/dy.
                        return Some(CavebotAction::Idle);
                    }
                    // Sin game_coords: esperar (Idle) o fallback.
                    self.node_seed_wait = self.node_seed_wait.saturating_add(1);
                    if self.node_seed_wait >= SEED_WAIT_TICKS {
                        tracing::warn!(
                            "Node ({},{},{}): tile-hashing no disponible tras {} ticks — \
                             fallback legacy (registrando target como baseline). \
                             El char DEBE estar en esa posición o la navegación falla.",
                            x, y, z, SEED_WAIT_TICKS
                        );
                        self.prev_node = Some((x, y, z));
                        self.node_seed_wait = 0;
                        // Fallback: advance al siguiente step (comportamiento legacy).
                        return None;
                    }
                    // Aún esperando: Idle sin advance.
                    return Some(CavebotAction::Idle);
                }
            };

            let dx = x - prev.0;
            let dy = y - prev.1;

            if dx == 0 && dy == 0 {
                // Mismo XY que prev_node: no hay que caminar. Pero si el z real
                // difiere del target, el char está en el piso equivocado (caso
                // típico: seed coincide en XY pero cavebot espera z distinto).
                // El minimap click NO se emite porque dx=dy=0, asi que no hay
                // auto-pathfind a ladders — tenemos que pausar explícitamente.
                if let Some(pause) = self.validate_z_arrival(ctx, x, y, z) {
                    return Some(pause);
                }
                self.prev_node = Some((x, y, z));
                return None;
            }

            let Some((cx, cy)) = ctx.minimap_center else {
                tracing::warn!("Node ({},{},{}): minimap no calibrado — saltando", x, y, z);
                self.prev_node = Some((x, y, z));
                return None;
            };

            let click_x = cx + dx * self.tuning.pixels_per_tile;
            let click_y = cy + dy * self.tuning.pixels_per_tile;

            tracing::info!(
                "Node ({},{},{}): minimap click ({},{}) [dx={}, dy={}, from=({},{},{})]",
                x, y, z, click_x, click_y, dx, dy, prev.0, prev.1, prev.2
            );

            self.node_phase = 1;
            self.node_phase_start_tick = ctx.tick;
            self.node_saw_moving = false;
            self.node_accum_dx = 0;
            self.node_accum_dy = 0;
            self.node_expect_dx = dx * self.tuning.pixels_per_tile;
            self.node_expect_dy = dy * self.tuning.pixels_per_tile;
            self.node_saw_displacement = false;
            self.node_idle_ticks = 0;
            self.node_reclick_count = 0;
            self.node_initial_click_retries = 0;

            return Some(CavebotAction::Click { vx: click_x, vy: click_y });
        }

        // ── Fase 1: esperar llegada con displacement acumulado ──────
        if self.node_phase == 1 {
            let elapsed = ctx.tick.saturating_sub(self.node_phase_start_tick);

            // F6: Click-missed retry. Si tras N ticks no hay señal de
            // movimiento ni displacement, re-emitir el click inicial.
            if !self.node_saw_moving
                && !self.node_saw_displacement
                && self.node_initial_click_retries < self.tuning.initial_click_retries
            {
                let wait = self.tuning.initial_click_wait as u64
                    * (self.node_initial_click_retries as u64 + 1);
                if elapsed >= wait {
                    if let Some((cx, cy)) = ctx.minimap_center {
                        let click_x = cx + self.node_expect_dx;
                        let click_y = cy + self.node_expect_dy;
                        self.node_initial_click_retries += 1;
                        tracing::info!(
                            "Node ({},{},{}): click-missed retry #{} at ({},{})",
                            x, y, z, self.node_initial_click_retries, click_x, click_y
                        );
                        return Some(CavebotAction::Click { vx: click_x, vy: click_y });
                    }
                }
            }

            // Hard timeout — usar posición estimada, NO el destino
            if elapsed >= self.tuning.timeout_ticks {
                let est = self.estimated_position().unwrap_or((x, y, z));
                tracing::warn!(
                    "Node ({},{},{}): timeout ({}s) — pos estimada ({},{},{}) — avanzando",
                    x, y, z, elapsed / 30, est.0, est.1, est.2
                );
                if let Some(pause) = self.validate_z_arrival(ctx, x, y, z) {
                    return Some(pause);
                }
                self.prev_node = Some(est);
                self.node_phase = 0;
                return None;
            }

            // Acumular displacement (negado: camera shift → character movement)
            if let Some((raw_dx, raw_dy)) = ctx.minimap_displacement {
                self.node_accum_dx += -raw_dx;
                self.node_accum_dy += -raw_dy;
                self.node_saw_displacement = true;
            }

            // Track idle
            match ctx.is_moving {
                Some(true) => {
                    self.node_saw_moving = true;
                    self.node_idle_ticks = 0;
                    self.node_none_moving_ticks = 0;
                }
                Some(false) => {
                    self.node_idle_ticks += 1;
                    self.node_none_moving_ticks = 0;
                }
                None => {
                    // Sensor degradado: contar como idle para arrival.
                    self.node_none_moving_ticks += 1;
                    self.node_idle_ticks += 1;
                }
            }

            if self.node_saw_displacement {
                // ── Displacement path: confirmación positiva ─────────
                let remain_dx = self.node_expect_dx - self.node_accum_dx;
                let remain_dy = self.node_expect_dy - self.node_accum_dy;
                let manhattan = remain_dx.abs() + remain_dy.abs();

                // Arrived: displacement matches AND character stopped
                if manhattan <= self.tuning.displacement_tolerance
                    && ctx.is_moving != Some(true)
                    && self.node_idle_ticks >= self.tuning.arrived_idle_ticks
                {
                    tracing::info!(
                        "Node ({},{},{}): arrived (disp accum=({},{}), expect=({},{}), remain={})",
                        x, y, z, self.node_accum_dx, self.node_accum_dy,
                        self.node_expect_dx, self.node_expect_dy, manhattan
                    );
                    if let Some(pause) = self.validate_z_arrival(ctx, x, y, z) {
                        return Some(pause);
                    }
                    self.prev_node = Some((x, y, z));
                    self.node_phase = 0;
                    return None;
                }

                // Re-click: stuck mid-walk
                if self.node_idle_ticks >= self.tuning.reclick_idle_ticks
                    && manhattan > self.tuning.displacement_tolerance
                    && self.node_reclick_count < self.tuning.max_reclicks
                {
                    if let Some((cx, cy)) = ctx.minimap_center {
                        let remain_tiles_dx = remain_dx / self.tuning.pixels_per_tile;
                        let remain_tiles_dy = remain_dy / self.tuning.pixels_per_tile;
                        if remain_tiles_dx != 0 || remain_tiles_dy != 0 {
                            let click_x = cx + remain_tiles_dx * self.tuning.pixels_per_tile;
                            let click_y = cy + remain_tiles_dy * self.tuning.pixels_per_tile;
                            self.node_reclick_count += 1;
                            self.node_idle_ticks = 0;
                            self.node_saw_moving = false;
                            tracing::info!(
                                "Node ({},{},{}): re-click #{} remaining ({},{}) tiles",
                                x, y, z, self.node_reclick_count, remain_tiles_dx, remain_tiles_dy
                            );
                            return Some(CavebotAction::Click { vx: click_x, vy: click_y });
                        }
                    } else {
                        // F3: minimap_center perdido — contar como re-click fallido
                        // para que max-reclicks avance en vez de colgar 30s.
                        self.node_reclick_count += 1;
                        self.node_idle_ticks = 0;
                        tracing::warn!(
                            "Node ({},{},{}): re-click #{} skipped (minimap_center=None)",
                            x, y, z, self.node_reclick_count
                        );
                    }
                }

                // Max re-clicks exhausted — usar posición estimada
                if self.node_idle_ticks >= self.tuning.reclick_idle_ticks
                    && self.node_reclick_count >= self.tuning.max_reclicks
                {
                    let est = self.estimated_position().unwrap_or((x, y, z));
                    tracing::warn!(
                        "Node ({},{},{}): max re-clicks — pos estimada ({},{},{}) — avanzando",
                        x, y, z, est.0, est.1, est.2
                    );
                    if let Some(pause) = self.validate_z_arrival(ctx, x, y, z) {
                        return Some(pause);
                    }
                    self.prev_node = Some(est);
                    self.node_phase = 0;
                    return None;
                }
            } else {
                // ── Fallback: no displacement data → is_moving only ─

                // F4: re-click en fallback — si el char se trabó, re-intentar click.
                if self.node_saw_moving
                    && self.node_idle_ticks >= self.tuning.reclick_idle_ticks
                    && self.node_reclick_count < self.tuning.max_reclicks
                {
                    if let Some((cx, cy)) = ctx.minimap_center {
                        let dx = x - self.prev_node.map(|p| p.0).unwrap_or(x);
                        let dy = y - self.prev_node.map(|p| p.1).unwrap_or(y);
                        if dx != 0 || dy != 0 {
                            let click_x = cx + dx * self.tuning.pixels_per_tile;
                            let click_y = cy + dy * self.tuning.pixels_per_tile;
                            self.node_reclick_count += 1;
                            self.node_idle_ticks = 0;
                            self.node_saw_moving = false;
                            tracing::info!(
                                "Node ({},{},{}): fallback re-click #{} (full delta)",
                                x, y, z, self.node_reclick_count
                            );
                            return Some(CavebotAction::Click { vx: click_x, vy: click_y });
                        }
                    } else {
                        self.node_reclick_count += 1;
                        self.node_idle_ticks = 0;
                        tracing::warn!(
                            "Node ({},{},{}): fallback re-click #{} skipped (no minimap_center)",
                            x, y, z, self.node_reclick_count
                        );
                    }
                }

                // Arrival: char walked+stopped con idle suficiente O reclicks agotados.
                if self.node_saw_moving
                    && (self.node_idle_ticks >= self.tuning.fallback_stop_ticks
                        || self.node_reclick_count >= self.tuning.max_reclicks)
                {
                    // F2: usar estimated_position en vez de asumir target.
                    let est = self.estimated_position().unwrap_or((x, y, z));
                    tracing::info!("Node ({},{},{}): arrived (fallback, reclicks={})", x, y, z, self.node_reclick_count);
                    if let Some(pause) = self.validate_z_arrival(ctx, x, y, z) {
                        return Some(pause);
                    }
                    self.prev_node = Some(est);
                    self.node_phase = 0;
                    return None;
                }

                // F1: sensor-degraded — is_moving=None persistente.
                if !self.node_saw_moving
                    && self.node_none_moving_ticks >= self.tuning.sensor_degraded_ticks
                {
                    tracing::warn!(
                        "Node ({},{},{}): sensor-degraded fallback (is_moving=None {}t)",
                        x, y, z, self.node_none_moving_ticks
                    );
                    if let Some(pause) = self.validate_z_arrival(ctx, x, y, z) {
                        return Some(pause);
                    }
                    self.prev_node = Some((x, y, z));
                    self.node_phase = 0;
                    return None;
                }
            }

            return Some(CavebotAction::Idle);
        }

        Some(CavebotAction::Idle)
    }

    /// Lógica compartida de Walk. Retorna:
    /// - `Some(action)` → devolver al caller (KeyTap o Idle)
    /// - `None` → step terminado, el caller debe advance + continue
    #[allow(clippy::too_many_arguments)]
    fn tick_walk(
        &mut self,
        ctx: &TickContext,
        idx: usize,
        ticks_in_step: u64,
        hidcode: u8,
        duration_ms: u64,
        interval_ms: u64,
        check_stuck: bool,
    ) -> Option<CavebotAction> {
        let duration_ticks = ms_to_ticks(duration_ms, self.fps);
        if ticks_in_step >= duration_ticks {
            self.stuck_walk_ticks = 0;
            return None; // advance
        }
        // Stuck detection (opcional — SkipIfBlocked usa su propio watchdog).
        // Solo activo si el minimap está calibrado (is_moving = Some).
        // Con None deshabilitamos para no cortar walk steps prematuramente.
        if check_stuck && self.last_emit_tick.is_some() {
            if ctx.is_moving == Some(true) {
                self.stuck_walk_ticks = 0;
            } else if ctx.is_moving == Some(false) || ctx.is_moving.is_none() {
                // F10: is_moving=None también incrementa stuck counter.
                // Sin minimap calibrado, no hay confirmación de movimiento.
                self.stuck_walk_ticks += 1;
                if self.stuck_walk_ticks >= STUCK_THRESHOLD_TICKS {
                    tracing::warn!(
                        "Walk stuck: {} ticks sin movimiento en step {} (key={:#04x}, is_moving={:?}) — avanzando",
                        self.stuck_walk_ticks, idx, hidcode, ctx.is_moving
                    );
                    self.stuck_walk_ticks = 0;
                    return None; // advance
                }
            }
        }
        // Primer emit.
        let Some(last) = self.last_emit_tick else {
            self.last_emit_tick = Some(ctx.tick);
            return Some(CavebotAction::KeyTap(hidcode));
        };
        // Re-emit por interval.
        if interval_ms == 0 {
            return Some(CavebotAction::Idle);
        }
        let interval_ticks = ms_to_ticks(interval_ms, self.fps).max(1);
        if ctx.tick.saturating_sub(last) >= interval_ticks {
            self.last_emit_tick = Some(ctx.tick);
            return Some(CavebotAction::KeyTap(hidcode));
        }
        Some(CavebotAction::Idle)
    }

    /// Reanuda el step actual tras volver de Fighting/Emergency.
    ///
    /// Walk y Wait: se REANUDAN preservando el tiempo ya transcurrido.
    /// Reiniciar un Walk perdería la posición relativa — el char reemitiría
    /// la dirección completa desde una posición desconocida, acumulando drift.
    ///
    /// Loot y NpcDialog: se REINICIAN porque su estado interno (clicks, frases)
    /// es dependiente de la posición y debe volver a comenzar.
    pub fn restart_current_step(&mut self, tick: u64) {
        let Some(idx) = self.current else { return };
        match &self.steps[idx].kind {
            StepKind::Walk { .. } | StepKind::Wait { .. } => {
                // Resumir: no tocar started_tick — el tiempo ya transcurrido
                // se conserva y el step continúa desde donde lo dejó.
                self.last_emit_tick = None;
                self.stuck_walk_ticks = 0;
            }
            StepKind::Node { .. } => {
                // Resumir: preservar phase, accum, expect, saw_displacement, reclick_count.
                // Solo resetear tracking de movimiento para re-evaluar tras combate.
                self.node_saw_moving = false;
                self.node_idle_ticks = 0;
            }
            _ => {
                // Reinicio completo para steps no-temporales o con estado propio.
                self.started_tick = tick;
                self.last_emit_tick = None;
                self.loot_clicks_done = 0;
                self.npc_phrase_idx = 0;
                self.npc_wait_start = None;
                self.open_trade_phase = 0;
                self.open_trade_phrase_idx = 0;
                self.open_trade_phase_start = 0;
                self.stuck_walk_ticks = 0;
                self.buy_phase = 0;
                self.buy_clicks_done = 0;
                self.buy_last_click_tick = 0;
                self.buy_amount_digit_idx = 0;
                self.type_field_phase = 0;
                self.type_field_char_idx = 0;
                self.type_field_phase_start = 0;
                self.reset_node_state();
            }
        }
    }

    /// Salto forzado a un label nombrado, usado por el graceful session-cap.
    ///
    /// A diferencia de `jump_to_label` (pensado para hot-reload), este método
    /// existe para inyecciones externas al cavebot (p. ej. emergency refill
    /// disparado desde el game loop). Semánticamente es un wrapper fino pero
    /// deja explícito el call-site. Retorna `false` si el label no existe en
    /// el script actual; el llamante debe decidir qué hacer (loggear, pausar).
    #[allow(dead_code)] // wired desde core::loop_ (session warning)
    pub fn force_goto_label(&mut self, name: &str, tick: u64) -> bool {
        self.jump_to_label(name, tick)
    }

}

/// Convierte ms a ticks a la frecuencia dada (ceil).
fn ms_to_ticks(ms: u64, fps: u32) -> u64 {
    if ms == 0 || fps == 0 {
        return 0;
    }
    (ms * fps as u64).div_ceil(1000)
}

/// Formatea un snapshot de `inventory_counts` a un string compacto y
/// determinístico (ordenado por nombre). Usado en el `reason` de SafetyPause
/// para el stash-full detection del step StowAllItems.
///
/// Filtra counts iguales a 0 para mantener el resumen corto y ordena por
/// nombre para que la salida sea reproducible en tests/logs.
fn fmt_counts_summary(counts: &std::collections::HashMap<String, u32>) -> String {
    let mut entries: Vec<(&String, &u32)> = counts.iter().filter(|(_, &c)| c > 0).collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    if entries.is_empty() {
        return "{}".into();
    }
    let parts: Vec<String> = entries.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
    format!("{{{}}}", parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cavebot::step::{Condition, Step, StepKind, StandUntil};

    fn ctx(tick: u64) -> TickContext {
        TickContext { tick, ..Default::default() }
    }

    fn ctx_full(tick: u64, hp: Option<f32>, mana: Option<f32>, kills: u64, combat: bool) -> TickContext {
        TickContext {
            tick,
            hp_ratio: hp,
            mana_ratio: mana,
            total_kills: kills,
            in_combat: combat,
            ..Default::default()
        }
    }

    fn step(kind: StepKind) -> Step {
        Step { label: None, kind, verify: None }
    }

    fn labeled(name: &str, kind: StepKind) -> Step {
        Step { label: Some(name.into()), kind, verify: None }
    }

    // ── Walk ──────────────────────────────────────────────────────────

    #[test]
    fn walk_emits_once_then_waits_until_duration() {
        let mut cb = Cavebot::new(
            vec![step(StepKind::Walk { hidcode: 0xAA, duration_ms: 100, interval_ms: 0 })],
            false, 30,
        );
        // Tick 0: primer emit.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::KeyTap(0xAA));
        // Ticks 1..2: idle (interval=0, solo emite al inicio).
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(2)), CavebotAction::Idle);
        // Tick 3: 100ms @ 30fps = 3 ticks → duración cumplida, lista no loop
        //         → Finished.
        assert_eq!(cb.tick(&mut ctx(3)), CavebotAction::Finished);
    }

    #[test]
    fn walk_reemits_at_interval() {
        let mut cb = Cavebot::new(
            vec![step(StepKind::Walk { hidcode: 0xAA, duration_ms: 1000, interval_ms: 200 })],
            false, 30,
        );
        // tick 0: primer emit
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::KeyTap(0xAA));
        // 200ms = 6 ticks → tick 6 es el próximo emit
        for t in 1..6 {
            assert_eq!(cb.tick(&mut ctx(t)), CavebotAction::Idle, "tick {}", t);
        }
        assert_eq!(cb.tick(&mut ctx(6)), CavebotAction::KeyTap(0xAA));
    }

    // ── Hotkey ────────────────────────────────────────────────────────

    #[test]
    fn hotkey_emits_once_then_advances() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Hotkey { hidcode: 0xF1 }),
                step(StepKind::Wait { duration_ms: 100 }),
            ],
            false, 30,
        );
        // Primer tick: emit hotkey y avanza al Wait.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::KeyTap(0xF1));
        assert_eq!(cb.current, Some(1));
    }

    // ── Wait ──────────────────────────────────────────────────────────

    #[test]
    fn wait_blocks_until_duration_elapses() {
        let mut cb = Cavebot::new(
            vec![step(StepKind::Wait { duration_ms: 100 })],
            true, 30,  // loop=true
        );
        // 100ms = 3 ticks
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(2)), CavebotAction::Idle);
        // Tick 3: cumple duración → loopea al inicio (el mismo Wait).
        assert_eq!(cb.tick(&mut ctx(3)), CavebotAction::Idle);
        assert_eq!(cb.current, Some(0));
    }

    // ── Label / Goto ──────────────────────────────────────────────────

    #[test]
    fn goto_jumps_to_resolved_index() {
        let mut cb = Cavebot::new(
            vec![
                labeled("start", StepKind::Label),
                step(StepKind::Hotkey { hidcode: 0x11 }),
                step(StepKind::Goto { target_label: "start".into(), target_idx: 0 }),
            ],
            false, 30,
        );
        // Tick 0: Label (avanza sin emit) → Hotkey (emit 0x11)
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::KeyTap(0x11));
        // Tick 1: avanza al Goto, salta a idx=0 (Label), avanza al Hotkey, emit
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::KeyTap(0x11));
    }

    // ── GotoIf ────────────────────────────────────────────────────────

    #[test]
    fn goto_if_branches_on_condition() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::GotoIf {
                    target_label: "safe".into(),
                    target_idx: 2,
                    condition: Condition::HpBelow(0.5),
                }),
                step(StepKind::Hotkey { hidcode: 0xAA }),  // idx=1 (fallthrough)
                labeled("safe", StepKind::Hotkey { hidcode: 0xBB }),  // idx=2 (jump target)
            ],
            false, 30,
        );
        // Con HP=0.3 (<0.5) → salta a idx=2, emit 0xBB.
        let mut ctx = ctx_full(0, Some(0.3), None, 0, false);
        assert_eq!(cb.tick(&mut ctx), CavebotAction::KeyTap(0xBB));

        // Reset para probar fallthrough.
        let mut cb2 = Cavebot::new(
            vec![
                step(StepKind::GotoIf {
                    target_label: "safe".into(),
                    target_idx: 2,
                    condition: Condition::HpBelow(0.5),
                }),
                step(StepKind::Hotkey { hidcode: 0xAA }),
                labeled("safe", StepKind::Hotkey { hidcode: 0xBB }),
            ],
            false, 30,
        );
        ctx.hp_ratio = Some(0.8);  // >0.5 → fallthrough
        assert_eq!(cb2.tick(&mut ctx), CavebotAction::KeyTap(0xAA));
    }

    #[test]
    fn goto_if_timer_ticks_elapsed_fires_after_threshold() {
        // Wait(0) → GotoIf(timer_ticks_elapsed(5), "done") → Hotkey(0xAA) → Label("done") → Hotkey(0xBB)
        // El Wait(0) avanza inmediatamente. El GotoIf debe esperar 5 ticks
        // antes de saltar a "done".
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Wait { duration_ms: 0 }),
                step(StepKind::GotoIf {
                    target_label: "done".into(),
                    target_idx: 3,
                    condition: Condition::TimerTicksElapsed(5),
                }),
                step(StepKind::Hotkey { hidcode: 0xAA }),  // fallthrough
                labeled("done", StepKind::Hotkey { hidcode: 0xBB }),  // jump target
            ],
            false, 30,
        );
        // Tick 0: Wait(0) avanza → GotoIf con ticks_in_step=0 < 5 → fallthrough
        //         → Hotkey(0xAA) emite y avanza a idx 3 ("done").
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::KeyTap(0xAA));
        // Tick 1: idx 3 = Hotkey(0xBB) emite y avanza a None.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::KeyTap(0xBB));
        // Tick 2: current=None → Finished.
        assert_eq!(cb.tick(&mut ctx(2)), CavebotAction::Finished);

        // Caso 2: Wait largo para que el GotoIf acumule ticks y salte.
        // Wait(200ms=6 ticks) → GotoIf(timer_ticks_elapsed(5)) → Hotkey(0xAA) → Hotkey(0xBB)
        // El GotoIf no se alcanza hasta tick 6 (Wait termina).
        // En tick 6 el GotoIf entra con started_tick=6, ticks=0 < 5 → fallthrough.
        let mut cb2 = Cavebot::new(
            vec![
                step(StepKind::Wait { duration_ms: 200 }),  // idx 0: 200ms = 6 ticks
                step(StepKind::GotoIf {
                    target_label: "done".into(),
                    target_idx: 3,
                    condition: Condition::TimerTicksElapsed(5),
                }),                                          // idx 1
                step(StepKind::Hotkey { hidcode: 0xAA }),   // idx 2: fallthrough
                labeled("done", StepKind::Hotkey { hidcode: 0xBB }),  // idx 3: jump target
            ],
            false, 30,
        );
        // Ticks 0..5: Wait idle.
        for t in 0..6 {
            assert_eq!(cb2.tick(&mut ctx(t)), CavebotAction::Idle, "wait tick {t}");
        }
        // Tick 6: Wait done → GotoIf (started_tick=6, ticks=0 < 5) → fallthrough → 0xAA.
        assert_eq!(cb2.tick(&mut ctx(6)), CavebotAction::KeyTap(0xAA));
    }

    // ── Stand ─────────────────────────────────────────────────────────

    #[test]
    fn stand_mobs_killed_completes_after_n_kills() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Stand { until: StandUntil::MobsKilled(2), max_wait_ms: 30000 }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        // Tick 0: entra al Stand con kills_at_start=0.
        let mut c0 = ctx_full(0, None, None, 0, true);
        assert_eq!(cb.tick(&mut c0), CavebotAction::Idle);

        // Tick 10: aún 0 kills desde start, sigue en Stand.
        let mut c10 = ctx_full(10, None, None, 0, true);
        assert_eq!(cb.tick(&mut c10), CavebotAction::Idle);

        // Tick 20: 1 kill → aún no cumple.
        let mut c20 = ctx_full(20, None, None, 1, true);
        assert_eq!(cb.tick(&mut c20), CavebotAction::Idle);

        // Tick 30: 2 kills → cumple → avanza al Hotkey → emit.
        let mut c30 = ctx_full(30, None, None, 2, false);
        assert_eq!(cb.tick(&mut c30), CavebotAction::KeyTap(0xCC));
    }

    #[test]
    fn stand_timer_completes_after_ms() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Stand { until: StandUntil::TimerMs(100), max_wait_ms: 30000 }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        let mut c0 = ctx(0);
        assert_eq!(cb.tick(&mut c0), CavebotAction::Idle);
        // 100ms @ 30fps = 3 ticks.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(2)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(3)), CavebotAction::KeyTap(0xCC));
    }

    #[test]
    fn stand_max_wait_timeout_forces_advance() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Stand { until: StandUntil::MobsKilled(99), max_wait_ms: 100 }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Idle);
        // 100ms = 3 ticks — a tick 3 el timeout se dispara aunque no haya kills.
        assert_eq!(cb.tick(&mut ctx(3)), CavebotAction::KeyTap(0xCC));
    }

    #[test]
    fn stand_hp_full_completes_when_hp_recovers() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Stand { until: StandUntil::HpFull, max_wait_ms: 30000 }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        // HP 0.5 → no cumple.
        let mut c0 = ctx_full(0, Some(0.5), None, 0, false);
        assert_eq!(cb.tick(&mut c0), CavebotAction::Idle);
        // HP 0.95 → cumple → avanza.
        let mut c1 = ctx_full(1, Some(0.96), None, 0, false);
        assert_eq!(cb.tick(&mut c1), CavebotAction::KeyTap(0xCC));
    }

    // ── Loot ──────────────────────────────────────────────────────────

    #[test]
    fn loot_emits_n_clicks_then_advances() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Loot { vx: 100, vy: 200, retry_count: 3 }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        // Primer click al tick 0.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Click { vx: 100, vy: 200 });
        // Hasta tick 5 espera (delay de 6 ticks entre clicks).
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(5)), CavebotAction::Idle);
        // Tick 6: segundo click.
        assert_eq!(cb.tick(&mut ctx(6)), CavebotAction::Click { vx: 100, vy: 200 });
        // Tick 12: tercer click.
        assert_eq!(cb.tick(&mut ctx(12)), CavebotAction::Click { vx: 100, vy: 200 });
        // Tick 18: retry_count alcanzado → avanza al Hotkey y emite.
        assert_eq!(cb.tick(&mut ctx(18)), CavebotAction::KeyTap(0xCC));
    }

    // ── SkipIfBlocked ─────────────────────────────────────────────────

    #[test]
    fn skip_if_blocked_fires_after_inactivity() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::SkipIfBlocked {
                    inner: Box::new(StepKind::Walk {
                        hidcode: 0xAA, duration_ms: 10000, interval_ms: 300,
                    }),
                    max_wait_ms: 200,  // 6 ticks
                }),
                step(StepKind::Hotkey { hidcode: 0xBB }),
            ],
            false, 30,
        );
        // Contexto sin actividad (last_activity_tick=0 siempre).
        // Tick 0: emite la tecla del inner Walk.
        let mut c0 = ctx_full(0, None, None, 0, false);
        assert_eq!(cb.tick(&mut c0), CavebotAction::KeyTap(0xAA));

        // Tick 6: 200ms desde last_activity=0, dispara skip.
        let mut c6 = TickContext { tick: 6, last_activity_tick: 0, ..Default::default() };
        assert_eq!(cb.tick(&mut c6), CavebotAction::KeyTap(0xBB));
    }

    // ── Integration: loop completo ────────────────────────────────────

    #[test]
    fn loop_with_labels_and_stand_runs_multiple_cycles() {
        // Mini cavebot: hunt → stand hasta 2 kills → walk back → goto start
        let mut cb = Cavebot::new(
            vec![
                labeled("start", StepKind::Label),
                step(StepKind::Walk { hidcode: 0xA1, duration_ms: 100, interval_ms: 0 }),
                step(StepKind::Stand { until: StandUntil::MobsKilled(2), max_wait_ms: 30000 }),
                step(StepKind::Walk { hidcode: 0xA2, duration_ms: 100, interval_ms: 0 }),
                step(StepKind::Goto { target_label: "start".into(), target_idx: 0 }),
            ],
            true, 30,
        );

        // Tick 0: Label → Walk 0xA1 emit.
        let mut c = ctx_full(0, None, None, 0, false);
        assert_eq!(cb.tick(&mut c), CavebotAction::KeyTap(0xA1));

        // Tick 3 (100ms @ 30fps): walk termina, entra a Stand (0 kills).
        let mut c = ctx_full(3, None, None, 0, false);
        assert_eq!(cb.tick(&mut c), CavebotAction::Idle);
        assert!(matches!(&cb.steps[cb.current.unwrap()].kind, StepKind::Stand { .. }));

        // Tick 10: simula 2 kills desde tick 3, Stand completa → walk back emit.
        let mut c = ctx_full(10, None, None, 2, false);
        assert_eq!(cb.tick(&mut c), CavebotAction::KeyTap(0xA2));

        // Tick 13: walk back termina → Goto → salta a start (idx 0, Label),
        // avanza al Walk 0xA1, emite. Pero OJO: el counter de kills sigue a 2,
        // así que el próximo Stand con MobsKilled(2) completará inmediatamente.
        let mut c = ctx_full(13, None, None, 2, false);
        assert_eq!(cb.tick(&mut c), CavebotAction::KeyTap(0xA1));
    }

    // ── Edge cases ────────────────────────────────────────────────────

    #[test]
    fn infinite_goto_loop_breaks_after_safety_limit() {
        // Goto → Goto → Goto... sin step emitter. Debe pausar por safety.
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Goto { target_label: "x".into(), target_idx: 1 }),
                step(StepKind::Goto { target_label: "x".into(), target_idx: 0 }),
            ],
            false, 30,
        );
        let mut c = ctx(0);
        let action = cb.tick(&mut c);
        assert_eq!(action, CavebotAction::Idle);
        assert_eq!(cb.current, None, "safety limit debe desactivar el cavebot");
    }

    #[test]
    fn finished_returns_finished_action() {
        let mut cb = Cavebot::new(
            vec![step(StepKind::Hotkey { hidcode: 0xAA })],
            false, 30,
        );
        // Primer tick: emit y avanza a None (no loop).
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::KeyTap(0xAA));
        // Segundo tick: Finished.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Finished);
    }

    // ── NpcDialog (Fase D) ────────────────────────────────────────────

    #[test]
    fn npc_dialog_emits_phrases_in_order_then_waits() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::NpcDialog {
                    phrases: vec!["hi".into(), "trade".into()],
                    wait_prompt_ms: 100,
                }),
                step(StepKind::Hotkey { hidcode: 0xBB }),
            ],
            false, 30,
        );
        // Tick 0: primera frase → Say("hi").
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Say("hi".into()));
        // Tick 1: segunda frase → Say("trade").
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Say("trade".into()));
        // Tick 2: sin más frases → empezar wait.
        assert_eq!(cb.tick(&mut ctx(2)), CavebotAction::Idle);
        // Tick 4 (3 ticks = 100ms después): wait NO cumplido aún (2+3=5).
        assert_eq!(cb.tick(&mut ctx(4)), CavebotAction::Idle);
        // Tick 5: wait cumplido (5-2=3 ticks >= 3), avanza al Hotkey y emite.
        assert_eq!(cb.tick(&mut ctx(5)), CavebotAction::KeyTap(0xBB));
    }

    #[test]
    fn npc_dialog_with_zero_wait_advances_immediately() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::NpcDialog {
                    phrases: vec!["hi".into()],
                    wait_prompt_ms: 0,
                }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Say("hi".into()));
        // Tick 1: sin frases pendientes, wait=0 → avanza y emite Hotkey.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::KeyTap(0xCC));
    }

    #[test]
    fn npc_dialog_resets_on_restart_current_step() {
        let mut cb = Cavebot::new(
            vec![step(StepKind::NpcDialog {
                phrases: vec!["hi".into(), "trade".into()],
                wait_prompt_ms: 500,
            })],
            false, 30,
        );
        // Emitir la primera frase.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Say("hi".into()));
        // Simular interrupción por combate + restart.
        cb.restart_current_step(100);
        // Tras restart, debe reemitir desde la primera frase.
        let mut c = ctx(100);
        assert_eq!(cb.tick(&mut c), CavebotAction::Say("hi".into()));
    }

    // ── OpenNpcTrade (greeting + click en bag button) ─────────────────

    /// Fase 0 → 1 → 2 → 3 → advance con 1 sola frase:
    ///   tick 0: Say("hi"), phase→1 no aún (phrase_idx++, sigue en 0)
    ///   tick 1: transición a phase 1, registra phase_start=1, Idle
    ///   tick 2..24: Idle (wait_button_ms=800 → 24 ticks @ 30Hz)
    ///   tick 25: Click bag button, phase→2, phase_start=25
    ///   tick 26..39: Idle (POST_CLICK_WAIT_MS=500 → 15 ticks @ 30Hz)
    ///   tick 40: advance → next step emits
    #[test]
    fn open_npc_trade_full_phase_progression() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::OpenNpcTrade {
                    greeting_phrases: vec!["hi".into()],
                    bag_button_vx: 350,
                    bag_button_vy: 400,
                    wait_button_ms: 800,
                }),
                step(StepKind::Hotkey { hidcode: 0xBB }),
            ],
            false, 30,
        );
        // Tick 0: primera frase → Say("hi"). phrase_idx=1 pero aún en fase 0.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Say("hi".into()));

        // Tick 1: sin más frases, transiciona a fase 1 con phase_start=1
        // y devuelve Idle.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Idle);

        // 800ms @ 30fps = 24 ticks. phase_start=1 → click emite en tick 25
        // (25-1=24 >= 24).
        for t in 2..25 {
            assert_eq!(
                cb.tick(&mut ctx(t)), CavebotAction::Idle,
                "phase 1 wait tick {}", t
            );
        }
        // Tick 25: wait cumplido → Click bag button → phase→2.
        assert_eq!(
            cb.tick(&mut ctx(25)),
            CavebotAction::Click { vx: 350, vy: 400 }
        );

        // 500ms @ 30fps = 15 ticks. phase_start=25 → advance en tick 40.
        for t in 26..40 {
            assert_eq!(
                cb.tick(&mut ctx(t)), CavebotAction::Idle,
                "phase 2 wait tick {}", t
            );
        }
        // Tick 40: post-click wait cumplido → advance al Hotkey → emit.
        assert_eq!(cb.tick(&mut ctx(40)), CavebotAction::KeyTap(0xBB));
    }

    /// Con múltiples greeting_phrases, se emiten una por tick antes de
    /// pasar a fase 1.
    #[test]
    fn open_npc_trade_emits_multiple_greeting_phrases_in_order() {
        let mut cb = Cavebot::new(
            vec![step(StepKind::OpenNpcTrade {
                greeting_phrases: vec!["hi".into(), "yes".into()],
                bag_button_vx: 100,
                bag_button_vy: 200,
                wait_button_ms: 0, // sin wait para test focused en fase 0
            })],
            false, 30,
        );
        // Tick 0: primera frase.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Say("hi".into()));
        // Tick 1: segunda frase.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Say("yes".into()));
    }

    /// Con wait_button_ms=0, fase 1 cumple en el tick de transición (wait=0).
    #[test]
    fn open_npc_trade_with_zero_wait_button_ms_advances_fast() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::OpenNpcTrade {
                    greeting_phrases: vec!["hi".into()],
                    bag_button_vx: 42,
                    bag_button_vy: 43,
                    wait_button_ms: 0,
                }),
                step(StepKind::Hotkey { hidcode: 0xAA }),
            ],
            false, 30,
        );
        // Tick 0: Say.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Say("hi".into()));
        // Tick 1: transición a fase 1 → Idle (el click no se emite el mismo
        // tick; se chequea el timer en el siguiente tick).
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Idle);
        // Tick 2: fase 1 chequea wait_ticks=0 → cumple → emit Click.
        assert_eq!(
            cb.tick(&mut ctx(2)),
            CavebotAction::Click { vx: 42, vy: 43 }
        );
    }

    /// restart_current_step (tras salir de Fighting/Emergency) debe
    /// resetear el estado interno del step — debe re-emitir desde la
    /// primera frase.
    #[test]
    fn open_npc_trade_resets_on_restart_current_step() {
        let mut cb = Cavebot::new(
            vec![step(StepKind::OpenNpcTrade {
                greeting_phrases: vec!["hi".into(), "yes".into()],
                bag_button_vx: 1,
                bag_button_vy: 2,
                wait_button_ms: 500,
            })],
            false, 30,
        );
        // Consumir la primera frase.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Say("hi".into()));
        // Simular restart (combate + return to walking).
        cb.restart_current_step(100);
        // Tras restart, debe re-emitir desde la primera frase.
        let mut c = ctx(100);
        assert_eq!(cb.tick(&mut c), CavebotAction::Say("hi".into()));
    }

    // ── A1: kills_at_step_start baseline ──────────────────────────────

    #[test]
    fn stand_mobs_killed_uses_delta_not_total() {
        // Stand { until: MobsKilled(2) } debe esperar 2 kills DESDE que
        // entró al Stand, no 2 kills totales de la sesión.
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Wait { duration_ms: 0 }),  // step 0: skip
                step(StepKind::Stand { until: StandUntil::MobsKilled(2), max_wait_ms: 0 }),
            ],
            false, 30,
        );
        // tick 0: Wait(0) → advance al Stand. Ya hay 10 kills globales.
        let mut c0 = ctx_full(0, None, None, 10, true);
        assert_eq!(cb.tick(&mut c0), CavebotAction::Idle); // Stand comienza
        assert_eq!(cb.current, Some(1));

        // tick 1: 10 kills globales (mismos) → delta = 0 → no avanza.
        let mut c1 = ctx_full(1, None, None, 10, true);
        assert_eq!(cb.tick(&mut c1), CavebotAction::Idle);
        assert_eq!(cb.current, Some(1));

        // tick 2: 11 kills → delta = 1 → aún no.
        let mut c2 = ctx_full(2, None, None, 11, true);
        assert_eq!(cb.tick(&mut c2), CavebotAction::Idle);
        assert_eq!(cb.current, Some(1));

        // tick 3: 12 kills → delta = 2 → avanza.
        let mut c3 = ctx_full(3, None, None, 12, false);
        assert_eq!(cb.tick(&mut c3), CavebotAction::Finished);
    }

    // ── A2: restart_current_step resets stuck counter ─────────────────

    #[test]
    fn restart_current_step_resets_stuck_walk_ticks() {
        let mut cb = Cavebot::new(
            vec![step(StepKind::Walk { hidcode: 0xAA, duration_ms: 5000, interval_ms: 400 })],
            true, 30,
        );
        // ctx con is_moving = Some(false) para que stuck detection se active.
        let stuck_ctx = |t| TickContext { tick: t, is_moving: Some(false), ..Default::default() };
        // Emit primero.
        assert_eq!(cb.tick(&mut stuck_ctx(0)), CavebotAction::KeyTap(0xAA));
        // Simular 30 ticks sin movimiento → stuck counter sube.
        for t in 1..=30 {
            cb.tick(&mut stuck_ctx(t));
        }
        assert!(cb.stuck_walk_ticks > 0);

        // Restart (como haría el FSM al volver de Fighting).
        cb.restart_current_step(100);
        assert_eq!(cb.stuck_walk_ticks, 0);
    }

    // ── A3: tick_walk helper consistency ──────────────────────────────

    #[test]
    fn skip_if_blocked_walk_matches_direct_walk() {
        // Un Walk directo y un SkipIfBlocked(Walk) deben emitir igual
        // (salvo stuck detection, que está deshabilitada en SkipIfBlocked).
        let walk = StepKind::Walk { hidcode: 0xBB, duration_ms: 300, interval_ms: 100 };
        let mut cb_walk = Cavebot::new(vec![step(walk.clone())], false, 30);

        let skip = StepKind::SkipIfBlocked {
            inner: Box::new(walk),
            max_wait_ms: 10000,
        };
        let mut cb_skip = Cavebot::new(vec![step(skip)], false, 30);

        // Ambos deben emitir KeyTap en tick 0.
        assert_eq!(cb_walk.tick(&mut ctx(0)), CavebotAction::KeyTap(0xBB));
        assert_eq!(cb_skip.tick(&mut ctx(0)), CavebotAction::KeyTap(0xBB));

        // Ambos Idle en ticks intermedios.
        for t in 1..3 {
            assert_eq!(cb_walk.tick(&mut ctx(t)), CavebotAction::Idle, "walk tick {t}");
            assert_eq!(cb_skip.tick(&mut ctx(t)), CavebotAction::Idle, "skip tick {t}");
        }

        // Ambos re-emit en tick 3 (100ms = 3 ticks @ 30Hz).
        assert_eq!(cb_walk.tick(&mut ctx(3)), CavebotAction::KeyTap(0xBB));
        assert_eq!(cb_skip.tick(&mut ctx(3)), CavebotAction::KeyTap(0xBB));
    }

    // ── Node (minimap click + displacement-based arrival) ──────────

    fn cavebot_with_prev_node(target: (i32, i32, i32), prev: (i32, i32, i32)) -> Cavebot {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Node { x: target.0, y: target.1, z: target.2, max_wait_ms: 30_000 }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        cb.prev_node = Some(prev);
        cb
    }

    fn ctx_mm(tick: u64, is_moving: Option<bool>, disp: Option<(i32, i32)>) -> TickContext {
        TickContext {
            tick,
            is_moving,
            minimap_center: Some((500, 300)),
            minimap_displacement: disp,
            ..Default::default()
        }
    }

    /// Sin `game_coords` disponible, el primer Node espera tile-hashing
    /// durante SEED_WAIT_TICKS (60) y tras timeout cae al comportamiento
    /// legacy: registra target como baseline y avanza. Test simula los
    /// 60 ticks de wait + 1 para que advance.
    #[test]
    fn node_first_fallback_when_no_game_coords() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Node { x: 100, y: 200, z: 7, max_wait_ms: 30_000 }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        // SEED_WAIT_TICKS=60: los primeros 59 ticks (seed_wait=1..59) devuelven
        // Idle sin avanzar. En el tick 60 (seed_wait=60), el threshold se
        // alcanza y cae al fallback que registra target como baseline + advance.
        for t in 0..59 {
            assert_eq!(cb.tick(&mut ctx_mm(t, None, None)), CavebotAction::Idle);
        }
        // Tick 59 = 60th iteration: threshold reached → fallback + advance.
        assert_eq!(cb.tick(&mut ctx_mm(59, None, None)), CavebotAction::KeyTap(0xCC));
        assert_eq!(cb.prev_node, Some((100, 200, 7)));
    }

    /// Con `game_coords` real disponible, el primer Node semilla desde ahí
    /// en vez de saltar — lo que permite que el cavebot CAMINE al target
    /// en vez de asumir que ya está en esa posición.
    #[test]
    fn node_first_seeds_from_game_coords() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Node { x: 110, y: 205, z: 7, max_wait_ms: 30_000 }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        // Char está realmente en (100, 200, 7). Seed debe usar esto, no el target.
        let ctx_with_coords = |tick: u64| {
            let mut c = ctx_mm(tick, Some(false), None);
            c.game_coords = Some((100, 200, 7));
            c
        };
        // Tick 0: seed desde (100,200,7) — Idle (return None en phase 0).
        assert_eq!(cb.tick(&mut ctx_with_coords(0)), CavebotAction::Idle);
        assert_eq!(cb.prev_node, Some((100, 200, 7)));
        // Tick 1: prev_node existe, dx=10 dy=5 → emite Click al minimap.
        let action = cb.tick(&mut ctx_with_coords(1));
        assert!(matches!(action, CavebotAction::Click { .. }),
                "expected Click, got {:?}", action);
    }

    /// Z mismatch al arrival emite SafetyPause con reason explícito.
    #[test]
    fn node_arrival_z_mismatch_triggers_safety_pause() {
        // Cavebot con prev_node ya seteado al XY del target pero en z distinto.
        // Target z=7 (lo que el cavebot espera), prev.z=6 (char estaba en
        // otro piso antes). dx==dy==0 → branch de zero-offset. Ahí validamos z.
        // cavebot_with_prev_node(target, prev):
        let mut cb = cavebot_with_prev_node((100, 200, 7), (100, 200, 6));
        // Char real reporta z=6 (un piso abajo del target z=7)
        let mut ctx = ctx_mm(0, Some(false), None);
        ctx.game_coords = Some((100, 200, 6));
        let action = cb.tick(&mut ctx);
        match action {
            CavebotAction::SafetyPause { reason } => {
                assert!(reason.contains("node_z_mismatch"), "reason: {}", reason);
            }
            other => panic!("expected SafetyPause, got {:?}", other),
        }
    }

    #[test]
    fn node_zero_offset_skips() {
        let mut cb = cavebot_with_prev_node((100, 200, 7), (100, 200, 6));
        assert_eq!(cb.tick(&mut ctx_mm(0, None, None)), CavebotAction::KeyTap(0xCC));
    }

    #[test]
    fn node_emits_click_at_minimap_offset() {
        // prev=(100,200), target=(110,205) → dx=10, dy=5 → click (520, 310)
        let mut cb = cavebot_with_prev_node((110, 205, 7), (100, 200, 7));
        assert_eq!(
            cb.tick(&mut ctx_mm(0, Some(false), None)),
            CavebotAction::Click { vx: 520, vy: 310 }
        );
        assert_eq!(cb.node_phase, 1);
        assert_eq!(cb.node_expect_dx, 20); // 10 * 2
        assert_eq!(cb.node_expect_dy, 10); // 5 * 2
    }

    #[test]
    fn node_arrives_by_displacement() {
        // target 10 tiles right → expect (20, 0)
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // click

        // Walk: 10 frames with displacement (-2, 0) each → accum (20, 0)
        for t in 1..=10 {
            cb.tick(&mut ctx_mm(t, Some(true), Some((-2, 0))));
        }
        assert_eq!(cb.node_accum_dx, 20);

        // Stop: 10 idle ticks → arrived (arrived_idle_ticks=10)
        for t in 11..20 {
            assert_eq!(cb.tick(&mut ctx_mm(t, Some(false), None)), CavebotAction::Idle);
        }
        assert_eq!(cb.tick(&mut ctx_mm(20, Some(false), None)), CavebotAction::KeyTap(0xCC));
        assert_eq!(cb.prev_node, Some((110, 200, 7)));
    }

    #[test]
    fn node_direction_pause_does_not_false_trigger() {
        // target (5, 5) → expect (10, 10)
        let mut cb = cavebot_with_prev_node((105, 205, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // click

        // Walk 3 tiles right → accum (6, 0)
        for t in 1..=3 {
            cb.tick(&mut ctx_mm(t, Some(true), Some((-2, 0))));
        }
        // Direction pause: 10 frames idle, NO displacement
        // manhattan = |10-6| + |10-0| = 14 > tolerance (4)
        for t in 4..=13 {
            assert_eq!(cb.tick(&mut ctx_mm(t, Some(false), None)), CavebotAction::Idle);
        }
        // NOT arrived — manhattan still 14
        assert_eq!(cb.node_phase, 1);
    }

    #[test]
    fn node_reclick_when_stuck_midwalk() {
        // target 10 tiles right → expect (20, 0)
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // click

        // Walk 3 tiles → accum (6, 0)
        for t in 1..=3 {
            cb.tick(&mut ctx_mm(t, Some(true), Some((-2, 0))));
        }
        // Stuck: 60 idle ticks → re-click
        for t in 4..63 {
            cb.tick(&mut ctx_mm(t, Some(false), None));
        }
        // Tick 63: idle_ticks >= 60 → re-click with remaining (14/2=7 tiles)
        let action = cb.tick(&mut ctx_mm(63, Some(false), None));
        // Remaining: expect(20,0) - accum(6,0) = (14,0) → 7 tiles → click at (514, 300)
        assert_eq!(action, CavebotAction::Click { vx: 514, vy: 300 });
        assert_eq!(cb.node_reclick_count, 1);
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn node_max_reclicks_then_advance() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // initial click

        // Simulate 3 re-clicks by manipulating state directly
        cb.node_reclick_count = 3;
        cb.node_saw_displacement = true;
        cb.node_accum_dx = 4; // partial — not at target (expect=20)
        cb.node_idle_ticks = 0;

        // Feed idle ticks until max reclicks advance fires.
        // It should emit KeyTap(0xCC) from the sentinel Hotkey on the advance tick.
        let mut found = false;
        for t in 1..=120 {
            let action = cb.tick(&mut ctx_mm(t, Some(false), None));
            if action == CavebotAction::KeyTap(0xCC) {
                found = true;
                break;
            }
        }
        assert!(found, "node should advance after max reclicks exhausted");
    }

    #[test]
    fn node_fallback_no_displacement() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // click

        // Walk with is_moving but ZERO displacement
        for t in 1..=10 {
            cb.tick(&mut ctx_mm(t, Some(true), None));
        }
        assert!(cb.node_saw_moving);
        assert!(!cb.node_saw_displacement);

        // Re-click fires at 60 idle ticks, arrival at 90 idle ticks.
        // Feed idle until arrival. Re-click at t=70 resets idle+saw_moving,
        // so char must "walk" again before next idle window.
        for t in 11..70 {
            cb.tick(&mut ctx_mm(t, Some(false), None));
        }
        // t=70: re-click fires (idle=60), resets saw_moving=false, idle=0.
        let reclick = cb.tick(&mut ctx_mm(70, Some(false), None));
        assert_eq!(reclick, CavebotAction::Click { vx: 520, vy: 300 });

        // Simulate walk again after re-click.
        for t in 71..=75 {
            cb.tick(&mut ctx_mm(t, Some(true), None));
        }
        // Then idle until max reclicks exhausted or NODE_FALLBACK_STOP_TICKS.
        // After 3 reclicks, arrival fires immediately when idle >= 90 or reclicks >= 3.
        // Push reclick_count to max by fast-forwarding.
        cb.node_reclick_count = 3;
        cb.node_saw_moving = true;
        cb.node_idle_ticks = 0;
        // Now next idle tick should trigger arrival (reclicks exhausted).
        assert_eq!(cb.tick(&mut ctx_mm(76, Some(false), None)), CavebotAction::KeyTap(0xCC));
        // F2: prev_node = estimated_position = prev(100,200,7) since accum=0.
        assert_eq!(cb.prev_node, Some((100, 200, 7)));
    }

    #[test]
    fn node_arrives_when_is_moving_none() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, None, None)); // click

        // Walk: displacement con is_moving=None. idle se acumula en paralelo.
        // expect=(20,0). Tick 10: accum=20, manhattan=0, idle=10 ≥ 10 → arrival.
        for t in 1..10 {
            assert_eq!(cb.tick(&mut ctx_mm(t, None, Some((-2, 0)))), CavebotAction::Idle);
        }
        assert_eq!(cb.tick(&mut ctx_mm(10, None, Some((-2, 0)))), CavebotAction::KeyTap(0xCC));
        assert_eq!(cb.prev_node, Some((110, 200, 7)));
    }

    #[test]
    fn node_sensor_degraded_fallback() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, None, None)); // click

        // is_moving=None siempre, sin displacement.
        // F6 click-missed retries fire at ticks 30 and 60. Sensor-degraded at 90.
        for t in 1..30 {
            assert_eq!(cb.tick(&mut ctx_mm(t, None, None)), CavebotAction::Idle);
        }
        // Tick 30: F6 retry #1
        assert_eq!(cb.tick(&mut ctx_mm(30, None, None)), CavebotAction::Click { vx: 520, vy: 300 });
        for t in 31..60 {
            assert_eq!(cb.tick(&mut ctx_mm(t, None, None)), CavebotAction::Idle);
        }
        // Tick 60: F6 retry #2
        assert_eq!(cb.tick(&mut ctx_mm(60, None, None)), CavebotAction::Click { vx: 520, vy: 300 });
        // After max retries (2), sensor-degraded fallback when none_moving >= 90.
        // Ticks 30 and 60 were Click returns (no idle tracking), so need 2 extra.
        for t in 61..92 {
            assert_eq!(cb.tick(&mut ctx_mm(t, None, None)), CavebotAction::Idle);
        }
        assert_eq!(cb.tick(&mut ctx_mm(92, None, None)), CavebotAction::KeyTap(0xCC));
    }

    #[test]
    fn node_reclick_counts_when_minimap_center_none() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // click (phase 0 usa minimap_center)

        // Walk 3 tiles con displacement.
        for t in 1..=3 {
            cb.tick(&mut ctx_mm(t, Some(true), Some((-2, 0))));
        }
        assert_eq!(cb.node_accum_dx, 6);

        // Ahora minimap_center desaparece (anchor lost).
        let ctx_no_mm = |tick: u64| TickContext {
            tick,
            is_moving: Some(false),
            minimap_center: None,
            minimap_displacement: None,
            ..Default::default()
        };

        // 60 idle ticks → re-click, pero minimap_center=None → reclick_count++.
        for t in 4..64 {
            cb.tick(&mut ctx_no_mm(t));
        }
        assert!(cb.node_reclick_count >= 1);

        // Fast-forward: max reclicks → advance con estimated position.
        cb.node_reclick_count = 3;
        cb.node_idle_ticks = 60;
        let action = cb.tick(&mut ctx_no_mm(200));
        assert_eq!(action, CavebotAction::KeyTap(0xCC));
        // estimated: prev(100) + accum(6)/2 = 103.
        assert_eq!(cb.prev_node, Some((103, 200, 7)));
    }

    #[test]
    fn node_fallback_reclicks_before_arrival() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // click

        // Walk sin displacement.
        for t in 1..=5 {
            cb.tick(&mut ctx_mm(t, Some(true), None));
        }
        assert!(cb.node_saw_moving);

        // 59 idle ticks (no re-click yet).
        for t in 6..65 {
            assert_eq!(cb.tick(&mut ctx_mm(t, Some(false), None)), CavebotAction::Idle);
        }
        // Tick 65: idle=60 → re-click fires.
        let action = cb.tick(&mut ctx_mm(65, Some(false), None));
        assert_eq!(action, CavebotAction::Click { vx: 520, vy: 300 });
        assert_eq!(cb.node_reclick_count, 1);
    }

    #[test]
    fn node_displacement_zero_activates_displacement_path() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // click

        // Some((0,0)) = sensor confirma no movimiento → displacement path activo.
        for t in 1..=5 {
            cb.tick(&mut ctx_mm(t, Some(false), Some((0, 0))));
        }
        assert!(cb.node_saw_displacement, "Some((0,0)) debe activar displacement path");
    }

    // ── F2: hot-reload smooth helpers ─────────────────────────────────

    #[test]
    fn find_label_returns_index_for_existing_label() {
        let cb = Cavebot::new(
            vec![
                labeled("start", StepKind::Label),
                step(StepKind::Hotkey { hidcode: 0xBB }),
                labeled("hunt", StepKind::Label),
            ],
            false, 30,
        );
        assert_eq!(cb.find_label("start"), Some(0));
        assert_eq!(cb.find_label("hunt"), Some(2));
        assert_eq!(cb.find_label("missing"), None);
    }

    #[test]
    fn find_label_only_matches_label_step_kind() {
        let cb = Cavebot::new(
            vec![
                // labeled() con StepKind::Hotkey — no debe matchear como label.
                labeled("start", StepKind::Hotkey { hidcode: 0xAA }),
                // Label verdadero
                labeled("hunt", StepKind::Label),
            ],
            false, 30,
        );
        assert_eq!(cb.find_label("start"), None); // no es StepKind::Label
        assert_eq!(cb.find_label("hunt"), Some(1));
    }

    #[test]
    fn current_label_name_returns_most_recent_label() {
        let mut cb = Cavebot::new(
            vec![
                labeled("start", StepKind::Label),
                step(StepKind::Hotkey { hidcode: 0xAA }),
                labeled("hunt", StepKind::Label),
                step(StepKind::Hotkey { hidcode: 0xBB }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        // current=0 (start label) → "start"
        assert_eq!(cb.current_label_name(), Some("start".to_string()));

        // current=1 (hotkey after start) → still "start"
        cb.current = Some(1);
        assert_eq!(cb.current_label_name(), Some("start".to_string()));

        // current=3 (after "hunt" label) → "hunt"
        cb.current = Some(3);
        assert_eq!(cb.current_label_name(), Some("hunt".to_string()));

        // current=4 (further from hunt) → still "hunt"
        cb.current = Some(4);
        assert_eq!(cb.current_label_name(), Some("hunt".to_string()));
    }

    #[test]
    fn current_label_name_returns_none_when_no_labels() {
        let cb = Cavebot::new(
            vec![
                step(StepKind::Hotkey { hidcode: 0xAA }),
                step(StepKind::Hotkey { hidcode: 0xBB }),
            ],
            false, 30,
        );
        assert_eq!(cb.current_label_name(), None);
    }

    #[test]
    fn jump_to_label_succeeds_and_resets_state() {
        let mut cb = Cavebot::new(
            vec![
                labeled("start", StepKind::Label),
                step(StepKind::Hotkey { hidcode: 0xAA }),
                labeled("hunt", StepKind::Label),
                step(StepKind::Hotkey { hidcode: 0xBB }),
            ],
            false, 30,
        );
        cb.current = Some(1);
        // Simular algo de estado.
        cb.loot_clicks_done = 2;
        cb.npc_phrase_idx = 5;

        assert!(cb.jump_to_label("hunt", 100));
        assert_eq!(cb.current, Some(2));
        assert_eq!(cb.started_tick, 100);
        assert_eq!(cb.loot_clicks_done, 0);  // reset
        assert_eq!(cb.npc_phrase_idx, 0);     // reset
    }

    #[test]
    fn jump_to_label_returns_false_for_missing_label() {
        let mut cb = Cavebot::new(
            vec![labeled("start", StepKind::Label)],
            false, 30,
        );
        assert!(!cb.jump_to_label("nonexistent", 50));
        assert_eq!(cb.current, Some(0));  // sin cambios
    }

    // ── Deposit / BuyItem / CheckSupplies ─────────────────────────────

    #[test]
    fn deposit_emits_right_click_wait_left_click() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Deposit {
                    chest_vx: 1850, chest_vy: 300,
                    stow_vx: 1900, stow_vy: 340,
                    menu_wait_ms: 100, process_ms: 100,
                }),
                step(StepKind::Hotkey { hidcode: 0xAA }),
            ],
            false, 30,
        );
        // Tick 0: right-click en chest → phase 1.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::RightClick { vx: 1850, vy: 300 });
        assert_eq!(cb.deposit_phase, 1);
        // 100ms @ 30fps = 3 ticks. Tick 1,2: idle esperando menu.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(2)), CavebotAction::Idle);
        // Tick 3: menu_wait expiró → left-click stow → phase 2.
        assert_eq!(cb.tick(&mut ctx(3)), CavebotAction::Click { vx: 1900, vy: 340 });
        assert_eq!(cb.deposit_phase, 2);
        // Tick 4,5: idle esperando process.
        assert_eq!(cb.tick(&mut ctx(4)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(5)), CavebotAction::Idle);
        // Tick 6: process_ms expiró → advance → Hotkey 0xAA.
        assert_eq!(cb.tick(&mut ctx(6)), CavebotAction::KeyTap(0xAA));
    }

    #[test]
    fn buy_item_legacy_emits_select_then_confirm_loop() {
        // Flujo legacy: amount_* = None → N clicks de confirm.
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::BuyItem {
                    item_vx: 120, item_vy: 340,
                    amount_vx: None, amount_vy: None,
                    confirm_vx: 300, confirm_vy: 520,
                    quantity: 3, spacing_ms: 100,
                }),
                step(StepKind::Hotkey { hidcode: 0xBB }),
            ],
            false, 30,
        );
        // Tick 0: click select item → phase 1.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Click { vx: 120, vy: 340 });
        assert_eq!(cb.buy_phase, 1);
        // 100ms = 3 ticks spacing. Ticks 1,2: idle.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(2)), CavebotAction::Idle);
        // Tick 3: primer confirm.
        assert_eq!(cb.tick(&mut ctx(3)), CavebotAction::Click { vx: 300, vy: 520 });
        assert_eq!(cb.buy_clicks_done, 1);
        // Ticks 4,5: idle.
        assert_eq!(cb.tick(&mut ctx(4)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(5)), CavebotAction::Idle);
        // Tick 6: segundo confirm.
        assert_eq!(cb.tick(&mut ctx(6)), CavebotAction::Click { vx: 300, vy: 520 });
        // Tick 9: tercer y último confirm.
        cb.tick(&mut ctx(7));
        cb.tick(&mut ctx(8));
        assert_eq!(cb.tick(&mut ctx(9)), CavebotAction::Click { vx: 300, vy: 520 });
        assert_eq!(cb.buy_clicks_done, 3);
        // Tick 10: advance → Hotkey 0xBB.
        assert_eq!(cb.tick(&mut ctx(10)), CavebotAction::KeyTap(0xBB));
    }

    /// Flujo Amount (Tibia 12): valida que las 9 fases se ejecuten en orden
    /// con los KeyTap de cada dígito en medio y un solo click de confirm al
    /// final. Usa quantity=12 → tipea '1', '2' (HID 0x1E, 0x1F).
    ///
    /// Timing con fps=30, spacing_ms=100:
    ///   - spacing_ticks        = 3
    ///   - post_item_ticks      = 6  (200ms)
    ///   - post_amount_ticks    = 5  (150ms, ceil)
    ///   - post_digits_ticks    = 5  (150ms, ceil)
    ///   - inter_digit_ticks    = 2  (50ms, ceil)
    #[test]
    fn buy_item_amount_flow_types_digits_then_single_confirm() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::BuyItem {
                    item_vx: 120, item_vy: 340,
                    amount_vx: Some(500), amount_vy: Some(500),
                    confirm_vx: 900, confirm_vy: 700,
                    quantity: 12, spacing_ms: 100,
                }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );

        // t=0: phase 0 → click item, phase=1.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Click { vx: 120, vy: 340 });
        assert_eq!(cb.buy_phase, 1);

        // t=1..5: phase 1 esperando post_item (6 ticks) → Idle.
        for t in 1..=5 {
            assert_eq!(cb.tick(&mut ctx(t)), CavebotAction::Idle, "t={}", t);
        }

        // t=6: phase 1 cumplido → click amount, phase=2.
        assert_eq!(cb.tick(&mut ctx(6)), CavebotAction::Click { vx: 500, vy: 500 });
        assert_eq!(cb.buy_phase, 2);

        // t=7..10: phase 2 esperando post_amount (5 ticks) → Idle.
        for t in 7..=10 {
            assert_eq!(cb.tick(&mut ctx(t)), CavebotAction::Idle, "t={}", t);
        }

        // t=11: phase 2 cumplido → continue → phase 3 emite KeyTap('1').
        //       '1' → HID 0x1E.
        assert_eq!(cb.tick(&mut ctx(11)), CavebotAction::KeyTap(0x1E));
        assert_eq!(cb.buy_amount_digit_idx, 1);
        assert_eq!(cb.buy_phase, 4);

        // t=12: phase 4 entre dígitos, elapsed=1 < 2 → Idle.
        assert_eq!(cb.tick(&mut ctx(12)), CavebotAction::Idle);

        // t=13: phase 4 cumplido → phase 3 → KeyTap('2') = HID 0x1F.
        assert_eq!(cb.tick(&mut ctx(13)), CavebotAction::KeyTap(0x1F));
        assert_eq!(cb.buy_amount_digit_idx, 2);
        assert_eq!(cb.buy_phase, 4);

        // t=14..17: phase 4 post-digits wait (5 ticks) → Idle.
        for t in 14..=17 {
            assert_eq!(cb.tick(&mut ctx(t)), CavebotAction::Idle, "t={}", t);
        }

        // t=18: phase 4 cumplido → phase 5 → Click confirm (UNA sola vez).
        assert_eq!(cb.tick(&mut ctx(18)), CavebotAction::Click { vx: 900, vy: 700 });
        assert_eq!(cb.buy_phase, 6);

        // t=19, 20: phase 6 esperando spacing_ms (3 ticks) → Idle.
        assert_eq!(cb.tick(&mut ctx(19)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(20)), CavebotAction::Idle);

        // t=21: phase 6 cumplido → advance → Hotkey 0xCC.
        assert_eq!(cb.tick(&mut ctx(21)), CavebotAction::KeyTap(0xCC));
    }

    /// El flujo Amount debe emitir EXACTAMENTE un click de confirm,
    /// independientemente de quantity. Ese es el cambio semántico clave
    /// vs el legacy (que emitía N clicks).
    #[test]
    fn buy_item_amount_flow_emits_exactly_one_confirm_click() {
        // quantity=100 → 3 dígitos '1','0','0'. Si el flujo legacy se
        // activara por error, vendrían 100 clicks al confirm.
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::BuyItem {
                    item_vx: 10, item_vy: 20,
                    amount_vx: Some(30), amount_vy: Some(40),
                    confirm_vx: 50, confirm_vy: 60,
                    quantity: 100, spacing_ms: 100,
                }),
                step(StepKind::Hotkey { hidcode: 0xDD }),
            ],
            false, 30,
        );

        let mut confirm_click_count = 0;
        let mut keytaps: Vec<u8> = Vec::new();
        let mut advanced = false;

        // Simular hasta advance o un límite generoso.
        for t in 0..200 {
            let action = cb.tick(&mut ctx(t));
            match action {
                CavebotAction::Click { vx: 50, vy: 60 } => confirm_click_count += 1,
                CavebotAction::KeyTap(hid) => {
                    if hid == 0xDD {
                        // Llegamos al Hotkey siguiente → advance ejecutado.
                        advanced = true;
                        break;
                    }
                    keytaps.push(hid);
                }
                _ => {}
            }
        }

        assert!(advanced, "BuyItem amount flow no avanzó tras 200 ticks");
        assert_eq!(
            confirm_click_count, 1,
            "amount flow debe emitir EXACTAMENTE 1 click al confirm, no {}",
            confirm_click_count
        );
        // 3 dígitos tipeados: '1', '0', '0' → 0x1E, 0x27, 0x27.
        assert_eq!(keytaps, vec![0x1E, 0x27, 0x27]);
    }

    /// Si solo uno de amount_vx/amount_vy está presente, el parser debe
    /// rechazar explícitamente para evitar ambigüedad silenciosa.
    #[test]
    fn buy_item_parser_rejects_only_one_amount_coord() {
        use crate::cavebot::parser;
        use std::io::Write;
        use std::path::Path;

        let tmp = std::env::temp_dir().join("buy_item_half_amount.toml");
        let src = r#"
            [[step]]
            kind = "buy_item"
            item_vx = 100
            item_vy = 200
            amount_vx = 300
            confirm_vx = 400
            confirm_vy = 500
            quantity = 10
        "#;
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(src.as_bytes()).unwrap();
        drop(f);
        let r = parser::load(Path::new(&tmp), 30);
        let _ = std::fs::remove_file(&tmp);
        assert!(r.is_err(), "buy_item con solo amount_vx debe ser error");
    }

    /// Backward-compat del parser: un buy_item sin amount_* debe parsear OK
    /// y crear un StepKind::BuyItem con amount_vx=None, amount_vy=None.
    #[test]
    fn buy_item_parser_accepts_no_amount_fields_legacy() {
        use crate::cavebot::parser;
        use std::io::Write;
        use std::path::Path;

        let tmp = std::env::temp_dir().join("buy_item_legacy.toml");
        let src = r#"
            [[step]]
            kind = "buy_item"
            item_vx = 100
            item_vy = 200
            confirm_vx = 400
            confirm_vy = 500
            quantity = 5
            spacing_ms = 200
        "#;
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(src.as_bytes()).unwrap();
        drop(f);
        let cb = parser::load(Path::new(&tmp), 30).expect("legacy buy_item debe parsear");
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(cb.steps.len(), 1);
        match &cb.steps[0].kind {
            StepKind::BuyItem { amount_vx, amount_vy, quantity, spacing_ms, .. } => {
                assert_eq!(*amount_vx, None);
                assert_eq!(*amount_vy, None);
                assert_eq!(*quantity, 5);
                assert_eq!(*spacing_ms, 200);
            }
            _ => panic!("expected BuyItem"),
        }
    }

    /// Parser acepta buy_item con AMBOS amount_vx y amount_vy presentes
    /// y construye el StepKind con Some(_) en ambos.
    #[test]
    fn buy_item_parser_accepts_full_amount_fields() {
        use crate::cavebot::parser;
        use std::io::Write;
        use std::path::Path;

        let tmp = std::env::temp_dir().join("buy_item_amount.toml");
        let src = r#"
            [[step]]
            kind = "buy_item"
            item_vx = 100
            item_vy = 200
            amount_vx = 300
            amount_vy = 310
            confirm_vx = 400
            confirm_vy = 500
            quantity = 100
            spacing_ms = 250
        "#;
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(src.as_bytes()).unwrap();
        drop(f);
        let cb = parser::load(Path::new(&tmp), 30).expect("amount buy_item debe parsear");
        let _ = std::fs::remove_file(&tmp);
        match &cb.steps[0].kind {
            StepKind::BuyItem { amount_vx, amount_vy, quantity, .. } => {
                assert_eq!(*amount_vx, Some(300));
                assert_eq!(*amount_vy, Some(310));
                assert_eq!(*quantity, 100);
            }
            _ => panic!("expected BuyItem"),
        }
    }

    #[test]
    fn check_supplies_advances_when_all_ok() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::CheckSupplies {
                    requirements: vec![("mana_potion".into(), 2), ("uh".into(), 1)],
                    on_fail_label: "refill".into(),
                    on_fail_idx: 2,
                }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
                labeled("refill", StepKind::Hotkey { hidcode: 0xDD }),
            ],
            false, 30,
        );
        // Con 2 mana_potion y 1 uh en ctx → all_ok → advance → Hotkey 0xCC.
        let mut ctx = TickContext { tick: 0, ..Default::default() };
        ctx.inventory_counts.insert("mana_potion".into(), 2);
        ctx.inventory_counts.insert("uh".into(), 1);
        assert_eq!(cb.tick(&mut ctx), CavebotAction::KeyTap(0xCC));
    }

    #[test]
    fn check_supplies_jumps_to_on_fail_when_missing() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::CheckSupplies {
                    requirements: vec![("mana_potion".into(), 5)],
                    on_fail_label: "refill".into(),
                    on_fail_idx: 2,
                }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
                labeled("refill", StepKind::Hotkey { hidcode: 0xDD }),
            ],
            false, 30,
        );
        // Con solo 2 mana_potion (necesita 5) → jump a refill → Hotkey 0xDD.
        let mut ctx = TickContext { tick: 0, ..Default::default() };
        ctx.inventory_counts.insert("mana_potion".into(), 2);
        assert_eq!(cb.tick(&mut ctx), CavebotAction::KeyTap(0xDD));
    }

    #[test]
    fn has_item_condition_with_sufficient_count() {
        use crate::cavebot::step::Condition;
        let mut ctx = TickContext::default();
        ctx.inventory_counts.insert("mana_potion".into(), 5);
        let c1 = Condition::HasItem { name: "mana_potion".into(), min_count: 3 };
        let c2 = Condition::HasItem { name: "mana_potion".into(), min_count: 10 };
        let c3 = Condition::HasItem { name: "missing_item".into(), min_count: 1 };
        assert!(c1.eval(&ctx));
        assert!(!c2.eval(&ctx));
        assert!(!c3.eval(&ctx));
    }

    // ── StowAllItems stash-full detection (Task 2.3) ──────────────────
    //
    // Helper para construir un ctx con inventory_counts pre-poblado.
    fn ctx_inv(tick: u64, inv: &[(&str, u32)]) -> TickContext {
        let mut c = TickContext { tick, ..Default::default() };
        for (k, v) in inv {
            c.inventory_counts.insert((*k).into(), *v);
        }
        c
    }

    /// Avanza un tick del cavebot con un inventory snapshot dado,
    /// descartando la acción emitida (útil para llegar a un estado sin
    /// aserciones intermedias).
    fn tick_with_inv(cb: &mut Cavebot, tick: u64, inv: &[(&str, u32)]) -> CavebotAction {
        cb.tick(&mut ctx_inv(tick, inv))
    }

    #[test]
    fn stow_all_items_pauses_when_inventory_stable_two_iters() {
        // Inventario estable durante 2 iteraciones consecutivas → SafetyPause.
        // Config: menu_wait=100ms (3 ticks @ 30fps), process=100ms (3 ticks).
        // Una iter = right-click + wait(3) + click + wait(3) = 6 ticks.
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::StowAllItems {
                    slot_vx: 1600, slot_vy: 50,
                    menu_offset_x: 90, menu_offset_y: 197,
                    menu_wait_ms: 100, stow_process_ms: 100,
                    max_iterations: 8,
                }),
                step(StepKind::Hotkey { hidcode: 0xEE }),
            ],
            false, 30,
        );
        let inv = &[("mana_potion", 5), ("gold_coin", 20)];

        // ── Iter 1 ─────────────────────────────────────────────────────
        // Tick 0: phase 0 → right-click. Captura baseline en este tick.
        assert_eq!(tick_with_inv(&mut cb, 0, inv), CavebotAction::RightClick { vx: 1600, vy: 50 });
        assert!(cb.stow_baseline_counts.is_some(), "baseline capturado en iter 1 phase 0");
        assert_eq!(cb.stow_stale_iters, 0);
        // Ticks 1,2: phase 1 idle.
        for t in 1..=2 { assert_eq!(tick_with_inv(&mut cb, t, inv), CavebotAction::Idle); }
        // Tick 3: click menu → phase 2.
        assert_eq!(tick_with_inv(&mut cb, 3, inv), CavebotAction::Click { vx: 1690, vy: 247 });
        // Ticks 4,5: phase 2 idle.
        for t in 4..=5 { assert_eq!(tick_with_inv(&mut cb, t, inv), CavebotAction::Idle); }
        // Tick 6: process expira → iter++, inventario no cambió → stale=1,
        //         continua al phase 0 del iter 2 y emite right-click.
        assert_eq!(tick_with_inv(&mut cb, 6, inv), CavebotAction::RightClick { vx: 1600, vy: 50 });
        assert_eq!(cb.stow_stale_iters, 1, "1 iter sin drop");
        assert_eq!(cb.stow_iterations_done, 1);

        // ── Iter 2 ─────────────────────────────────────────────────────
        // Ticks 7..11: phase 1 → click menu → phase 2 idle.
        for t in 7..=8 { assert_eq!(tick_with_inv(&mut cb, t, inv), CavebotAction::Idle); }
        assert_eq!(tick_with_inv(&mut cb, 9, inv), CavebotAction::Click { vx: 1690, vy: 247 });
        for t in 10..=11 { assert_eq!(tick_with_inv(&mut cb, t, inv), CavebotAction::Idle); }
        // Tick 12: process del iter 2 expira → stale=2 → SafetyPause.
        let action = tick_with_inv(&mut cb, 12, inv);
        match action {
            CavebotAction::SafetyPause { reason } => {
                assert!(
                    reason.contains("stow:stash_full"),
                    "reason debe mencionar 'stow:stash_full': {}", reason
                );
                assert!(
                    reason.contains("mana_potion=5"),
                    "reason debe incluir el summary del baseline: {}", reason
                );
            }
            other => panic!("esperado SafetyPause tras 2 iters stale, got {:?}", other),
        }
        // Estado reseteado post-pause.
        assert!(cb.stow_baseline_counts.is_none(), "baseline reseteado tras pause");
        assert_eq!(cb.stow_stale_iters, 0);
        assert_eq!(cb.stow_iterations_done, 0);
    }

    #[test]
    fn stow_all_items_does_not_pause_when_drop_follows_stale_iter() {
        // 1 iter stale + 1 iter con drop → stale vuelve a 0, NO pause.
        // Confirma que un drop intermitente "resetea" el contador.
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::StowAllItems {
                    slot_vx: 1600, slot_vy: 50,
                    menu_offset_x: 90, menu_offset_y: 197,
                    menu_wait_ms: 100, stow_process_ms: 100,
                    max_iterations: 8,
                }),
                step(StepKind::Hotkey { hidcode: 0xEE }),
            ],
            false, 30,
        );
        let inv_full    = &[("mana_potion", 5), ("gold_coin", 20)];
        let inv_dropped = &[("mana_potion", 5), ("gold_coin", 18)]; // gold_coin bajó

        // ── Iter 1: inventario estable (mismo que el baseline capturado). ──
        assert_eq!(tick_with_inv(&mut cb, 0, inv_full), CavebotAction::RightClick { vx: 1600, vy: 50 });
        for t in 1..=2 { tick_with_inv(&mut cb, t, inv_full); }
        tick_with_inv(&mut cb, 3, inv_full);
        for t in 4..=5 { tick_with_inv(&mut cb, t, inv_full); }
        // Tick 6: iter 1 cierra con stale=1. Re-emite right-click del iter 2.
        tick_with_inv(&mut cb, 6, inv_full);
        assert_eq!(cb.stow_stale_iters, 1);

        // ── Iter 2: el inventory baja (gold_coin 20 → 18). Drop detectado ──
        //             al final del iter → stale=0 + baseline refreshed.
        for t in 7..=8 { tick_with_inv(&mut cb, t, inv_dropped); }
        tick_with_inv(&mut cb, 9, inv_dropped);
        for t in 10..=11 { tick_with_inv(&mut cb, t, inv_dropped); }
        // Tick 12: process expira → detect drop → stale=0 + baseline refreshed.
        //          NO pause, sigue iterando (emite right-click del iter 3).
        let action = tick_with_inv(&mut cb, 12, inv_dropped);
        assert_eq!(action, CavebotAction::RightClick { vx: 1600, vy: 50 });
        assert_eq!(cb.stow_stale_iters, 0, "drop detectado → stale reset");
        assert_eq!(cb.stow_iterations_done, 2);
        assert!(cb.stow_baseline_counts.is_some(), "baseline sigue presente (refreshed)");
        // El baseline refrescado debe reflejar el inv_dropped (no el original).
        let baseline = cb.stow_baseline_counts.as_ref().unwrap();
        assert_eq!(baseline.get("gold_coin").copied(), Some(18));
    }

    #[test]
    fn stow_all_items_completes_when_inventory_drops_every_iter() {
        // Inventory baja cada iter → stale siempre 0 → completa hasta
        // max_iterations sin pause y avanza al próximo step.
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::StowAllItems {
                    slot_vx: 1600, slot_vy: 50,
                    menu_offset_x: 90, menu_offset_y: 197,
                    menu_wait_ms: 100, stow_process_ms: 100,
                    max_iterations: 3,
                }),
                step(StepKind::Hotkey { hidcode: 0xEE }),
            ],
            false, 30,
        );
        // Inventories que van bajando iter a iter.
        let inv1 = &[("mana_potion", 5), ("gold_coin", 20)];
        let inv2 = &[("mana_potion", 5), ("gold_coin", 15)];
        let inv3 = &[("mana_potion", 5), ("gold_coin", 10)];

        // Iter 1 (tick 0..6): baseline capturado con inv1.
        tick_with_inv(&mut cb, 0, inv1);
        for t in 1..=5 { tick_with_inv(&mut cb, t, inv1); }
        // Tick 6: process expira → drop (20→15) → stale=0. Emite RC iter 2.
        assert_eq!(tick_with_inv(&mut cb, 6, inv2), CavebotAction::RightClick { vx: 1600, vy: 50 });
        assert_eq!(cb.stow_stale_iters, 0);
        assert_eq!(cb.stow_iterations_done, 1);

        // Iter 2 (tick 7..12): usa inv2 durante toda la iter (el último tick
        // toma el snapshot final para comparar contra el baseline actual).
        for t in 7..=11 { tick_with_inv(&mut cb, t, inv2); }
        // Tick 12: process expira → comparación contra baseline=inv2 con
        // current=inv3 (drop 15→10) → stale=0. Emite RC iter 3.
        assert_eq!(tick_with_inv(&mut cb, 12, inv3), CavebotAction::RightClick { vx: 1600, vy: 50 });
        assert_eq!(cb.stow_stale_iters, 0);
        assert_eq!(cb.stow_iterations_done, 2);

        // Iter 3 (tick 13..18): usa inv3 para el tick final.
        for t in 13..=17 { tick_with_inv(&mut cb, t, inv3); }
        // Tick 18: process expira → iter 3 termina. Inventario igual a
        // baseline=inv3 (no bajó en este mismo tick) → stale=1, pero
        // iterations_done=3 == max_iterations en el próximo tick de
        // re-evaluación → completa y avanza al Hotkey 0xEE.
        // (Al llegar a max_iterations el state reset incluye baseline/stale
        // antes del advance.)
        let action = tick_with_inv(&mut cb, 18, inv3);
        // Avanza al siguiente step en el mismo tick vía `continue` y emite
        // el KeyTap del Hotkey.
        assert_eq!(action, CavebotAction::KeyTap(0xEE));
        assert_eq!(cb.stow_iterations_done, 0, "reset tras completar el step");
        assert!(cb.stow_baseline_counts.is_none(), "baseline limpio tras completar");
    }

    #[test]
    fn node_phase0_retries_click_when_no_movement() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // click initial

        // No movement, no displacement for 30 ticks → F6 retry.
        for t in 1..30 {
            assert_eq!(cb.tick(&mut ctx_mm(t, Some(false), None)), CavebotAction::Idle);
        }
        // Tick 30: retry #1
        assert_eq!(cb.tick(&mut ctx_mm(30, Some(false), None)), CavebotAction::Click { vx: 520, vy: 300 });
        assert_eq!(cb.node_initial_click_retries, 1);

        // Still no movement for 30 more ticks → retry #2
        for t in 31..60 {
            assert_eq!(cb.tick(&mut ctx_mm(t, Some(false), None)), CavebotAction::Idle);
        }
        assert_eq!(cb.tick(&mut ctx_mm(60, Some(false), None)), CavebotAction::Click { vx: 520, vy: 300 });
        assert_eq!(cb.node_initial_click_retries, 2);

        // After max retries, no more retries — falls through to normal idle.
        assert_eq!(cb.tick(&mut ctx_mm(61, Some(false), None)), CavebotAction::Idle);
    }

    #[test]
    fn walk_stuck_detection_fires_when_is_moving_none() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::Walk { hidcode: 0xAA, duration_ms: 5000, interval_ms: 500 }),
                step(StepKind::Hotkey { hidcode: 0xBB }),
            ],
            false, 30,
        );
        // Tick 0: first emit.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::KeyTap(0xAA));
        // is_moving=None for 60 ticks → stuck detection fires → advance to Hotkey.
        let stuck_ctx_none = |t: u64| TickContext { tick: t, is_moving: None, ..Default::default() };
        for t in 1..60 {
            cb.tick(&mut stuck_ctx_none(t));
        }
        // Tick 60: stuck threshold reached → advance → Hotkey 0xBB.
        assert_eq!(cb.tick(&mut stuck_ctx_none(60)), CavebotAction::KeyTap(0xBB));
    }

    #[test]
    fn node_combat_restart_preserves_accumulators() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None)); // click

        // Walk 5 tiles → accum (10, 0)
        for t in 1..=5 {
            cb.tick(&mut ctx_mm(t, Some(true), Some((-2, 0))));
        }
        assert_eq!(cb.node_accum_dx, 10);
        assert_eq!(cb.node_phase, 1);

        // Combat interruption
        cb.restart_current_step(100);
        assert_eq!(cb.node_phase, 1, "phase preserved");
        assert_eq!(cb.node_accum_dx, 10, "accum preserved");
        assert_eq!(cb.node_expect_dx, 20, "expect preserved");
        assert!(!cb.node_saw_moving, "saw_moving reset");
        assert_eq!(cb.node_idle_ticks, 0, "idle reset");
    }

    #[test]
    fn node_timeout_forces_advance() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        cb.tick(&mut ctx_mm(0, Some(false), None));
        for t in 1..900 { cb.tick(&mut ctx_mm(t, Some(true), None)); }
        assert_eq!(cb.tick(&mut ctx_mm(900, Some(true), None)), CavebotAction::KeyTap(0xCC));
    }

    #[test]
    fn node_no_minimap_skips() {
        let mut cb = cavebot_with_prev_node((110, 200, 7), (100, 200, 7));
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::KeyTap(0xCC));
    }

    // ── TypeInField (click + tipeo en input field no-chat) ───────────

    /// Fase 0 emite Click al field; luego, tras el wait post-click, la
    /// fase 2 emite un KeyTap por cada caracter mapeable respetando
    /// `char_spacing_ms` entre ellos; al final wait_after_type_ms + advance.
    #[test]
    fn type_in_field_clicks_then_types_each_char_with_spacing() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::TypeInField {
                    field_vx: 426,
                    field_vy: 261,
                    text: "abc".into(),
                    wait_after_click_ms: 100, // 3 ticks @ 30Hz
                    wait_after_type_ms:  100, // 3 ticks @ 30Hz
                    char_spacing_ms:     100, // 3 ticks @ 30Hz
                }),
                step(StepKind::Hotkey { hidcode: 0xCC }),
            ],
            false, 30,
        );
        // Tick 0: phase 0 → Click al field.
        assert_eq!(
            cb.tick(&mut ctx(0)),
            CavebotAction::Click { vx: 426, vy: 261 }
        );
        // Phase 1 wait_after_click_ms = 100ms @ 30fps = 3 ticks.
        // phase_start=0, cumple en tick 3 (3-0 >= 3) → re-entra phase 2 y
        // emite inmediatamente el primer char 'a' (HID 0x04).
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(2)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(3)), CavebotAction::KeyTap(0x04));
        // Spacing 100ms = 3 ticks entre taps. Idle ticks 4..5, tap en tick 6.
        assert_eq!(cb.tick(&mut ctx(4)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(5)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(6)), CavebotAction::KeyTap(0x05)); // 'b'
        // Spacing otra vez → tap 'c' en tick 9.
        assert_eq!(cb.tick(&mut ctx(7)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(8)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(9)), CavebotAction::KeyTap(0x06)); // 'c'
        // Transición a phase 3 (wait_after_type) → Idle, luego 3 ticks.
        // tick 10 re-entra phase 2, ve char_idx fuera de rango, pasa a phase 3
        // con phase_start=10, devuelve Idle.
        assert_eq!(cb.tick(&mut ctx(10)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(11)), CavebotAction::Idle);
        assert_eq!(cb.tick(&mut ctx(12)), CavebotAction::Idle);
        // Tick 13: wait cumplido → advance al Hotkey → emit 0xCC.
        assert_eq!(cb.tick(&mut ctx(13)), CavebotAction::KeyTap(0xCC));
    }

    /// text="ab" → exactamente KeyTap(0x04) para 'a' y KeyTap(0x05) para 'b'
    /// tras el Click inicial. Verifica el mapeo ASCII→HID del subset soportado.
    #[test]
    fn type_in_field_text_ab_emits_exact_hid_codes() {
        let mut cb = Cavebot::new(
            vec![
                step(StepKind::TypeInField {
                    field_vx: 10,
                    field_vy: 20,
                    text: "ab".into(),
                    wait_after_click_ms: 0,
                    wait_after_type_ms:  0,
                    char_spacing_ms:     0, // sin espera entre chars
                }),
                step(StepKind::Hotkey { hidcode: 0xEE }),
            ],
            false, 30,
        );
        // Tick 0: Click.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Click { vx: 10, vy: 20 });
        // Tick 1: phase 1 wait=0 cumple, continue a phase 2, tap 'a' → 0x04.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::KeyTap(0x04));
        // Tick 2: spacing=0 → primer tick ya cumple threshold, tap 'b' → 0x05.
        assert_eq!(cb.tick(&mut ctx(2)), CavebotAction::KeyTap(0x05));
    }

    /// Chars no-mapeables (mayúsculas, símbolos) se skipean con warn sin panic.
    /// Solo emite KeyTaps por los chars soportados.
    #[test]
    fn type_in_field_skips_unmappable_chars_without_panic() {
        // 'A' (mayúscula) y '!' no son mapeables; 'b' sí.
        let mut cb = Cavebot::new(
            vec![step(StepKind::TypeInField {
                field_vx: 0, field_vy: 0,
                text: "A!b".into(),
                wait_after_click_ms: 0,
                wait_after_type_ms:  0,
                char_spacing_ms:     0,
            })],
            false, 30,
        );
        // Click.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Click { vx: 0, vy: 0 });
        // Phase 2 skipea 'A' y '!' en el mismo tick y emite KeyTap para 'b'.
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::KeyTap(0x05)); // 'b'
    }

    /// Tras restart_current_step, el step debe re-emitir desde la fase 0
    /// (el click inicial) porque el focus del input no está garantizado tras
    /// una interrupción de combate.
    #[test]
    fn type_in_field_resets_on_restart_current_step() {
        let mut cb = Cavebot::new(
            vec![step(StepKind::TypeInField {
                field_vx: 1, field_vy: 2,
                text: "ab".into(),
                wait_after_click_ms: 0,
                wait_after_type_ms:  0,
                char_spacing_ms:     0,
            })],
            false, 30,
        );
        // Consumir click + primer tap.
        assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::Click { vx: 1, vy: 2 });
        assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::KeyTap(0x04)); // 'a'
        // Restart: debe volver a fase 0 (click), NO seguir tipeando 'b'.
        cb.restart_current_step(100);
        assert_eq!(cb.type_field_phase, 0);
        assert_eq!(cb.type_field_char_idx, 0);
        assert_eq!(
            cb.tick(&mut ctx(100)),
            CavebotAction::Click { vx: 1, vy: 2 }
        );
    }

    // ── StepVerify (Fase 2D) ──────────────────────────────────────────
    //
    // Covers the verify/postcondition pipeline:
    //   1. Step emits as usual (Hotkey tap, etc.)
    //   2. advance() sees step.verify=Some → enters verifying mode (Idle)
    //   3. Subsequent ticks evaluate the VerifyCheck:
    //        - pass → do_advance + continue
    //        - timeout → apply VerifyFailAction (SafetyPause / Advance / GotoLabel)
    //        - neither → stay in verifying, emit Idle
    //
    // Tests use small timeout_ms (33 @ fps=30 = 1 tick) to make timeouts
    // reachable in a few ticks.
    mod verify_tests {
        use super::*;
        use crate::cavebot::step::{StepVerify, VerifyCheck, VerifyFailAction};

        /// Build a step with an attached postcondition.
        fn step_with_verify(kind: StepKind, verify: StepVerify) -> Step {
            Step { label: None, kind, verify: Some(verify) }
        }

        /// Context builder with a custom ui_matches list.
        fn ctx_ui(tick: u64, matches: Vec<&str>) -> TickContext {
            TickContext {
                tick,
                ui_matches: matches.into_iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }
        }

        // 1. TemplateVisible PASS
        #[test]
        fn verify_template_visible_passes_when_ui_matches_contains_name() {
            let verify = StepVerify {
                check: VerifyCheck::TemplateVisible { name: "foo".into(), roi: None },
                timeout_ms: 3000,
                on_fail: VerifyFailAction::SafetyPause,
            };
            let mut cb = Cavebot::new(
                vec![
                    step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify),
                    step(StepKind::Hotkey { hidcode: 0xF2 }),
                ],
                false, 30,
            );
            // tick 0: Hotkey emits F1 → advance() enters verify mode.
            assert_eq!(cb.tick(&mut ctx_ui(0, vec![])), CavebotAction::KeyTap(0xF1));
            assert_eq!(cb.current, Some(0));
            assert!(cb.verifying.is_some());
            // tick 1: ctx.ui_matches contains "foo" → PASS → do_advance → step 1 emits F2.
            // (Hotkey at step 1 has no verify, so after emitting it advances past end → current=None.)
            assert_eq!(cb.tick(&mut ctx_ui(1, vec!["foo"])), CavebotAction::KeyTap(0xF2));
            assert!(cb.verifying.is_none());
        }

        // 2. TemplateVisible TIMEOUT → SafetyPause
        #[test]
        fn verify_template_visible_safety_pause_on_timeout() {
            let verify = StepVerify {
                check: VerifyCheck::TemplateVisible { name: "foo".into(), roi: None },
                timeout_ms: 33,
                on_fail: VerifyFailAction::SafetyPause,
            };
            let mut cb = Cavebot::new(
                vec![step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify)],
                false, 30,
            );
            // tick 0: emit + enter verify.
            assert_eq!(cb.tick(&mut ctx_ui(0, vec![])), CavebotAction::KeyTap(0xF1));
            assert!(cb.verifying.is_some());
            // tick 1: elapsed = 1 tick, timeout_ticks = ceil(33*30/1000) = 1
            //   → elapsed >= timeout_ticks → SafetyPause.
            let action = cb.tick(&mut ctx_ui(1, vec![]));
            match action {
                CavebotAction::SafetyPause { reason } => {
                    assert!(reason.contains("verify_failed"), "reason: {}", reason);
                    assert!(reason.contains("foo"), "reason: {}", reason);
                }
                other => panic!("expected SafetyPause, got {:?}", other),
            }
            assert!(cb.verifying.is_none());
        }

        // 3. TemplateAbsent PASS
        #[test]
        fn verify_template_absent_passes_when_ui_matches_empty() {
            let verify = StepVerify {
                check: VerifyCheck::TemplateAbsent { name: "foo".into(), roi: None },
                timeout_ms: 3000,
                on_fail: VerifyFailAction::SafetyPause,
            };
            let mut cb = Cavebot::new(
                vec![
                    step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify),
                    step(StepKind::Hotkey { hidcode: 0xF2 }),
                ],
                false, 30,
            );
            // tick 0: emit + verify.
            assert_eq!(cb.tick(&mut ctx_ui(0, vec![])), CavebotAction::KeyTap(0xF1));
            // tick 1: ui_matches=["bar"] doesn't contain "foo" → pass immediately → step 1 emits F2.
            assert_eq!(cb.tick(&mut ctx_ui(1, vec!["bar"])), CavebotAction::KeyTap(0xF2));
        }

        // 4. TemplateAbsent TIMEOUT → GotoLabel
        #[test]
        fn verify_template_absent_timeout_goto_label() {
            let verify = StepVerify {
                check: VerifyCheck::TemplateAbsent { name: "foo".into(), roi: None },
                timeout_ms: 33,
                on_fail: VerifyFailAction::GotoLabel {
                    target_label: "recovery".into(),
                    target_idx: 0,
                },
            };
            let mut cb = Cavebot::new(
                vec![
                    labeled("recovery", StepKind::Label),
                    step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify),
                    step(StepKind::Hotkey { hidcode: 0xF2 }),
                ],
                false, 30,
            );
            // tick 0: Label consumed → advance to idx 1 → Hotkey emits F1 + enters verify.
            assert_eq!(cb.tick(&mut ctx_ui(0, vec![])), CavebotAction::KeyTap(0xF1));
            assert_eq!(cb.current, Some(1));
            assert!(cb.verifying.is_some());
            // tick 1: ui_matches contains "foo" → verify fails (we wanted absent) → timeout.
            //   on_fail=GotoLabel{target_idx=0}, so jump back to Label(0), then advance to idx 1.
            //   But idx 1 has verify, so after emit it re-enters verifying.
            //   Result: current=1 again (Label→1), Hotkey F1 emits again.
            let action = cb.tick(&mut ctx_ui(1, vec!["foo"]));
            assert_eq!(action, CavebotAction::KeyTap(0xF1));
            assert_eq!(cb.current, Some(1), "should loop back via GotoLabel target_idx=0 → Label → idx 1");
            assert!(cb.verifying.is_some());
        }

        // 5. ConditionMet PASS
        #[test]
        fn verify_condition_met_passes() {
            use crate::cavebot::step::Condition;
            let verify = StepVerify {
                check: VerifyCheck::ConditionMet(Condition::HpBelow(0.5)),
                timeout_ms: 3000,
                on_fail: VerifyFailAction::SafetyPause,
            };
            let mut cb = Cavebot::new(
                vec![
                    step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify),
                    step(StepKind::Hotkey { hidcode: 0xF2 }),
                ],
                false, 30,
            );
            // tick 0: HP=0.8 (not <0.5), hotkey emits. Enters verifying.
            let mut c0 = TickContext { tick: 0, hp_ratio: Some(0.8), ..Default::default() };
            assert_eq!(cb.tick(&mut c0), CavebotAction::KeyTap(0xF1));
            // tick 1: HP=0.3 (<0.5) → pass immediately → step 1 emits F2.
            let mut c1 = TickContext { tick: 1, hp_ratio: Some(0.3), ..Default::default() };
            assert_eq!(cb.tick(&mut c1), CavebotAction::KeyTap(0xF2));
        }

        // 6. ConditionMet TIMEOUT → Advance
        #[test]
        fn verify_condition_timeout_advance() {
            use crate::cavebot::step::Condition;
            let verify = StepVerify {
                check: VerifyCheck::ConditionMet(Condition::HpBelow(0.5)),
                timeout_ms: 33,
                on_fail: VerifyFailAction::Advance,
            };
            let mut cb = Cavebot::new(
                vec![
                    step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify),
                    step(StepKind::Hotkey { hidcode: 0xF2 }),
                ],
                false, 30,
            );
            // tick 0: HP=0.9 never below 0.5, hotkey emits. Enters verifying.
            let mut c0 = TickContext { tick: 0, hp_ratio: Some(0.9), ..Default::default() };
            assert_eq!(cb.tick(&mut c0), CavebotAction::KeyTap(0xF1));
            // tick 1: still 0.9, timeout elapsed → on_fail=Advance → step 1 emits F2.
            let mut c1 = TickContext { tick: 1, hp_ratio: Some(0.9), ..Default::default() };
            assert_eq!(cb.tick(&mut c1), CavebotAction::KeyTap(0xF2));
        }

        // 7. InventoryDelta positive PASS (gained items)
        #[test]
        fn verify_inventory_delta_positive_passes_when_gained() {
            let verify = StepVerify {
                check: VerifyCheck::InventoryDelta {
                    item: "mp".into(),
                    min_abs_delta: 3,
                    require_positive: true,
                },
                timeout_ms: 3000,
                on_fail: VerifyFailAction::SafetyPause,
            };
            let mut cb = Cavebot::new(
                vec![
                    step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify),
                    step(StepKind::Hotkey { hidcode: 0xF2 }),
                ],
                false, 30,
            );
            // tick 0: mp=5 at entry. emit + verify (snapshot captured = 5).
            let mut c0 = TickContext {
                tick: 0,
                inventory_stacks: std::collections::HashMap::from([("mp".to_string(), 5u32)]),
                ..Default::default()
            };
            assert_eq!(cb.tick(&mut c0), CavebotAction::KeyTap(0xF1));
            assert_eq!(cb.inventory_at_step_start.get("mp"), Some(&5u32));
            // tick 1: mp=10. delta=+5 >= 3, require_positive ✓ → pass → step 1 emits F2.
            let mut c1 = TickContext {
                tick: 1,
                inventory_stacks: std::collections::HashMap::from([("mp".to_string(), 10u32)]),
                ..Default::default()
            };
            assert_eq!(cb.tick(&mut c1), CavebotAction::KeyTap(0xF2));
        }

        // 8. InventoryDelta positive FAILS when items lost → SafetyPause
        #[test]
        fn verify_inventory_delta_positive_fails_when_lost() {
            let verify = StepVerify {
                check: VerifyCheck::InventoryDelta {
                    item: "mp".into(),
                    min_abs_delta: 3,
                    require_positive: true,
                },
                timeout_ms: 33,
                on_fail: VerifyFailAction::SafetyPause,
            };
            let mut cb = Cavebot::new(
                vec![step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify)],
                false, 30,
            );
            // tick 0: mp=5 at entry. emit + verify.
            let mut c0 = TickContext {
                tick: 0,
                inventory_stacks: std::collections::HashMap::from([("mp".to_string(), 5u32)]),
                ..Default::default()
            };
            assert_eq!(cb.tick(&mut c0), CavebotAction::KeyTap(0xF1));
            // tick 1: mp=2 (delta=-3), require_positive → fails. Timeout elapsed → SafetyPause.
            let mut c1 = TickContext {
                tick: 1,
                inventory_stacks: std::collections::HashMap::from([("mp".to_string(), 2u32)]),
                ..Default::default()
            };
            let action = cb.tick(&mut c1);
            assert!(
                matches!(action, CavebotAction::SafetyPause { .. }),
                "expected SafetyPause, got {:?}",
                action
            );
        }

        // 9. InventoryDelta abs mode passes either direction
        #[test]
        fn verify_inventory_delta_abs_mode_passes_either_direction() {
            let verify = StepVerify {
                check: VerifyCheck::InventoryDelta {
                    item: "mp".into(),
                    min_abs_delta: 3,
                    require_positive: false,
                },
                timeout_ms: 3000,
                on_fail: VerifyFailAction::SafetyPause,
            };
            let mut cb = Cavebot::new(
                vec![
                    step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify),
                    step(StepKind::Hotkey { hidcode: 0xF2 }),
                ],
                false, 30,
            );
            // tick 0: mp=10 at entry.
            let mut c0 = TickContext {
                tick: 0,
                inventory_stacks: std::collections::HashMap::from([("mp".to_string(), 10u32)]),
                ..Default::default()
            };
            assert_eq!(cb.tick(&mut c0), CavebotAction::KeyTap(0xF1));
            // tick 1: mp=5. delta=-5, abs=5 >= 3, require_positive=false → pass.
            let mut c1 = TickContext {
                tick: 1,
                inventory_stacks: std::collections::HashMap::from([("mp".to_string(), 5u32)]),
                ..Default::default()
            };
            assert_eq!(cb.tick(&mut c1), CavebotAction::KeyTap(0xF2));
        }

        // 10. Step without verify: no verify state ever set.
        #[test]
        fn step_without_verify_does_not_enter_verifying() {
            let mut cb = Cavebot::new(
                vec![
                    step(StepKind::Hotkey { hidcode: 0xF1 }),
                    step(StepKind::Hotkey { hidcode: 0xF2 }),
                ],
                false, 30,
            );
            assert!(cb.verifying.is_none());
            assert_eq!(cb.tick(&mut ctx(0)), CavebotAction::KeyTap(0xF1));
            assert!(cb.verifying.is_none());
            assert_eq!(cb.tick(&mut ctx(1)), CavebotAction::KeyTap(0xF2));
            assert!(cb.verifying.is_none());
        }

        // 11. Verify waiting doesn't trigger the 64-iter loop cap (returns Idle per tick).
        #[test]
        fn verify_preserves_through_iteration_loop_cap() {
            let verify = StepVerify {
                check: VerifyCheck::TemplateVisible { name: "foo".into(), roi: None },
                timeout_ms: 60_000, // far beyond any test
                on_fail: VerifyFailAction::SafetyPause,
            };
            let mut cb = Cavebot::new(
                vec![step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify)],
                false, 30,
            );
            // tick 0: emit + verify.
            assert_eq!(cb.tick(&mut ctx_ui(0, vec![])), CavebotAction::KeyTap(0xF1));
            // Ticks 1..100: always Idle, never Panic, cavebot stays at step 0.
            for t in 1..=100u64 {
                assert_eq!(cb.tick(&mut ctx_ui(t, vec![])), CavebotAction::Idle, "tick {}", t);
                assert_eq!(cb.current, Some(0));
                assert!(cb.verifying.is_some());
            }
        }

        // 12. jump_to_label clears verifying state.
        #[test]
        fn jump_to_clears_verifying_state() {
            let verify = StepVerify {
                check: VerifyCheck::TemplateVisible { name: "foo".into(), roi: None },
                timeout_ms: 3000,
                on_fail: VerifyFailAction::SafetyPause,
            };
            let mut cb = Cavebot::new(
                vec![
                    labeled("home", StepKind::Label),
                    step_with_verify(StepKind::Hotkey { hidcode: 0xF1 }, verify),
                ],
                true, 30,
            );
            // tick 0: Label consumed → advance to idx 1 → Hotkey emits + enters verify.
            assert_eq!(cb.tick(&mut ctx_ui(0, vec![])), CavebotAction::KeyTap(0xF1));
            assert!(cb.verifying.is_some());
            // Manual jump_to_label bypasses the verify intercept → state cleared.
            assert!(cb.jump_to_label("home", 100));
            assert!(cb.verifying.is_none());
        }
    }
}
