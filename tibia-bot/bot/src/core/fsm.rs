/// fsm.rs — Máquina de estados por prioridad del bot.
///
/// decide() recibe un snapshot del estado y la Perception del tick actual.
/// Retorna la acción de mayor prioridad. El orden de los `if` define la prioridad.

use crate::config::Hotkeys;
use crate::core::state::GameState;
use crate::safety::timing::sample_gauss_ticks;
use crate::sense::perception::Perception;

/// Umbral de HP para activar Emergency (30%).
const HP_CRITICAL_RATIO: f32 = 0.30;
/// Umbral de mana para activar Emergency (20%).
const MANA_CRITICAL_RATIO: f32 = 0.20;

/// Estado actual de la FSM del bot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(dead_code)] // extension point: Refilling
pub enum FsmState {
    #[default]
    Idle,
    Paused,
    /// Navegando hacia un waypoint.
    Walking,
    /// En combate activo.
    Fighting,
    /// HP/mana bajo — huyendo o comiendo.
    Emergency,
    /// Refillando (comprando supplies, etc.)
    Refilling,
}

/// Evento que puede disparar una transición de estado.
#[derive(Debug, Clone)]
#[allow(dead_code)] // extension point: ResumeRequested
pub enum BotEvent {
    /// Tick del game loop — input principal de la FSM.
    Tick,
    /// El operador pausó el bot desde /pause.
    PauseRequested,
    /// El operador reanudó el bot desde /resume.
    ResumeRequested,
}

/// Acción que el game loop ejecuta tras llamar a decide().
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // extension point: MoveTo
pub enum BotAction {
    /// No hacer nada este tick.
    Idle,
    /// Mover el personaje hacia (vx, vy) en coordenadas del viewport.
    MoveTo { vx: i32, vy: i32 },
    /// Usar una hotkey (KEY_TAP).
    UseHotkey { hidcode: u8 },
    /// Click en coordenadas del viewport.
    Click { vx: i32, vy: i32 },
    /// Right-click en coordenadas del viewport (abre context menu).
    RightClick { vx: i32, vy: i32 },
}

/// Acción que el sistema de waypoints/cavebot puede pedir este tick.
/// Más genérico que `BotAction` porque limita las opciones a las que un
/// cavebot puede emitir sensatamente.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaypointEmit {
    /// Pulsar una tecla (directional walk, hotkey, etc).
    KeyTap(u8),
    /// Click en coordenada del viewport (para loot de corpses).
    Click { vx: i32, vy: i32 },
    /// Right-click en coordenada del viewport (context menu).
    RightClick { vx: i32, vy: i32 },
}

/// Input de la `WaypointList` al FSM para un tick dado.
/// Permite distinguir "no hay lista" de "hay lista pero no emite este tick",
/// para que el FSM pueda reportar `Walking` consistentemente mientras está
/// ejecutando waypoints aunque no emita acción en cada tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaypointHint {
    /// No hay lista cargada o está pausada — el FSM cae a Idle.
    Inactive,
    /// Hay lista ejecutándose; `emit` = acción a dispatchar ESTE tick (None = esperar).
    Active { emit: Option<WaypointEmit> },
}

/// Agrupa todos los inputs de un tick para `Fsm::decide`. Pasar un struct
/// en vez de 8 parámetros sueltos hace la API sostenible cuando aparecen
/// nuevos inputs (Fase 4 añadió `heal_override`, Fase 5 probablemente
/// añadirá flags de safety).
pub struct DecideContext<'a> {
    pub game:          &'a GameState,
    pub event:         BotEvent,
    pub perception:    &'a Perception,
    pub hotkeys:       &'a Hotkeys,
    pub cd_heal:       u64,
    pub cd_attack:     u64,
    pub waypoint_hint: WaypointHint,
    /// Override de la hotkey de heal: si `Some`, se usa en vez de
    /// `hotkeys.heal_spell` cuando la FSM emite un heal por HP crítico.
    /// Viene de SpellTable o de un script Lua `on_low_hp`.
    pub heal_override: Option<u8>,
    /// Override de la hotkey de ataque: si `Some`, se usa en vez de
    /// `hotkeys.attack_default`. Viene de SpellTable.
    pub attack_override: Option<u8>,
    /// Gate del HealthSystem. Cuando `degraded == Heavy`, las acciones
    /// "normal" (walking, looting) se suprimen; solo emergency actions
    /// (heal crítico, escape) pasan. SafeMode ya es safety pause externa;
    /// Light permite todo. Default `allow_all` (equivalente a sin gate)
    /// para retro-compat con consumers que no setean este campo.
    ///
    /// Sólo efectivo si `[health].apply_degradation = true` en config.
    /// Si false, BotLoop pasa `HealthGate::permissive()` siempre.
    pub health_gate: HealthGateDecision,
}

/// Veredicto compacto del HealthGate consumido por FSM. Evita depender
/// del lifetime del Arc<HealthStatus> adentro del DecideContext.
/// Default = permisivo (allow all) — invariant deliberado para que cualquier
/// consumer que no setee explícitamente el gate NO termine bloqueando.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthGateDecision {
    /// Si `false`, FSM debería suprimir acciones "normal priority"
    /// (attack, walking, looting). Default `true`.
    pub allow_normal: bool,
    /// Si `false`, también suprimir emergency (heal crítico, escape).
    /// Solo `false` cuando degradation == SafeMode (raro — BotLoop
    /// maneja SafeMode via safety pause antes del FSM).
    pub allow_emergency: bool,
}

impl HealthGateDecision {
    /// Gate permisivo: deja pasar todo. Usar cuando HealthSystem está
    /// desactivado O apply_degradation=false O el degradation level actual
    /// es None/Light.
    pub fn permissive() -> Self {
        Self { allow_normal: true, allow_emergency: true }
    }
    /// Gate restrictivo (Heavy): bloquea normal, permite emergency.
    pub fn heavy() -> Self {
        Self { allow_normal: false, allow_emergency: true }
    }
    /// Gate SafeMode: bloquea todo (FSM no debería decidir nada).
    /// BotLoop typically route this via safety_pause instead of FSM gate.
    pub fn safe_mode() -> Self {
        Self { allow_normal: false, allow_emergency: false }
    }
}

impl Default for HealthGateDecision {
    /// IMPORTANTE: default es permisivo para preservar semántica
    /// "sin HealthSystem = bot opera normal". Derive default daría
    /// false/false que bloquearía todo.
    fn default() -> Self { Self::permissive() }
}

/// Cooldowns con modelo "next allowed tick". Evitan spam cuando la percepción
/// no cambia entre ticks (HP sigue bajo durante varios frames tras un heal).
///
/// Al emitir una acción, el siguiente valor permitido se fija en
/// `tick + cooldown_ticks`. Si `FsmTiming` está presente, el cooldown se
/// samplea gaussianamente; si no, se usa el fallback fijo del `DecideContext`.
///
/// `None` = nunca emitida → la próxima llamada siempre dispara. Evita el bug
/// de "tick=0, last=0, delta=0 < cd" que bloquearía el primer heal al arrancar.
///
/// **Attack cooldowns (dual timer)**:
/// - `next_attack_tick`: **safety floor** — no emit antes de este tick
///   (evita spam accidental y respeta humanización del config).
/// - `attack_keepalive_tick`: **keep-alive** — emit obligatorio al llegar
///   a este tick si seguimos en combate. Rescate por si el target bar
///   detector falla puntualmente.
#[derive(Debug, Clone, Copy, Default)]
struct Cooldowns {
    next_heal_tick:         Option<u64>,
    next_attack_tick:       Option<u64>,
    attack_keepalive_tick:  Option<u64>,
}

impl Cooldowns {
    fn ready(next: Option<u64>, tick: u64) -> bool {
        next.is_none_or(|n| tick >= n)
    }
}

/// Ticks entre emisiones de keep-alive del attack cuando NO se recibe
/// signal de cambio via `target_active`.
///
/// **150 @ 30Hz = 5s**. Ajustado tras audit in-game contra Thais Rat Cave:
/// con un cliente Tibia moderno que NO pinta bordes rojos en slots atacados,
/// el detector `is_being_attacked` retorna siempre false. Sin target signal
/// efectivo, el keepalive es la ÚNICA fuente de emits tras el primer tick
/// de combate. Si queremos cubrir el caso "char pierde target y no lo
/// recupera automáticamente", necesitamos re-pulsar PgDown rápido.
///
/// 5s es un compromiso: suficiente para dejar a Chase+Attack del char
/// terminar de matar un mob (los rats mueren en 2-3s), pero corto enough
/// para re-targetear rápido si el char queda idle.
///
/// Valor histórico: 900 (30s) — asumía que target_active estaría disponible
/// como signal primario, keepalive solo como fallback raro. Esto no se
/// cumplió con el cliente del usuario.
const ATTACK_KEEPALIVE_TICKS: u64 = 150;

/// Parámetros de humanización temporal en la FSM. Cuando está presente,
/// los cooldowns se muestrean gaussianamente. Ausente = determinista.
#[derive(Debug, Clone, Copy)]
pub struct FsmTiming {
    pub heal_cd_mean_ms:   f64,
    pub heal_cd_std_ms:    f64,
    pub attack_cd_mean_ms: f64,
    pub attack_cd_std_ms:  f64,
    pub fps:               u32,
}

pub struct Fsm {
    pub state: FsmState,
    cooldowns: Cooldowns,
    /// Samplers de jitter gaussiano opcionales. Ver `Fsm::with_timing`.
    timing:    Option<FsmTiming>,
    /// **Target activo del tick anterior**. Usado para detectar el flanco
    /// `true → false` ("char perdió target") que dispara el PgDown de
    /// retarget. `None` = nunca observado (primer tick tras reset).
    ///
    /// Este es el único state de combate que queda tras la Fase A. Reemplaza
    /// a `last_enemy_count`, `pending_decrease*`, `retarget_pending`,
    /// `last_attack_emit_count` — todos eliminados porque el signal
    /// `target_active` es directo y binario.
    prev_target_active: Option<bool>,
}

impl Fsm {
    pub fn new() -> Self {
        Self {
            state:              FsmState::Idle,
            cooldowns:          Cooldowns::default(),
            timing:             None,
            prev_target_active: None,
        }
    }

    /// Crea una FSM con samplers gaussianos para jittear cooldowns.
    pub fn with_timing(timing: FsmTiming) -> Self {
        Self {
            state:              FsmState::Idle,
            cooldowns:          Cooldowns::default(),
            timing:             Some(timing),
            prev_target_active: None,
        }
    }

    /// Samplea la duración del próximo cooldown de heal.
    /// Si no hay `timing` (tests deterministas), retorna el fallback fijo.
    fn sample_heal_cd(&self, fallback_ticks: u64) -> u64 {
        match self.timing {
            Some(t) => sample_gauss_ticks(t.heal_cd_mean_ms, t.heal_cd_std_ms, t.fps),
            None    => fallback_ticks,
        }
    }

    fn sample_attack_cd(&self, fallback_ticks: u64) -> u64 {
        match self.timing {
            Some(t) => sample_gauss_ticks(t.attack_cd_mean_ms, t.attack_cd_std_ms, t.fps),
            None    => fallback_ticks,
        }
    }

    /// Resetea el estado privado relacionado con el combate (tracker de
    /// target del tick anterior). Llamado al entrar en Paused o tras un
    /// resume, para evitar que state stale contamine la decisión.
    fn reset_combat_state(&mut self) {
        self.prev_target_active = None;
        // Los cooldowns NO se resetean — un cooldown activo debe respetarse
        // incluso tras un resume (el bot no debe spamear por reanudar).
    }

    /// decide() — núcleo de la FSM.
    /// Transiciona el estado interno y retorna la acción a ejecutar.
    /// El game loop llama esto una vez por tick.
    ///
    /// Ver `DecideContext` para los inputs agrupados.
    pub fn decide(&mut self, ctx: &DecideContext<'_>) -> BotAction {
        let action = self.decide_internal(ctx);
        // ── HealthGate post-decide filter ─────────────────────────────────
        // Aplica sólo cuando `[health].apply_degradation = true` hace que
        // BotLoop nos pase un gate no-permissive.
        //
        // - allow_normal=false (Heavy): bloquea acciones no-emergency.
        //   Emergency = FSM state == Emergency (HP crítico, heal disparado).
        // - allow_emergency=false (SafeMode): bloquea todo. Normalmente
        //   BotLoop ya disparó safety pause antes de llegar aquí, pero
        //   mantenemos el chequeo como defensa en profundidad.
        if !ctx.health_gate.allow_emergency {
            return BotAction::Idle;
        }
        if !ctx.health_gate.allow_normal
            && self.state != FsmState::Emergency
            && !matches!(action, BotAction::Idle)
        {
            tracing::debug!(
                "HealthGate: suppressing {:?} in state {:?} (degraded: allow_normal=false)",
                action, self.state
            );
            return BotAction::Idle;
        }
        action
    }

    /// Versión interna sin HealthGate — preserva todo el flow existente
    /// para mantener tests y no impactar semántica de priority chain.
    fn decide_internal(&mut self, ctx: &DecideContext<'_>) -> BotAction {
        // ── Prioridad 0: control del operador ─────────────────────────────────
        match ctx.event {
            BotEvent::PauseRequested => {
                if self.state != FsmState::Paused {
                    self.reset_combat_state();
                }
                self.state = FsmState::Paused;
                return BotAction::Idle;
            }
            BotEvent::ResumeRequested => {
                if self.state == FsmState::Paused {
                    self.state = FsmState::Idle;
                    self.reset_combat_state();
                }
            }
            BotEvent::Tick => {}
        }

        if ctx.game.is_paused {
            if self.state != FsmState::Paused {
                self.reset_combat_state();
            }
            self.state = FsmState::Paused;
            return BotAction::Idle;
        }

        let tick = ctx.game.tick;

        // ── Prioridad 1: emergencia (HP/mana crítico) ─────────────────────────
        let hp_critical = ctx.perception.vitals.hp
            .map(|bar| bar.is_critical(HP_CRITICAL_RATIO))
            .unwrap_or(false);
        let mana_critical = ctx.perception.vitals.mana
            .map(|bar| bar.is_critical(MANA_CRITICAL_RATIO))
            .unwrap_or(false);

        if hp_critical || mana_critical {
            self.state = FsmState::Emergency;
            if Cooldowns::ready(self.cooldowns.next_heal_tick, tick) {
                // Sample el cooldown del próximo heal (jittered o fijo).
                let cd = self.sample_heal_cd(ctx.cd_heal);
                self.cooldowns.next_heal_tick = Some(tick + cd);
                // HP crítico tiene prioridad sobre mana crítico.
                // `heal_override` (de un script Lua `on_low_hp`) tiene
                // prioridad sobre la hotkey default del config.
                let hidcode = if hp_critical {
                    ctx.heal_override.unwrap_or(ctx.hotkeys.heal_spell)
                } else {
                    ctx.hotkeys.mana_spell
                };
                return BotAction::UseHotkey { hidcode };
            }
            return BotAction::Idle;
        }

        // ── Prioridad 2: combate (Fase A — target bar signal directo) ──────
        //
        // **Lógica radicalmente simplificada**:
        //
        // 1. `has_combat` = hay mobs visibles en el battle list. Este signal
        //    sigue viniendo del detector histerético del panel — es nuestro
        //    trigger de "estamos en combate".
        //
        // 2. `target_active` = signal BINARIO del `TargetDetector` leyendo la
        //    barra de HP del target arriba del viewport. Es el signal
        //    **directo** de "el char tiene un mob seleccionado".
        //
        // 3. La FSM emite PgDown (`attack_default`) cuando:
        //    - Hay combate, Y
        //    - El char NO tiene target (acaba de perderlo o nunca tuvo), Y
        //    - El safety floor ya expiró
        //
        // 4. Fallback cuando el `TargetDetector` no está disponible
        //    (`target_active = None`, típicamente porque la ROI no está
        //    calibrada todavía): emitir PgDown una vez al entrar en combate
        //    y luego cada `ATTACK_KEEPALIVE_TICKS` como rescate. Esto es
        //    peor que Fase A pero mejor que Idle total.
        //
        // **Por qué esto funciona**: el "flanco de pérdida de target" es el
        // event REAL que queremos. No dependemos de adivinar por count de
        // battle list. Si el target está ahí → no emit. Si no → emit una vez.
        // Sin flancos espurios por flicker del detector de battle list.
        let has_combat = ctx.perception.battle.has_enemies();

        if has_combat {
            self.state = FsmState::Fighting;

            let safety_floor_ok = Cooldowns::ready(self.cooldowns.next_attack_tick, tick);
            let keepalive_expired = Cooldowns::ready(self.cooldowns.attack_keepalive_tick, tick);

            let should_emit = match ctx.perception.target_active {
                // Fase A: tenemos signal directo del target bar.
                Some(target_active) => {
                    let prev = self.prev_target_active;
                    self.prev_target_active = Some(target_active);

                    // Primera observación (recién entrando en combate o tras
                    // reset) O flanco true → false (acaba de perder target).
                    let lost_target = matches!(prev, Some(true)) && !target_active;
                    let first_observation = prev.is_none() && !target_active;

                    // Emit solo si no hay target Y algo ameritó emit.
                    if !target_active {
                        safety_floor_ok && (lost_target || first_observation || keepalive_expired)
                    } else {
                        // Tiene target activo: no emit. Solo refrescamos
                        // el keepalive para que NO vuelva a dispararse
                        // mientras el char siga atacando el mismo mob.
                        self.cooldowns.attack_keepalive_tick =
                            Some(tick + ATTACK_KEEPALIVE_TICKS);
                        false
                    }
                }
                // Fallback legacy: sin ROI de target. Usamos solo el trigger
                // "primera observación de combat" + keepalive periódico.
                // Menos preciso pero evita quedarse Idle sin target ROI.
                None => {
                    let first_combat = self.prev_target_active.is_none();
                    if first_combat {
                        self.prev_target_active = Some(false);
                    }
                    safety_floor_ok && (first_combat || keepalive_expired)
                }
            };

            if should_emit {
                let cd = self.sample_attack_cd(ctx.cd_attack);
                self.cooldowns.next_attack_tick      = Some(tick + cd);
                self.cooldowns.attack_keepalive_tick = Some(tick + ATTACK_KEEPALIVE_TICKS);
                let hidcode = ctx.attack_override.unwrap_or(ctx.hotkeys.attack_default);
                return BotAction::UseHotkey { hidcode };
            }

            return BotAction::Idle;
        }

        // No hay enemies — limpiar state combat.
        self.prev_target_active = None;

        // ── Prioridad 3: waypoints ────────────────────────────────────────────
        // El BotLoop consulta la WaypointList/Cavebot antes de llamar a decide()
        // y le pasa el resultado como `waypoint_hint`. El estado `Walking`
        // persiste mientras haya lista activa — aunque no emitamos acción este
        // tick, el `Walking` refleja la intención para /status.
        if let WaypointHint::Active { emit } = ctx.waypoint_hint {
            self.state = FsmState::Walking;
            return match emit {
                Some(WaypointEmit::KeyTap(hidcode))       => BotAction::UseHotkey { hidcode },
                Some(WaypointEmit::Click { vx, vy })      => BotAction::Click { vx, vy },
                Some(WaypointEmit::RightClick { vx, vy }) => BotAction::RightClick { vx, vy },
                None                                      => BotAction::Idle,
            };
        }

        // ── Fallback ──────────────────────────────────────────────────────────
        self.state = FsmState::Idle;
        BotAction::Idle
    }

    /// ¿La FSM está en un estado que interrumpe la ejecución de waypoints?
    /// El BotLoop usa esto para decidir cuándo re-iniciar el step actual
    /// de la `WaypointList` al volver a Walking.
    pub fn is_interrupting_waypoints(&self) -> bool {
        matches!(self.state, FsmState::Emergency | FsmState::Fighting)
    }

    /// Snapshot del estado interno del FSM para `/fsm/debug`.
    pub fn debug_snapshot(&self) -> crate::core::state::FsmDebugSnapshot {
        crate::core::state::FsmDebugSnapshot {
            state:                 format!("{:?}", self.state),
            next_heal_tick:        self.cooldowns.next_heal_tick,
            next_attack_tick:      self.cooldowns.next_attack_tick,
            attack_keepalive_tick: self.cooldowns.attack_keepalive_tick,
            prev_target_active:    self.prev_target_active,
        }
    }
}

impl Default for Fsm {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sense::perception::{BattleEntry, BattleList, CharVitals, EntryKind, Perception, VitalBar};

    fn test_hotkeys() -> Hotkeys {
        Hotkeys {
            heal_spell:     0x3A, // F1
            heal_potion:    0x3B, // F2
            mana_spell:     0x3C, // F3
            attack_default: 0x2C, // Space
            loot_hotkey:    None,
        }
    }

    fn game_at_tick(tick: u64) -> GameState {
        GameState { tick, ..Default::default() }
    }

    /// Construye una Perception con HP/mana en el ratio dado.
    fn perception_vitals(hp: Option<f32>, mana: Option<f32>) -> Perception {
        let bar = |r: f32| VitalBar {
            ratio: r,
            filled_px: (r * 100.0) as u32,
            total_px: 100,
        };
        Perception {
            vitals: CharVitals {
                hp:   hp.map(bar),
                mana: mana.map(bar),
            },
            ..Default::default()
        }
    }

    fn perception_with_enemy() -> Perception {
        perception_with_enemy_target(None)
    }

    /// Helper con target_active explícito. `None` = sin ROI configurada,
    /// fuerza el fallback legacy del FSM. `Some(true)` = char atacando un
    /// mob. `Some(false)` = hay enemigos pero char sin target.
    fn perception_with_enemy_target(target_active: Option<bool>) -> Perception {
        let full_hp = VitalBar { ratio: 1.0, filled_px: 100, total_px: 100 };
        Perception {
            vitals: CharVitals { hp: Some(full_hp), mana: Some(full_hp) },
            battle: BattleList {
                enemy_count_filtered: None,
                entries: vec![BattleEntry {
                    kind:     EntryKind::Monster,
                    row:      0,
                    hp_ratio: Some(1.0),
                    name:     None,
                    is_being_attacked: false,
                }],
                slot_debug: vec![],
            },
            target_active,
            ..Default::default()
        }
    }

    #[test]
    fn idle_when_everything_nominal() {
        let mut fsm = Fsm::new();
        let game = game_at_tick(0);
        let perception = perception_vitals(Some(1.0), Some(1.0));

        let action = fsm.decide(&DecideContext { game: &game, event: BotEvent::Tick, perception: &perception, hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30, waypoint_hint: WaypointHint::Inactive, heal_override: None, attack_override: None, health_gate: HealthGateDecision::default() });
        assert_eq!(action, BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Idle);
    }

    #[test]
    fn emergency_hp_critical_emits_heal_spell() {
        let mut fsm = Fsm::new();
        let game = game_at_tick(100);
        let perception = perception_vitals(Some(0.20), Some(1.0));

        let action = fsm.decide(&DecideContext { game: &game, event: BotEvent::Tick, perception: &perception, hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30, waypoint_hint: WaypointHint::Inactive, heal_override: None, attack_override: None, health_gate: HealthGateDecision::default() });
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x3A });
        assert_eq!(fsm.state, FsmState::Emergency);
    }

    #[test]
    fn emergency_mana_critical_emits_mana_spell_when_hp_ok() {
        let mut fsm = Fsm::new();
        let game = game_at_tick(100);
        let perception = perception_vitals(Some(1.0), Some(0.10));

        let action = fsm.decide(&DecideContext { game: &game, event: BotEvent::Tick, perception: &perception, hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30, waypoint_hint: WaypointHint::Inactive, heal_override: None, attack_override: None, health_gate: HealthGateDecision::default() });
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x3C });
        assert_eq!(fsm.state, FsmState::Emergency);
    }

    #[test]
    fn emergency_hp_takes_priority_over_mana() {
        let mut fsm = Fsm::new();
        let game = game_at_tick(100);
        let perception = perception_vitals(Some(0.10), Some(0.10));

        let action = fsm.decide(&DecideContext { game: &game, event: BotEvent::Tick, perception: &perception, hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30, waypoint_hint: WaypointHint::Inactive, heal_override: None, attack_override: None, health_gate: HealthGateDecision::default() });
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x3A }); // heal, no mana
    }

    #[test]
    fn heal_cooldown_prevents_spam() {
        let mut fsm = Fsm::new();
        let perception = perception_vitals(Some(0.15), Some(1.0));

        // Tick 100: emite heal.
        let a1 = fsm.decide(&DecideContext {
               game: &game_at_tick(100),
               event: BotEvent::Tick,
               perception: &perception,
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(a1, BotAction::UseHotkey { hidcode: 0x3A });

        // Tick 105 (delta 5 < cd 10): Idle aunque HP siga bajo.
        let a2 = fsm.decide(&DecideContext {
               game: &game_at_tick(105),
               event: BotEvent::Tick,
               perception: &perception,
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(a2, BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Emergency, "sigue en Emergency aunque no emita");

        // Tick 110 (delta 10 = cd): vuelve a emitir.
        let a3 = fsm.decide(&DecideContext {
               game: &game_at_tick(110),
               event: BotEvent::Tick,
               perception: &perception,
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(a3, BotAction::UseHotkey { hidcode: 0x3A });
    }

    #[test]
    fn fighting_emits_attack_hotkey() {
        let mut fsm = Fsm::new();
        let action = fsm.decide(&DecideContext {
               game: &game_at_tick(100),
               event: BotEvent::Tick,
               perception: &perception_with_enemy(),
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x2C });
        assert_eq!(fsm.state, FsmState::Fighting);
    }

    /// Helper para construir una Perception con N monsters en la battle list.
    fn perception_with_n_enemies(n: usize) -> Perception {
        let full_hp = VitalBar { ratio: 1.0, filled_px: 100, total_px: 100 };
        let entries: Vec<BattleEntry> = (0..n).map(|i| BattleEntry {
            kind:     EntryKind::Monster,
            row:      i as u8,
            hp_ratio: Some(1.0),
            name:     None,
            is_being_attacked: false,
        }).collect();
        Perception {
            vitals: CharVitals { hp: Some(full_hp), mana: Some(full_hp) },
            battle: BattleList {
                entries,
                slot_debug: vec![],
                enemy_count_filtered: None,
            },
            ..Default::default()
        }
    }

    #[test]
    fn fighting_stable_enemy_count_does_not_respam() {
        // Con el nuevo event-driven targeting, ver el mismo enemy count en
        // ticks consecutivos NO debe emitir más attacks — el char está
        // melee-ing al target actual y pulsar PgDown cambiaría de objetivo.
        let mut fsm = Fsm::new();
        let p = perception_with_enemy(); // 1 monster

        let a1 = fsm.decide(&DecideContext {
               game: &game_at_tick(100),
               event: BotEvent::Tick,
               perception: &p,
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        // Flanco de subida (prev=None, now=1) → emit.
        assert_eq!(a1, BotAction::UseHotkey { hidcode: 0x2C });
        assert_eq!(fsm.state, FsmState::Fighting);

        // Ticks siguientes con el mismo enemy → NO emit (está atacando).
        for t in 101..200 {
            let a = fsm.decide(&DecideContext {
                   game: &game_at_tick(t),
                   event: BotEvent::Tick,
                   perception: &p,
                   hotkeys: &test_hotkeys(),
                   cd_heal: 10,
                   cd_attack: 30,
                   waypoint_hint: WaypointHint::Inactive,
                   heal_override: None,
                   attack_override: None,
                   health_gate: HealthGateDecision::default(),
               });
            assert_eq!(a, BotAction::Idle, "tick {}: esperaba Idle con enemy estable", t);
            assert_eq!(fsm.state, FsmState::Fighting);
        }
    }

    // ── Tests del nuevo target_active signal (Fase A) ──────────────────────

    #[test]
    fn fighting_no_target_emits_pgdown() {
        // has_combat + target_active=false (primera observación) → emit.
        let mut fsm = Fsm::new();
        let p = perception_with_enemy_target(Some(false));

        let a = fsm.decide(&DecideContext {
            game: &game_at_tick(0), event: BotEvent::Tick, perception: &p,
            hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
            waypoint_hint: WaypointHint::Inactive, heal_override: None,
            attack_override: None,
            health_gate: HealthGateDecision::default(),
        });
        assert_eq!(a, BotAction::UseHotkey { hidcode: 0x2C },
            "sin target y hay combat, debe emit PgDown");
        assert_eq!(fsm.state, FsmState::Fighting);
    }

    #[test]
    fn fighting_target_active_does_not_emit() {
        // has_combat + target_active=true → char ya atacando, no emit.
        let mut fsm = Fsm::new();
        let p = perception_with_enemy_target(Some(true));

        let a = fsm.decide(&DecideContext {
            game: &game_at_tick(0), event: BotEvent::Tick, perception: &p,
            hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
            waypoint_hint: WaypointHint::Inactive, heal_override: None,
            attack_override: None,
            health_gate: HealthGateDecision::default(),
        });
        assert_eq!(a, BotAction::Idle,
            "con target activo no debe emit (evita rotar targets)");
        assert_eq!(fsm.state, FsmState::Fighting);
    }

    #[test]
    fn fighting_target_lost_triggers_retarget() {
        // Secuencia: target=true durante combate → target=false → emit retarget.
        let mut fsm = Fsm::new();
        let p_with    = perception_with_enemy_target(Some(true));
        let p_without = perception_with_enemy_target(Some(false));

        // Tick 0: char tiene target → no emit.
        let a = fsm.decide(&DecideContext {
            game: &game_at_tick(0), event: BotEvent::Tick, perception: &p_with,
            hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
            waypoint_hint: WaypointHint::Inactive, heal_override: None,
            attack_override: None,
            health_gate: HealthGateDecision::default(),
        });
        assert_eq!(a, BotAction::Idle);

        // Tick 100: target perdido (mob muerto) → emit PgDown para nuevo target.
        // Flanco true→false, safety_floor ok (nunca emitimos antes).
        let a = fsm.decide(&DecideContext {
            game: &game_at_tick(100), event: BotEvent::Tick, perception: &p_without,
            hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
            waypoint_hint: WaypointHint::Inactive, heal_override: None,
            attack_override: None,
            health_gate: HealthGateDecision::default(),
        });
        assert_eq!(a, BotAction::UseHotkey { hidcode: 0x2C },
            "flanco target_active true→false debe disparar retarget");
    }

    #[test]
    fn fighting_target_flicker_does_not_spam() {
        // Con target_active=true constante, solo emitiríamos en un flanco
        // real → false. Mientras siga true, no emit NUNCA (excepto keepalive).
        // Esto es el test crítico: NO ROTA targets por noise del detector.
        let mut fsm = Fsm::new();
        let p = perception_with_enemy_target(Some(true));

        // Ticks 0..500 con target_active=true → 0 emits.
        for t in 0..500 {
            let a = fsm.decide(&DecideContext {
                game: &game_at_tick(t), event: BotEvent::Tick, perception: &p,
                hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
                waypoint_hint: WaypointHint::Inactive, heal_override: None,
                attack_override: None,
                health_gate: HealthGateDecision::default(),
            });
            assert_eq!(a, BotAction::Idle,
                "tick {}: target activo, nunca debe emit por flanco", t);
        }
    }

    #[test]
    fn fighting_target_active_resets_keepalive() {
        // Si el char tiene target activo cada tick, el keepalive NUNCA debe
        // dispararse (se refresca constantemente). Esto protege sesiones
        // largas: el keepalive es fallback, no el signal primario.
        let mut fsm = Fsm::new();
        let p = perception_with_enemy_target(Some(true));

        // Corremos MÁS ticks que ATTACK_KEEPALIVE_TICKS (900). Todos deben ser Idle.
        for t in 0..1000 {
            let a = fsm.decide(&DecideContext {
                game: &game_at_tick(t), event: BotEvent::Tick, perception: &p,
                hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
                waypoint_hint: WaypointHint::Inactive, heal_override: None,
                attack_override: None,
                health_gate: HealthGateDecision::default(),
            });
            assert_eq!(a, BotAction::Idle,
                "tick {}: keepalive NO debe expirar mientras target activo", t);
        }
    }

    #[test]
    fn fighting_keepalive_emits_after_150_ticks_in_fallback() {
        // En fallback mode (sin target ROI), si hay combate y no hay flancos
        // durante >ATTACK_KEEPALIVE_TICKS (150 = 5s @ 30Hz), el keepalive
        // dispara un PgDown rescate.
        //
        // NOTA: con target_active=Some(true) el keepalive se refresca cada
        // tick. Este test usa perception_with_enemy() que tiene target_active
        // = None → fuerza el path fallback.
        let mut fsm = Fsm::new();
        let p = perception_with_enemy(); // target_active = None → fallback

        // Tick 0: primera observación en fallback → emit.
        let a = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::Tick, perception: &p,
               hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive, heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(a, BotAction::UseHotkey { hidcode: 0x2C });

        // Ticks 1..149: sin flancos, safety_floor puede permitir pero sin
        // trigger (keepalive no expirado) → NO emit.
        for t in 1..150 {
            let a = fsm.decide(&DecideContext {
                   game: &game_at_tick(t),
                   event: BotEvent::Tick, perception: &p,
                   hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
                   waypoint_hint: WaypointHint::Inactive, heal_override: None,
                   attack_override: None,
                   health_gate: HealthGateDecision::default(),
               });
            assert_eq!(a, BotAction::Idle, "tick {}: esperaba Idle antes del keepalive", t);
        }

        // Tick 150: keepalive expira → emit rescate.
        let a = fsm.decide(&DecideContext {
               game: &game_at_tick(150),
               event: BotEvent::Tick, perception: &p,
               hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive, heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(a, BotAction::UseHotkey { hidcode: 0x2C },
            "tick 150: keepalive debió disparar PgDown");
    }

    #[test]
    fn fighting_reappearance_after_zero_retriggers() {
        // Flanco de aparición: enemy_count 1 → 0 → 1 debe re-disparar un
        // PgDown (el char mató al anterior y aparece otro nuevo después).
        let mut fsm = Fsm::new();
        let p_enemy = perception_with_enemy();
        let p_empty = perception_vitals(Some(1.0), Some(1.0));

        // Tick 0: 1 enemy → emit.
        let a = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::Tick, perception: &p_enemy,
               hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive, heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(a, BotAction::UseHotkey { hidcode: 0x2C });

        // Tick 50: 0 enemies → Idle state, tracker a 0.
        let a = fsm.decide(&DecideContext {
               game: &game_at_tick(50),
               event: BotEvent::Tick, perception: &p_empty,
               hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive, heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(a, BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Idle);

        // Tick 100: nuevo enemy aparece → emit (flanco de subida desde 0).
        let a = fsm.decide(&DecideContext {
               game: &game_at_tick(100),
               event: BotEvent::Tick, perception: &p_enemy,
               hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive, heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(a, BotAction::UseHotkey { hidcode: 0x2C });
    }

    #[test]
    fn emergency_beats_fighting() {
        let mut fsm = Fsm::new();
        let mut p = perception_with_enemy();
        // Sobrescribir HP a crítico — hay enemigo visible pero también HP bajo.
        p.vitals.hp = Some(VitalBar { ratio: 0.10, filled_px: 10, total_px: 100 });

        let action = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::Tick,
               perception: &p,
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x3A }); // heal, no attack
        assert_eq!(fsm.state, FsmState::Emergency);
    }

    #[test]
    fn paused_resets_combat_state_then_reengages_cleanly() {
        // Entrar en Fighting con target activo → pausar → el state de combate
        // (prev_target_active) debe olvidarse. Al reanudar con un mob sin
        // target, se debe emitir un nuevo retarget (primera observación limpia).
        let mut fsm = Fsm::new();
        let p_enemy_no_target = perception_with_enemy_target(Some(false));
        let p_enemy_with_target = perception_with_enemy_target(Some(true));
        let p_empty = perception_vitals(Some(1.0), Some(1.0));

        // Tick 0: target activo durante combate → prev_target_active=Some(true).
        fsm.decide(&DecideContext {
            game: &game_at_tick(0), event: BotEvent::Tick, perception: &p_enemy_with_target,
            hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
            waypoint_hint: WaypointHint::Inactive, heal_override: None,
            attack_override: None,
            health_gate: HealthGateDecision::default(),
        });
        assert_eq!(fsm.prev_target_active, Some(true));

        // Tick 10: Pausar → state stale del Fighting se borra.
        fsm.decide(&DecideContext {
            game: &game_at_tick(10), event: BotEvent::PauseRequested,
            perception: &p_enemy_with_target,
            hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
            waypoint_hint: WaypointHint::Inactive, heal_override: None,
            attack_override: None,
            health_gate: HealthGateDecision::default(),
        });
        assert_eq!(fsm.state, FsmState::Paused);
        assert_eq!(fsm.prev_target_active, None,
            "combat state debe resetearse al entrar en Paused");

        // Tick 20: Resume con perception sin enemies → state=Idle.
        fsm.decide(&DecideContext {
            game: &game_at_tick(20), event: BotEvent::ResumeRequested,
            perception: &p_empty,
            hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
            waypoint_hint: WaypointHint::Inactive, heal_override: None,
            attack_override: None,
            health_gate: HealthGateDecision::default(),
        });
        assert_eq!(fsm.state, FsmState::Idle);
        assert_eq!(fsm.prev_target_active, None,
            "prev_target_active debe seguir None tras resume sin combat");

        // Tick 30: nuevo enemy sin target → debe ser primera observación limpia
        // (no reutilizar el Some(true) del Fighting anterior).
        let a = fsm.decide(&DecideContext {
            game: &game_at_tick(30), event: BotEvent::Tick, perception: &p_enemy_no_target,
            hotkeys: &test_hotkeys(), cd_heal: 10, cd_attack: 30,
            waypoint_hint: WaypointHint::Inactive, heal_override: None,
            attack_override: None,
            health_gate: HealthGateDecision::default(),
        });
        assert_eq!(a, BotAction::UseHotkey { hidcode: 0x2C },
            "tras resume, nuevo enemy sin target debe disparar emit limpio");
    }

    #[test]
    fn pause_event_forces_paused_state() {
        let mut fsm = Fsm::new();
        let action = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::PauseRequested,
               perception: &perception_with_enemy(),
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(action, BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Paused);
    }

    #[test]
    fn paused_game_state_blocks_actions() {
        let mut fsm = Fsm::new();
        let mut game = game_at_tick(100);
        game.is_paused = true;

        let action = fsm.decide(&DecideContext {
               game: &game,
               event: BotEvent::Tick,
               perception: &perception_vitals(Some(0.05), Some(1.0)),
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(action, BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Paused);
    }

    #[test]
    fn resume_event_clears_paused_state() {
        let mut fsm = Fsm::new();
        fsm.state = FsmState::Paused;
        let action = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::ResumeRequested,
               perception: &perception_vitals(Some(1.0), Some(1.0)),
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        // Tras resume, el mismo tick decide normalmente → Idle (todo nominal).
        assert_eq!(action, BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Idle);
    }

    // ── Waypoint hint tests ─────────────────────────────────────────────────

    #[test]
    fn walking_emits_waypoint_hint_when_idle() {
        let mut fsm = Fsm::new();
        let action = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::Tick,
               perception: &perception_vitals(Some(1.0), Some(1.0)),
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Active { emit: Some(WaypointEmit::KeyTap(0x60)) },
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x60 });
        assert_eq!(fsm.state, FsmState::Walking);
    }

    #[test]
    fn walking_state_persists_on_silent_tick_with_active_list() {
        // Active { key: None } significa "lista activa pero no emitir este tick".
        // La FSM debe seguir en Walking (no volver a Idle) para que /status
        // reporte consistentemente que el bot está ejecutando waypoints.
        let mut fsm = Fsm::new();
        let action = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::Tick,
               perception: &perception_vitals(Some(1.0), Some(1.0)),
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Active { emit: None },
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(action, BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Walking);
    }

    #[test]
    fn emergency_overrides_waypoint_hint() {
        let mut fsm = Fsm::new();
        let action = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::Tick,
               perception: &perception_vitals(Some(0.10), Some(1.0)),
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Active { emit: Some(WaypointEmit::KeyTap(0x60)) },
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        // HP crítico gana — ignora el hint del waypoint.
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x3A });
        assert_eq!(fsm.state, FsmState::Emergency);
    }

    #[test]
    fn fighting_overrides_waypoint_hint() {
        let mut fsm = Fsm::new();
        let action = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::Tick,
               perception: &perception_with_enemy(),
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Active { emit: Some(WaypointEmit::KeyTap(0x60)) },
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        // Hay enemigo → ataque gana, el waypoint queda en pausa.
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x2C });
        assert_eq!(fsm.state, FsmState::Fighting);
    }

    #[test]
    fn no_waypoint_hint_stays_idle() {
        let mut fsm = Fsm::new();
        let action = fsm.decide(&DecideContext {
               game: &game_at_tick(0),
               event: BotEvent::Tick,
               perception: &perception_vitals(Some(1.0), Some(1.0)),
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: WaypointHint::Inactive,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        assert_eq!(action, BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Idle);
    }

    #[test]
    fn is_interrupting_waypoints_matches_state() {
        let mut fsm = Fsm::new();
        assert!(!fsm.is_interrupting_waypoints());
        fsm.state = FsmState::Walking;
        assert!(!fsm.is_interrupting_waypoints());
        fsm.state = FsmState::Fighting;
        assert!(fsm.is_interrupting_waypoints());
        fsm.state = FsmState::Emergency;
        assert!(fsm.is_interrupting_waypoints());
        fsm.state = FsmState::Paused;
        assert!(!fsm.is_interrupting_waypoints());
    }

    // ── Integración FSM + WaypointList + combat interruption ────────────────
    //
    // Estos tests replican la lógica del BotLoop::run() paso a paso:
    //   1. Si el tick anterior estaba "interrupting", llamar
    //      waypoints.restart_current_step(tick).
    //   2. Consultar `waypoints.tick_action(tick)` como hint.
    //   3. Llamar `fsm.decide(...)`.
    //   4. Guardar `fsm.is_interrupting_waypoints()` para el siguiente tick.
    //
    // Esto valida el contrato entre los dos módulos (cosa que los tests
    // unitarios de cada uno por separado no garantizan).

    use crate::waypoints::{Step, WaypointList};

    /// Simula un tick del BotLoop: orquesta waypoint hint + fsm.decide() +
    /// restart-on-resume. Retorna (action, fsm_state, was_interrupting_this_tick).
    fn simulate_tick(
        fsm: &mut Fsm,
        wl: &mut WaypointList,
        tick: u64,
        perception: &Perception,
        prev_was_interrupting: bool,
    ) -> (BotAction, FsmState, bool) {
        if prev_was_interrupting {
            wl.restart_current_step(tick);
        }
        let hint = if wl.is_running() {
            WaypointHint::Active { emit: wl.tick_action(tick).map(WaypointEmit::KeyTap) }
        } else {
            WaypointHint::Inactive
        };
        let mut game = game_at_tick(tick);
        // Simulamos que el BotLoop usó `tick_num` del GameState antes de tick += 1.
        game.tick = tick;
        let action = fsm.decide(&DecideContext {
               game: &game,
               event: BotEvent::Tick,
               perception: &perception,
               hotkeys: &test_hotkeys(),
               cd_heal: 10,
               cd_attack: 30,
               waypoint_hint: hint,
               heal_override: None,
               attack_override: None,
               health_gate: HealthGateDecision::default(),
           });
        let interrupting = fsm.is_interrupting_waypoints();
        (action, fsm.state.clone(), interrupting)
    }

    /// Construye un `WaypointList` manualmente (sin TOML) para tests.
    fn wl_for_tests(steps: Vec<Step>, loop_: bool) -> WaypointList {
        // Usa el constructor helper público del módulo waypoints (expuesto
        // vía un paso intermedio: cargamos un TOML temporal).
        // El nombre del archivo usa un counter atomic + PID para evitar
        // colisiones cuando los tests corren en paralelo dentro del mismo
        // proceso (cargo test paraleliza por defecto).
        use std::io::Write as _;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let uniq = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        let name = format!("tibia_bot_fsm_integration_{}_{}.toml", std::process::id(), uniq);
        let path = dir.join(name);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "loop = {}", loop_).unwrap();
            for st in &steps {
                writeln!(f, "[[step]]").unwrap();
                writeln!(f, "label = \"{}\"", st.label).unwrap();
                writeln!(f, "key = \"{}\"", st.key).unwrap();
                writeln!(f, "duration_ms = {}", st.duration_ms).unwrap();
                writeln!(f, "interval_ms = {}", st.interval_ms).unwrap();
            }
        }
        let wl = WaypointList::load(&path, 30).unwrap();
        let _ = std::fs::remove_file(&path);
        wl
    }

    #[test]
    fn integration_walking_to_fighting_restarts_step_on_return() {
        // Lista con un solo step largo: walk_north 2000ms, interval 300ms.
        let steps = vec![Step {
            label:       "walk_north".into(),
            key:         "Numpad8".into(),
            hidcode:     Some(0x60),
            duration_ms: 2000,
            interval_ms: 300,
        }];
        let mut wl  = wl_for_tests(steps, false);
        let mut fsm = Fsm::new();
        let nominal = perception_vitals(Some(1.0), Some(1.0));
        let combat  = perception_with_enemy();
        let mut interrupt_flag = false;

        // Tick 0: Walking, emite primer key_tap.
        let (a0, s0, i0) = simulate_tick(&mut fsm, &mut wl, 0, &nominal, interrupt_flag);
        assert_eq!(a0, BotAction::UseHotkey { hidcode: 0x60 });
        assert_eq!(s0, FsmState::Walking);
        assert!(!i0);
        interrupt_flag = i0;

        // Tick 1: Walking silencioso (dentro del interval).
        let (a1, s1, i1) = simulate_tick(&mut fsm, &mut wl, 1, &nominal, interrupt_flag);
        assert_eq!(a1, BotAction::Idle);
        assert_eq!(s1, FsmState::Walking);
        interrupt_flag = i1;

        // Tick 2: aparece enemigo → Fighting, cooldown permite ataque.
        let (a2, s2, i2) = simulate_tick(&mut fsm, &mut wl, 2, &combat, interrupt_flag);
        assert_eq!(a2, BotAction::UseHotkey { hidcode: 0x2C }); // attack_default
        assert_eq!(s2, FsmState::Fighting);
        assert!(i2, "Fighting debe marcar interrupting");
        interrupt_flag = i2;

        // Tick 3: sigue el enemigo pero dentro de cd_attack (30 ticks) → Idle pero Fighting.
        let (a3, s3, i3) = simulate_tick(&mut fsm, &mut wl, 3, &combat, interrupt_flag);
        assert_eq!(a3, BotAction::Idle);
        assert_eq!(s3, FsmState::Fighting);
        assert!(i3);
        interrupt_flag = i3;

        // Tick 40: el enemigo desaparece. El tick anterior estaba interrupting
        // → el loop llama restart_current_step(40). La WaypointList emite el
        // PRIMER key_tap del step de nuevo (porque el restart borra last_emit).
        let (a40, s40, i40) = simulate_tick(&mut fsm, &mut wl, 40, &nominal, interrupt_flag);
        assert_eq!(a40, BotAction::UseHotkey { hidcode: 0x60 },
            "tras salir de combat el step reinicia y emite primer tap");
        assert_eq!(s40, FsmState::Walking);
        assert!(!i40);
    }

    #[test]
    fn integration_walking_to_emergency_restarts_step_on_return() {
        // Mismo patrón con Emergency (HP crítico) en vez de Fighting.
        let steps = vec![Step {
            label:       "walk_north".into(),
            key:         "Numpad8".into(),
            hidcode:     Some(0x60),
            duration_ms: 2000,
            interval_ms: 300,
        }];
        let mut wl  = wl_for_tests(steps, false);
        let mut fsm = Fsm::new();
        let nominal = perception_vitals(Some(1.0), Some(1.0));
        let low_hp  = perception_vitals(Some(0.10), Some(1.0));
        let mut interrupt_flag = false;

        // Tick 0: Walking.
        let (_, s0, i0) = simulate_tick(&mut fsm, &mut wl, 0, &nominal, interrupt_flag);
        assert_eq!(s0, FsmState::Walking);
        interrupt_flag = i0;

        // Tick 1: HP crítico → Emergency + heal_spell.
        let (a1, s1, i1) = simulate_tick(&mut fsm, &mut wl, 1, &low_hp, interrupt_flag);
        assert_eq!(a1, BotAction::UseHotkey { hidcode: 0x3A }); // heal_spell
        assert_eq!(s1, FsmState::Emergency);
        assert!(i1);
        interrupt_flag = i1;

        // Tick 100: HP recuperado → Walking con step reiniciado.
        let (a100, s100, _) = simulate_tick(&mut fsm, &mut wl, 100, &nominal, interrupt_flag);
        assert_eq!(a100, BotAction::UseHotkey { hidcode: 0x60 });
        assert_eq!(s100, FsmState::Walking);
    }

    #[test]
    fn integration_combat_does_not_advance_step_timer() {
        // Si el combate dura 10 ticks, el step de 60 ticks no debe terminarse
        // durante el combate — al volver, debe empezar de cero con 60 ticks enteros.
        let steps = vec![
            Step {
                label:       "walk_north".into(),
                key:         "Numpad8".into(),
                hidcode:     Some(0x60),
                duration_ms: 2000,  // 60 ticks
                interval_ms: 0,     // un solo emit al inicio
            },
            Step {
                label:       "walk_south".into(),
                key:         "Numpad2".into(),
                hidcode:     Some(0x5A),
                duration_ms: 1000,
                interval_ms: 0,
            },
        ];
        let mut wl  = wl_for_tests(steps, false);
        let mut fsm = Fsm::new();
        let nominal = perception_vitals(Some(1.0), Some(1.0));
        let combat  = perception_with_enemy();
        let mut interrupt_flag = false;

        // Tick 0: emite walk_north (primer tap del step).
        let (a, _, i) = simulate_tick(&mut fsm, &mut wl, 0, &nominal, interrupt_flag);
        assert_eq!(a, BotAction::UseHotkey { hidcode: 0x60 });
        interrupt_flag = i;

        // Ticks 1..50: combate constante. El step no avanza.
        for t in 1..=50 {
            let (_, state, flag) = simulate_tick(&mut fsm, &mut wl, t, &combat, interrupt_flag);
            assert_eq!(state, FsmState::Fighting);
            interrupt_flag = flag;
        }
        assert_eq!(wl.current_label().unwrap().0, 0, "el step no debe haber avanzado durante el combate");

        // Tick 51: combate termina. Restart del step → vuelve a emitir walk_north.
        let (a51, _, i51) = simulate_tick(&mut fsm, &mut wl, 51, &nominal, interrupt_flag);
        assert_eq!(a51, BotAction::UseHotkey { hidcode: 0x60 });
        interrupt_flag = i51;

        // Ticks 52..110: nominal. Con duration=60 ticks e interval=0, el step
        // debe expirar en tick 111 (51+60) y pasar al walk_south.
        for t in 52..111 {
            let (_, _, flag) = simulate_tick(&mut fsm, &mut wl, t, &nominal, interrupt_flag);
            interrupt_flag = flag;
            assert_eq!(wl.current_label().unwrap().0, 0, "tick {}: sigue en walk_north", t);
        }
        // Tick 111: walk_north expira, advance a walk_south, emite 0x5A.
        let (a111, _, _) = simulate_tick(&mut fsm, &mut wl, 111, &nominal, interrupt_flag);
        assert_eq!(a111, BotAction::UseHotkey { hidcode: 0x5A });
        assert_eq!(wl.current_label().unwrap().1, "walk_south");
    }

    // ── M2: Integration tests para secuencias de transiciones FSM ────────

    /// Helper para invocar decide() con defaults razonables.
    fn decide(
        fsm: &mut Fsm,
        game: &GameState,
        perception: &Perception,
        event: BotEvent,
    ) -> BotAction {
        fsm.decide(&DecideContext {
            game,
            event,
            perception,
            hotkeys: &test_hotkeys(),
            cd_heal: 10,
            cd_attack: 30,
            waypoint_hint: WaypointHint::Inactive,
            heal_override: None,
            attack_override: None,
            health_gate: HealthGateDecision::default(),
        })
    }

    #[test]
    fn idle_to_emergency_to_idle_sequence() {
        let mut fsm = Fsm::new();

        // Tick 0: HP full → Idle
        let game = game_at_tick(0);
        let p_full = perception_vitals(Some(1.0), Some(1.0));
        assert_eq!(decide(&mut fsm, &game, &p_full, BotEvent::Tick), BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Idle);

        // Tick 100: HP cae a 20% → Emergency, emite heal spell
        let game = game_at_tick(100);
        let p_low = perception_vitals(Some(0.20), Some(1.0));
        let action = decide(&mut fsm, &game, &p_low, BotEvent::Tick);
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x3A });
        assert_eq!(fsm.state, FsmState::Emergency);

        // Tick 200: HP recovered → Idle (con cooldown ya pasado)
        let game = game_at_tick(200);
        let p_recovered = perception_vitals(Some(1.0), Some(1.0));
        assert_eq!(decide(&mut fsm, &game, &p_recovered, BotEvent::Tick), BotAction::Idle);
        assert_eq!(fsm.state, FsmState::Idle);
    }

    #[test]
    fn fighting_emits_attack_and_stays_in_state() {
        let mut fsm = Fsm::new();
        let game = game_at_tick(50);
        let p_combat = perception_with_enemy_target(Some(false));

        // Primer tick con combat → Fighting + emit attack.
        let action = decide(&mut fsm, &game, &p_combat, BotEvent::Tick);
        assert!(matches!(action, BotAction::UseHotkey { .. }),
            "esperaba UseHotkey, got {:?}", action);
        assert_eq!(fsm.state, FsmState::Fighting);
    }

    #[test]
    fn safety_pause_event_forces_idle() {
        let mut fsm = Fsm::new();
        let game = game_at_tick(100);
        // Aún con HP crítico, PauseRequested debe forzar Idle.
        let p_critical = perception_vitals(Some(0.10), Some(0.10));
        let action = decide(&mut fsm, &game, &p_critical, BotEvent::PauseRequested);
        assert_eq!(action, BotAction::Idle);
        // Pause es prioritario sobre Emergency.
        assert_eq!(fsm.state, FsmState::Paused);
    }

    #[test]
    fn multi_tick_fsm_remains_consistent() {
        let mut fsm = Fsm::new();
        let p_nominal = perception_vitals(Some(1.0), Some(1.0));

        // 30 ticks de nominal — debe quedarse en Idle sin diverger.
        for tick in 0..30 {
            let game = game_at_tick(tick);
            let action = decide(&mut fsm, &game, &p_nominal, BotEvent::Tick);
            assert_eq!(action, BotAction::Idle, "tick {} non-idle", tick);
            assert_eq!(fsm.state, FsmState::Idle, "tick {} state drift", tick);
        }
    }

    #[test]
    fn emergency_priority_over_combat_when_both_active() {
        let mut fsm = Fsm::new();
        let game = game_at_tick(50);

        // Construir una Perception con HP crítico Y enemies.
        let critical_hp = VitalBar { ratio: 0.15, filled_px: 15, total_px: 100 };
        let mut p = perception_with_enemy_target(Some(true));
        p.vitals.hp = Some(critical_hp);

        let action = decide(&mut fsm, &game, &p, BotEvent::Tick);
        // Emergency (heal) tiene prioridad sobre Fighting (attack).
        assert_eq!(action, BotAction::UseHotkey { hidcode: 0x3A });
        assert_eq!(fsm.state, FsmState::Emergency);
    }
}
