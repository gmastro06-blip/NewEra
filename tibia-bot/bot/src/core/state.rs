use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use parking_lot::RwLock;

use crate::core::fsm::FsmState;
use crate::sense::metrics::VisionMetrics;
use crate::sense::perception::Perception;

/// Capacidad máxima del ring buffer de eventos de combate. 200 ~ últimos 7
/// minutos a 30Hz si solo se pushean transiciones y emits (no cada tick).
pub const COMBAT_EVENTS_CAP: usize = 200;

/// Estado completo del personaje leído del frame actual.
/// Los campos son opcionales hasta que la visión lo confirme.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // extension point: populated by vision, consumed when combat logic reads them
pub struct CharStatus {
    // derive Clone necesario para el snapshot en el game loop
    /// HP actual y máximo. None = no parseado todavía.
    pub hp:       Option<(u32, u32)>,
    pub mana:     Option<(u32, u32)>,
    pub stamina:  Option<u32>,
    pub level:    Option<u32>,
    pub in_battle: bool,
}

/// Modo operativo del sistema de waypoints.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum WaypointMode {
    #[default]
    Idle,
    Hunting,
    Returning,
    Custom(String),
}

/// Métricas de rendimiento expuestas en /status.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    /// Ticks procesados desde que arrancó el bot.
    pub ticks_total:      u64,
    /// Ticks que excedieron el presupuesto de tiempo (overruns).
    pub ticks_overrun:    u64,
    /// NDI: latencia promedio rolling (ms) de los últimos 30 frames.
    pub ndi_latency_ms:   f64,
    /// Comando → respuesta de Pico: latencia promedio rolling (ms).
    pub pico_latency_ms:  f64,
    /// Procesamiento interno del bot (ms) promedio rolling.
    pub bot_proc_ms:      f64,
}

/// Estado actual de la WaypointList publicado por el loop para el HTTP.
/// El loop posee la `WaypointList` de verdad; esto es un snapshot de solo lectura.
#[derive(Debug, Clone, Default)]
pub struct WaypointStatus {
    /// ¿Hay una lista cargada?
    pub loaded:        bool,
    /// ¿Está ejecutándose?
    pub enabled:       bool,
    /// Número total de steps en la lista (0 si no cargada).
    pub total_steps:   usize,
    /// Índice del step activo (None si terminó la lista o no hay lista).
    pub current_index: Option<usize>,
    /// Label del step activo, copiado del struct.
    pub current_label: Option<String>,
    /// ¿La lista está en modo loop?
    pub loop_:         bool,
}

/// Snapshot del estado del motor de scripting. Publicado por el loop en cada
/// tick para que `/scripts/status` pueda leerlo sin tocar el `ScriptEngine`
/// (que vive en el thread del loop y no es `Send`).
#[derive(Debug, Clone, Default)]
pub struct ScriptStatus {
    /// ¿Hay scripting enabled? (engine creado, script_dir configurado)
    pub enabled:      bool,
    /// Archivos .lua cargados (paths absolutos).
    pub loaded_files: Vec<String>,
    /// Errores del último tick (runtime, parsing, etc). Se reemplaza cada tick.
    pub last_errors:  Vec<String>,
}

// ── Observability (Fase B) ───────────────────────────────────────────────────

/// Snapshot del estado INTERNO del FSM para /fsm/debug. Expone cooldowns,
/// timestamps de próximos emits y flags que normalmente no se exponen.
#[derive(Debug, Clone, Default)]
pub struct FsmDebugSnapshot {
    pub state:                  String,
    pub next_heal_tick:         Option<u64>,
    pub next_attack_tick:       Option<u64>,
    pub attack_keepalive_tick:  Option<u64>,
    /// Último valor de target_active observado por el FSM (tras decidir).
    /// `None` = primer tick o tras reset; Some(false) = sin target;
    /// Some(true) = atacando.
    pub prev_target_active:     Option<bool>,
}

/// Un evento de combate loggeado al ring buffer. Cada entry captura un
/// momento relevante: transición de estado FSM, emit de acción, flanco de
/// target perdido, etc. Se sobreescriben en orden FIFO cuando el buffer
/// alcanza `COMBAT_EVENTS_CAP`.
#[derive(Debug, Clone)]
pub struct CombatEvent {
    /// Tick del game loop en que ocurrió.
    pub tick:          u64,
    /// ms desde UNIX epoch.
    pub ts_ms:         u64,
    /// Estado del FSM en el momento del evento.
    pub fsm_state:     String,
    /// Acción emitida (o "Idle" si solo fue cambio de estado).
    pub action:        String,
    /// Razón del evento (corta, humana).
    pub reason:        String,
    /// Ratio HP del char cuando ocurrió (0.0..1.0, o null).
    pub hp_ratio:      Option<f32>,
    /// Target activo en el momento (si la ROI está configurada).
    pub target_active: Option<bool>,
    /// Enemigos en el battle list (count raw, sin debounce).
    pub enemy_count:   u32,
}

/// Contadores de acciones emitidas por el dispatcher, separados por tipo.
/// Usado en `/dispatch/stats` para monitorear rate y totales.
#[derive(Debug, Clone, Default)]
pub struct DispatchStats {
    /// Total de PgDown (u otros `attack_default`) emitidos.
    pub attacks_total:   u64,
    /// Total de `heal_spell` / `heal_potion` emitidos.
    pub heals_total:     u64,
    /// Total de `mana_spell` emitidos.
    pub mana_total:      u64,
    /// Cualquier otra hotkey emitida (waypoints, scripts, etc).
    pub other_total:     u64,
    /// Timestamps (ms desde epoch) del último emit por tipo. Útil para
    /// calcular "emits en los últimos 60s" en el cliente.
    pub last_attack_ms:  Option<u64>,
    pub last_heal_ms:    Option<u64>,
    pub last_mana_ms:    Option<u64>,
}

/// Snapshot del detector de target — expuesto para /vision/target/debug.
#[derive(Debug, Clone, Default)]
pub struct TargetDebug {
    /// ROI está configurada (si no, el fallback legacy está activo en FSM).
    pub configured:      bool,
    /// Lectura más reciente: active/inactive.
    pub active:          Option<bool>,
    /// Conteo cromático del último frame.
    pub hits:             u32,
    /// Threshold aplicado (alto=ACTIVE, bajo=STICKY).
    pub threshold_used:   u32,
}

/// Snapshot ligero del cavebot para `/cavebot/status`.
#[derive(Debug, Clone, Default)]
pub struct CavebotSnapshot {
    pub loaded:        bool,
    pub enabled:       bool,
    pub total_steps:   usize,
    pub current_index: Option<usize>,
    pub current_label: Option<String>,
    pub current_kind:  String,
    pub loop_:         bool,
    /// Nombre del hunt profile cargado (si el TOML lo declara), para
    /// observability en `/cavebot/status`. `None` = sin profile.
    pub hunt_profile:  Option<String>,
    /// True si el step actual está en fase de verify poll (post-acción,
    /// esperando postcondition). Permite distinguir "ejecutando step" vs
    /// "esperando evidencia de efecto" en el status endpoint.
    pub verifying:     bool,
    /// Baselines del hunt profile `[metrics]` section, copiados al load.
    /// Usados por /metrics (Prometheus) para exponer expected vs actual
    /// como health signal del hunt. `None` si el profile no los declara.
    pub expected_xp_per_hour:      Option<u64>,
    pub expected_kills_per_hour:   Option<u64>,
    pub expected_loot_gp_per_hour: Option<u64>,
    pub expected_cycle_min:        Option<u32>,
}

/// Estado global compartido entre el game loop, HTTP server y resto de módulos.
/// Se accede vía Arc<RwLock<GameState>> — writes solo en el game loop.
/// Clone permite que el game loop tome un snapshot rápido sin mantener el lock.
#[derive(Debug, Default, Clone)]
pub struct GameState {
    #[allow(dead_code)] // extension point
    pub char_status:    CharStatus,
    pub waypoint_mode:  WaypointMode,
    /// Pausa controlada por el operador (desde /pause o el loop de seguridad).
    pub is_paused:      bool,
    /// Timestamp del último tick ejecutado.
    pub last_tick_at:   Option<Instant>,
    /// Número de tick actual (monotónico desde el arranque).
    pub tick:           u64,
    pub metrics:        Metrics,
    /// Última percepción producida por el sistema de visión.
    pub last_perception: Option<Perception>,
    /// Historial rodante de métricas de visión (HP, mana, costo).
    pub vision_metrics: VisionMetrics,
    /// Estado actual de la FSM — escrito por el game loop tras cada decide().
    pub fsm_state:      FsmState,
    /// Snapshot del estado de waypoints — publicado por el loop para el HTTP.
    pub waypoint_status: WaypointStatus,
    /// Snapshot del estado de scripting — publicado por el loop para el HTTP.
    pub script_status:   ScriptStatus,
    /// Razón por la que safety pausó el bot. `None` = operación normal.
    /// Valores: "break:micro" / "break:medium" / "break:long" / "prompt:npc_trade".
    pub safety_pause_reason: Option<String>,
    /// Contador acumulado de acciones descartadas por el rate limiter.
    pub safety_rate_dropped: u64,

    // ── Observability (Fase B) ───────────────────────────────────────────
    /// Snapshot del state interno del FSM (cooldowns, flags). Actualizado
    /// por el game loop después de cada `fsm.decide()`.
    pub fsm_debug:     FsmDebugSnapshot,
    /// Ring buffer de eventos de combate. Solo el game loop escribe aquí.
    /// Capacidad máxima: `COMBAT_EVENTS_CAP`.
    pub combat_events: VecDeque<CombatEvent>,
    /// Contadores de acciones dispatched.
    pub dispatch_stats: DispatchStats,
    /// Snapshot del detector de target.
    pub target_debug:  TargetDebug,
    /// Snapshot del cavebot.
    pub cavebot_status: CavebotSnapshot,
    /// Snapshot de stats del MinimapMatcher (CCORR template matching).
    /// Actualizado por Vision::tick cada `coords_detect_interval` frames.
    /// `None` hasta la primera detection.
    pub matcher_stats: Option<crate::sense::vision::game_coords::MatcherStatsSnapshot>,
}

/// Alias conveniente — todo el código usa este tipo.
pub type SharedState = Arc<RwLock<GameState>>;

/// Crea un SharedState inicial.
pub fn new_shared_state() -> SharedState {
    Arc::new(RwLock::new(GameState::default()))
}
