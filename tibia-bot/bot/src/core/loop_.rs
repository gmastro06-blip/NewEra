/// loop_.rs — Game loop "Sense → Think → Act → Tick" a 30 Hz.
/// (Nombre con underscore porque `loop` es palabra clave de Rust.)
///
/// Propiedades de fiabilidad:
/// - NUNCA crashea: todos los errores son capturados y loggeados.
/// - Tick budgeting: si un tick tarda más de 1/fps segundos, loggea overrun
///   y no duerme — el próximo tick empieza de inmediato para recuperar tiempo.
/// - El lag nunca se acumula: usamos deadline absoluto en lugar de sleep(dt).
/// - Si la visión o la actuación fallan, el loop sigue en el siguiente tick.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use tokio::runtime::Handle;
use tracing::{debug, info, warn};

use crate::act::Actuator;
use crate::config::{Config, Hotkeys};
use crate::core::fsm::{BotAction, BotEvent, DecideContext, Fsm, FsmTiming, WaypointHint};
use crate::core::state::{ScriptStatus, SharedState, WaypointStatus};
use crate::safety::{
    BreakScheduler, HumanNoise, RateLimiter, ReactionGate, WeightedChoice,
};
use crate::safety::breaks::BreakStatus;
use crate::safety::session_limit::SessionLimit;
use crate::scripting::{ScriptEngine, ScriptResult, TickContext};
use crate::sense::frame_buffer::FrameBuffer;
use crate::sense::vision::Vision;
use crate::cavebot::{Cavebot, CavebotAction, TickContext as CavebotTickContext};
use crate::waypoints::WaypointList;

/// Comandos que el HTTP server envía al game loop.
/// Se procesan al inicio de cada tick (1 por tick máximo para no bloquear).
#[derive(Debug)]
pub enum LoopCommand {
    /// Carga una nueva WaypointList desde disco, reemplazando la actual.
    LoadWaypoints { path: PathBuf, enabled: bool },
    /// Pausa la ejecución de waypoints sin borrar la lista cargada.
    PauseWaypoints,
    /// Reanuda la ejecución de waypoints desde el step actual.
    ResumeWaypoints,
    /// Descarga la WaypointList actual.
    ClearWaypoints,
    /// Recarga todos los scripts Lua desde el path dado (o el actual si None).
    ReloadScripts { path: Option<PathBuf> },
    // ── Cavebot (Fase C) ───────────────────────────────────────────────
    /// Carga un archivo de cavebot desde disco (formato TOML extendido con
    /// labels, goto, stand, etc). Reemplaza el cavebot actual si había uno.
    LoadCavebot { path: PathBuf, enabled: bool },
    /// Pausa el cavebot sin borrarlo.
    PauseCavebot,
    /// Reanuda el cavebot desde el step actual.
    ResumeCavebot,
    /// Descarga el cavebot cargado.
    ClearCavebot,
    /// Salta el cavebot a un label específico (útil para test focused).
    JumpToCavebotLabel { label: String },
    // ── Recording (F1.4) ────────────────────────────────────────────────
    /// Inicia grabación de perception snapshots a JSONL.
    /// Si `path` es None, usa el default `session.jsonl`.
    StartRecording { path: Option<String> },
    /// Detiene la grabación actual y flushea el archivo.
    StopRecording,
}

pub struct BotLoop {
    config:    Config,
    hotkeys:   Hotkeys,
    state:     SharedState,
    buffer:    Arc<FrameBuffer>,
    actuator:  Arc<Actuator>,
    rt_handle: Handle,
    vision:    Vision,
    commands:  Receiver<LoopCommand>,
    waypoints: Option<WaypointList>,
    /// true = la lista cargada está activa. false = cargada pero pausada.
    waypoints_enabled: bool,
    /// Directorio de scripts Lua. El ScriptEngine NO vive aquí porque
    /// `mlua::Lua` no es `Send` — se crea dentro de `run()` en el thread.
    script_dir: Option<PathBuf>,
    /// Cavebot v2 (Fase C). Si está presente y enabled, toma prioridad sobre
    /// el `waypoints` legacy. Ambos coexisten para permitir migración suave.
    cavebot: Option<Cavebot>,
    cavebot_enabled: bool,
    /// Grabador opcional de Perception snapshots (F1 replay tool).
    recorder: Option<crate::sense::recorder::PerceptionRecorder>,
    /// Contador consecutivo de ticks sin frame NDI (watchdog de visión).
    /// Se resetea a 0 cuando llega un frame. Cuando supera
    /// `NO_FRAME_PAUSE_TICKS` se dispara una safety pause y se vuelve a 0
    /// para no re-disparar en loop.
    no_frame_ticks: u32,
}

/// Umbral de ticks consecutivos sin frame NDI antes de pausar por seguridad.
/// A 30 Hz son ~4 segundos — suficiente para distinguir un hiccup puntual
/// de OBS/DistroAV muerto de verdad.
const NO_FRAME_PAUSE_TICKS: u32 = 120;

impl BotLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config:    Config,
        hotkeys:   Hotkeys,
        state:     SharedState,
        buffer:    Arc<FrameBuffer>,
        actuator:  Arc<Actuator>,
        rt_handle: Handle,
        vision:    Vision,
        commands:  Receiver<LoopCommand>,
    ) -> Self {
        // Auto-cargar WaypointList si el config apunta a un archivo existente.
        let fps = config.loop_config.target_fps;
        let (waypoints, waypoints_enabled) = if config.waypoints.path.is_empty() {
            (None, false)
        } else {
            let path = PathBuf::from(&config.waypoints.path);
            match WaypointList::load(&path, fps) {
                Ok(wl) => {
                    info!("Waypoints cargados desde '{}': {} steps, loop={}, enabled={}",
                          path.display(), wl.steps.len(), wl.loop_, config.waypoints.enabled);
                    (Some(wl), config.waypoints.enabled)
                }
                Err(e) => {
                    warn!("No se pudieron cargar waypoints desde '{}': {:#}", path.display(), e);
                    (None, false)
                }
            }
        };

        // Auto-cargar cavebot si el config apunta a un archivo existente.
        let (cavebot, cavebot_enabled) = if config.cavebot.path.is_empty() {
            (None, false)
        } else {
            let path = PathBuf::from(&config.cavebot.path);
            let tuning = crate::cavebot::runner::NodeTuning::from_config(&config.cavebot);
            match crate::cavebot::parser::load_with_tuning(&path, fps, tuning) {
                Ok(cb) => {
                    info!("Cavebot cargado desde '{}': {} steps, loop={}, enabled={}",
                          path.display(), cb.steps.len(), cb.loop_, config.cavebot.enabled);
                    (Some(cb), config.cavebot.enabled)
                }
                Err(e) => {
                    warn!("No se pudo cargar cavebot desde '{}': {:#}", path.display(), e);
                    (None, false)
                }
            }
        };

        // Script dir opcional — el ScriptEngine se crea dentro del thread
        // del game loop (ver `run()`) porque `mlua::Lua` no es `Send`.
        let script_dir = if config.scripting.script_dir.is_empty() {
            None
        } else {
            Some(PathBuf::from(&config.scripting.script_dir))
        };

        // F1 recorder: si config.recording.enabled, crear PerceptionRecorder.
        let recorder = if config.recording.enabled {
            let path = if config.recording.path.is_empty() {
                PathBuf::from("session.jsonl")
            } else {
                PathBuf::from(&config.recording.path)
            };
            let interval = config.recording.interval_ticks.unwrap_or(1);
            Some(crate::sense::recorder::PerceptionRecorder::new(path, interval))
        } else {
            None
        };

        Self {
            config, hotkeys, state, buffer, actuator, rt_handle, vision,
            commands, waypoints, waypoints_enabled, script_dir,
            cavebot,
            cavebot_enabled,
            recorder,
            no_frame_ticks: 0,
        }
    }

    /// Lanza el game loop en un thread dedicado. No retorna.
    pub fn spawn(self) {
        std::thread::Builder::new()
            .name("game-loop".into())
            .spawn(move || {
                self.run();
            })
            .expect("No se pudo lanzar el thread game-loop");
    }

    fn run(mut self) {
        let target_fps      = self.config.loop_config.target_fps;
        let tick_budget     = Duration::from_secs_f64(1.0 / target_fps as f64);
        let cd_heal         = self.config.actions.heal_cooldown_ticks;
        let cd_attack       = self.config.actions.attack_cooldown_ticks;
        let stuck_threshold = self.config.waypoints.stuck_threshold_ticks;
        let script_budget   = self.config.scripting.tick_budget_ms;

        // ── Safety (Fase 5) ──────────────────────────────────────────────────
        let safety = self.config.safety.clone();

        // FSM con timing jittered si safety.humanize_timing = true.
        let mut fsm = if safety.humanize_timing {
            Fsm::with_timing(FsmTiming {
                heal_cd_mean_ms:   safety.heal_cd_mean_ms,
                heal_cd_std_ms:    safety.heal_cd_std_ms,
                attack_cd_mean_ms: safety.attack_cd_mean_ms,
                attack_cd_std_ms:  safety.attack_cd_std_ms,
                fps:               target_fps,
            })
        } else {
            Fsm::new()
        };

        // Reaction gates para HP crítico y detección de enemigo.
        let mut hp_gate = ReactionGate::new(
            "hp_critical",
            if safety.humanize_timing { safety.reaction_hp_mean_ms    } else { 0.0 },
            if safety.humanize_timing { safety.reaction_hp_std_ms     } else { 0.0 },
            target_fps,
        );
        let mut enemy_gate = ReactionGate::new(
            "enemy_found",
            if safety.humanize_timing { safety.reaction_enemy_mean_ms } else { 0.0 },
            if safety.humanize_timing { safety.reaction_enemy_std_ms  } else { 0.0 },
            target_fps,
        );

        // Rate limiter global — red de seguridad contra bursts.
        let mut rate_limiter = RateLimiter::new("global", safety.max_actions_per_sec);

        // Spell table: si hay [[spell]] en config, usarlo. Sino, fallback legacy.
        let jitter_factor = if safety.humanize_timing { Some(0.25) } else { None };
        let mut spell_table = if !self.config.spells.is_empty() {
            info!("SpellTable: {} spells configurados", self.config.spells.len());
            crate::core::spell_table::SpellTable::from_configs(&self.config.spells, target_fps, jitter_factor)
        } else {
            crate::core::spell_table::SpellTable::from_legacy(
                &self.hotkeys,
                cd_heal * 1000 / target_fps as u64,    // ticks → ms
                cd_attack * 1000 / target_fps as u64,
            )
        };
        // Legacy heal_choice se mantiene solo si no hay [[spell]] y heal_variation está activa.
        let heal_choice = if self.config.spells.is_empty() && safety.heal_variation {
            WeightedChoice::new(vec![
                (self.hotkeys.heal_spell,  safety.heal_spell_weight),
                (self.hotkeys.heal_potion, safety.heal_potion_weight),
            ])
        } else {
            WeightedChoice::new(vec![])
        };

        // Break scheduler (opt-in).
        let mut breaks = if safety.breaks_enabled {
            Some(BreakScheduler::new_standard(Instant::now()))
        } else {
            None
        };

        // Session duration cap (opt-in via max_session_hours > 0).
        // Baseline = tick actual (normalmente 0 al arrancar, pero defensivo
        // por si el loop corre con estado pre-existente).
        // `session_warning_min > 0` habilita el graceful refill: T-N min
        // antes del cap, el loop fuerza un goto al label "refill" del
        // cavebot (si existe) para vaciar el bag antes de pausar.
        let session_start_tick = self.state.read().tick;
        let mut session_limit = SessionLimit::new(
            safety.max_session_hours,
            target_fps,
            session_start_tick,
        )
        .map(|sl| sl.with_warning_min(safety.session_warning_min, target_fps));
        // Latch: una vez disparado el warning no volvemos a inyectar el
        // goto "refill" cada tick. El FSM/cavebot siguen su camino hasta
        // que is_expired aplique el hard pause, o hasta que el operador
        // reanude manualmente.
        let mut session_warning_active = false;
        if session_limit.is_some() {
            info!(
                "Safety: session cap activo — {:.2}h (baseline tick={}, warning_min={:.1})",
                safety.max_session_hours, session_start_tick, safety.session_warning_min,
            );
        }

        // Human noise emitter (opt-in).
        let mut human_noise = if safety.human_noise_enabled && !safety.human_noise_keys.is_empty() {
            let hids: Vec<u8> = safety.human_noise_keys.iter()
                .filter_map(|k| crate::act::keycode::parse(k).ok())
                .collect();
            if hids.is_empty() {
                None
            } else {
                Some(HumanNoise::new(
                    hids,
                    safety.human_noise_interval_mean_s,
                    safety.human_noise_interval_std_s,
                    Instant::now(),
                ))
            }
        } else {
            None
        };

        // ── ScriptEngine ──────────────────────────────────────────────────────
        // Se crea aquí, dentro del thread del loop, porque `mlua::Lua` no es
        // `Send`. `load_dir` puede fallar sin romper el loop — simplemente no
        // hay scripts activos en ese caso.
        let mut scripts: Option<ScriptEngine> = match ScriptEngine::new(script_budget) {
            Ok(mut eng) => {
                if let Some(dir) = self.script_dir.clone() {
                    match eng.load_dir(&dir) {
                        Ok(()) => {
                            info!("ScriptEngine listo, {} archivo(s) cargado(s) desde '{}'",
                                  eng.loaded_files().len(), dir.display());
                        }
                        Err(e) => {
                            warn!("No se pudieron cargar scripts de '{}': {:#}", dir.display(), e);
                        }
                    }
                }
                Some(eng)
            }
            Err(e) => {
                warn!("No se pudo crear ScriptEngine: {:#}. Scripting deshabilitado.", e);
                None
            }
        };

        info!("Game loop arrancando a {} Hz (presupuesto/tick = {:.1}ms)",
              target_fps, tick_budget.as_secs_f64() * 1000.0);
        if safety.humanize_timing {
            info!("Safety: humanización temporal ON — jitter {:.0}±{:.0}ms heal, {:.0}±{:.0}ms attack, reaction ~{:.0}ms HP",
                  safety.heal_cd_mean_ms, safety.heal_cd_std_ms,
                  safety.attack_cd_mean_ms, safety.attack_cd_std_ms,
                  safety.reaction_hp_mean_ms);
        }

        let mut next_tick = Instant::now();
        let mut prev_was_interrupting = false;
        let mut current_pause_reason: Option<String> = None;

        // ── Phase C.1: FSM state change tracking ────────────────────────────
        // Usados para disparar `on_fsm_state_change(new_state, reason)` a los
        // scripts Lua cuando el FSM transiciona o la safety pause cambia.
        // Inicializamos a "Idle" + None para que el primer tick con state
        // distinto (o con reason) ya dispare el hook (evento sintético de
        // "bot arrancó con estado X").
        let mut prev_fsm_state_str: String = "Idle".to_string();
        let mut prev_pause_reason_str: Option<String> = None;

        // ── Kill counter + activity tracking (para cavebot Stand/GotoIf) ────
        // `kills_total` se incrementa cada vez que `target_active` hace
        // flanco true → false mientras hay combat. Es la aproximación más
        // directa a "mató a su target actual".
        let mut kills_total: u64 = 0;
        let mut prev_target_active_for_kills: Option<bool> = None;
        let mut prev_enemy_count_for_cavebot: u64 = 0;
        // `last_activity_tick`: último tick donde hubo combate o delta HP.
        // Usado por SkipIfBlocked del cavebot.
        let mut last_activity_tick: u64 = 0;
        let mut last_hp_for_activity: f32 = 1.0;

        // ── Typing buffer (Fase D: bot.say()) ────────────────────────────
        // Cada string pendiente de tipear se convierte en una secuencia de
        // HID keycodes: [Enter, chars..., Enter]. El primer Enter abre el
        // chat de Tibia, el segundo envía el mensaje.
        //
        // Pace: típico humano 150-300ms/char con alta varianza. Anti-detection
        // 2026-04-17: spacing aleatorio por char ∈ [TYPING_MIN, TYPING_MAX]
        // ticks. Antes era fijo en 4 ticks (133ms) — uniformity es un
        // fingerprint de bot (humanos varían naturalmente entre teclas).
        //
        // Rango: 3-8 ticks @ 30Hz = 100-267ms por tecla. Respeta rate
        // limiter global (8 keys/s default) en el peor caso (100ms = 10/s).
        let mut typing_buffer: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        let mut typing_next_tick: u64 = 0;
        const TYPING_MIN_TICKS: u64 = 3;
        const TYPING_MAX_TICKS: u64 = 8;

        // ── Auto-loot state (kill-driven, smoothed sparkles como guard) ──
        //
        // **Design definitivo tras 3 iteraciones in-vivo**:
        //
        // El auto-loot se dispara en respuesta a KILLS REALES DEL BOT, no
        // a sparkles puros. Los sparkles sirven como GUARD — solo emitimos
        // si hay loot visible Y el bot ha hecho kills pendientes por lootear.
        //
        // **Contador `kills_pending_loot`**:
        // - +1 cada vez que `enemy_count` baja (un mob murió)
        // - -1 cada emit exitoso (asumimos que Quick Loot toma 1 corpse por press)
        // - No baja de 0 (saturating)
        //
        // **Trigger del emit**:
        // - sparkles_max >= THRESHOLD (hay loot visible)
        // - kills_pending_loot > 0 (algo mío por lootear)
        // - cooldown_ok (≥3s desde último emit, pace humano)
        // - safety_ok (no Emergency, no paused)
        //
        // **Por qué funciona**:
        // - **1 kill → 1 emit**: ratio exacto 1:1 en combate single-target
        // - **Multi-kill burst**: contador acumula, emits espaciados cooldown=3s
        // - **Pre-existing corpses**: sparkles visible pero kills_pending=0 → no emit
        // - **Loot rechazado por client**: QL falla, sparkles persisten, pero el
        //   contador baja a 0 tras el primer emit → no más spam
        // - **No circuit breaker artificial**: el contador ES el circuit breaker
        //
        // **Sparkles smoothing**:
        // Las sparkles de Tibia pulsan con ciclo ~60 ticks (2s) y duty cycle bajo.
        // Ventana rolling max de 120 ticks (4s) asegura que cualquier peak queda
        // memorizado hasta bien después del siguiente peak.
        let loot_hotkey = self.hotkeys.loot_hotkey;
        let mut loot_next_tick: u64 = 0;
        let mut kills_pending_loot: u32 = 0;
        let mut prev_enemy_count_for_kills: u32 = 0;
        let mut sparkles_window: std::collections::VecDeque<u32> = std::collections::VecDeque::with_capacity(120);
        const SPARKLES_WINDOW_SIZE: usize = 120;
        const LOOT_COOLDOWN_TICKS: u64 = 90; // 3s @ 30 Hz — humanización anti-spam

        loop {
            let tick_start = Instant::now();
            let tick_num   = self.state.read().tick;

            // ── COMMANDS (HTTP → loop) ────────────────────────────────────────
            // Drenar hasta 4 comandos por tick. Los comandos que no caben se
            // procesan en el siguiente tick (el canal es unbounded así que no
            // se pierden). Limitar el drain protege el budget del tick.
            for _ in 0..4 {
                match self.commands.try_recv() {
                    Ok(LoopCommand::ReloadScripts { path }) => {
                        // ReloadScripts se procesa aquí (no en handle_command)
                        // porque necesita acceso a la variable local `scripts`
                        // que vive en el stack de run(), no en `self`.
                        let dir = path.or_else(|| self.script_dir.clone());
                        match dir {
                            Some(dir) => {
                                // Si el engine falló al arrancar y está None,
                                // intentar recrearlo ahora (recovery).
                                if scripts.is_none() {
                                    match ScriptEngine::new(script_budget) {
                                        Ok(eng) => {
                                            info!("ScriptEngine recreado tras fallo previo");
                                            scripts = Some(eng);
                                        }
                                        Err(e) => {
                                            warn!("ReloadScripts: no se pudo recrear ScriptEngine: {:#}", e);
                                        }
                                    }
                                }

                                if let Some(eng) = scripts.as_mut() {
                                    match eng.load_dir(&dir) {
                                        Ok(()) => {
                                            info!("Scripts recargados desde '{}': {} archivo(s)",
                                                  dir.display(), eng.loaded_files().len());
                                            self.script_dir = Some(dir);
                                        }
                                        Err(e) => {
                                            warn!("Reload scripts falló: {:#}", e);
                                        }
                                    }
                                }
                            }
                            None => warn!("ReloadScripts ignorado: no hay script_dir"),
                        }
                    }
                    Ok(cmd) => self.handle_command(cmd, tick_num),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        warn!("Command channel desconectado — HTTP server murió?");
                        break;
                    }
                }
            }

            // ── SENSE ─────────────────────────────────────────────────────────
            let frame_arc = self.buffer.load_arc();
            let has_frame = frame_arc.is_some();

            // ── WATCHDOG: no-frame ────────────────────────────────────────────
            // Si NDI muere (OBS crasheó, cable desconectado, DistroAV caído) el
            // buffer queda sin frames. La visión cae a `Perception::default()` y
            // el bot seguiría "ciego" emitiendo acciones sobre estado vacío.
            // Tras ~4s sin frame (NO_FRAME_PAUSE_TICKS @ 30 Hz) forzamos pausa
            // con reason "vision:no_frame". Solo loggeamos una vez al disparar
            // para no spamear si el frame oscila entre visible/ausente.
            match step_no_frame_watchdog(&mut self.no_frame_ticks, has_frame, NO_FRAME_PAUSE_TICKS) {
                NoFrameStep::TriggerPause => {
                    warn!(
                        "Watchdog: {} ticks consecutivos sin frame NDI — pausando bot \
                         (vision:no_frame). Verificar OBS / DistroAV / conexión de red.",
                        NO_FRAME_PAUSE_TICKS
                    );
                    {
                        let mut g = self.state.write();
                        g.is_paused = true;
                        g.safety_pause_reason = Some("vision:no_frame".into());
                    }
                    // Propagar al tracking local para que el resto del tick
                    // (is_safety_paused, hooks Lua on_fsm_state_change, etc.)
                    // vea la pausa con la misma reason coherente.
                    current_pause_reason = Some("vision:no_frame".into());
                }
                NoFrameStep::FrameOk => {
                    // Fix 2026-04-17: si frames volvieron y la reason previa
                    // era "vision:no_frame", la limpiamos para que el FSM
                    // pueda salir de Paused cuando el operador re-resume.
                    // Sin esta limpieza, la reason queda pegada y el FSM
                    // siempre vuelve a Paused aunque is_paused=false.
                    //
                    // Limpiamos AMBAS: current_pause_reason (variable local
                    // del loop que el FSM chequea) y state.safety_pause_reason
                    // (struct compartido expuesto vía HTTP).
                    if current_pause_reason.as_deref() == Some("vision:no_frame") {
                        current_pause_reason = None;
                    }
                    let mut g = self.state.write();
                    if g.safety_pause_reason.as_deref() == Some("vision:no_frame") {
                        g.safety_pause_reason = None;
                    }
                }
                NoFrameStep::Accumulating => {}
            }

            // ── VISION ────────────────────────────────────────────────────────
            //
            // Antes del tick, declarar qué templates OnDemand necesita el step
            // actual del cavebot. Sin esto, el UiDetector procesa TODOS los
            // templates cada ciclo (cycle time ~23s, bench 2026-04-18). Con
            // esto, en steady state el cycle cae a <50ms.
            //
            // La lista se calcula desde el step "actual" (el que correría en
            // este tick). Hay hasta SUBMIT_INTERVAL (500ms) de retardo entre
            // una transición de step y el UiDetector viendo un match — trivial
            // comparado con los 23s previos.
            if let Some(cb) = self.cavebot.as_ref() {
                let required = cb.required_ui_templates();
                let refs: Vec<&str> = required.iter().map(String::as_str).collect();
                self.vision.set_ui_demand(&refs);
            } else {
                // Sin cavebot activo → ningún OnDemand (solo Always).
                self.vision.set_ui_demand(&[]);
            }

            let vision_start = Instant::now();
            let perception = if let Some(ref frame) = *frame_arc {
                self.vision.tick(frame, tick_num)
            } else {
                crate::sense::perception::Perception {
                    frame_tick: tick_num,
                    ..Default::default()
                }
            };
            let vision_cost_ms = vision_start.elapsed().as_secs_f32() * 1000.0;

            // ── BATTLE DEBUG (cada 100 ticks al nivel DEBUG) ──────────────────
            if tick_num.is_multiple_of(100) {
                for slot in &perception.battle.slot_debug {
                    debug!(
                        "battle slot row={} y={} red={} blue={} yellow={} kind={:?}",
                        slot.row, slot.frame_y,
                        slot.red_hits, slot.blue_hits, slot.yellow_hits,
                        slot.kind,
                    );
                }
            }

            // ── KILL COUNTER (Fase C) ─────────────────────────────────────────
            // Doble fuente: (1) flanco target_active true→false y (2) drops en
            // enemy_count. Tomamos el máximo delta por tick para no double-count.
            let mut kills_this_tick = 0u64;
            // Fuente 1: flanco target_active
            if let Some(target_active) = perception.target_active {
                let was_active = matches!(prev_target_active_for_kills, Some(true));
                if was_active && !target_active && perception.battle.has_enemies() {
                    kills_this_tick = 1;
                }
                prev_target_active_for_kills = Some(target_active);
            }
            // Fuente 2: drops en enemy_count (más fiable en multi-mob)
            let current_enemy_count = perception.battle.enemy_count() as u64;
            if current_enemy_count < prev_enemy_count_for_cavebot {
                let delta = prev_enemy_count_for_cavebot - current_enemy_count;
                kills_this_tick = kills_this_tick.max(delta);
            }
            prev_enemy_count_for_cavebot = current_enemy_count;
            kills_total = kills_total.saturating_add(kills_this_tick);
            // Tracking de actividad para SkipIfBlocked del cavebot.
            let any_activity = perception.battle.has_enemies()
                || perception.vitals.hp.map(|b| (b.ratio - last_hp_for_activity).abs() > 0.01).unwrap_or(false);
            if any_activity {
                last_activity_tick = tick_num;
            }
            if let Some(hp) = perception.vitals.hp { last_hp_for_activity = hp.ratio; }

            // ── WAYPOINTS / CAVEBOT: consulta el hint antes de la FSM ─────────
            // Congelar cavebot durante safety pause para evitar que avance
            // contadores/fases mientras las acciones están bloqueadas.
            // current_pause_reason persiste entre ticks (se actualizó en el tick anterior).
            let is_safety_paused_for_cavebot = current_pause_reason.is_some();

            // Si la FSM estaba en Emergency/Fighting el tick anterior, reinicia
            // el step actual al volver — el personaje pudo haberse movido.
            if prev_was_interrupting {
                if let Some(wl) = &mut self.waypoints {
                    wl.restart_current_step(tick_num);
                }
                if let Some(cb) = &mut self.cavebot {
                    cb.restart_current_step(tick_num);
                }
            }

            // Prioridad: cavebot v2 > waypoints legacy.
            // Si safety pausó el bot, NO tickear el cavebot — congelar su estado.
            let waypoint_hint = if is_safety_paused_for_cavebot {
                WaypointHint::Inactive
            } else if self.cavebot_enabled {
                if let Some(cb) = self.cavebot.as_mut() {
                    if !cb.is_running() {
                        WaypointHint::Inactive
                    } else {
                        let mut cb_ctx = CavebotTickContext {
                            tick: tick_num,
                            hp_ratio: perception.vitals.hp.map(|b| b.ratio),
                            mana_ratio: perception.vitals.mana.map(|b| b.ratio),
                            total_kills: kills_total,
                            ticks_in_current_step: 0, // runner lo actualiza cada iteración
                            in_combat: perception.battle.has_enemies(),
                            last_activity_tick,
                            ui_matches: perception.ui_matches.clone(),
                            ui_match_infos: perception.ui_match_infos.clone(),
                            is_moving: perception.is_moving,
                            enemy_count: perception.battle.enemy_count() as u32,
                            loot_sparkles: perception.loot_sparkles,
                            minimap_center: self.vision.minimap_center(),
                            minimap_displacement: perception.minimap_displacement,
                            game_coords: perception.game_coords,
                            inventory_counts: perception.inventory_counts.clone(),
                            inventory_stacks: perception.inventory_stacks.clone(),
                        };
                        match cb.tick(&mut cb_ctx) {
                            CavebotAction::Idle          => WaypointHint::Active { emit: None },
                            CavebotAction::KeyTap(hid)   => WaypointHint::Active {
                                emit: Some(crate::core::fsm::WaypointEmit::KeyTap(hid)),
                            },
                            CavebotAction::Click { vx, vy } => WaypointHint::Active {
                                emit: Some(crate::core::fsm::WaypointEmit::Click { vx, vy }),
                            },
                            CavebotAction::RightClick { vx, vy } => WaypointHint::Active {
                                emit: Some(crate::core::fsm::WaypointEmit::RightClick { vx, vy }),
                            },
                            CavebotAction::Say(text) => {
                                // Encolar la frase en el typing buffer: Enter + chars + Enter.
                                typing_buffer.push_back(0x28); // Enter
                                for c in text.chars() {
                                    if let Some(hid) = crate::act::keycode::ascii_to_hid(c) {
                                        typing_buffer.push_back(hid);
                                    }
                                }
                                typing_buffer.push_back(0x28); // Enter
                                // Durante este tick el cavebot no emite nada directo —
                                // el typing buffer se vaciará en próximos ticks.
                                WaypointHint::Active { emit: None }
                            }
                            CavebotAction::Finished       => {
                                info!("Cavebot terminado");
                                self.cavebot_enabled = false;
                                WaypointHint::Inactive
                            }
                            CavebotAction::SafetyPause { reason } => {
                                // El cavebot detectó una condición inválida (e.g. char
                                // en piso equivocado tras navegación). Pausamos el bot
                                // con reason explícito y desactivamos cavebot para evitar
                                // cadena de acciones inútiles.
                                warn!("Cavebot SafetyPause: {}", reason);
                                {
                                    let mut g = self.state.write();
                                    g.is_paused = true;
                                    g.safety_pause_reason = Some(reason.clone());
                                }
                                // FIX 2026-04-17: propagar reason a la variable
                                // local para que el snapshot al final del tick
                                // (line ~1145) no la borre con None.
                                current_pause_reason = Some(reason);
                                self.cavebot_enabled = false;
                                WaypointHint::Inactive
                            }
                        }
                    }
                } else {
                    WaypointHint::Inactive
                }
            } else {
                // Fallback legacy: WaypointList temporal.
                match (self.waypoints_enabled, self.waypoints.as_mut()) {
                    (true, Some(wl)) if wl.is_running() => {
                        if wl.tick_stuck_check(tick_num, stuck_threshold) {
                            warn!(
                                "Waypoint stuck en step {:?} durante >{} ticks — pausando",
                                wl.current_label(), stuck_threshold
                            );
                            self.waypoints_enabled = false;
                            WaypointHint::Inactive
                        } else {
                            WaypointHint::Active {
                                emit: wl.tick_action(tick_num)
                                    .map(crate::core::fsm::WaypointEmit::KeyTap),
                            }
                        }
                    }
                    _ => WaypointHint::Inactive,
                }
            };

            // ── SCRIPTING: on_tick + on_low_hp ────────────────────────────────
            // `on_tick` se llama cada tick con un contexto read-only. Ahora
            // puede retornar una tecla (string) que se interpretará como
            // override de acción para este tick — útil para lógica custom
            // de múltiples thresholds (ej. heal proactivo a HP<50%).
            //
            // `on_low_hp` se llama cuando HP < HP_CRITICAL_RATIO (30%).
            // Recibe un TickContext completo (hp, mana, enemies, fsm, tick)
            // para que pueda decidir según mana disponible, etc.
            let hp_ratio   = perception.vitals.hp.map(|b| b.ratio);
            let mana_ratio = perception.vitals.mana.map(|b| b.ratio);
            let enemy_count = perception.battle.enemy_count() as u32;
            let mut heal_override: Option<u8> = None;
            let mut tick_override: Option<u8> = None;

            if let Some(eng) = scripts.as_mut() {
                eng.clear_errors();

                let ctx = TickContext {
                    tick:        tick_num,
                    hp_ratio,
                    mana_ratio,
                    enemy_count,
                    fsm_state:   format!("{:?}", fsm.state),
                    ui_matches:  perception.ui_matches.clone(),
                };

                // on_tick: si retorna Hotkey(hid), lo guardamos como override
                // proactivo que se aplicará en el branch de Fighting/Walking
                // si no hay heal_override de on_low_hp.
                match eng.fire_on_tick(&ctx) {
                    ScriptResult::Hotkey(hid) => { tick_override = Some(hid); }
                    ScriptResult::Noop        => {}
                    ScriptResult::Error(msg)  => {
                        warn!("on_tick script error: {}", msg);
                    }
                }

                // on_low_hp: solo si HP < HP_CRITICAL_RATIO (30%).
                // Recibe el mismo ctx para que pueda chequear mana, etc.
                if let Some(ratio) = hp_ratio {
                    if ratio < 0.30 {
                        match eng.fire_on_low_hp(&ctx) {
                            ScriptResult::Hotkey(hid) => { heal_override = Some(hid); }
                            ScriptResult::Noop        => {}
                            ScriptResult::Error(msg)  => {
                                warn!("on_low_hp script error: {}", msg);
                            }
                        }
                    }
                }
            }
            // `tick_override` se aplica DESPUÉS del FSM decide (ver más abajo),
            // solo si el FSM no entró en Emergency y el rate limiter lo permite.
            // Es una acción "proactiva" del script (ej. heal a HP<50% antes de
            // que llegue a Emergency).

            // ── SAFETY: Prompt detection (login/death/captcha) ───────────────
            // El detector corre en background thread (~500ms por ciclo).
            // tick() es no-bloqueante: envía parches al worker y drena
            // resultados. Solo actualiza current_pause_reason cuando llega
            // un resultado nuevo del background.
            // NUNCA intentamos auto-responder prompts.
            if safety.prompt_detection_enabled && self.vision.prompts.is_loaded() {
                if let Some(ref frame) = *frame_arc {
                    if let Some(result) = self.vision.prompts.tick(frame) {
                        match result {
                            Some(prompt_kind) => {
                                let reason = prompt_kind.as_str().to_string();
                                if current_pause_reason.as_deref() != Some(&reason) {
                                    warn!("Safety: detectado {} — pausando bot", reason);
                                    current_pause_reason = Some(reason);
                                }
                            }
                            None => {
                                if current_pause_reason.as_deref()
                                    .map(|s| s.starts_with("prompt:"))
                                    .unwrap_or(false)
                                {
                                    info!("Safety: prompt ya no detectado — reanudando");
                                    current_pause_reason = None;
                                }
                            }
                        }
                    }
                }
            }

            // ── SAFETY: Reaction gates + break scheduler ─────────────────────
            // Los reaction gates introducen un delay humano entre detectar
            // una nueva amenaza y actuar. Si el gate está armado pero no
            // abierto, ocultamos la amenaza a la FSM (perception "parece" OK)
            // para que no emita nada todavía.
            let hp_is_critical = hp_ratio.map(|r| r < 0.30).unwrap_or(false);
            hp_gate.update(hp_is_critical, tick_num);
            enemy_gate.update(perception.battle.has_enemies(), tick_num);

            // Break scheduler: si breaks_enabled, comprueba si toca pausar.
            let now_inst = Instant::now();
            let in_combat_for_breaks = perception.battle.has_enemies();
            if let Some(sched) = breaks.as_mut() {
                match sched.tick(now_inst) {
                    BreakStatus::Started(kind) if !in_combat_for_breaks => {
                        info!("Safety: iniciando {}", kind.as_str());
                        current_pause_reason = Some(kind.as_str().to_string());
                    }
                    BreakStatus::Started(kind) => {
                        // Combate activo — posponer break hasta que termine.
                        debug!("Safety: {} pospuesto (en combate)", kind.as_str());
                    }
                    BreakStatus::Ended(kind) => {
                        info!("Safety: terminando {}", kind.as_str());
                        current_pause_reason = None;
                    }
                    BreakStatus::Active(_) => {
                        // Pausa sigue vigente — mantener current_pause_reason.
                    }
                    BreakStatus::None => {
                        // Solo limpiar si la razón actual era un break.
                        if let Some(r) = current_pause_reason.as_ref() {
                            if r.starts_with("break:") {
                                current_pause_reason = None;
                            }
                        }
                    }
                }
            }

            // ── SAFETY: Focus detection (bridge-side) ────────────────────
            // El bridge reporta NOFOCUS cuando Tibia no tiene el foco.
            // Pausamos para evitar que HID commands vayan a otra ventana.
            if self.actuator.is_focus_lost() {
                if current_pause_reason.as_deref() != Some("focus:tibia_not_foreground") {
                    warn!("Safety: Tibia perdió el foco — pausando bot");
                    current_pause_reason = Some("focus:tibia_not_foreground".into());
                }
            } else if current_pause_reason.as_deref() == Some("focus:tibia_not_foreground") {
                info!("Safety: Tibia recuperó el foco — reanudando");
                current_pause_reason = None;
            }

            // ── SAFETY: Session duration cap (Task 2.2) ──────────────────
            // Dos fases:
            //   1. is_warning (graceful): T-N min antes del cap. Si el
            //      cavebot está enabled y tiene un label "refill", forzamos
            //      un goto para vaciar bag antes de pausar. One-shot via
            //      `session_warning_active`.
            //   2. is_expired (hard): se dispara al pasar max_session_hours.
            //      Setea pause reason y descarta session_limit para no
            //      re-loggear. La pausa persiste hasta intervención manual.
            if let Some(ref sl) = session_limit {
                if !session_warning_active && sl.is_warning(tick_num) {
                    session_warning_active = true;
                    if self.cavebot_enabled {
                        if let Some(cb) = self.cavebot.as_mut() {
                            if cb.force_goto_label("refill", tick_num) {
                                warn!(
                                    "Session warning ({:.1} min margin): forcing goto refill before cap",
                                    safety.session_warning_min,
                                );
                            } else {
                                warn!(
                                    "Session warning ({:.1} min margin): no 'refill' label in cavebot — cap hará pause directo",
                                    safety.session_warning_min,
                                );
                            }
                        } else {
                            warn!(
                                "Session warning ({:.1} min margin): no cavebot loaded — cap hará pause directo",
                                safety.session_warning_min,
                            );
                        }
                    } else {
                        warn!(
                            "Session warning ({:.1} min margin): cavebot disabled — cap hará pause directo",
                            safety.session_warning_min,
                        );
                    }
                }
                if sl.is_expired(tick_num) {
                    let elapsed = sl.elapsed_hours(tick_num, target_fps);
                    warn!(
                        "Safety: cap de sesión alcanzado ({:.2}h) — pausando bot",
                        elapsed,
                    );
                    current_pause_reason = Some("session:max_duration_reached".into());
                    session_limit = None;
                }
            }

            // Si hay una pausa de safety, forzar que la FSM no emita nada
            // marcándolo a nivel del GameState. El FSM honra `is_paused`.
            let is_safety_paused = current_pause_reason.is_some();

            // SpellTable: evaluar heal y attack override.
            //
            // Fallback en None: 0.5 (no 1.0). Razón: F1.2 vitals debouncing ya
            // filtra single-frame bad reads (<5 frames → retorna last_hp_stable).
            // Cuando hp_ratio llega None AQUÍ, es porque hubo ≥5 frames bad
            // seguidos (150ms+) = problema sostenido de reader. En ese caso
            // asumir full HP (1.0) es peligroso: cancela heals. 0.5 es neutral:
            // pick_heal probablemente no dispare (mayoría de thresholds >0.5),
            // pero no pollutes la decisión con "full HP confirmado".
            let spell_ctx = crate::core::spell_table::SpellContext {
                hp:      hp_ratio.unwrap_or(0.5),
                mana:    mana_ratio.unwrap_or(0.5),
                enemies: perception.battle.enemy_count() as u32,
                tick:    tick_num,
            };
            if heal_override.is_none() {
                if !self.config.spells.is_empty() {
                    heal_override = spell_table.pick_heal(&spell_ctx);
                } else if safety.heal_variation {
                    heal_override = heal_choice.pick();
                }
            }
            let attack_override = spell_table.pick_attack(&spell_ctx);

            // Si el hp_gate está armado (esperando reaction delay), enmascarar
            // HP crítico pasando un Perception modificado al FSM. Si el gate
            // está abierto (reacción consumida), dejar pasar.
            //
            // Estrategia minimalista: clonamos la Perception y ajustamos el
            // ratio a "full" si el gate no permite reaccionar. Esto evita
            // tocar la signature del FSM.
            let perception_for_fsm = if safety.humanize_timing && hp_is_critical && !hp_gate.is_open(tick_num) {
                let mut p = perception.clone();
                // "Esconder" HP crítico para que el FSM no emita heal aún.
                // Subimos el ratio artificialmente a 0.99.
                if let Some(ref mut b) = p.vitals.hp {
                    b.ratio = 0.99;
                }
                p
            } else if safety.humanize_timing && perception.battle.has_enemies() && !enemy_gate.is_open(tick_num) {
                let mut p = perception.clone();
                // "Esconder" enemigos durante el reaction delay.
                p.battle.entries.clear();
                p
            } else {
                perception.clone()
            };

            // ── THINK (FSM) ───────────────────────────────────────────────────
            // Si safety pausó el bot (break/prompt), forzamos Paused event.
            let fsm_event = if is_safety_paused { BotEvent::PauseRequested } else { BotEvent::Tick };
            let action = {
                let g = self.state.read();
                fsm.decide(&DecideContext {
                    game: &g,
                    event: fsm_event,
                    perception: &perception_for_fsm,
                    hotkeys: &self.hotkeys,
                    cd_heal,
                    cd_attack,
                    waypoint_hint,
                    heal_override,
                    attack_override,
                })
            };
            let fsm_state_snapshot = fsm.state.clone();
            prev_was_interrupting = fsm.is_interrupting_waypoints();

            // ── Phase C.1: Fire on_fsm_state_change si hubo transición ────
            // Comparamos contra prev_fsm_state_str y prev_pause_reason_str.
            // Si cualquiera cambió, disparamos el hook con el NUEVO state +
            // NUEVA reason. El hook es best-effort (log/alerta) — no puede
            // override la transición.
            //
            // Este es el gancho para que scripts Lua detecten char:dead
            // (reason = "prompt:char_select"), disconnect (reason = "prompt:login"),
            // breaks iniciados/finalizados, etc.
            {
                let new_state_str = format!("{:?}", fsm_state_snapshot);
                let new_reason_str = current_pause_reason.clone();
                if new_state_str != prev_fsm_state_str || new_reason_str != prev_pause_reason_str {
                    if let Some(eng) = scripts.as_mut() {
                        let result = eng.fire_on_fsm_state_change(
                            &new_state_str,
                            new_reason_str.as_deref(),
                        );
                        if let ScriptResult::Error(msg) = result {
                            warn!("on_fsm_state_change script error: {}", msg);
                        }
                    }
                    prev_fsm_state_str = new_state_str;
                    prev_pause_reason_str = new_reason_str;
                }
            }

            // ── RATE LIMIT ────────────────────────────────────────────────────
            // Hard cap global de acciones/segundo. Si se excede, se descarta
            // la acción (no se cola). Protege contra bugs que producirían spam.
            let limited_action = match action {
                BotAction::Idle => BotAction::Idle,
                ref other => {
                    if rate_limiter.allow(now_inst) {
                        other.clone()
                    } else {
                        debug!("Rate limiter descartó acción: {:?}", other);
                        BotAction::Idle
                    }
                }
            };

            // ── TRACKING de emits para /dispatch/stats y /combat/events ──
            // Acumulamos aquí todas las hotkeys que se van a despachar en
            // este tick para contarlas más abajo en el bookkeeping.
            let mut emitted_hotkeys: Vec<(u8, &'static str)> = Vec::new();

            // ── HUMAN NOISE (opt-in) ─────────────────────────────────────────
            // Solo en Idle (sin combate ni emergencia). El noise NO se cuenta
            // contra el rate limiter — es deliberadamente bajo volumen.
            if matches!(limited_action, BotAction::Idle)
                && !is_safety_paused
                && matches!(fsm_state_snapshot, crate::core::fsm::FsmState::Idle)
            {
                if let Some(hn) = human_noise.as_mut() {
                    if let Some(hid) = hn.tick(now_inst) {
                        debug!("HumanNoise: emitting 0x{:02X}", hid);
                        self.dispatch_action(BotAction::UseHotkey { hidcode: hid });
                        emitted_hotkeys.push((hid, "human_noise"));
                    }
                }
            }

            // ── TICK OVERRIDE (on_tick return value) ─────────────────────────
            // Si el script `on_tick` retornó una tecla como acción proactiva
            // (ej. heal a HP<50%), se despacha AQUÍ — después del FSM y su
            // rate-limited action — solo si:
            //   (1) el FSM no entró en Emergency (que ya usa heal_override)
            //   (2) el rate limiter lo permite
            //   (3) no estamos en safety pause
            //
            // Esto permite que scripts Lua implementen multi-threshold heal
            // (ej. Exura normal a HP<50%, Exura Ico a HP<30%) sin requerir
            // cambiar la arquitectura del FSM.
            if let Some(hid) = tick_override {
                if !is_safety_paused
                    && !matches!(fsm_state_snapshot, crate::core::fsm::FsmState::Emergency)
                    && rate_limiter.allow(now_inst)
                {
                    debug!("Script tick_override: emitting 0x{:02X}", hid);
                    self.dispatch_action(BotAction::UseHotkey { hidcode: hid });
                    emitted_hotkeys.push((hid, "script_tick_override"));
                }
            }

            // ── TYPING BUFFER (Fase D: bot.say()) ────────────────────────────
            // Drena cualquier string nuevo que haya encolado `bot.say()` en
            // este tick y lo convierte en una secuencia de HID keycodes:
            // [Enter, chars..., Enter]. El primer Enter abre el chat de Tibia,
            // el segundo envía el mensaje.
            if let Some(eng) = scripts.as_ref() {
                for text in eng.drain_say_queue() {
                    typing_buffer.push_back(0x28); // Enter — abrir chat
                    for c in text.chars() {
                        if let Some(hid) = crate::act::keycode::ascii_to_hid(c) {
                            typing_buffer.push_back(hid);
                        } else {
                            tracing::debug!(
                                "bot.say: caracter '{}' no tipeable — ignorado", c
                            );
                        }
                    }
                    typing_buffer.push_back(0x28); // Enter — enviar
                }
            }

            // Emit un char del typing buffer si es el momento (paced).
            if !typing_buffer.is_empty()
                && !is_safety_paused
                && tick_num >= typing_next_tick
                && rate_limiter.allow(now_inst)
            {
                let hid = typing_buffer.pop_front().unwrap();
                debug!("typing: emitting 0x{:02X}", hid);
                self.dispatch_action(BotAction::UseHotkey { hidcode: hid });
                emitted_hotkeys.push((hid, "typing"));
                // Spacing aleatorio por char (anti-detection, ver comentario
                // arriba en declaración de TYPING_MIN/MAX_TICKS).
                use rand::Rng;
                let next_spacing = rand::thread_rng()
                    .gen_range(TYPING_MIN_TICKS..=TYPING_MAX_TICKS);
                typing_next_tick = tick_num + next_spacing;
            }

            // ── AUTO-LOOT (kill-driven + sparkles guard) ─────────────────
            //
            // Trigger: 1 emit de F12 por cada kill del bot, SI hay sparkles
            // visibles Y el cooldown expiró. Esto garantiza ratio 1:1 loot:kill
            // sin spam y sin under-loot.
            //
            // Incrementamos `kills_pending_loot` cuando `enemy_count` baja
            // (un mob murió). Decrementamos cuando emitimos F12 (asumimos
            // que Quick Loot limpió 1 corpse).

            // Push nuevo sample de sparkles a la ventana rolling para smoothing.
            sparkles_window.push_back(perception.loot_sparkles);
            if sparkles_window.len() > SPARKLES_WINDOW_SIZE {
                sparkles_window.pop_front();
            }
            let sparkles_max = sparkles_window.iter().copied().max().unwrap_or(0);

            // Track kills: cuando enemy_count baja, un mob murió.
            let observed_enemy_count = perception.battle.enemy_count() as u32;
            if observed_enemy_count < prev_enemy_count_for_kills {
                let kills_this_tick = prev_enemy_count_for_kills - observed_enemy_count;
                kills_pending_loot = kills_pending_loot.saturating_add(kills_this_tick);
                debug!(
                    "auto_loot: +{} kills detected ({}->{}); pending={}",
                    kills_this_tick, prev_enemy_count_for_kills,
                    observed_enemy_count, kills_pending_loot,
                );
            }
            prev_enemy_count_for_kills = observed_enemy_count;

            if let Some(loot_hid) = loot_hotkey {
                use crate::sense::vision::loot::LOOT_SPARKLE_THRESHOLD;

                let safety_ok = !is_safety_paused
                    && !matches!(fsm_state_snapshot, crate::core::fsm::FsmState::Emergency);
                let sparkles_hi = sparkles_max >= LOOT_SPARKLE_THRESHOLD;
                let kills_pending = kills_pending_loot > 0;
                let cooldown_ok = tick_num >= loot_next_tick;

                if sparkles_hi && kills_pending && cooldown_ok && safety_ok && rate_limiter.allow(now_inst) {
                    debug!(
                        "auto_loot: emit (pending={}, sparkles_max={}, 0x{:02X})",
                        kills_pending_loot, sparkles_max, loot_hid
                    );
                    self.dispatch_action(BotAction::UseHotkey { hidcode: loot_hid });
                    emitted_hotkeys.push((loot_hid, "auto_loot"));
                    kills_pending_loot = kills_pending_loot.saturating_sub(1);
                    loot_next_tick = tick_num + LOOT_COOLDOWN_TICKS;
                }
            }

            // ── ACT ───────────────────────────────────────────────────────────
            // Despacho no bloqueante: el comando se ejecuta en el runtime tokio
            // y el game loop continúa de inmediato para respetar el deadline.
            if let BotAction::UseHotkey { hidcode } = limited_action {
                emitted_hotkeys.push((hidcode, "fsm"));
            }
            self.dispatch_action(limited_action);

            // ── TICK BOOKKEEPING ──────────────────────────────────────────────
            let elapsed = tick_start.elapsed();
            let waypoint_status_snapshot = self.snapshot_waypoint_status();
            let cavebot_status_snapshot = match &self.cavebot {
                Some(cb) => cb.snapshot(self.cavebot_enabled),
                None     => crate::core::state::CavebotSnapshot::default(),
            };
            let script_status_snapshot   = snapshot_script_status(scripts.as_ref());
            let rate_dropped_snapshot    = rate_limiter.dropped_count();
            let pause_reason_snapshot    = current_pause_reason.clone();
            let fsm_debug_snapshot       = fsm.debug_snapshot();
            let fsm_state_for_event      = format!("{:?}", fsm_state_snapshot);
            let now_ms_for_event         = now_ms();
            let matcher_stats_snapshot   = self.vision.matcher_stats();
            {
                let mut g = self.state.write();
                g.tick += 1;
                g.last_tick_at = Some(tick_start);
                g.fsm_state    = fsm_state_snapshot;
                g.waypoint_status  = waypoint_status_snapshot;
                g.cavebot_status   = cavebot_status_snapshot;
                g.script_status    = script_status_snapshot;
                g.safety_pause_reason = pause_reason_snapshot;
                g.safety_rate_dropped = rate_dropped_snapshot;
                g.matcher_stats       = Some(matcher_stats_snapshot);

                // ── Observability (Fase B) ──────────────────────────────
                g.fsm_debug = fsm_debug_snapshot;
                g.target_debug = crate::core::state::TargetDebug {
                    configured:     self.vision.calibration.target_hp_bar.is_some(),
                    active:         perception.target_active,
                    hits:           perception.target_hits,
                    threshold_used: 0, // solo el detector lo sabe — aproximado
                };

                // Dispatch stats: incrementar contadores por hotkey.
                for (hid, _source) in &emitted_hotkeys {
                    let category = categorize_hotkey(*hid, &self.hotkeys);
                    match category {
                        EmitCategory::Attack => {
                            g.dispatch_stats.attacks_total += 1;
                            g.dispatch_stats.last_attack_ms = Some(now_ms_for_event);
                        }
                        EmitCategory::Heal => {
                            g.dispatch_stats.heals_total += 1;
                            g.dispatch_stats.last_heal_ms = Some(now_ms_for_event);
                        }
                        EmitCategory::Mana => {
                            g.dispatch_stats.mana_total += 1;
                            g.dispatch_stats.last_mana_ms = Some(now_ms_for_event);
                        }
                        EmitCategory::Other => {
                            g.dispatch_stats.other_total += 1;
                        }
                    }
                }

                // Combat events: push si se emitió alguna acción. No
                // loggeamos cada tick en Idle para no saturar el buffer.
                let action_emitted = !emitted_hotkeys.is_empty();
                if action_emitted {
                    use crate::core::state::CombatEvent;
                    let action_str = if emitted_hotkeys.is_empty() {
                        "Idle".to_string()
                    } else {
                        emitted_hotkeys.iter()
                            .map(|(h, s)| format!("0x{:02X}({})", h, s))
                            .collect::<Vec<_>>()
                            .join("+")
                    };
                    let reason = if perception.battle.has_enemies() {
                        match perception.target_active {
                            Some(true)  => "combat+target".to_string(),
                            Some(false) => "combat+no_target".to_string(),
                            None        => "combat".to_string(),
                        }
                    } else {
                        "idle".to_string()
                    };
                    let event = CombatEvent {
                        tick:          g.tick,
                        ts_ms:         now_ms_for_event,
                        fsm_state:     fsm_state_for_event.clone(),
                        action:        action_str,
                        reason,
                        hp_ratio:      perception.vitals.hp.map(|b| b.ratio),
                        target_active: perception.target_active,
                        enemy_count:   perception.battle.enemy_count() as u32,
                    };
                    g.combat_events.push_back(event);
                    while g.combat_events.len() > crate::core::state::COMBAT_EVENTS_CAP {
                        g.combat_events.pop_front();
                    }
                }
                // ── VisionMetrics ──────────────────────────────────────────
                // Extract ratios before moving perception into last_perception.
                let hp_ratio   = perception.vitals.hp.map(|b| b.ratio);
                let mana_ratio = perception.vitals.mana.map(|b| b.ratio);
                // F1 recorder: grabar snapshot serializable antes de mover perception.
                if let Some(ref mut rec) = self.recorder {
                    let snap = perception.to_snapshot();
                    rec.record(&snap);
                }
                g.last_perception = Some(perception);

                if let Some(r) = hp_ratio   { g.vision_metrics.push_hp(r);   }
                if let Some(r) = mana_ratio { g.vision_metrics.push_mana(r); }
                g.vision_metrics.push_cost(vision_cost_ms);

                g.metrics.ticks_total += 1;
                g.metrics.bot_proc_ms = rolling_avg(
                    g.metrics.bot_proc_ms,
                    elapsed.as_secs_f64() * 1000.0,
                    30,
                );

                if elapsed > tick_budget {
                    g.metrics.ticks_overrun += 1;
                    warn!(
                        "Tick overrun: {:.1}ms (presupuesto {:.1}ms) — tick #{}",
                        elapsed.as_secs_f64() * 1000.0,
                        tick_budget.as_secs_f64() * 1000.0,
                        g.tick,
                    );
                } else {
                    debug!(
                        "Tick #{}: {:.1}ms frame={}",
                        g.tick,
                        elapsed.as_secs_f64() * 1000.0,
                        if has_frame { "ok" } else { "none" },
                    );
                }
            }

            // ── SLEEP hasta el próximo deadline ───────────────────────────────
            let now = Instant::now();
            let (sleep_dur, new_next) = compute_tick_sleep(next_tick, tick_budget, now);
            next_tick = new_next;
            if !sleep_dur.is_zero() {
                std::thread::sleep(sleep_dur);
            }
        }
    }

    /// Genera un snapshot del estado de waypoints para publicarlo en GameState.
    fn snapshot_waypoint_status(&self) -> WaypointStatus {
        match &self.waypoints {
            None => WaypointStatus::default(),
            Some(wl) => {
                let (idx, label) = match wl.current_label() {
                    Some((i, l)) => (Some(i), Some(l.to_string())),
                    None => (None, None),
                };
                WaypointStatus {
                    loaded:        true,
                    enabled:       self.waypoints_enabled,
                    total_steps:   wl.steps.len(),
                    current_index: idx,
                    current_label: label,
                    loop_:         wl.loop_,
                }
            }
        }
    }

    /// Procesa un comando recibido del HTTP server.
    fn handle_command(&mut self, cmd: LoopCommand, tick: u64) {
        match cmd {
            LoopCommand::LoadWaypoints { path, enabled } => {
                let fps = self.config.loop_config.target_fps;
                match WaypointList::load(&path, fps) {
                    Ok(mut wl) => {
                        // Asegurar que el stuck tracker arranca en el tick
                        // de carga, no en el tick 0 (que podría ser hace
                        // minutos en un bot de larga vida).
                        wl.reset_stuck_tracker(tick);
                        info!("Waypoints recargados desde '{}': {} steps, enabled={}",
                              path.display(), wl.steps.len(), enabled);
                        self.waypoints = Some(wl);
                        self.waypoints_enabled = enabled;
                    }
                    Err(e) => {
                        warn!("Hot-reload falló para '{}': {:#}", path.display(), e);
                    }
                }
            }
            LoopCommand::PauseWaypoints => {
                info!("Waypoints pausados en tick {}", tick);
                self.waypoints_enabled = false;
            }
            LoopCommand::ResumeWaypoints => {
                if self.cavebot_enabled {
                    warn!("ResumeWaypoints ignorado: cavebot activo (tiene prioridad)");
                } else if let Some(wl) = &mut self.waypoints {
                    wl.restart_current_step(tick);
                    wl.reset_stuck_tracker(tick);
                    info!("Waypoints reanudados en tick {} desde step {:?}",
                          tick, wl.current_label());
                    self.waypoints_enabled = true;
                } else {
                    warn!("ResumeWaypoints ignorado: no hay lista cargada");
                }
            }
            LoopCommand::ClearWaypoints => {
                info!("Waypoints descargados");
                self.waypoints = None;
                self.waypoints_enabled = false;
            }
            // ── Cavebot (Fase C) ──────────────────────────────────────────
            LoopCommand::LoadCavebot { path, enabled } => {
                let fps = self.config.loop_config.target_fps;
                let tuning = crate::cavebot::runner::NodeTuning::from_config(&self.config.cavebot);
                // Hot-reload smooth: capturar el label actual del cavebot previo
                // para saltar al mismo label en el script nuevo (si existe).
                let old_label = self.cavebot.as_ref()
                    .and_then(|cb| cb.current_label_name());
                match crate::cavebot::parser::load_with_tuning(&path, fps, tuning) {
                    Ok(mut cb) => {
                        let mut resume_info = String::new();
                        if let Some(ref lbl) = old_label {
                            if cb.jump_to_label(lbl, 0) {
                                resume_info = format!(", resumed at label '{}'", lbl);
                            } else {
                                resume_info = format!(", label '{}' not found in new script → start", lbl);
                            }
                        }
                        info!("Cavebot cargado desde '{}': {} steps, loop={}, enabled={}{}",
                              path.display(), cb.steps.len(), cb.loop_, enabled, resume_info);
                        self.cavebot = Some(cb);
                        self.cavebot_enabled = enabled;
                        // Desactivar el waypoints legacy para evitar conflicto.
                        if self.waypoints_enabled {
                            info!("Cavebot v2 activo → desactivando waypoints legacy");
                            self.waypoints_enabled = false;
                        }
                    }
                    Err(e) => {
                        warn!("Error cargando cavebot '{}': {:#}", path.display(), e);
                    }
                }
            }
            LoopCommand::PauseCavebot => {
                info!("Cavebot pausado en tick {}", tick);
                self.cavebot_enabled = false;
            }
            LoopCommand::ResumeCavebot => {
                if let Some(cb) = &mut self.cavebot {
                    if cb.current.is_none() {
                        warn!("ResumeCavebot ignorado: lista terminada (recargar con /cavebot/load)");
                    } else {
                        cb.restart_current_step(tick);
                        self.waypoints_enabled = false; // mutual exclusion
                        self.cavebot_enabled = true;
                        info!("Cavebot reanudado en tick {} desde step {:?}", tick, cb.current);
                    }
                } else {
                    warn!("ResumeCavebot ignorado: no hay cavebot cargado");
                }
            }
            LoopCommand::ClearCavebot => {
                info!("Cavebot descargado");
                self.cavebot = None;
                self.cavebot_enabled = false;
            }
            LoopCommand::JumpToCavebotLabel { label } => {
                if let Some(cb) = &mut self.cavebot {
                    if cb.jump_to_label(&label, tick) {
                        info!("Cavebot saltó a label '{}' en tick {}", label, tick);
                    } else {
                        warn!("JumpToCavebotLabel: label '{}' no encontrado en script", label);
                    }
                } else {
                    warn!("JumpToCavebotLabel ignorado: no hay cavebot cargado");
                }
            }
            LoopCommand::ReloadScripts { .. } => {
                // Se procesa inline en run() (líneas ~376-409) porque necesita
                // acceso al ScriptEngine local. Este arm es unreachable en la
                // práctica, pero mantenemos el match exhaustivo sin panic en
                // debug — si llegara aquí por un refactor, solo warning.
                warn!("ReloadScripts llegó a handle_command — debería procesarse inline en run()");
            }
            LoopCommand::StartRecording { path } => {
                let final_path = PathBuf::from(path.unwrap_or_else(|| "session.jsonl".to_string()));
                let interval = self.config.recording.interval_ticks.unwrap_or(1);
                let rec = crate::sense::recorder::PerceptionRecorder::new(
                    final_path.clone(),
                    interval,
                );
                if rec.is_enabled() {
                    info!("Recording iniciado: {} (interval={} ticks)", final_path.display(), interval);
                    self.recorder = Some(rec);
                } else {
                    warn!("StartRecording falló: no se pudo crear archivo '{}'", final_path.display());
                }
            }
            LoopCommand::StopRecording => {
                if let Some(rec) = self.recorder.take() {
                    let count = rec.records_written();
                    info!("Recording detenido: {} records escritos", count);
                    drop(rec); // flushes on drop
                } else {
                    warn!("StopRecording ignorado: no hay recording activo");
                }
            }
        }
    }

    /// Envía la acción al actuator en background. No bloquea el game loop:
    /// la tarea se spawna en el runtime tokio y el loop continúa al siguiente tick.
    fn dispatch_action(&self, action: BotAction) {
        match action {
            BotAction::Idle => {}
            BotAction::UseHotkey { hidcode } => {
                let actuator = Arc::clone(&self.actuator);
                self.rt_handle.spawn(async move {
                    if let Err(e) = actuator.key_tap(hidcode).await {
                        warn!("key_tap 0x{:02X} falló: {}", hidcode, e);
                    }
                });
            }
            BotAction::Click { vx, vy } => {
                let actuator = Arc::clone(&self.actuator);
                self.rt_handle.spawn(async move {
                    if let Err(e) = actuator.click(vx, vy, "L").await {
                        warn!("click ({},{}) falló: {}", vx, vy, e);
                    }
                });
            }
            BotAction::RightClick { vx, vy } => {
                let actuator = Arc::clone(&self.actuator);
                self.rt_handle.spawn(async move {
                    if let Err(e) = actuator.click(vx, vy, "R").await {
                        warn!("right_click ({},{}) falló: {}", vx, vy, e);
                    }
                });
            }
            BotAction::MoveTo { vx, vy } => {
                let actuator = Arc::clone(&self.actuator);
                self.rt_handle.spawn(async move {
                    actuator.mouse_move(vx, vy).await;
                });
            }
        }
    }
}

fn rolling_avg(prev: f64, new_val: f64, n: u64) -> f64 {
    if n == 0 { return new_val; }
    (prev * (n - 1) as f64 + new_val) / n as f64
}

/// Computa cuánto dormir y el próximo deadline del scheduler 30Hz.
///
/// Usa el modelo de **deadline absoluto**: `next_tick` es el instante en que
/// el siguiente tick debe empezar. Sumamos `tick_budget` a cada iteración.
///
/// **Anti-drift**: si el tick actual excedió el presupuesto (`now > next_tick`),
/// descartamos el atraso acumulado reseteando `next_tick = now`. Esto evita
/// "catching up" que acumularía frames y violaría aún más el budget.
///
/// Retorna `(sleep_duration, new_next_tick)`.
fn compute_tick_sleep(
    next_tick: Instant,
    tick_budget: Duration,
    now: Instant,
) -> (Duration, Instant) {
    let deadline = next_tick + tick_budget;
    if deadline > now {
        (deadline - now, deadline)
    } else {
        // Tick overrun — reset deadline a now para no acumular drift.
        (Duration::ZERO, now)
    }
}

/// Resultado de un paso del watchdog de no-frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoFrameStep {
    /// Llegó frame: contador reseteado, no hay pausa nueva.
    FrameOk,
    /// Sin frame pero bajo umbral: contador incrementado, no hay pausa nueva.
    Accumulating,
    /// Se superó el umbral: disparar safety pause "vision:no_frame" y
    /// resetear el contador para no re-disparar cada tick.
    TriggerPause,
}

/// Actualiza el contador de ticks sin frame y decide si corresponde disparar
/// la safety pause "vision:no_frame".
///
/// Se extrae como función pura para testearla sin levantar game loop.
/// El caller es responsable de mutar el estado compartido en `TriggerPause`.
fn step_no_frame_watchdog(
    counter: &mut u32,
    has_frame: bool,
    threshold: u32,
) -> NoFrameStep {
    if has_frame {
        *counter = 0;
        NoFrameStep::FrameOk
    } else {
        *counter = counter.saturating_add(1);
        if *counter >= threshold {
            *counter = 0;
            NoFrameStep::TriggerPause
        } else {
            NoFrameStep::Accumulating
        }
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;

    /// Caso ideal: ticks instantáneos, el deadline avanza exactamente
    /// `tick_budget` cada iteración sin drift.
    #[test]
    fn happy_path_no_drift() {
        let budget = Duration::from_millis(33); // ~30Hz
        let start = Instant::now();
        let mut next_tick = start;

        for i in 1..=10 {
            // Simulamos que el tick toma 0ms — ya estamos en next_tick.
            let now = next_tick;
            let (sleep, new_next) = compute_tick_sleep(next_tick, budget, now);
            next_tick = new_next;
            // Sleep debería ser exactamente el budget.
            assert_eq!(sleep, budget, "iter {i}");
            // El deadline avanza linealmente.
            let expected = start + budget * i;
            assert_eq!(next_tick, expected, "iter {i}");
        }
    }

    /// Cuando un tick excede el presupuesto, next_tick se resetea a `now`
    /// (drop del atraso) en lugar de acumular lag.
    #[test]
    fn tick_overrun_resets_deadline_to_now() {
        let budget = Duration::from_millis(33);
        let start = Instant::now();
        let next_tick = start;

        // Simulamos un tick que tardó 50ms > budget.
        let now = start + Duration::from_millis(50);
        let (sleep, new_next) = compute_tick_sleep(next_tick, budget, now);

        // No hay sleep (ya estamos atrasados).
        assert_eq!(sleep, Duration::ZERO);
        // next_tick se reseteó a now (no a start + 33ms).
        assert_eq!(new_next, now);
    }

    /// Tras un overrun, el siguiente tick reanuda ritmo normal desde
    /// la nueva base, sin intentar "recuperar" los ticks perdidos.
    #[test]
    fn no_catch_up_after_overrun() {
        let budget = Duration::from_millis(33);
        let start = Instant::now();
        let mut next_tick = start;

        // Tick 1: overrun de 20ms.
        let now1 = start + Duration::from_millis(53);
        let (sleep1, nn1) = compute_tick_sleep(next_tick, budget, now1);
        assert_eq!(sleep1, Duration::ZERO);
        next_tick = nn1; // = now1

        // Tick 2: normal, 0ms de proc. Debería avanzar un budget completo.
        let now2 = next_tick;
        let (sleep2, nn2) = compute_tick_sleep(next_tick, budget, now2);
        assert_eq!(sleep2, budget);
        // El nuevo deadline es relativo a now1 + budget, NO a start + 2*budget.
        assert_eq!(nn2, now1 + budget);
    }

    /// Tick pequeño (10ms) con budget de 33ms: duerme los 23ms restantes
    /// y avanza el deadline exactamente un budget.
    #[test]
    fn partial_tick_sleeps_remaining_time() {
        let budget = Duration::from_millis(33);
        let start = Instant::now();
        let next_tick = start;

        // El tick tomó 10ms.
        let now = start + Duration::from_millis(10);
        let (sleep, new_next) = compute_tick_sleep(next_tick, budget, now);

        // Debería dormir 23ms para llegar a start + 33ms.
        assert_eq!(sleep, Duration::from_millis(23));
        assert_eq!(new_next, start + budget);
    }

    /// Run simulado de N ticks con timing variable: verifica que
    /// `ticks_total` incrementa 1 por iteración y que overruns se cuentan bien.
    #[test]
    fn simulate_run_counts_ticks_and_overruns() {
        let budget = Duration::from_millis(33);
        let start = Instant::now();
        let mut next_tick = start;
        let mut ticks_total = 0u64;
        let mut ticks_overrun = 0u64;

        // Secuencia de procesamiento: [10, 50, 20, 100, 5] ms por tick.
        let tick_procs = [10u64, 50, 20, 100, 5];
        let mut clock = start;

        for proc_ms in tick_procs {
            clock += Duration::from_millis(proc_ms);
            ticks_total += 1;
            if proc_ms > 33 {
                ticks_overrun += 1;
            }
            let (sleep, nn) = compute_tick_sleep(next_tick, budget, clock);
            next_tick = nn;
            clock += sleep;
        }

        assert_eq!(ticks_total, 5);
        // Ticks con proc > 33ms: 50 y 100 → 2 overruns.
        assert_eq!(ticks_overrun, 2);
    }
}

#[cfg(test)]
mod no_frame_watchdog_tests {
    use super::*;

    /// 120 ticks consecutivos sin frame → dispara pausa en el tick 120 exacto
    /// y resetea el contador para no re-disparar en el tick siguiente.
    #[test]
    fn triggers_pause_at_threshold() {
        let mut counter: u32 = 0;

        // Ticks 1..=119 acumulan sin disparar.
        for i in 1..NO_FRAME_PAUSE_TICKS {
            let step = step_no_frame_watchdog(&mut counter, false, NO_FRAME_PAUSE_TICKS);
            assert_eq!(
                step, NoFrameStep::Accumulating,
                "tick {i}: esperaba Accumulating"
            );
            assert_eq!(counter, i, "tick {i}: contador debería ser {i}");
        }

        // Tick 120 (== NO_FRAME_PAUSE_TICKS) → dispara pausa.
        let step = step_no_frame_watchdog(&mut counter, false, NO_FRAME_PAUSE_TICKS);
        assert_eq!(step, NoFrameStep::TriggerPause);
        // Tras disparar, el contador se resetea.
        assert_eq!(counter, 0, "counter debe resetearse tras disparar");

        // Tick 121 sin frame: vuelve a acumular desde 1, no re-dispara aún.
        let step = step_no_frame_watchdog(&mut counter, false, NO_FRAME_PAUSE_TICKS);
        assert_eq!(step, NoFrameStep::Accumulating);
        assert_eq!(counter, 1);
    }

    /// 100 ticks sin frame < umbral → NO dispara pausa, contador sigue vivo.
    #[test]
    fn does_not_trigger_below_threshold() {
        let mut counter: u32 = 0;

        for i in 1..=100u32 {
            let step = step_no_frame_watchdog(&mut counter, false, NO_FRAME_PAUSE_TICKS);
            assert_eq!(step, NoFrameStep::Accumulating, "tick {i}");
            assert_eq!(counter, i);
        }

        // Nunca se llamó TriggerPause — confirmado por los asserts arriba.
        assert!(counter < NO_FRAME_PAUSE_TICKS);
    }

    /// Llegada de un frame resetea el contador a 0 y retorna FrameOk.
    #[test]
    fn frame_resets_counter() {
        let mut counter: u32 = 50;

        let step = step_no_frame_watchdog(&mut counter, true, NO_FRAME_PAUSE_TICKS);
        assert_eq!(step, NoFrameStep::FrameOk);
        assert_eq!(counter, 0);
    }

    /// Un solo frame entremedio reinicia la cuenta: 119 sin frame + 1 frame +
    /// 119 sin frame → NO dispara (el frame bajó el contador a 0).
    #[test]
    fn single_frame_prevents_trigger() {
        let mut counter: u32 = 0;

        for _ in 0..NO_FRAME_PAUSE_TICKS - 1 {
            step_no_frame_watchdog(&mut counter, false, NO_FRAME_PAUSE_TICKS);
        }
        assert_eq!(counter, NO_FRAME_PAUSE_TICKS - 1);

        // Un frame llega → reset.
        let step = step_no_frame_watchdog(&mut counter, true, NO_FRAME_PAUSE_TICKS);
        assert_eq!(step, NoFrameStep::FrameOk);
        assert_eq!(counter, 0);

        // Otros 119 sin frame — aún no dispara.
        for _ in 0..NO_FRAME_PAUSE_TICKS - 1 {
            let step = step_no_frame_watchdog(&mut counter, false, NO_FRAME_PAUSE_TICKS);
            assert_eq!(step, NoFrameStep::Accumulating);
        }
        assert_eq!(counter, NO_FRAME_PAUSE_TICKS - 1);
    }

    /// Tras disparar, si los frames vuelven el watchdog retorna a FrameOk
    /// inmediatamente y no hay estado residual.
    #[test]
    fn frame_after_trigger_recovers() {
        let mut counter: u32 = 0;

        // Llegar al umbral y disparar.
        for _ in 0..NO_FRAME_PAUSE_TICKS {
            step_no_frame_watchdog(&mut counter, false, NO_FRAME_PAUSE_TICKS);
        }
        assert_eq!(counter, 0, "contador reseteado tras disparar");

        // El frame vuelve.
        let step = step_no_frame_watchdog(&mut counter, true, NO_FRAME_PAUSE_TICKS);
        assert_eq!(step, NoFrameStep::FrameOk);
        assert_eq!(counter, 0);
    }

    /// Confirma la constante: 120 ticks @ 30 Hz ≈ 4s.
    #[test]
    fn threshold_constant_is_four_seconds_at_30hz() {
        let seconds = NO_FRAME_PAUSE_TICKS as f64 / 30.0;
        assert!((seconds - 4.0).abs() < 0.001);
    }
}

/// Categoría de una hotkey emitida — usado para los contadores de
/// `DispatchStats`. `attack_default` → Attack; `heal_spell`/`heal_potion` →
/// Heal; `mana_spell` → Mana; otra → Other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitCategory {
    Attack,
    Heal,
    Mana,
    Other,
}

fn categorize_hotkey(hid: u8, hotkeys: &crate::config::Hotkeys) -> EmitCategory {
    if hid == hotkeys.attack_default {
        EmitCategory::Attack
    } else if hid == hotkeys.heal_spell || hid == hotkeys.heal_potion {
        EmitCategory::Heal
    } else if hid == hotkeys.mana_spell {
        EmitCategory::Mana
    } else {
        EmitCategory::Other
    }
}

/// Timestamp actual en milisegundos desde UNIX epoch. Usado en eventos y
/// stats de dispatch para calcular rates y ordenar en el cliente.
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Snapshot del estado del ScriptEngine para publicar en GameState.
/// Función libre porque el engine vive como variable local de `run()`.
fn snapshot_script_status(eng: Option<&ScriptEngine>) -> ScriptStatus {
    match eng {
        None => ScriptStatus::default(),
        Some(e) => ScriptStatus {
            enabled:      true,
            loaded_files: e.loaded_files().iter()
                .map(|p| p.display().to_string())
                .collect(),
            last_errors:  e.last_errors().to_vec(),
        },
    }
}
