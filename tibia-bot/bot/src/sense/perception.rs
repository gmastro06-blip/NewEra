/// perception.rs — Structs de percepción: resultado de analizar un frame NDI.
///
/// Estos tipos son inmutables una vez construidos y se pasan entre módulos
/// sin necesidad de locks. El game loop los almacena en GameState.

use std::time::Instant;

/// Snapshot serializable de una Perception. Usado por el replay tool (F1)
/// para grabar sesiones en JSONL y reinyectarlas offline al FSM/cavebot.
///
/// No incluye datos raw del minimap (demasiado pesado) — solo campos derivados
/// que el FSM y cavebot necesitan para decidir.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PerceptionSnapshot {
    pub tick:                 u64,
    /// Unix ms timestamp (reconstruido a partir de `captured_at` via wall clock).
    pub captured_at_ms:       Option<u64>,
    pub hp_ratio:             Option<f32>,
    pub mana_ratio:           Option<f32>,
    pub in_combat:            bool,
    pub enemy_count:          u32,
    pub target_active:        Option<bool>,
    pub is_moving:            Option<bool>,
    pub minimap_displacement: Option<(i32, i32)>,
    pub game_coords:          Option<(i32, i32, i32)>,
    pub loot_sparkles:        u32,
    pub ui_matches:           Vec<String>,
    pub conditions:           Vec<String>,
    pub inventory_counts:     std::collections::HashMap<String, u32>,
    pub inventory_stacks:     std::collections::HashMap<String, u32>,
    /// Veredicto del AnchorTracker en este tick.
    /// `#[serde(default)]` → JSONL antiguos sin el campo cargan como `Ok`.
    #[serde(default)]
    pub anchor_drift:         super::vision::anchors::DriftStatus,
}

/// Snapshot completo de lo que el sistema de visión leyó en un frame.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // extension point: minimap, captured_at, minimap_diff populated for future use
pub struct Perception {
    /// Vitales del personaje (HP, mana).
    pub vitals:     CharVitals,
    /// Lista de batalla (criaturas visibles en el panel lateral).
    pub battle:     BattleList,
    /// Condiciones de estado activas (iconos en la UI).
    pub conditions: StatusConditions,
    /// Snapshot del minimapa.
    pub minimap:    Option<MinimapSnapshot>,
    /// **Target activo** del char — signal binario leído del target info bar
    /// encima del viewport. `true` si el char tiene un mob seleccionado y
    /// está atacándolo. `false` si no hay target (char idle o entre targets).
    ///
    /// Valor por defecto `false`. El FSM usa este signal para decidir si
    /// emitir PgDown: si hay combat pero `!target_active`, necesita seleccionar
    /// uno; si ya hay target, no emit (evita rotar el objetivo por ruido).
    ///
    /// `None` si la ROI `target_hp_bar` no está configurada o no cabe en el
    /// frame. En ese caso el FSM debe caer al fallback legacy (count flancos).
    pub target_active: Option<bool>,
    /// Conteo de hits cromáticos del último read del target — solo para
    /// diagnóstico HTTP. No usado por el FSM.
    pub target_hits:   u32,
    /// **Sparkles de loot visibles** alrededor del char (área 3×3 tiles).
    /// Contador de píxeles "blanco puro saturado" en el loot_area. Si supera
    /// `LOOT_SPARKLE_THRESHOLD`, hay loot disponible para ser recogido.
    ///
    /// 0 = sin ROI calibrada O sin loot visible. Ver `vision/loot.rs`.
    pub loot_sparkles: u32,
    /// Elementos de UI detectados por template matching en este frame.
    /// Contiene los nombres de los templates de `assets/templates/ui/` que
    /// hacen match. Vacío si no hay templates cargados o ninguno coincide.
    /// Accesible en Lua como `ctx.ui["depot_chest"]`.
    pub ui_matches: Vec<String>,
    /// Coordenadas center-x/y + dims de cada template matched.
    /// Usado por OpenNpcTrade con `bag_button_template = "..."` para click
    /// genérico en el bag icon del greeting sin hardcodear coords por NPC.
    /// Key = nombre del template, Value = (center_x, center_y, width, height).
    /// Vacío si no hay matches. Hasta ~500ms stale (cache async del UiDetector).
    pub ui_match_infos: std::collections::HashMap<String, (u32, u32, u32, u32)>,
    /// Timestamp de cuando se procesó este frame.
    pub captured_at: Option<Instant>,
    /// Número de frame en que se generó esta percepción.
    pub frame_tick: u64,
    /// Diferencia L1 normalizada [0.0..1.0] del minimapa respecto al frame anterior.
    /// 0.0 = sin minimap ROI o primer frame. >0 = el char se movió.
    pub minimap_diff: f32,
    /// `Some(true)` si el char se movió (minimap_diff supera umbral).
    /// `Some(false)` si no se movió. `None` si minimap no está calibrado.
    /// El cavebot deshabilita stuck detection cuando es `None`.
    pub is_moving: Option<bool>,
    /// Desplazamiento del minimap en píxeles: (dx, dy).
    /// +dx = derecha, +dy = abajo. `None` si no hubo movimiento o no calibrado.
    pub minimap_displacement: Option<(i32, i32)>,
    /// Coordenadas absolutas del personaje (x, y, z) leídas por tile-hashing.
    /// `None` si el map index no está cargado o no hubo match.
    pub game_coords: Option<(i32, i32, i32)>,
    /// Conteo de items detectados en inventario por template matching.
    /// Key = nombre del template (sin .png), value = número de slots con match.
    /// Vacío si inventory vision no está calibrada.
    pub inventory_counts: std::collections::HashMap<String, u32>,
    /// Suma de unidades por item leídas via OCR del stack count (M1).
    /// Si los digit templates no están cargados, suele coincidir con
    /// inventory_counts (1 unit per slot).
    pub inventory_stacks: std::collections::HashMap<String, u32>,
    /// Veredicto del AnchorTracker sobre consistencia geométrica de anchors.
    /// `Ok` → ROIs confiables. `Inconsistent` → anchors divergen, offset no
    /// fiable. `AllLost` → ningún anchor matcheó, bot ciego al shift de ventana.
    /// El game loop usa este valor (con histéresis) para safety-pausar.
    pub anchor_drift: super::vision::anchors::DriftStatus,
}

impl Perception {
    /// Convierte a un snapshot serializable (formato del replay tool).
    /// Pierde los datos raw del minimap y cualquier Instant — substituye
    /// captured_at por un epoch ms calculado via wall clock.
    pub fn to_snapshot(&self) -> PerceptionSnapshot {
        use std::time::{SystemTime, UNIX_EPOCH};
        let captured_at_ms = self.captured_at.as_ref().map(|_| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        });
        PerceptionSnapshot {
            tick:                 self.frame_tick,
            captured_at_ms,
            hp_ratio:             self.vitals.hp.map(|b| b.ratio),
            mana_ratio:           self.vitals.mana.map(|b| b.ratio),
            in_combat:            self.battle.has_enemies(),
            enemy_count:          self.battle.enemy_count() as u32,
            target_active:        self.target_active,
            is_moving:            self.is_moving,
            minimap_displacement: self.minimap_displacement,
            game_coords:          self.game_coords,
            loot_sparkles:        self.loot_sparkles,
            ui_matches:           self.ui_matches.clone(),
            conditions:           self.conditions.active.iter().map(|c| format!("{:?}", c)).collect(),
            inventory_counts:     self.inventory_counts.clone(),
            inventory_stacks:     self.inventory_stacks.clone(),
            anchor_drift:         self.anchor_drift,
        }
    }
}

// ── Vitales ───────────────────────────────────────────────────────────────────

/// HP y mana del personaje, leídos por muestreo de píxeles en las barras.
#[derive(Debug, Clone, Default)]
pub struct CharVitals {
    /// HP: (actual, máximo). None si la barra no se encontró todavía.
    pub hp:   Option<VitalBar>,
    /// Mana: (actual, máximo). None si la barra no se encontró todavía.
    pub mana: Option<VitalBar>,
}

/// Valor de una barra vital: ratio en [0.0, 1.0] y estimación numérica.
#[derive(Debug, Clone, Copy)]
pub struct VitalBar {
    /// Fracción llenada de la barra: **0.0 = vacío, 1.0 = lleno al 100%**.
    ///
    /// Rango interno: siempre 0.0..=1.0.
    /// Las respuestas HTTP multiplican este valor por 100 antes de enviarlo
    /// (p. ej. `hp_percent = ratio * 100.0`).
    /// El FSM compara directamente con este valor: `HP_CRITICAL_RATIO = 0.30`.
    pub ratio: f32,
    /// Columnas (o píxeles) llenos contados — base del ratio.
    pub filled_px: u32,
    /// Total de columnas (o píxeles) en el ROI — denominador del ratio.
    pub total_px: u32,
}

impl VitalBar {
    pub fn new(filled_px: u32, total_px: u32) -> Self {
        let ratio = if total_px == 0 { 0.0 } else { filled_px as f32 / total_px as f32 };
        Self { ratio, filled_px, total_px }
    }

    pub fn is_critical(&self, threshold: f32) -> bool {
        self.ratio < threshold
    }
}

// ── Battle list ───────────────────────────────────────────────────────────────

/// Información de diagnóstico de un slot escaneado en el panel de batalla.
/// Permite inspeccionar cuántos píxeles de cada color se encontraron por fila,
/// útil para calibrar umbrales y detectar falsos positivos/negativos.
#[derive(Debug, Clone, Default)]
pub struct SlotDebug {
    /// Índice de fila (0 = primera fila del panel).
    pub row: u8,
    /// Coordenada Y absoluta en el frame donde empieza este slot.
    pub frame_y: u32,
    /// Hits de rojo (monstruo) encontrados en el borde izquierdo.
    pub red_hits: u32,
    /// Hits de azul (jugador) encontrados en el borde izquierdo.
    pub blue_hits: u32,
    /// Hits de amarillo (NPC) encontrados en el borde izquierdo.
    pub yellow_hits: u32,
    /// Hits totales del detector HP-bar fallback (`is_bar_filled`). En los
    /// clientes modernos de Tibia (sin colored borders) **este es el canal
    /// principal** de clasificación. Expuesto aquí para diagnosticar
    /// falsos positivos/negativos del sticky-until-empty.
    pub hp_bar_hits: u32,
    /// `true` si el slot tiene el highlight rojo de "char atacando este mob".
    /// Detectado via dominancia RGB en el borde del slot — inspirado en
    /// TibiaPilotNG pero usando relación de canales en vez de valores grayscale
    /// exactos (más robusto frente al jitter JPEG del NDI).
    ///
    /// Este signal reemplaza la necesidad de una ROI `target_hp_bar` separada:
    /// si cualquier slot tiene `is_being_attacked=true`, el char tiene target.
    pub is_being_attacked: bool,
    /// Clasificación final asignada a este slot.
    pub kind: Option<EntryKind>,
}

/// Lista de criaturas/jugadores en el panel de batalla.
#[derive(Debug, Clone, Default)]
pub struct BattleList {
    pub entries: Vec<BattleEntry>,
    /// Datos de diagnóstico por slot escaneado (uno por fila del panel).
    /// Sólo se rellena durante el escaneo; vacío cuando no hay frame reciente.
    pub slot_debug: Vec<SlotDebug>,
}

impl BattleList {
    #[allow(dead_code)] // extension point
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    pub fn has_enemies(&self) -> bool {
        self.entries.iter().any(|e| matches!(e.kind, EntryKind::Monster | EntryKind::Unknown))
    }

    #[allow(dead_code)]
    pub fn has_player(&self) -> bool {
        self.entries.iter().any(|e| matches!(e.kind, EntryKind::Player))
    }

    pub fn enemy_count(&self) -> usize {
        self.entries.iter().filter(|e| matches!(e.kind, EntryKind::Monster)).count()
    }

    /// `true` si cualquier entry tiene `is_being_attacked=true`. Usado para
    /// derivar el signal `target_active` sin requerir una ROI separada del
    /// target info bar. Inspirado en el approach de TibiaPilotNG.
    pub fn has_attacked_entry(&self) -> bool {
        self.entries.iter().any(|e| e.is_being_attacked)
    }
}

/// Una entrada en la lista de batalla.
#[derive(Debug, Clone)]
pub struct BattleEntry {
    /// Tipo de entidad detectado por análisis de color del borde.
    pub kind: EntryKind,
    /// Fila en el panel (0 = arriba).
    pub row: u8,
    /// Ratio de HP estimado de la criatura (0.0–1.0). None si no se pudo leer.
    pub hp_ratio: Option<f32>,
    /// Nombre de la entidad si se pudo extraer (futuro).
    #[allow(dead_code)] // extension point: OCR de nombre
    pub name: Option<String>,
    /// `true` si el char está atacando ESTE entry (highlight rojo en el slot).
    /// Detectado por dominancia RGB en el borde izquierdo del slot.
    pub is_being_attacked: bool,
}

/// Clasificación del tipo de entidad en batalla.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // extension point: Unknown
pub enum EntryKind {
    /// Criatura/monstruo (borde rojo).
    Monster,
    /// Otro jugador (borde azul).
    Player,
    /// NPC (borde amarillo/verde).
    Npc,
    /// No identificado (borde de color no reconocido).
    Unknown,
}

// ── Status conditions ─────────────────────────────────────────────────────────

/// Condiciones de estado activas en el personaje (detectadas por template matching).
#[derive(Debug, Clone, Default)]
pub struct StatusConditions {
    pub active: Vec<Condition>,
}

impl StatusConditions {
    #[allow(dead_code)] // extension point: Lua/FSM condition checks
    pub fn has(&self, c: Condition) -> bool { self.active.contains(&c) }
}

/// Condición de estado individual.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Poisoned,
    Burning,
    Electrified,
    Drowning,
    Freezing,
    Dazzled,
    Cursed,
    Bleeding,
    Haste,
    Protection,
    Strengthened,
    InFight,
    Hungry,
    Drunk,
    MagicShield,
    SlowedDown,
}

// ── Minimap ───────────────────────────────────────────────────────────────────

/// Recorte del minimapa (datos crudos para análisis de navegación futura).
#[derive(Debug, Clone)]
pub struct MinimapSnapshot {
    /// Ancho del recorte en píxeles.
    pub width:  u32,
    /// Alto del recorte en píxeles.
    pub height: u32,
    /// Datos BGRA del recorte.
    pub data:   Vec<u8>,
}
