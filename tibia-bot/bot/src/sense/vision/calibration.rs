/// calibration.rs — Structs de calibración y persistencia en calibration.toml.
///
/// La calibración define los ROIs de cada elemento de la UI de Tibia,
/// expresados como coordenadas absolutas dentro del frame NDI (1920x1080).
/// Se cargan al arrancar y se pueden regenerar con `bin/calibrate`.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Calibración completa del bot.
/// Todos los ROIs son opcionales: si un campo es None la feature correspondiente
/// queda deshabilitada en vez de crashear.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(dead_code)] // used by binaries and extension point
pub struct Calibration {
    /// ROI de la barra de HP (píxeles del frame NDI).
    #[serde(default)]
    pub hp_bar:     Option<RoiDef>,
    /// ROI de la barra de mana.
    #[serde(default)]
    pub mana_bar:   Option<RoiDef>,
    /// Panel completo de battle list.
    #[serde(default)]
    pub battle_list: Option<RoiDef>,
    /// Área de los iconos de condición.
    #[serde(default)]
    pub status_icons: Option<RoiDef>,
    /// Minimapa.
    #[serde(default)]
    pub minimap:    Option<RoiDef>,
    /// Viewport del juego (área de juego sin UI).
    #[serde(default)]
    pub game_viewport: Option<RoiDef>,
    /// Anclas de referencia para estabilizar los ROIs si la ventana se mueve.
    #[serde(default)]
    pub anchors:    Vec<AnchorDef>,
    /// ROI donde buscar el modal de NPC trade (buy/sell de shopkeepers).
    #[serde(default)]
    pub prompt_npc_trade:   Option<RoiDef>,
    /// ROI donde buscar la pantalla de login (post-disconnect/crash/server save).
    #[serde(default)]
    pub prompt_login:       Option<RoiDef>,
    /// ROI donde buscar la pantalla de character select (post-login o post-death).
    #[serde(default)]
    pub prompt_char_select: Option<RoiDef>,
    /// ROIs de búsqueda para templates de UI genéricos (cargados desde
    /// `assets/templates/ui/`). Clave = nombre del archivo PNG (sin extensión).
    /// Si un template no tiene ROI, se busca en el frame completo (más lento).
    ///
    /// Ejemplo en calibration.toml:
    /// ```toml
    /// [ui_rois]
    /// depot_chest = { x = 1200, y = 0, w = 700, h = 500 }
    /// stow_menu   = { x = 900,  y = 0, w = 900, h = 800 }
    /// ```
    #[serde(default)]
    pub ui_rois: HashMap<String, RoiDef>,
    /// ROI de la barra de HP del TARGET actual (encima del viewport). Se usa
    /// como señal binaria "char tiene target / no tiene". Si esta área tiene
    /// suficientes píxeles cromáticos = hay target; si está gris = no hay.
    ///
    /// Fase A del plan reemplaza el event-driven targeting (por count del
    /// battle list) por este signal directo. Cuando el char no tiene target
    /// pero hay combat, el bot emite PgDown. Con target, no emit.
    #[serde(default)]
    pub target_hp_bar:      Option<RoiDef>,
    /// ROIs de los slots del inventario (backpack visible en la UI lateral).
    /// Cada ROI típicamente es 32×32 px. El InventoryReader escanea todos
    /// los slots y template-matchea contra `assets/templates/inventory/*.png`.
    ///
    /// **Opción A (recomendada)**: usar `[inventory_grid]` para auto-generar
    /// los slots a partir de una posición base + dimensiones del grid.
    ///
    /// **Opción B**: definir cada slot manualmente con `[[inventory_slot]]`.
    /// Solo se usa si `inventory_grid` está vacío.
    #[serde(default, rename = "inventory_slot")]
    pub inventory_slots: Vec<RoiDef>,
    /// Grid auto-generado del backpack. Si está presente, tiene prioridad sobre
    /// `inventory_slots`. Calcula las ROIs de todos los slots a partir de un
    /// origen + tamaño/gap/filas/columnas.
    ///
    /// Defaults razonables para Tibia 12 @ 1920×1080:
    /// ```toml
    /// [inventory_grid]
    /// x = 1760          # top-left del primer slot
    /// y = 420
    /// slot_size = 32
    /// gap = 2
    /// cols = 4
    /// rows = 5
    /// ```
    #[serde(default)]
    pub inventory_grid: Option<InventoryGrid>,
    /// **Opción C**: layout con N backpacks stacked verticalmente.
    ///
    /// Común para cavebot donde cada backpack contiene un tipo de item
    /// (loot, potions, runes, food, gold, supplies). Cada backpack tiene
    /// su propio title bar + un row de slots + capacity bar. El detector
    /// genera las ROIs de slots iterando por cada backpack en la strip.
    ///
    /// Si esta opción está presente, tiene **prioridad sobre `inventory_grid`
    /// e `inventory_slot`**.
    ///
    /// Ejemplo:
    /// ```toml
    /// [inventory_backpack_strip]
    /// x              = 1567  # top-left del primer backpack en el strip
    /// y              = 22
    /// backpack_w     = 174   # ancho de una ventana de backpack
    /// backpack_h     = 67    # alto de una ventana de backpack (title + 1 row + bottom)
    /// backpack_count = 8     # número de backpacks stacked
    /// slot_x_offset  = 6     # margen interno del backpack a la 1a columna
    /// slot_y_offset  = 18    # margen interno (abajo del title bar) a la 1a fila
    /// slot_size      = 32    # tamaño del slot (ancho = alto)
    /// slot_gap       = 2
    /// slot_cols      = 4     # slots por fila dentro de cada backpack
    /// slot_rows      = 1     # filas dentro de cada backpack
    /// ```
    #[serde(default)]
    pub inventory_backpack_strip: Option<InventoryBackpackStrip>,
}

/// Grid auto-generado del backpack: expande a N slots individuales.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct InventoryGrid {
    pub x:         u32,
    pub y:         u32,
    pub slot_size: u32,
    pub gap:       u32,
    pub cols:      u32,
    pub rows:      u32,
}

impl Default for InventoryGrid {
    /// Defaults para Tibia 12 @ 1920×1080 con backpack estándar
    /// en el right sidebar debajo del equipment panel.
    fn default() -> Self {
        Self {
            x:         1760,
            y:         420,
            slot_size: 32,
            gap:       2,
            cols:      4,
            rows:      5,
        }
    }
}

impl InventoryGrid {
    /// Expande el grid a una lista de ROIs individuales.
    #[allow(dead_code)] // used by vision/mod.rs and http handlers
    pub fn expand(&self) -> Vec<RoiDef> {
        let mut out = Vec::with_capacity((self.cols * self.rows) as usize);
        let stride = self.slot_size + self.gap;
        for row in 0..self.rows {
            for col in 0..self.cols {
                out.push(RoiDef {
                    x: self.x + col * stride,
                    y: self.y + row * stride,
                    w: self.slot_size,
                    h: self.slot_size,
                });
            }
        }
        out
    }
}

/// Layout de N backpacks stacked verticalmente en una strip.
///
/// Útil para cavebot donde cada backpack contiene un tipo de item y todos
/// están minimizados a 1 fila. El detector genera
/// `backpack_count * slot_rows * slot_cols` ROIs iterando por cada backpack.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct InventoryBackpackStrip {
    /// Top-left del primer backpack (píxeles absolutos del frame).
    pub x: u32,
    pub y: u32,
    /// Dimensiones de una ventana de backpack.
    pub backpack_w: u32,
    pub backpack_h: u32,
    /// Número de backpacks stacked verticalmente.
    pub backpack_count: u32,
    /// Offset interno dentro de cada backpack hasta el primer slot.
    pub slot_x_offset: u32,
    pub slot_y_offset: u32,
    /// Dimensiones del slot individual.
    pub slot_size: u32,
    pub slot_gap: u32,
    /// Slots por backpack.
    pub slot_cols: u32,
    pub slot_rows: u32,
}

impl Default for InventoryBackpackStrip {
    /// Defaults basados en mediciones GIMP del usuario 2026-04-15:
    /// - Strip vertical en (1567, 22), ancho 178, alto 1005
    /// - Backpack individual 174×67 (title + 1 row + capacity bar)
    /// - 8 backpacks stacked × 4 slots cada uno = 32 slots total
    fn default() -> Self {
        Self {
            x:              1567,
            y:              22,
            backpack_w:     174,
            backpack_h:     67,
            backpack_count: 8,
            slot_x_offset:  6,
            slot_y_offset:  18,
            slot_size:      32,
            slot_gap:       2,
            slot_cols:      4,
            slot_rows:      1,
        }
    }
}

impl InventoryBackpackStrip {
    /// Expande la strip a una lista de ROIs individuales, uno por cada slot.
    ///
    /// Orden: `backpack 0 → slots 0..N`, `backpack 1 → slots 0..N`, etc.
    #[allow(dead_code)] // used by vision/mod.rs and http handlers
    pub fn expand(&self) -> Vec<RoiDef> {
        let total = (self.backpack_count * self.slot_rows * self.slot_cols) as usize;
        let mut out = Vec::with_capacity(total);
        let slot_stride = self.slot_size + self.slot_gap;

        for bp in 0..self.backpack_count {
            let bp_y = self.y + bp * self.backpack_h;
            for row in 0..self.slot_rows {
                for col in 0..self.slot_cols {
                    out.push(RoiDef {
                        x: self.x + self.slot_x_offset + col * slot_stride,
                        y: bp_y + self.slot_y_offset + row * slot_stride,
                        w: self.slot_size,
                        h: self.slot_size,
                    });
                }
            }
        }
        out
    }
}

/// Definición de un ROI en coordenadas de frame (píxeles absolutos).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RoiDef {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl RoiDef {
    #[allow(dead_code)] // used in main binary + tests, not visible to diagnostic bins
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }
}

/// Rol del anchor en la estrategia de failover.
/// - `Primary` (default): se usa directamente. Si ≥1 primary matchea, los
///   fallbacks se ignoran y el cluster se calcula solo sobre primaries.
/// - `Fallback`: solo se usa cuando CERO primaries están matcheando. Sirve
///   de red de seguridad cuando un template primary se degrada (template
///   obsoleto por patch de Tibia, región bajo overlay, etc.).
///
/// Calibration típica: 2 primaries cross-geométricos + 1-2 fallbacks en
/// regiones independientes. Mientras primaries sanos, fallbacks están
/// "dormidos" (no impactan el offset, pero se siguen trackeando).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorRole {
    #[default]
    Primary,
    Fallback,
}

/// Ancla: región de la pantalla con textura estable que sirve de referencia.
/// El AnchorTracker la busca en cada frame para calcular el desplazamiento
/// de la ventana de Tibia y ajustar todos los ROIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)] // extension point: anchor definition for calibration
pub struct AnchorDef {
    /// Nombre identificador (ej: "hp_bar_frame", "mana_icon").
    pub name:          String,
    /// Posición esperada del ancla en el frame de referencia.
    pub expected_roi:  RoiDef,
    /// Ruta al archivo de template (PNG) relativa a assets/anchors/.
    pub template_path: String,
    /// Rol en la estrategia failover. Default `Primary` para compat con
    /// calibration.toml existentes — los anchors actuales son todos primary.
    /// Marcar `role = "fallback"` solo para anchors de reserva.
    #[serde(default)]
    pub role:          AnchorRole,
}

impl Calibration {
    /// Carga calibration.toml desde disco.
    #[allow(dead_code)] // used in main binary, not visible to some diagnostic bins
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("No se encontró calibration.toml en '{}'", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("calibration.toml inválido en '{}'", path.display()))
    }

    /// Guarda la calibración a disco en formato TOML.
    #[allow(dead_code)] // extension point: calibrate binary
    pub fn save(&self, path: &Path) -> Result<()> {
        let raw = toml::to_string_pretty(self)
            .context("Error serializando calibration.toml")?;
        std::fs::write(path, raw)
            .with_context(|| format!("No se pudo escribir '{}'", path.display()))
    }

    /// Retorna true si hay suficientes ROIs definidos para ejecutar la visión básica.
    #[allow(dead_code)] // extension point: validation in binaries
    pub fn is_usable(&self) -> bool {
        self.hp_bar.is_some() && self.mana_bar.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_calibration_not_usable() {
        let cal = Calibration::default();
        assert!(!cal.is_usable());
    }

    #[test]
    fn round_trip_toml() {
        let mut cal = Calibration::default();
        cal.hp_bar   = Some(RoiDef::new(10, 20, 100, 8));
        cal.mana_bar = Some(RoiDef::new(10, 30, 100, 8));
        cal.anchors.push(AnchorDef {
            name:          "hp_frame".into(),
            expected_roi:  RoiDef::new(5, 15, 20, 20),
            template_path: "hp_frame.png".into(),
            role:          AnchorRole::Primary,
        });

        let serialized = toml::to_string_pretty(&cal).unwrap();
        let parsed: Calibration = toml::from_str(&serialized).unwrap();

        assert!(parsed.is_usable());
        let hp = parsed.hp_bar.unwrap();
        assert_eq!(hp.x, 10);
        assert_eq!(hp.y, 20);
        assert_eq!(hp.w, 100);
        assert_eq!(hp.h, 8);
        assert_eq!(parsed.anchors.len(), 1);
        assert_eq!(parsed.anchors[0].name, "hp_frame");
    }

    #[test]
    fn load_from_missing_file_returns_err() {
        let result = Calibration::load(Path::new("/nonexistent/calibration.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn save_and_reload() {
        let mut cal = Calibration::default();
        cal.hp_bar = Some(RoiDef::new(1, 2, 3, 4));
        cal.mana_bar = Some(RoiDef::new(5, 6, 7, 8));

        let dir = std::env::temp_dir();
        let path = dir.join("tibia_bot_test_calibration.toml");
        cal.save(&path).unwrap();

        let loaded = Calibration::load(&path).unwrap();
        assert!(loaded.is_usable());
        let hp = loaded.hp_bar.unwrap();
        assert_eq!((hp.x, hp.y, hp.w, hp.h), (1, 2, 3, 4));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn backpack_strip_default_expands_to_32_slots() {
        let strip = InventoryBackpackStrip::default();
        let slots = strip.expand();
        // 8 backpacks × 4 cols × 1 row = 32 slots.
        assert_eq!(slots.len(), 32);
        // El primer slot: (x + slot_x_offset, y + slot_y_offset) = (1573, 40)
        assert_eq!(slots[0].x, 1573);
        assert_eq!(slots[0].y, 40);
        assert_eq!(slots[0].w, 32);
        assert_eq!(slots[0].h, 32);
        // Segundo slot del primer backpack: x + stride (32+2=34) → 1607
        assert_eq!(slots[1].x, 1607);
        assert_eq!(slots[1].y, 40);
        // Cuarto slot (último de backpack 0): 1573 + 3*34 = 1675
        assert_eq!(slots[3].x, 1675);
        assert_eq!(slots[3].y, 40);
        // Primer slot del backpack 1: mismo x, y = 22 + 67 + 18 = 107
        assert_eq!(slots[4].x, 1573);
        assert_eq!(slots[4].y, 107);
        // Primer slot del backpack 7 (último): y = 22 + 7*67 + 18 = 509
        assert_eq!(slots[28].x, 1573);
        assert_eq!(slots[28].y, 509);
    }

    #[test]
    fn backpack_strip_custom_layout_expands_correctly() {
        // 3 backpacks × 4 cols × 2 rows = 24 slots.
        let strip = InventoryBackpackStrip {
            x:              1000,
            y:              100,
            backpack_w:     200,
            backpack_h:     100,
            backpack_count: 3,
            slot_x_offset:  10,
            slot_y_offset:  20,
            slot_size:      32,
            slot_gap:       2,
            slot_cols:      4,
            slot_rows:      2,
        };
        let slots = strip.expand();
        assert_eq!(slots.len(), 24);
        // 1er slot: (1010, 120)
        assert_eq!((slots[0].x, slots[0].y), (1010, 120));
        // 2a fila, 1er slot del mismo backpack: (1010, 120 + 34)
        assert_eq!((slots[4].x, slots[4].y), (1010, 154));
        // 1er slot del 2do backpack: (1010, 220)
        assert_eq!((slots[8].x, slots[8].y), (1010, 220));
    }

    #[test]
    fn backpack_strip_roundtrip_toml() {
        let mut cal = Calibration::default();
        cal.inventory_backpack_strip = Some(InventoryBackpackStrip::default());
        let serialized = toml::to_string_pretty(&cal).unwrap();
        let parsed: Calibration = toml::from_str(&serialized).unwrap();
        let strip = parsed.inventory_backpack_strip.expect("strip roundtripped");
        assert_eq!(strip.backpack_count, 8);
        assert_eq!(strip.slot_cols, 4);
        assert_eq!(strip.x, 1567);
    }
}
