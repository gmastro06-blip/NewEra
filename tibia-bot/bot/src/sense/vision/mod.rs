/// vision/mod.rs — Orquestador del sistema de visión.
///
/// `Vision` combina calibración, anclas, templates y parsers para producir
/// un `Perception` a partir de un frame NDI. Se llama una vez por tick
/// desde el game loop.

pub mod anchors;
pub mod battle_list;
pub mod calibration;
pub mod color;
pub mod crop;
pub mod hp_mana;
pub mod loot;
pub mod game_coords;
pub mod inventory;
pub mod inventory_ocr;
pub mod minimap;
pub mod prompts;
pub mod status_icons;
pub mod target;
pub mod templates;
pub mod ui_detector;

use std::path::Path;

use tracing::{debug, info, warn};

use crate::sense::frame_buffer::Frame;
use crate::sense::perception::{CharVitals, Perception};

use self::anchors::{AnchorConfig, AnchorTracker};
use self::battle_list::BattleListDetector;
use self::calibration::Calibration;
use self::hp_mana::{read_hp_by_edge, read_mana_by_edge};
use self::prompts::{PromptDetector, PromptKind};
use self::target::TargetDetector;
use self::templates::TemplateStore;
use self::ui_detector::UiDetector;

/// Umbral de diff L1 del minimapa para considerar que el char se movió.
/// Datos empíricos con Tibia + DistroAV NDI:
///   - Ruido idle (animaciones minimap): 0.006-0.016
///   - Movimiento real (1+ tiles):       0.070-0.085+
///   - Gap limpio: 0.016 .. 0.070
/// 0.025 está en el centro del gap con buen margen a ambos lados.
const MOVEMENT_DIFF_THRESHOLD: f32 = 0.025;

/// Frames consecutivos con diff <= threshold para desactivar is_moving (histéresis).
/// Durante auto-walk, el minimap se desplaza en pasos discretos por tile (~350ms/tile).
/// Entre shifts, frames consecutivos son iguales (diff≈0). Los frames de alto diff
/// vienen cada 3-5 frames a 30Hz, así que con calm=5 la histéresis se rompe en los gaps.
/// 10 frames (~333ms a 30Hz). Con threshold=0.025, el ruido idle nunca cruza,
/// así que 10 frames de calm se alcanzan solo cuando el char realmente se detiene.
/// Durante auto-walk, los shifts de tile (diff=0.07+) vienen cada ~350ms,
/// así que al menos un shift reinicia el calm counter antes de llegar a 10.
const MOVEMENT_CALM_FRAMES: u32 = 10;

/// Sistema de visión completo.
pub struct Vision {
    pub calibration: Calibration,
    tracker:         AnchorTracker,
    status_templates: TemplateStore,
    pub prompts:     PromptDetector,
    /// Detector stateful del battle list (mantiene histéresis por slot).
    battle_detector: BattleListDetector,
    /// Detector stateful del target actual del char.
    pub target_detector: TargetDetector,
    /// Detector genérico de elementos de UI (depot chest, menús, etc).
    ui_detector: UiDetector,
    /// Snapshot del minimapa del frame anterior — para calcular diff de movimiento.
    prev_minimap: Option<crate::sense::perception::MinimapSnapshot>,
    /// Último estado reportado de is_moving — para loguear transiciones a nivel INFO.
    prev_is_moving: Option<bool>,
    /// Estado actual de la histéresis de movimiento (true = en movimiento).
    moving_hysteresis: bool,
    /// Frames consecutivos con diff <= threshold. Cuando alcanza MOVEMENT_CALM_FRAMES,
    /// la histéresis se desactiva (is_moving → false).
    calm_frame_count: u32,
    /// Número de frame procesado.
    frame_count:     u64,
    /// Índice de tile-hashes para posicionamiento absoluto.
    map_index: Option<game_coords::MapIndex>,
    /// CCORR-based fallback para game_coords. Usado cuando dHash falla (común
    /// con Tibia 12 por anti-aliasing). Más lento pero mucho más robusto.
    minimap_matcher: game_coords::MinimapMatcher,
    /// Intervalo de frames entre detecciones de coords (default 15).
    coords_detect_interval: u32,
    /// Pixels por tile en el minimap NDI (default 5). Usado para downsamplear
    /// los patches antes del hash para matchear la escala del index.
    ndi_tile_scale: u32,
    /// Último resultado de detección de coords (cacheado entre intervalos).
    last_game_coords: Option<(i32, i32, i32)>,
    /// Última HP confirmada (no transitoria). Usada como fallback cuando
    /// el reader retorna None o un valor que parece transient noise.
    last_hp_stable: Option<crate::sense::perception::VitalBar>,
    /// Última mana confirmada (mismo concepto que last_hp_stable).
    last_mana_stable: Option<crate::sense::perception::VitalBar>,
    /// Contador de frames consecutivos con HP=None o ratio=0.0. Cuando
    /// excede `vitals_panic_frames`, se considera el valor como real
    /// (probablemente char muerto o pantalla cambió).
    bad_hp_frames: u32,
    /// Contador análogo para mana.
    bad_mana_frames: u32,
    /// Reader de inventario (opcional).
    inventory_reader: Option<inventory::InventoryReader>,
    /// Último conteo de items por template (cacheado entre intervalos).
    last_inventory_counts: std::collections::HashMap<String, u32>,
    /// Última suma de unidades por item via OCR del stack count (M1).
    last_inventory_stacks: std::collections::HashMap<String, u32>,
    /// Intervalo de frames entre lecturas de inventario.
    inventory_detect_interval: u32,
}

impl Vision {
    /// Crea un Vision cargando calibración y templates desde disco.
    /// `assets_dir` es la raíz del directorio de assets (p.ej. "assets/").
    /// Si la calibración no existe, se usa la default (no-op).
    pub fn load(assets_dir: &Path) -> Self {
        let cal_path = assets_dir.join("calibration.toml");
        let calibration = match Calibration::load(&cal_path) {
            Ok(c) => {
                if c.is_usable() {
                    tracing::info!("Calibración cargada desde '{}'", cal_path.display());
                } else {
                    warn!(
                        "calibration.toml cargado pero sin ROIs de HP/mana. \
                         Ejecuta `calibrate` para configurar."
                    );
                }
                c
            }
            Err(e) => {
                warn!("calibration.toml no disponible ({}). Visión deshabilitada.", e);
                Calibration::default()
            }
        };

        // Cargar templates de anclas.
        let anchors_dir = assets_dir.join("anchors");
        let mut tracker = AnchorTracker::new(AnchorConfig::default());
        for anchor_def in &calibration.anchors {
            let tpl_path = anchors_dir.join(&anchor_def.template_path);
            let template = match self::templates::load_png_gray(&tpl_path) {
                Ok(img) => {
                    tracing::info!("Anchor template cargado: '{}'", tpl_path.display());
                    Some(img)
                }
                Err(e) => {
                    warn!("No se pudo cargar anchor template '{}': {}", tpl_path.display(), e);
                    None
                }
            };
            tracker.add(anchor_def.clone(), template);
        }

        // Cargar templates de status icons.
        let status_dir = assets_dir.join("templates").join("status");
        let status_templates = TemplateStore::load_dir(&status_dir);

        // Cargar detector de prompts (npc_trade + login + char_select).
        let prompts_dir = assets_dir.join("templates").join("prompts");
        let mut prompts = PromptDetector::new(0.10);
        let mut prompt_rois = std::collections::HashMap::new();
        if let Some(r) = calibration.prompt_npc_trade   { prompt_rois.insert(PromptKind::NpcTrade,   r); }
        if let Some(r) = calibration.prompt_login       { prompt_rois.insert(PromptKind::Login,      r); }
        if let Some(r) = calibration.prompt_char_select { prompt_rois.insert(PromptKind::CharSelect, r); }
        prompts.load_from_dir(&prompts_dir, &prompt_rois);
        if prompts.is_loaded() {
            tracing::info!("PromptDetector: {} template(s) cargados", prompts.template_count());
        }

        // Cargar detector genérico de UI (depot chest, menús contextuales, etc).
        let ui_dir = assets_dir.join("templates").join("ui");
        let mut ui_detector = UiDetector::new(0.20);
        ui_detector.load_dir(&ui_dir, &calibration.ui_rois);
        if !ui_detector.is_empty() {
            tracing::info!("UiDetector: {} template(s) cargados", ui_detector.len());
        }

        // Cargar inventory reader (template matching en slots del backpack).
        // Prioridad: inventory_grid > inventory_slots manuales.
        let inventory_dir = assets_dir.join("templates").join("inventory");
        let mut inv_reader = inventory::InventoryReader::new();
        inv_reader.load_templates(&inventory_dir);
        // Cargar digit templates para OCR de stack counts (has_stack).
        // Si el directorio no existe o está vacío, has_stack cae a slot count.
        let digits_dir = assets_dir.join("templates").join("digits");
        let digits_loaded = inv_reader.load_digit_templates(&digits_dir);
        if digits_loaded > 0 {
            tracing::info!(
                "InventoryReader: {} digit templates cargados desde {}",
                digits_loaded, digits_dir.display()
            );
        } else {
            tracing::warn!(
                "InventoryReader: 0 digit templates en {} → has_stack() degradará a has_item() (1 unidad por slot)",
                digits_dir.display()
            );
        }
        // Prioridad: backpack_strip > inventory_grid > inventory_slots manuales.
        let slots = if let Some(strip) = calibration.inventory_backpack_strip {
            let expanded = strip.expand();
            tracing::info!(
                "InventoryReader: backpack strip → {} slots ({} backpacks × {} cols × {} rows @ {},{})",
                expanded.len(), strip.backpack_count, strip.slot_cols, strip.slot_rows, strip.x, strip.y
            );
            expanded
        } else if let Some(grid) = calibration.inventory_grid {
            let expanded = grid.expand();
            tracing::info!(
                "InventoryReader: grid auto-generado → {} slots ({}×{} @ {},{})",
                expanded.len(), grid.cols, grid.rows, grid.x, grid.y
            );
            expanded
        } else {
            calibration.inventory_slots.clone()
        };
        inv_reader.set_slots(slots.clone());
        let inventory_reader = if !inv_reader.is_empty() {
            tracing::info!("InventoryReader: habilitado ({} slots)", slots.len());
            Some(inv_reader)
        } else {
            None
        };

        Self {
            calibration,
            tracker,
            status_templates,
            prompts,
            battle_detector: BattleListDetector::new(),
            target_detector: TargetDetector::new(),
            ui_detector,
            prev_minimap: None,
            prev_is_moving: None,
            moving_hysteresis: false,
            calm_frame_count: 0,
            frame_count: 0,
            map_index: None,
            minimap_matcher: game_coords::MinimapMatcher::new(),
            coords_detect_interval: 15,
            ndi_tile_scale: 5,
            last_game_coords: None,
            last_hp_stable: None,
            last_mana_stable: None,
            bad_hp_frames: 0,
            bad_mana_frames: 0,
            inventory_reader,
            last_inventory_counts: std::collections::HashMap::new(),
            last_inventory_stacks: std::collections::HashMap::new(),
            inventory_detect_interval: 15,
        }
    }

    /// Carga el índice de tile-hashing para posicionamiento absoluto.
    /// Llamar después de `load()` si se configura `[game_coords]`.
    pub fn load_map_index(&mut self, cfg: &crate::config::GameCoordsConfig) {
        if cfg.map_index_path.is_empty() {
            return;
        }
        let path = std::path::PathBuf::from(&cfg.map_index_path);
        match game_coords::MapIndex::load(&path) {
            Ok(idx) => {
                tracing::info!(
                    "MapIndex cargado: {} patches desde '{}'",
                    idx.total_patches, path.display()
                );
                self.map_index = Some(idx);
            }
            Err(e) => {
                warn!("MapIndex no disponible ({}). Coords dHash deshabilitadas.", e);
            }
        }
        if let Some(interval) = cfg.detect_interval {
            self.coords_detect_interval = interval.max(1);
        }
        if cfg.ndi_tile_scale > 0 {
            self.ndi_tile_scale = cfg.ndi_tile_scale;
            tracing::info!("game_coords ndi_tile_scale = {}", self.ndi_tile_scale);
        }

        // ── MinimapMatcher (CCORR fallback) ────────────────────────────────
        // Carga reference PNGs en RAM para template matching cuando dHash falla.
        // Se guía por los mismos floors que el map_index.
        if !cfg.minimap_dir.is_empty() {
            let dir = std::path::PathBuf::from(&cfg.minimap_dir);
            let floors: Vec<i32> = cfg.matcher_floors
                .as_ref()
                .map(|s| s.split(',').filter_map(|f| f.trim().parse().ok()).collect())
                .unwrap_or_default();
            match self.minimap_matcher.load_dir(&dir, &floors) {
                Ok((n, mb)) => {
                    tracing::info!(
                        "MinimapMatcher: {} sectores cargados ({} MB RAM, floors={:?})",
                        n, mb,
                        if floors.is_empty() { "all".to_string() } else { format!("{:?}", floors) }
                    );
                }
                Err(e) => {
                    warn!("MinimapMatcher no disponible ({}). CCORR fallback deshabilitado.", e);
                }
            }
            if cfg.matcher_threshold > 0.0 {
                self.minimap_matcher.match_threshold = cfg.matcher_threshold;
                tracing::info!("MinimapMatcher threshold = {:.4}", cfg.matcher_threshold);
            }
        }
    }

    /// Retorna el centro del minimap en coordenadas del viewport, ajustado
    /// por el anchor tracker. Usado por el cavebot Node para calcular clicks.
    /// `None` si el minimap no está calibrado.
    pub fn minimap_center(&self) -> Option<(i32, i32)> {
        self.calibration.minimap.map(|roi| {
            let adj = self.tracker.adjust_roi(roi);
            ((adj.x + adj.w / 2) as i32, (adj.y + adj.h / 2) as i32)
        })
    }

    /// Procesa un frame y retorna la Perception resultante.
    /// Nunca falla — retorna Perception::default() si la calibración no está lista.
    pub fn tick(&mut self, frame: &Frame, frame_tick: u64) -> Perception {
        self.frame_count += 1;

        if !self.calibration.is_usable() {
            return Perception {
                frame_tick,
                captured_at: Some(frame.captured_at),
                ..Default::default()
            };
        }

        // Actualizar anclas (matching corre en thread de fondo).
        self.tracker.tick(frame);
        let valid_anchors = self.tracker.valid_anchor_count();
        let total_anchors = self.tracker.total_anchor_count();
        // Loggear estado de anclas solo 1 vez por segundo (cada 30 ticks a 30Hz).
        if self.frame_count.is_multiple_of(30) {
            if total_anchors > 0 && valid_anchors == 0 {
                warn!(
                    "Anclas: 0/{} válidos — ROIs sin ajuste de ventana. \
                     Si persiste, revisar score en logs o bajar max_score.",
                    total_anchors
                );
            } else if total_anchors > 0 {
                debug!(
                    "Anclas: {}/{} válidos, offset={:?}",
                    valid_anchors, total_anchors,
                    self.tracker.window_offset()
                );
            }
        }

        // Ajustar ROIs con el offset de la ventana.
        let hp_roi   = self.calibration.hp_bar.map(|r| self.tracker.adjust_roi(r));
        let mana_roi = self.calibration.mana_bar.map(|r| self.tracker.adjust_roi(r));

        // Leer vitales con debouncing F1.2: filtra transitorios (frames
        // donde el reader retorna None o ratio=0 momentáneamente por
        // overlay/animación/UI flash). Solo propaga el "bad read" después
        // de N frames consecutivos.
        const VITALS_PANIC_FRAMES: u32 = 5; // ~150ms a 30Hz

        let raw_hp = hp_roi.and_then(|r| read_hp_by_edge(frame, r));
        let hp_is_bad = raw_hp.as_ref().map(|b| b.ratio < 0.001).unwrap_or(true);
        let hp_final = if hp_is_bad {
            self.bad_hp_frames += 1;
            if self.bad_hp_frames >= VITALS_PANIC_FRAMES {
                // Persistent bad read → confiar en el valor real (dead/screen change)
                raw_hp
            } else {
                // Transient noise → usar el último valor estable
                self.last_hp_stable.clone()
            }
        } else {
            self.bad_hp_frames = 0;
            self.last_hp_stable = raw_hp.clone();
            raw_hp
        };

        let raw_mana = mana_roi.and_then(|r| read_mana_by_edge(frame, r));
        let mana_is_bad = raw_mana.is_none();
        let mana_final = if mana_is_bad {
            self.bad_mana_frames += 1;
            if self.bad_mana_frames >= VITALS_PANIC_FRAMES {
                raw_mana
            } else {
                self.last_mana_stable.clone()
            }
        } else {
            self.bad_mana_frames = 0;
            self.last_mana_stable = raw_mana.clone();
            raw_mana
        };

        let vitals = CharVitals {
            hp:   hp_final,
            mana: mana_final,
        };

        // Leer battle list (stateful con histéresis).
        let battle = if let Some(roi) = self.calibration.battle_list.map(|r| self.tracker.adjust_roi(r)) {
            self.battle_detector.read(frame, roi)
        } else {
            Default::default()
        };

        // Leer status icons.
        let conditions = if let Some(roi) = self.calibration.status_icons.map(|r| self.tracker.adjust_roi(r)) {
            self::status_icons::read_status_icons(frame, roi, &self.status_templates)
        } else {
            Default::default()
        };

        // Capturar minimapa y calcular diff de movimiento.
        let minimap = self.calibration.minimap
            .map(|r| self.tracker.adjust_roi(r))
            .and_then(|r| self::minimap::capture_minimap(frame, r));

        let minimap_diff = match (&self.prev_minimap, &minimap) {
            (Some(prev), Some(curr)) => self::minimap::diff_l1(prev, curr),
            _ => 0.0,
        };
        // Histéresis de movimiento: activar inmediato, desactivar tras N frames calm.
        // None si no hay minimap calibrado — el stuck detector del cavebot
        // debe ignorar este campo cuando es None para no cortar walk steps.
        let is_moving: Option<bool> = if self.calibration.minimap.is_some() {
            let raw_moving = minimap_diff > MOVEMENT_DIFF_THRESHOLD;
            if raw_moving {
                self.moving_hysteresis = true;
                self.calm_frame_count = 0;
            } else if self.moving_hysteresis {
                self.calm_frame_count += 1;
                if self.calm_frame_count >= MOVEMENT_CALM_FRAMES {
                    self.moving_hysteresis = false;
                    self.calm_frame_count = 0;
                }
            }
            Some(self.moving_hysteresis)
        } else {
            None
        };
        // Log de transiciones a nivel INFO para diagnóstico sin RUST_LOG=debug.
        if is_moving != self.prev_is_moving && is_moving.is_some() {
            info!(
                "minimap: is_moving {:?} → {:?} (diff={:.6}, threshold={}, calm={})",
                self.prev_is_moving, is_moving, minimap_diff,
                MOVEMENT_DIFF_THRESHOLD, self.calm_frame_count
            );
        }
        self.prev_is_moving = is_moving;
        // Log periódico del diff a DEBUG para calibración fina.
        if self.frame_count % 30 == 0 && minimap_diff > 0.0 {
            debug!(
                "minimap_diff={:.6}, is_moving={:?}, threshold={}",
                minimap_diff, is_moving, MOVEMENT_DIFF_THRESHOLD
            );
        }
        // Cross-correlation: desplazamiento en píxeles del minimap.
        let minimap_displacement = match (&self.prev_minimap, &minimap) {
            (Some(prev), Some(curr)) => self::minimap::displacement(prev, curr),
            _ => None,
        };
        if let Some((dx, dy)) = minimap_displacement {
            info!("minimap_displacement: ({}, {}) px", dx, dy);
        }
        // Rotar el snapshot para el próximo tick.
        self.prev_minimap = minimap.clone();

        // Loot sparkles — área 3×3 tiles centrada en el char. Los corpses
        // con loot muestran un anillo de píxeles blancos que persiste hasta
        // ser looteado. Mucho más fiable que contar kills.
        let loot_sparkles = if let Some(vp) = self.calibration.game_viewport {
            let adjusted_vp = self.tracker.adjust_roi(vp);
            if let Some(area) = self::loot::compute_loot_area(adjusted_vp, 64) {
                self::loot::count_sparkle_pixels(frame, area)
            } else {
                0
            }
        } else {
            0
        };

        // Target info bar: señal binaria "char tiene target".
        //
        // Dos fuentes posibles, en orden de prioridad:
        //
        // 1. **ROI `target_hp_bar` calibrada** (Fase A original): lee la
        //    barra de HP del target encima del viewport. Preciso pero
        //    requiere calibración del usuario con GIMP.
        //
        // 2. **`BattleList::has_attacked_entry()`** (fix post-audit TibiaPilotNG):
        //    deriva target_active del `is_being_attacked` per-slot del battle
        //    list. No requiere ROI nueva — reutiliza el scan que ya hacemos.
        //    Funciona con cualquier cliente Tibia que pinte el highlight rojo
        //    en los slots atacados.
        //
        // La fuente (1) gana si está configurada; (2) es fallback transparente.
        // Si ninguna aplica (battle list vacío + no target ROI), `target_active`
        // queda en None y el FSM usa su fallback legacy de keepalive.
        let (target_active, target_hits) = if let Some(roi) = self.calibration.target_hp_bar
            .map(|r| self.tracker.adjust_roi(r))
        {
            // Fuente 1: target_hp_bar ROI calibrada.
            match self.target_detector.read(frame, roi) {
                Some(r) => (Some(r.active), r.hits),
                None    => (None, 0),
            }
        } else if battle.has_enemies() {
            // Fuente 2: derivar del battle list — zero calibration required.
            (Some(battle.has_attacked_entry()), 0)
        } else {
            (None, 0)
        };

        // Detectar elementos de UI genéricos (depot chest, stow menu, etc).
        // tick() es no-bloqueante: envía parches al background thread y drena
        // resultados. last_matches() retorna el resultado del último job completado
        // (puede ser hasta ~500ms antiguo, aceptable para cambios de UI lentos).
        self.ui_detector.tick(frame);
        let ui_matches = self.ui_detector.last_matches().to_vec();

        // Tile-hashing: detectar coordenadas absolutas cada N frames.
        // Estrategia de 2 pasos:
        //   1. dHash (rápido, O(1) con MapIndex): intentar primero.
        //   2. CCORR fallback (más lento pero robusto) si dHash falla.
        //
        // Cuando hay un `last_game_coords` reciente, el CCORR matcher solo
        // matchea contra el sector actual + 8 vecinos (~50-100ms). Sin last
        // known, brute force (~1-2 seg) usado solo 1 vez en boot.
        if self.frame_count % self.coords_detect_interval as u64 == 0 {
            if let Some(ref snap) = minimap {
                // Step 1: dHash
                let mut detected = None;
                if let Some(ref idx) = self.map_index {
                    detected = game_coords::detect_position(snap, idx, self.ndi_tile_scale);
                }
                // Step 2: CCORR fallback
                if detected.is_none() && !self.minimap_matcher.is_empty() {
                    let t0 = std::time::Instant::now();
                    detected = self.minimap_matcher.detect(
                        snap,
                        self.ndi_tile_scale,
                        self.last_game_coords,
                    );
                    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    // Log en info solo el primer brute force (slow path). Los
                    // subsiguientes narrow searches son debug para evitar spam.
                    if self.last_game_coords.is_none() {
                        tracing::info!(
                            "MinimapMatcher brute-force frame={} → {:?} ({:.1}ms)",
                            self.frame_count, detected, elapsed_ms
                        );
                    } else {
                        tracing::debug!(
                            "MinimapMatcher narrow frame={} last={:?} → {:?} ({:.1}ms)",
                            self.frame_count, self.last_game_coords, detected, elapsed_ms
                        );
                    }
                }
                if detected.is_some() {
                    self.last_game_coords = detected;
                }
            }
        }
        let game_coords = self.last_game_coords;

        // Inventory: contar items + leer stack counts via OCR cada N frames.
        if let Some(ref reader) = self.inventory_reader {
            if self.frame_count % self.inventory_detect_interval as u64 == 0 {
                let reading = reader.read_with_stacks(frame);
                self.last_inventory_counts = reading.slot_counts;
                self.last_inventory_stacks = reading.stack_totals;
            }
        }
        let inventory_counts = self.last_inventory_counts.clone();
        let inventory_stacks = self.last_inventory_stacks.clone();

        Perception {
            vitals,
            battle,
            conditions,
            minimap,
            target_active,
            target_hits,
            loot_sparkles,
            ui_matches,
            captured_at: Some(frame.captured_at),
            frame_tick,
            minimap_diff,
            is_moving,
            minimap_displacement,
            game_coords,
            inventory_counts,
            inventory_stacks,
        }
    }

    pub fn is_calibrated(&self) -> bool {
        self.calibration.is_usable()
    }
}
