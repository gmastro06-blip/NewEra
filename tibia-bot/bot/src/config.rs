use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::act::keycode;

/// Configuración completa del bot, cargada desde config.toml.
/// Todos los campos tienen defaults sensatos para desarrollo local.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub ndi:         NdiConfig,
    pub pico:        PicoConfig,
    pub http:        HttpConfig,
    pub coords:      CoordsConfig,
    #[serde(default)]
    pub loop_config: LoopConfig,
    #[serde(default)]
    pub actions:     ActionsConfig,
    #[serde(default)]
    pub waypoints:   WaypointsConfig,
    #[serde(default)]
    pub scripting:   ScriptingConfig,
    #[serde(default)]
    pub safety:      SafetyConfig,
    #[serde(default)]
    pub cavebot:     CavebotConfig,
    #[serde(default)]
    pub game_coords: GameCoordsConfig,
    #[serde(default)]
    pub recording:   RecordingConfig,
    #[serde(default)]
    pub ml:          MlConfig,
    #[serde(default)]
    pub health:      HealthConfig,
    #[serde(default)]
    pub inventory:   InventoryConfig,
    /// Tabla de spells con prioridades. Si vacía, se genera desde `[actions]`.
    #[serde(default, rename = "spell")]
    pub spells:      Vec<SpellConfig>,
}

/// Configuración del pipeline de detección de inventario (plan robustez
/// 2026-04-22). Todos los campos opcionales con defaults — un TOML sin
/// `[inventory]` usa los defaults históricos.
///
/// Permite tuning sin recompile durante la sesión live:
/// - Aumentar `detect_interval_ticks` si el inventory satura el budget.
/// - Reducir si querés reacción más rápida (trade-off con cost).
/// - Ajustar `empty_stddev_max` si la calibración de slot empty/llenos
///   demuestra ruido distinto al asumido.
#[derive(Debug, Clone, Deserialize)]
pub struct InventoryConfig {
    /// Cada cuántos ticks correr el reader completo (Stage A/B/C).
    /// Default 15 (~500 ms @ 30 Hz). Los ticks "dormidos" reutilizan el
    /// cache. Subir para reducir cost si el reader es cuello de botella;
    /// bajar si querés detección más ágil de cambios de inventory.
    #[serde(default = "default_inventory_detect_interval")]
    pub detect_interval_ticks: u32,

    /// Threshold de luma stddev para Stage A empty detection.
    /// Default 20.0 (gap empírico entre slot vacío ~8-12 y llenado ~30+).
    /// Subir si slots vacíos tienen textura notable (fondo no uniforme).
    /// Bajar si se pierden slots con items oscuros (sprite bajo stddev).
    #[serde(default = "default_inventory_empty_stddev_max")]
    pub empty_stddev_max: f32,
}

impl Default for InventoryConfig {
    fn default() -> Self {
        Self {
            detect_interval_ticks: default_inventory_detect_interval(),
            empty_stddev_max:      default_inventory_empty_stddev_max(),
        }
    }
}

fn default_inventory_detect_interval() -> u32 { 15 }
fn default_inventory_empty_stddev_max() -> f32 { 20.0 }

/// Configuración del HealthSystem (degradación adaptativa runtime).
///
/// El HealthSystem siempre corre y publica diagnósticos via
/// `/health/detailed`. Este flag controla **si las decisiones de
/// degradación se aplican efectivamente al bot**:
///
/// - `apply_degradation=false` (default): solo diagnostico, sin behavior change.
///   Operador puede observar score + issues pero el bot opera normalmente.
/// - `apply_degradation=true`: cuando el HealthSystem alcanza
///   `DegradationLevel::SafeMode`, BotLoop emite safety pause con reason
///   `health:safe_mode`. Light/Heavy aún sin acción (FSM gate pendiente).
///
/// Default conservador: false. Habilitar solo después de validar
/// thresholds con sesión live + tunear el HealthConfig por defecto.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HealthConfig {
    /// Si `true`, BotLoop traduce DegradationLevel::SafeMode en safety pause.
    #[serde(default)]
    pub apply_degradation: bool,
}

/// Configuración del runtime ML (Fase 2.5).
///
/// Reemplaza el matcher SSE de inventory por un classifier ONNX entrenado
/// con el pipeline de `ml/train_inventory_classifier.py`.
///
/// **Estado actual**: scaffold de configuración. La integración ort runtime
/// real (carga de ONNX + inferencia) se hará en commit posterior cuando
/// haya un modelo entrenado disponible para validar end-to-end.
///
/// Por ahora `use_ml=true` con `model_path` apuntando a archivo válido
/// loggea WARN y cae al fallback SSE matcher.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MlConfig {
    /// Si `true`, intenta cargar el modelo ONNX en `model_path` y usarlo
    /// para clasificar inventory slots. Si false (default), usa SSE matcher.
    #[serde(default)]
    pub use_ml: bool,
    /// Path al modelo ONNX (ej. `ml/models/inventory_v1.onnx`).
    /// Vacío deshabilita ML aunque `use_ml=true`.
    #[serde(default)]
    pub model_path: String,
    /// Path al JSON con classes (ej. `ml/models/inventory_v1.classes.json`).
    /// Generado automáticamente por el script de training.
    #[serde(default)]
    pub classes_path: String,
    /// Confidence threshold mínimo para aceptar una predicción.
    /// Predicciones con softmax max < threshold se descartan (slot empty).
    /// Default 0.80.
    #[serde(default = "default_ml_confidence")]
    pub confidence_threshold: f32,
}

fn default_ml_confidence() -> f32 { 0.80 }

#[derive(Debug, Clone, Deserialize)]
pub struct NdiConfig {
    /// Nombre de la fuente NDI tal como aparece en OBS/DistroAV.
    pub source_name:         String,
    /// Segundos entre reintentos si no se encuentra la fuente.
    #[serde(default = "default_ndi_retry")]
    pub retry_interval_secs: f64,
}

fn default_ndi_retry() -> f64 { 2.0 }

#[derive(Debug, Clone, Deserialize)]
pub struct PicoConfig {
    /// "IP_PC_GAMING:9000" — dirección del bridge en el PC gaming.
    pub bridge_addr:       String,
    /// Timeout de conexión TCP en ms.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_ms: u64,
    /// Timeout por comando individual (Pico debe responder en este tiempo).
    #[serde(default = "default_cmd_timeout")]
    pub command_timeout_ms: u64,
    /// Backoff exponencial máximo entre reintentos TCP en segundos.
    #[serde(default = "default_max_backoff")]
    pub max_backoff_secs:  u64,
    /// V-001: auth token que el bot envía como `AUTH <token>\n` tras conectar.
    /// Debe matchear el `[tcp].auth_token` del bridge_config.toml. Si None o
    /// empty, no se envía AUTH (requiere bridge sin token configurado también).
    #[serde(default)]
    pub auth_token:        Option<String>,
}

fn default_connect_timeout() -> u64 { 3_000 }
fn default_cmd_timeout()     -> u64 { 100   }
fn default_max_backoff()     -> u64 { 5     }

#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_http_addr")]
    pub listen_addr: String,
    /// Bearer token que debe presentarse en el header `Authorization: Bearer <token>`
    /// de cada request. `None` o string vacío = sin autenticación (default, pero
    /// inseguro si el HTTP está expuesto más allá de loopback).
    ///
    /// **V-002 mitigation**: genera un token aleatorio de 32+ chars y ponelo
    /// aquí para impedir que procesos user-mode sin el token accedan al bot.
    ///
    /// Generar en PowerShell:
    ///   [Convert]::ToBase64String([byte[]] (1..32 | % {Get-Random -Max 256}))
    #[serde(default)]
    pub auth_token: Option<String>,
    /// **V-007 mitigation**: cuando `true`, los endpoints de debug/introspección
    /// devuelven 404. Solo quedan disponibles `/health`, `/status`, `/pause`,
    /// `/resume` y `/metrics` (con auth). Elimina la superficie de
    /// fingerprinting que puede usar un malware local para identificar la
    /// máquina como bot server (ej `/cavebot/status` leak de `hunt_profile`,
    /// `/vision/grab/debug` con boxes amarillos sobre la UI).
    ///
    /// Default `false` para preservar debuggability durante calibración. Setear
    /// `true` en sesiones live con cuenta real.
    ///
    /// Endpoints gated por stealth_mode:
    ///   - /vision/grab/{anchors,battle,debug,inventory,loot}
    ///   - /vision/{perception,vitals,battle,status,inventory,cursor,
    ///     match_now,extract_template,matcher/stats,battle/debug,
    ///     target/debug,loot/debug}
    ///   - /fsm/debug, /combat/events, /dispatch/stats
    ///   - /cavebot/status, /waypoints/status, /scripts/status
    ///   - /test/grab, /test/inject_frame
    #[serde(default)]
    pub stealth_mode: bool,
}

/// Default HTTP bind: `127.0.0.1:8080` (loopback only).
///
/// **Anti-detection 2026-04-17**: bajado de `0.0.0.0` (all-interfaces) a
/// `127.0.0.1` (localhost only). Con el default anterior, el bot HTTP
/// server era alcanzable desde toda la LAN — cualquier host remoto podía
/// scrapear `/cavebot/status` (expone `hunt_profile`, `verifying`,
/// `current_step`) o `/metrics` (gauges bot-specific), lo que identifica
/// trivialmente la máquina como un bot server.
///
/// Si necesitás monitoring desde otra máquina (ej Prometheus en otro
/// host, Grafana remoto), overridear en config:
///   [http]
///   listen_addr = "0.0.0.0:8080"   # opt-in, LAN accessible
///
/// Y considerar firewall rule para limitar source IPs.
fn default_http_addr() -> String { "127.0.0.1:8080".into() }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoopConfig {
    /// Frecuencia objetivo del game loop en Hz.
    pub target_fps: u32,
}

impl Default for LoopConfig {
    fn default() -> Self { Self { target_fps: 30 } }
}

/// Geometría del entorno de escritorio del PC gaming.
/// Necesaria para convertir coords del viewport a coords HID absolutas.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[allow(dead_code)] // extension point: tibia_window_w/h
pub struct CoordsConfig {
    /// Origen X del virtual desktop. Puede ser NEGATIVO si hay monitores
    /// a la izquierda del primario. Default 0 (backward compat).
    /// Auto-configurado desde el bridge via GET_GEOMETRY al boot.
    pub vscreen_origin_x:       i32,
    /// Origen Y del virtual desktop. Puede ser NEGATIVO si hay monitores
    /// arriba del primario. Default 0.
    pub vscreen_origin_y:       i32,
    pub desktop_total_w:        u32,
    pub desktop_total_h:        u32,
    pub tibia_window_x:         i32,
    pub tibia_window_y:         i32,
    pub tibia_window_w:         u32,
    pub tibia_window_h:         u32,
    pub game_viewport_offset_x: i32,
    pub game_viewport_offset_y: i32,
    pub game_viewport_w:        u32,
    pub game_viewport_h:        u32,
}

impl Default for CoordsConfig {
    fn default() -> Self {
        Self {
            vscreen_origin_x:       0,
            vscreen_origin_y:       0,
            desktop_total_w:        1920,
            desktop_total_h:        1080,
            tibia_window_x:         0,
            tibia_window_y:         0,
            tibia_window_w:         1920,
            tibia_window_h:         1080,
            game_viewport_offset_x: 0,
            game_viewport_offset_y: 0,
            game_viewport_w:        1920,
            game_viewport_h:        1080,
        }
    }
}

/// Hotkeys configurables para las acciones del bot.
/// Los strings son nombres humanos ("F1", "Space", ...) — se convierten a
/// códigos HID bajo demanda via `Config::hotkeys()`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ActionsConfig {
    /// Hotkey para spell de curación (HP).
    pub heal_spell:     String,
    /// Hotkey para poción de curación (HP).
    pub heal_potion:    String,
    /// Hotkey para spell/poción de mana.
    pub mana_spell:     String,
    /// Hotkey para atacar la siguiente criatura.
    pub attack_default: String,
    /// Hotkey para lootear corpse (Quick Loot / Open Last Corpse de Tibia).
    /// Opcional. Si está vacío, el auto-loot está desactivado.
    pub loot_hotkey:    String,
    /// Ticks de cooldown entre emergencias (30 Hz → 10 ticks = ~333ms).
    pub heal_cooldown_ticks:   u64,
    /// Ticks de cooldown entre ataques (30 Hz → 30 ticks = ~1s).
    pub attack_cooldown_ticks: u64,
}

impl Default for ActionsConfig {
    fn default() -> Self {
        Self {
            heal_spell:            "F1".into(),
            heal_potion:           "F2".into(),
            mana_spell:            "F3".into(),
            attack_default:        "Space".into(),
            loot_hotkey:           "".into(),
            heal_cooldown_ticks:   10,
            attack_cooldown_ticks: 30,
        }
    }
}

/// Códigos HID pre-parseados a partir de `ActionsConfig`.
/// Los errores de parseo se detectan al cargar el config (fail-fast).
#[derive(Debug, Clone, Copy)]
pub struct Hotkeys {
    pub heal_spell:     u8,
    pub heal_potion:    u8,
    pub mana_spell:     u8,
    pub attack_default: u8,
    /// Hotkey de loot. `None` si `loot_hotkey=""` en el config (auto-loot off).
    pub loot_hotkey:    Option<u8>,
}

/// Config del sistema de waypoints. Si `path` está vacío el bot arranca
/// sin lista cargada (se puede cargar luego vía `POST /waypoints/load`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WaypointsConfig {
    /// Ruta al archivo TOML de waypoints. Vacío = no auto-cargar.
    pub path:    String,
    /// Si es false, la lista se carga pero no se ejecuta hasta
    /// `POST /waypoints/resume`. Útil para debugging.
    pub enabled: bool,
    /// Stuck watchdog: máximo de ticks que el iterador puede pasar sin
    /// avanzar a un step diferente antes de que el BotLoop emita una
    /// advertencia y pause waypoints. 0 = desactivado.
    /// Default: 1800 ticks ≈ 60 segundos a 30 Hz.
    pub stuck_threshold_ticks: u64,
}

impl Default for WaypointsConfig {
    fn default() -> Self {
        Self {
            path:                  String::new(),
            enabled:               false,
            stuck_threshold_ticks: 1800,
        }
    }
}

/// Config del cavebot (hunt automation). Si `path` está vacío el bot
/// arranca sin cavebot (se puede cargar luego vía `POST /cavebot/load`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CavebotConfig {
    /// Ruta al archivo TOML de cavebot. Vacío = no auto-cargar.
    pub path:    String,
    /// Si es false, el script se carga pero no se ejecuta hasta
    /// `POST /cavebot/resume`.
    pub enabled: bool,

    // ── Tuning de navegación por nodos ────────────────────────────────
    /// Píxeles por tile en el minimap (default 2).
    pub pixels_per_tile:          Option<i32>,
    /// Tolerancia Manhattan en px para declarar llegada (default 4).
    pub displacement_tolerance:   Option<i32>,
    /// Ticks idle para confirmar arrival con displacement (default 10).
    pub arrived_idle_ticks:       Option<u32>,
    /// Ticks idle antes de re-click (default 60).
    pub reclick_idle_ticks:       Option<u32>,
    /// Máximo re-clicks por nodo (default 3).
    pub max_reclicks:             Option<u8>,
    /// Timeout duro en ticks (default 900 = 30s).
    pub timeout_ticks:            Option<u64>,
}

/// Config para detección de coordenadas por tile-hashing del minimap.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GameCoordsConfig {
    /// Ruta al archivo de índice pre-computado (bincode). Vacío = deshabilitado.
    pub map_index_path: String,
    /// Frames entre detecciones (default 15 = ~500ms @ 30fps).
    pub detect_interval: Option<u32>,
    /// Pixels por tile en el minimap NDI. El minimap in-game renderiza
    /// cada tile como `ndi_tile_scale` pixels, mientras que el index
    /// (construido desde TibiaMaps.io PNGs) usa 1 px/tile.
    ///
    /// Este valor se usa para downsamplear los patches del NDI antes del
    /// hash para que la escala matchee. Valor típico: 5 para clientes
    /// Tibia 10+/12+ con minimap zoom default. Si el minimap está en otro
    /// zoom, ajustar (valores comunes: 3, 4, 5, 6).
    pub ndi_tile_scale: u32,
    /// Directorio de reference PNGs (Minimap_Color_*.png) para el CCORR
    /// fallback. Si está vacío, el fallback NO se usa y game_coords depende
    /// solo de dHash (que puede fallar con Tibia 12 por anti-aliasing).
    /// Valor típico: "assets/minimap/minimap"
    pub minimap_dir: String,
    /// Threshold SSD_NORMED para el CCORR matcher (lower=better match).
    /// Default 0.05 = match muy fuerte. 0 = usar el default del matcher (0.05).
    pub matcher_threshold: f32,
    /// CSV de pisos a cargar en el matcher (ej "6,7,8"). Vacío/None = todos
    /// los pisos del directorio (consume más RAM, ~70 MB vs ~15 MB por piso).
    pub matcher_floors: Option<String>,
    /// Si true (default), el matcher valida cada candidato ganador con un
    /// segundo patch de la esquina opuesta del minimap. Rechaza falsos
    /// positivos donde varios sectores muestran score SSD similar.
    ///
    /// Setear a `false` para debug / A-B testing del comportamiento
    /// pre-fix.
    pub disambiguation_enabled: bool,
    /// Coord inicial `[x, y, z]` para semillar el matcher en cold boot.
    ///
    /// Sin seed, la primera llamada a `detect()` hace full brute force
    /// sobre todos los sectores cargados → puede elegir un false positive
    /// lejano si varios sectores del mismo piso tienen patrones visuales
    /// similares (validado 2026-04-17 live en Ab'dendriel depot: matcher
    /// reportaba (33627, 31842, 6) en vez de (32681, 31686, 6), ~946
    /// tiles al este, sin que la disambiguation lo rechazara).
    ///
    /// Con `starting_coord = [32681, 31686, 6]`, el bot arranca con
    /// `last_game_coords = Some(seed)` → el primer detect() usa narrow
    /// search (sector actual + 8 vecinos, ~256 tiles radius) limitando
    /// al piso del seed. Los false positives lejanos quedan fuera del
    /// search space.
    ///
    /// **Precondición**: el char debe estar físicamente cerca del seed
    /// al arrancar el bot. Si el char está >256 tiles del seed, narrow
    /// search no encuentra match → el matcher hace force_full después
    /// del `COORDS_REVALIDATE_INTERVAL` y auto-recupera.
    ///
    /// Formato TOML: `starting_coord = [32681, 31686, 6]`.
    /// `None` (default) = sin seed, comportamiento legacy (cold boot
    /// full search).
    pub starting_coord: Option<[i32; 3]>,
}

impl Default for GameCoordsConfig {
    fn default() -> Self {
        Self {
            map_index_path:    String::new(),
            detect_interval:   None,
            ndi_tile_scale:    5,
            minimap_dir:       String::new(),
            matcher_threshold: 0.0,
            matcher_floors:    None,
            disambiguation_enabled: true,
            starting_coord:    None,
        }
    }
}

/// Config para grabar sesiones de perception a JSONL (F1 replay tool).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct RecordingConfig {
    /// Si true, arranca la grabación al iniciar el bot.
    pub enabled: bool,
    /// Path del archivo JSONL donde grabar. Vacío = "session.jsonl".
    pub path: String,
    /// Intervalo de ticks entre grabaciones (default 1 = todos los ticks).
    /// Usar >1 para reducir tamaño del archivo en sesiones largas.
    pub interval_ticks: Option<u32>,
}

/// Tipo de spell: heal, attack, o support.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SpellKind {
    Heal,
    Attack,
    Support,
}

fn default_one() -> f32 { 1.0 }
fn default_spell_cd() -> u64 { 1000 }

/// Entrada de la tabla de spells. Cada spell tiene una hotkey, tipo,
/// prioridad y condiciones para ser elegible.
#[derive(Debug, Clone, Deserialize)]
pub struct SpellConfig {
    /// Hotkey (nombre humano: "F1", "PageDown", etc.)
    pub key:         String,
    /// Tipo: heal, attack, support
    pub kind:        SpellKind,
    /// Prioridad (menor = mayor prioridad). Spells del mismo kind se evalúan en orden.
    #[serde(default = "default_priority")]
    pub priority:    u32,
    /// HP mínimo requerido [0.0..1.0]. Default 0.0.
    #[serde(default)]
    pub min_hp:      f32,
    /// HP máximo [0.0..1.0]. Spell solo elegible si HP <= max_hp. Default 1.0.
    #[serde(default = "default_one")]
    pub max_hp:      f32,
    /// Mana mínimo requerido [0.0..1.0]. Default 0.0.
    #[serde(default)]
    pub min_mana:    f32,
    /// Mana máximo. Spell solo elegible si mana <= max_mana. Default 1.0.
    #[serde(default = "default_one")]
    pub max_mana:    f32,
    /// Enemigos mínimos en battle list. 0 = sin requisito. Default 0.
    #[serde(default)]
    pub min_enemies: u32,
    /// Cooldown individual en ms. Default 1000.
    #[serde(default = "default_spell_cd")]
    pub cooldown_ms: u64,
}

fn default_priority() -> u32 { 1 }

/// Config del motor de scripting Lua. `script_dir` vacío = scripting
/// deshabilitado (el engine no se crea). Se puede cargar a posteriori vía
/// `POST /scripts/reload?path=...`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScriptingConfig {
    /// Directorio con archivos .lua para cargar al arrancar. Vacío = sin scripts.
    pub script_dir:     String,
    /// Budget de tiempo por hook (ms). Excederlo emite warning. 0 desactiva.
    pub tick_budget_ms: f64,
}

impl Default for ScriptingConfig {
    fn default() -> Self {
        Self {
            script_dir:     String::new(),
            tick_budget_ms: 5.0,
        }
    }
}

/// Config del módulo de seguridad / humanización (Fase 5).
///
/// **Filosofía**: los valores default están calibrados para producir
/// comportamiento razonable sin tuning manual. Ajústalos solo si entiendes
/// lo que hacen — tiempos demasiado agresivos pueden volver el bot
/// detectable, tiempos demasiado conservadores lo hacen inútil.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// ¿Aplicar humanización temporal (jitter + reaction time)?
    pub humanize_timing:  bool,

    // ── Jitter en cooldowns (ticks) ──────────────────────────────────────
    /// Media y desviación del cooldown de heal (en ms).
    pub heal_cd_mean_ms:   f64,
    pub heal_cd_std_ms:    f64,
    /// Media y desviación del cooldown de ataque (en ms).
    pub attack_cd_mean_ms: f64,
    pub attack_cd_std_ms:  f64,

    // ── Pre-send jitter en el Actuator (ms) ──────────────────────────────
    /// Delay aleatorio añadido antes de enviar cada key_tap a la Pico.
    pub presend_jitter_mean_ms: f64,
    pub presend_jitter_std_ms:  f64,

    // ── Reaction time (ms antes de la primera acción tras detectar) ──────
    pub reaction_hp_mean_ms:     f64,
    pub reaction_hp_std_ms:      f64,
    pub reaction_enemy_mean_ms:  f64,
    pub reaction_enemy_std_ms:   f64,

    // ── Rate limiting ────────────────────────────────────────────────────
    /// Máximo de acciones emitidas por segundo. Hard cap de seguridad.
    pub max_actions_per_sec: u32,

    // ── Variación de acciones ────────────────────────────────────────────
    /// Si true, alterna heal_spell y heal_potion con pesos.
    pub heal_variation: bool,
    /// Peso relativo del heal_spell (0-100).
    pub heal_spell_weight:  u32,
    /// Peso relativo del heal_potion (0-100).
    pub heal_potion_weight: u32,

    // ── Breaks ───────────────────────────────────────────────────────────
    pub breaks_enabled: bool,

    // ── Session cap (plan 10 días Task 2.2) ──────────────────────────────
    /// Cap duro de duración de sesión en horas. Tras este tiempo el bot
    /// pausa con `safety_pause_reason = "session:max_duration_reached"`.
    /// Previene farmeo indefinido (detección estadística) + da ventana
    /// de recovery natural. `0.0` = deshabilitado (no cap).
    pub max_session_hours: f64,
    /// Margen (en minutos) antes del cap durante el cual el bot fuerza un
    /// emergency refill cycle saltando al label "refill" del cavebot, si
    /// existe. Objetivo: llegar al depot y vaciar el bag antes del hard
    /// cap, en lugar de pausar con loot expuesto mid-hunt. `0.0` =
    /// deshabilitado (solo dispara el hard cap vía `is_expired`).
    pub session_warning_min: f64,

    // ── Human noise ──────────────────────────────────────────────────────
    pub human_noise_enabled: bool,
    /// Lista de hotkeys "seguras" para emitir como ruido (nombres de tecla).
    pub human_noise_keys: Vec<String>,
    /// Intervalo medio entre emisiones de ruido, en segundos.
    pub human_noise_interval_mean_s: f64,
    pub human_noise_interval_std_s:  f64,

    // ── Prompt detection ─────────────────────────────────────────────────
    /// Si true, pausa el bot cuando se detecta login/death/captcha.
    pub prompt_detection_enabled: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            humanize_timing:     true,
            heal_cd_mean_ms:     333.0, // 10 ticks @ 30Hz
            heal_cd_std_ms:      83.0,  // 2.5 ticks
            attack_cd_mean_ms:   1000.0,
            attack_cd_std_ms:    200.0,
            presend_jitter_mean_ms: 45.0,
            presend_jitter_std_ms:  15.0,
            reaction_hp_mean_ms:    180.0,
            reaction_hp_std_ms:     40.0,
            reaction_enemy_mean_ms: 250.0,
            reaction_enemy_std_ms:  60.0,
            max_actions_per_sec:    8,
            heal_variation:         true,
            heal_spell_weight:      70,
            heal_potion_weight:     30,
            breaks_enabled:         false, // off por default, opt-in
            max_session_hours:      0.0,   // 0 = deshabilitado por default
            session_warning_min:    5.0,   // 5 min de margen para graceful refill
            human_noise_enabled:    false,
            human_noise_keys:       vec![],
            human_noise_interval_mean_s: 180.0,
            human_noise_interval_std_s:  60.0,
            prompt_detection_enabled:    true,
        }
    }
}

impl Config {
    /// Carga config desde un archivo TOML.
    /// Falla de forma descriptiva si el archivo no existe o tiene campos inválidos.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("No se pudo leer '{}'", path.display()))?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("Config TOML inválido en '{}'", path.display()))?;
        // Validar hotkeys temprano para fallar al arrancar, no en el primer uso.
        cfg.hotkeys()
            .with_context(|| format!("Hotkeys inválidas en '{}'", path.display()))?;
        Ok(cfg)
    }

    /// Parsea los nombres de hotkeys de `actions` a códigos HID.
    /// Retorna error si cualquier nombre es inválido.
    pub fn hotkeys(&self) -> Result<Hotkeys> {
        // loot_hotkey es opcional: string vacío = desactivado.
        let loot_hotkey = if self.actions.loot_hotkey.is_empty() {
            None
        } else {
            Some(
                keycode::parse(&self.actions.loot_hotkey)
                    .context("actions.loot_hotkey")?,
            )
        };
        Ok(Hotkeys {
            heal_spell:     keycode::parse(&self.actions.heal_spell)
                .context("actions.heal_spell")?,
            heal_potion:    keycode::parse(&self.actions.heal_potion)
                .context("actions.heal_potion")?,
            mana_spell:     keycode::parse(&self.actions.mana_spell)
                .context("actions.mana_spell")?,
            attack_default: keycode::parse(&self.actions.attack_default)
                .context("actions.attack_default")?,
            loot_hotkey,
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse SOLO la sección `[game_coords]` desde un string TOML.
    fn parse_gc(toml_src: &str) -> Result<GameCoordsConfig> {
        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[serde(default)]
            game_coords: GameCoordsConfig,
        }
        let w: Wrapper = toml::from_str(toml_src)?;
        Ok(w.game_coords)
    }

    #[test]
    fn starting_coord_absent_defaults_to_none() {
        let cfg = parse_gc(r#"
[game_coords]
map_index_path = ""
"#).expect("parse");
        assert_eq!(cfg.starting_coord, None);
    }

    #[test]
    fn starting_coord_array_syntax_parses() {
        let cfg = parse_gc(r#"
[game_coords]
map_index_path = "assets/map_index.bin"
starting_coord = [32681, 31686, 6]
"#).expect("parse");
        assert_eq!(cfg.starting_coord, Some([32681, 31686, 6]));
    }

    #[test]
    fn starting_coord_negative_values_allowed() {
        // Algunos mapas Tibia tienen coords negativas en ciertos pisos
        // profundos o en otros continentes; i32 acepta el rango completo.
        let cfg = parse_gc(r#"
[game_coords]
map_index_path = ""
starting_coord = [-100, 50000, 15]
"#).expect("parse");
        assert_eq!(cfg.starting_coord, Some([-100, 50000, 15]));
    }

    #[test]
    fn starting_coord_too_few_elements_errors() {
        // Array con <3 elementos debe fallar (el tipo es `[i32; 3]` — serde
        // rechaza arrays más cortos).
        let err2 = parse_gc(r#"
[game_coords]
starting_coord = [32681, 31686]
"#).expect_err("array de 2 debería fallar");
        let msg = format!("{:#}", err2);
        assert!(
            msg.contains("invalid") || msg.contains("length")
                || msg.contains("expected") || msg.contains("3"),
            "error debería mencionar length/expected/3, got: {}",
            msg
        );
    }

    #[test]
    fn starting_coord_extra_elements_truncated_silently() {
        // Limitación conocida del crate `toml` 0.5+: arrays con MÁS elementos
        // que el tipo `[i32; N]` se truncan silenciosamente al 1er N en vez
        // de fallar. Documentamos el comportamiento aquí — si el user
        // escribe [1,2,3,4] por error, los valores 4+ se descartan sin warning.
        //
        // Mitigación: el validator binario (o un lint) debería flagear esto
        // si se vuelve un problema en práctica.
        let cfg = parse_gc(r#"
[game_coords]
starting_coord = [1, 2, 3, 4]
"#).expect("arity>3 no es un error, se trunca");
        assert_eq!(cfg.starting_coord, Some([1, 2, 3]),
                   "los elementos extra se descartan silenciosamente");
    }

    #[test]
    fn starting_coord_non_integer_errors() {
        // Floats no son válidos para tile coords (son enteros).
        let err = parse_gc(r#"
[game_coords]
starting_coord = [32681.5, 31686, 6]
"#).expect_err("float en array debería fallar");
        assert!(!format!("{:#}", err).is_empty());
    }

    #[test]
    fn game_coords_full_config_with_all_fields() {
        let cfg = parse_gc(r#"
[game_coords]
map_index_path         = "assets/map_index.bin"
detect_interval        = 20
ndi_tile_scale         = 5
minimap_dir            = "assets/minimap/minimap"
matcher_threshold      = 0.08
matcher_floors         = "6,7,8,9,10"
disambiguation_enabled = true
starting_coord         = [32681, 31686, 6]
"#).expect("parse");
        assert_eq!(cfg.map_index_path, "assets/map_index.bin");
        assert_eq!(cfg.detect_interval, Some(20));
        assert_eq!(cfg.ndi_tile_scale, 5);
        assert_eq!(cfg.minimap_dir, "assets/minimap/minimap");
        assert!((cfg.matcher_threshold - 0.08).abs() < 1e-6);
        assert_eq!(cfg.matcher_floors.as_deref(), Some("6,7,8,9,10"));
        assert!(cfg.disambiguation_enabled);
        assert_eq!(cfg.starting_coord, Some([32681, 31686, 6]));
    }

    #[test]
    fn game_coords_default_has_none_starting_coord() {
        let cfg = GameCoordsConfig::default();
        assert_eq!(cfg.starting_coord, None);
        assert!(cfg.disambiguation_enabled, "disambiguation defaults to true");
    }
}
