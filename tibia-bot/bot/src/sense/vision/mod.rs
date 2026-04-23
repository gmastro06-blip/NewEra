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
pub mod inventory_ml;
pub mod inventory_ocr;
pub mod inventory_slot;
pub mod minimap;
pub mod prompts;
pub mod region_monitor;
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

// MOVEMENT_CALM_FRAMES y VITALS_PANIC_FRAMES se movieron a `sense::filter`.
// El debouncing temporal vive ahora en `PerceptionFilter` aplicado por
// `BotLoop` entre Vision::tick y FSM. Esto deja Vision emitiendo señales
// raw verdaderas y centraliza la política temporal en un solo módulo.

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
    /// Número de frame procesado.
    frame_count:     u64,
    /// Template matcher (SSDNormalized) para posicionamiento absoluto.
    /// Reemplaza el dHash legacy que era demasiado frágil para Tibia 12.
    minimap_matcher: game_coords::MinimapMatcher,
    /// Counter para re-validación periódica del matcher. Incrementa cada
    /// detection (cada `coords_detect_interval` frames). Cuando excede
    /// `COORDS_REVALIDATE_INTERVAL`, la próxima detección usa brute force
    /// full en lugar de narrow — evita "stuck in false positive".
    coords_detects_since_full_search: u32,
    /// Último game_coords observado (para detectar cambio entre ticks).
    prev_game_coords: Option<(i32, i32, i32)>,
    /// Ticks desde el último cambio de game_coords. Crece si el char NO
    /// se mueve O si el matcher está stuck. Combinado con is_moving permite
    /// distinguir ambos casos (ver `is_game_coords_stale_while_moving`).
    game_coords_stale_ticks: u32,
    /// Flag que se setea una vez cuando detectamos "coords stale mientras
    /// is_moving=true" por >N ticks. Evita log spam (una sola alerta por
    /// incidente, se clearea cuando vuelve a moverse).
    reported_coords_stale: bool,
    /// Acumulador de displacement del minimap en pixels, usado para actualizar
    /// `last_game_coords` incrementalmente entre template matches.
    /// El template match corre cada 500ms (lento, ~80-160ms) mientras que el
    /// displacement frame-a-frame es barato (~1ms). Sumando los displacements
    /// entre matches, conseguimos tracking tile-perfect en tiempo real sin
    /// esperar al próximo template match.
    tracked_sub_tile_px: (i32, i32),
    /// Intervalo de frames entre detecciones de coords (default 15).
    coords_detect_interval: u32,
    /// Pixels por tile en el minimap NDI (default 5). Usado para downsamplear
    /// los patches antes del hash para matchear la escala del index.
    ndi_tile_scale: u32,
    /// Último resultado de detección de coords (cacheado entre intervalos).
    last_game_coords: Option<(i32, i32, i32)>,
    /// Reader de inventario (opcional).
    inventory_reader: Option<inventory::InventoryReader>,
    /// Último conteo de items por template (cacheado entre intervalos).
    last_inventory_counts: std::collections::HashMap<String, u32>,
    /// Último snapshot per-slot (item #2 plan robustez). Cacheado entre
    /// cadencias para propagar a Perception.inventory_slots sin re-run.
    last_inventory_slots: Vec<inventory_slot::SlotReading>,
    /// Última suma de unidades por item via OCR del stack count (M1).
    last_inventory_stacks: std::collections::HashMap<String, u32>,
    /// Intervalo de frames entre lecturas de inventario.
    /// Configurable via `[inventory].detect_interval_ticks`.
    inventory_detect_interval: u32,
    /// Stage A threshold (luma stddev). Configurable via
    /// `[inventory].empty_stddev_max`. Override del const
    /// `EMPTY_STDDEV_MAX` para tuning sin recompile.
    inventory_empty_stddev_max: f32,
    /// Classifier ML opcional (Fase 2.5). Cargado via `load_ml_model()` si
    /// `config.ml.use_ml=true`. Cuando Some Y `is_ready()`, el inventory
    /// reader delega primero a ML; si infer_slot devuelve None, fallback SSE.
    /// Sin feature `ml-runtime`, el reader devuelve None siempre (scaffold).
    ml_reader: Option<inventory_ml::MlInventoryReader>,
    /// Costo µs del último tick por reader, indexado por
    /// `crate::instrumentation::ReaderId as usize`. u16 cubre hasta 65 ms
    /// por reader — si uno se acerca, satura (y deberías investigar).
    /// Reseteado a [0; 12] al inicio de cada tick. Readers skippeados por
    /// cadencia quedan en 0 — el consumer interpreta 0 como "no corrió".
    last_reader_costs: [u16; crate::instrumentation::ReaderId::COUNT],
}

/// Macro para envolver un bloque de reader con timing. Escribe el costo µs
/// (saturado a u16::MAX) en `$self.last_reader_costs[$id as usize]` y
/// devuelve el valor del bloque sin modificarlo.
///
/// Borrow-safe: `__t` (Instant) no toca self; el body se evalúa primero
/// (puede tomar &mut self libremente); el assignment a last_reader_costs
/// ocurre cuando el body terminó y su borrow se liberó.
#[allow(unused_macros)]
macro_rules! timed_reader {
    ($self:ident, $id:expr, $body:block) => {{
        let __t = std::time::Instant::now();
        let __r = $body;
        $self.last_reader_costs[$id as usize] =
            __t.elapsed().as_micros().min(u16::MAX as u128) as u16;
        __r
    }};
}

impl Vision {
    /// Devuelve los slots de inventory configurados (para consumers externos
    /// como `DatasetRecorder`). Vacío si no hay inventory_reader.
    pub fn inventory_slots(&self) -> Vec<calibration::RoiDef> {
        self.inventory_reader
            .as_ref()
            .map(|r| r.slots())
            .unwrap_or_default()
    }

    /// Cantidad de anchors válidos (con match fresco) sobre el total
    /// configurado. Expuesto para instrumentación / health system.
    pub fn tracker_valid_count(&self) -> usize { self.tracker.valid_anchor_count() }
    pub fn tracker_total_count(&self) -> usize { self.tracker.total_anchor_count() }

    /// Carga el classifier ML (si `ml_config.use_ml=true` y los paths son válidos).
    /// Con feature `ml-runtime` activa, crea ort::Session; sin feature, scaffold
    /// que devuelve None en infer_slot (fallback SSE preservado).
    /// Llamar DESPUÉS de `Vision::load()` desde main.rs.
    pub fn load_ml_model(&mut self, ml_config: &crate::config::MlConfig) {
        if !ml_config.use_ml {
            return;
        }
        if ml_config.model_path.is_empty() || ml_config.classes_path.is_empty() {
            tracing::warn!("Vision::load_ml_model: use_ml=true pero model_path o classes_path vacíos");
            return;
        }
        let reader = inventory_ml::MlInventoryReader::load(
            Path::new(&ml_config.model_path),
            Path::new(&ml_config.classes_path),
            ml_config.confidence_threshold,
        );
        if reader.is_ready() {
            self.ml_reader = Some(reader);
        }
    }

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
            frame_count: 0,
            minimap_matcher: game_coords::MinimapMatcher::new(),
            coords_detects_since_full_search: 0,
            prev_game_coords: None,
            game_coords_stale_ticks: 0,
            reported_coords_stale: false,
            tracked_sub_tile_px: (0, 0),
            coords_detect_interval: 15,
            ndi_tile_scale: 5,
            last_game_coords: None,
            inventory_reader,
            last_inventory_counts: std::collections::HashMap::new(),
            last_inventory_slots:  Vec::new(),
            last_inventory_stacks: std::collections::HashMap::new(),
            inventory_detect_interval: 15,
            inventory_empty_stddev_max: inventory::EMPTY_STDDEV_MAX,
            ml_reader: None,  // populate via load_ml_model()
            last_reader_costs: [0; crate::instrumentation::ReaderId::COUNT],
        }
    }

    /// Costos en µs por reader durante el último `tick()`. Indexado por
    /// `crate::instrumentation::ReaderId as usize`. Readers que no
    /// corrieron este tick (por cadencia) quedan en 0.
    /// Llamado por BotLoop tras cada `vision.tick()` para poblar
    /// `TickMetrics.vision_per_reader_us`.
    pub fn last_reader_costs(&self) -> [u16; crate::instrumentation::ReaderId::COUNT] {
        self.last_reader_costs
    }

    /// Carga configuración de game_coords: ndi_tile_scale, detect_interval,
    /// y el MinimapMatcher (template matching SSDNormalized).
    ///
    /// Llamar después de `load()` si se configura `[game_coords]`.
    ///
    /// Nota 2026-04-15: el `map_index_path` (dHash precomputado) se ignora
    /// porque dHash es demasiado frágil al anti-aliasing del cliente Tibia 12.
    /// El archivo sigue soportado por `build_map_index` bin pero no se
    /// consume en runtime. Ver PLAN.md Phase B.2 para el rationale completo.
    /// Aplica config `[inventory]` a los parámetros del reader (item #7 plan
    /// robustez). Llamar después de `Vision::load()` desde main.rs. Rango
    /// válido: detect_interval_ticks ∈ [1, 300]; empty_stddev_max ∈ [0.0, 100.0].
    /// Valores fuera rango caen a default con warn log.
    pub fn apply_inventory_config(&mut self, cfg: &crate::config::InventoryConfig) {
        let interval = cfg.detect_interval_ticks;
        if (1..=300).contains(&interval) {
            self.inventory_detect_interval = interval;
        } else {
            tracing::warn!(
                "inventory.detect_interval_ticks={} fuera de [1, 300], usando default 15",
                interval
            );
        }
        let stddev = cfg.empty_stddev_max;
        if stddev.is_finite() && (0.0..=100.0).contains(&stddev) {
            self.inventory_empty_stddev_max = stddev;
            if let Some(ref mut reader) = self.inventory_reader {
                reader.set_empty_stddev_max(stddev);
            }
        } else {
            tracing::warn!(
                "inventory.empty_stddev_max={} inválido, usando default {}",
                stddev, inventory::EMPTY_STDDEV_MAX
            );
        }
        tracing::info!(
            "Inventory config: detect_interval={} ticks, empty_stddev_max={:.1}",
            self.inventory_detect_interval, self.inventory_empty_stddev_max
        );
    }

    pub fn load_map_index(&mut self, cfg: &crate::config::GameCoordsConfig) {
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
            // Disambiguation: log explícitamente ON/OFF al boot para que
            // sea fácil correlacionar comportamiento en sesiones live.
            self.minimap_matcher.disambiguation_enabled = cfg.disambiguation_enabled;
            if cfg.disambiguation_enabled {
                tracing::info!(
                    "MinimapMatcher disambiguation = ON (segundo patch de esquina opuesta \
                     valida top-K candidates; false positives se rechazan con None)"
                );
            } else {
                tracing::info!(
                    "MinimapMatcher disambiguation = OFF (comportamiento legacy: top-1 ganador \
                     sin segunda verificación)"
                );
            }
        }

        // ── Boot-time seed ─────────────────────────────────────────────────
        // Si el usuario configuró `starting_coord = [X, Y, Z]`, lo usamos
        // como semilla inicial de `last_game_coords`. El primer detect()
        // hará narrow search desde ese sector (+ 8 vecinos), evitando el
        // false positive global del cold boot. Validado 2026-04-17 live
        // en Ab'dendriel.
        if let Some([x, y, z]) = cfg.starting_coord {
            self.last_game_coords = Some((x, y, z));
            tracing::info!(
                "game_coords starting seed = ({}, {}, {}) — first detect() usará narrow \
                 search para evitar false positives de cold boot",
                x, y, z
            );
        } else if !cfg.minimap_dir.is_empty() {
            tracing::info!(
                "game_coords sin starting_coord — el primer detect() hará full brute force. \
                 Si el char está en un sector con patrón visual común (plazas, depots), \
                 considerar agregar `starting_coord = [X, Y, Z]` al config."
            );
        }
    }

    /// Retorna un snapshot de las stats del MinimapMatcher.
    /// Safe de llamar desde cualquier contexto (usa atomic loads internamente).
    pub fn matcher_stats(&self) -> game_coords::MatcherStatsSnapshot {
        self.minimap_matcher.stats_snapshot()
    }

    /// Mut accessor al MinimapMatcher para integration tests (inyectar
    /// reference sectors sin pasar por disk).
    ///
    /// # Safety
    /// Solo debe usarse desde tests. No hay invariantes que romper (matcher
    /// es simplemente un atlas de sectores), pero modificar durante operación
    /// normal puede causar inconsistencias de stats.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn matcher_mut_for_test(&mut self) -> &mut game_coords::MinimapMatcher {
        &mut self.minimap_matcher
    }

    /// Inyecta last_game_coords directamente, para tests de integración que
    /// necesitan bootstrap sin pasar por detect().
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn set_last_game_coords_for_test(&mut self, coords: Option<(i32, i32, i32)>) {
        self.last_game_coords = coords;
        self.tracked_sub_tile_px = (0, 0);
    }

    /// Read accessor a `last_game_coords` para tests de integración que
    /// validan el bootstrap-seed del matcher al aplicar config.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn last_game_coords_for_test(&self) -> Option<(i32, i32, i32)> {
        self.last_game_coords
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

    /// Activa/desactiva templates OnDemand del UiDetector según el step actual
    /// del cavebot. Llamar ANTES de `tick()` cada iteración del game loop.
    ///
    /// `desired` típicamente viene de `Cavebot::required_ui_templates()`. Sin
    /// esta llamada los templates OnDemand nunca se procesan → features como
    /// StepVerify sobre `stow_menu` / `depot_chest` no funcionan. Con la
    /// llamada, solo se procesan los templates relevantes al step en curso,
    /// reduciendo el cycle time del bg_worker de ~23s a <50ms en steady state
    /// (bench baseline 2026-04-18).
    pub fn set_ui_demand(&mut self, desired: &[&str]) {
        self.ui_detector.sync_on_demand(desired);
    }

    /// Procesa un frame y retorna la Perception resultante.
    /// Nunca falla — retorna Perception::default() si la calibración no está lista.
    pub fn tick(&mut self, frame: &Frame, frame_tick: u64) -> Perception {
        self.frame_count += 1;

        // Reset per-reader cost array. Readers que no corran este tick
        // (skip por cadencia) quedan en 0, semánticamente "no ejecutado".
        self.last_reader_costs = [0; crate::instrumentation::ReaderId::COUNT];

        if !self.calibration.is_usable() {
            return Perception {
                frame_tick,
                captured_at: Some(frame.captured_at),
                ..Default::default()
            };
        }

        // Actualizar anclas (matching corre en thread de fondo).
        timed_reader!(self, crate::instrumentation::ReaderId::Anchors, {
            self.tracker.tick(frame)
        });
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

        // Leer vitales RAW. El debouncing temporal (panic frames + EMA) lo
        // aplica `PerceptionFilter` aguas abajo en el game loop. Vision aquí
        // solo lee la barra y emite lo que vio; cualquier overlay transient
        // se refleja en el raw — el filter decide si suavizarlo.
        let vitals = timed_reader!(self, crate::instrumentation::ReaderId::HpMana, {
            let raw_hp   = hp_roi.and_then(|r| read_hp_by_edge(frame, r));
            let raw_mana = mana_roi.and_then(|r| read_mana_by_edge(frame, r));
            CharVitals { hp: raw_hp, mana: raw_mana }
        });

        // Leer battle list (stateful con histéresis).
        //
        // 2026-04-18: GATE por NPC greeting window. Cuando el greeting
        // está abierto, ocluye el mismo espacio que usaría el battle list.
        // El chat NPC + portrait + texto colored crean false positives
        // (392-1900 hits por row) que MAX_HP_BAR_PX (700) no filtra todos.
        // Solución: detectar greeting window via match_now SYNC (barato,
        // template 34x34, ROI 180x100, <5ms) y skippear battle list si
        // greeting visible — real battle list no puede coexistir con
        // NPC dialog en este espacio.
        // Battle reader incluye el greeting check sync (gate semántico de la
        // misma ROI) — los cuentamos juntos porque comparten área de pantalla
        // y el greeting es un fallback gating del battle, no un reader aparte.
        let battle = timed_reader!(self, crate::instrumentation::ReaderId::Battle, {
            let greeting_window_open = self.ui_detector
                .match_now(frame, "npc_trade_bag")
                .is_some();
            if greeting_window_open {
                // Greeting abierto → sin battle list real posible.
                Default::default()
            } else if let Some(roi) = self.calibration.battle_list.map(|r| self.tracker.adjust_roi(r)) {
                self.battle_detector.read(frame, roi)
            } else {
                Default::default()
            }
        });

        // Leer status icons.
        let conditions = timed_reader!(self, crate::instrumentation::ReaderId::Status, {
            if let Some(roi) = self.calibration.status_icons.map(|r| self.tracker.adjust_roi(r)) {
                self::status_icons::read_status_icons(frame, roi, &self.status_templates)
            } else {
                Default::default()
            }
        });

        // Bloque minimap completo: capture + diff_l1 + displacement.
        // Cross-correlation domina (~5 ms en debug). is_moving derivado
        // es ns y se incluye dentro del scope para coherencia semántica.
        let (minimap, minimap_diff, is_moving, minimap_displacement) =
        timed_reader!(self, crate::instrumentation::ReaderId::Minimap, {
            let minimap = self.calibration.minimap
                .map(|r| self.tracker.adjust_roi(r))
                .and_then(|r| self::minimap::capture_minimap(frame, r));
            let minimap_diff = match (&self.prev_minimap, &minimap) {
                (Some(prev), Some(curr)) => self::minimap::diff_l1(prev, curr),
                _ => 0.0,
            };
            let is_moving: Option<bool> = if self.calibration.minimap.is_some() {
                Some(minimap_diff > MOVEMENT_DIFF_THRESHOLD)
            } else {
                None
            };
            let minimap_displacement = match (&self.prev_minimap, &minimap) {
                (Some(prev), Some(curr)) => self::minimap::displacement(prev, curr),
                _ => None,
            };
            (minimap, minimap_diff, is_moving, minimap_displacement)
        });
        // Logs periódicos fuera del timing — son I/O, no medición de visión.
        if self.frame_count % 30 == 0 && minimap_diff > 0.0 {
            debug!(
                "minimap_diff={:.6}, is_moving_raw={:?}, threshold={}",
                minimap_diff, is_moving, MOVEMENT_DIFF_THRESHOLD
            );
        }
        if let Some((dx, dy)) = minimap_displacement {
            info!("minimap_displacement: ({}, {}) px", dx, dy);
        }
        // Rotar el snapshot para el próximo tick.
        self.prev_minimap = minimap.clone();

        // Loot sparkles — área 3×3 tiles centrada en el char. Los corpses
        // con loot muestran un anillo de píxeles blancos que persiste hasta
        // ser looteado. Mucho más fiable que contar kills.
        let loot_sparkles = timed_reader!(self, crate::instrumentation::ReaderId::Loot, {
            if let Some(vp) = self.calibration.game_viewport {
                let adjusted_vp = self.tracker.adjust_roi(vp);
                if let Some(area) = self::loot::compute_loot_area(adjusted_vp, 64) {
                    self::loot::count_sparkle_pixels(frame, area)
                } else {
                    0
                }
            } else {
                0
            }
        });

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
        let (target_active, target_hits) = timed_reader!(self, crate::instrumentation::ReaderId::Target, {
            if let Some(roi) = self.calibration.target_hp_bar
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
            }
        });

        // Detectar elementos de UI genéricos (depot chest, stow menu, etc).
        // tick() es no-bloqueante: envía parches al background thread y drena
        // resultados. last_matches() retorna el resultado del último job completado
        // (puede ser hasta ~500ms antiguo, aceptable para cambios de UI lentos).
        let (ui_matches, ui_match_infos) = timed_reader!(self, crate::instrumentation::ReaderId::UiMatch, {
            self.ui_detector.tick(frame);
            let mut ui_matches = self.ui_detector.last_matches();
            let mut ui_match_infos: std::collections::HashMap<String, (u32, u32, u32, u32)> =
                self.ui_detector.last_matches_map().into_iter()
                    .map(|(name, info)| (
                        name,
                        (info.center_x, info.center_y, info.template_w, info.template_h)
                    ))
                    .collect();
            // ── SYNC match override para templates CRÍTICOS ──────────────────
            // 2026-04-18: el async cache puede tener datos stale (hasta STICKY_TTL)
            // cuando el main loop bloquea por MinimapMatcher full search (4s). Para
            // templates chicos (ROI <200x200, template <50x50) donde match_now cuesta
            // <5ms, corremos SYNC y overrideamos el cache.
            const SYNC_PRIORITY_TEMPLATES: &[&str] = &["npc_trade_bag"];
            for name in SYNC_PRIORITY_TEMPLATES {
                if let Some(m) = self.ui_detector.match_now(frame, name) {
                    ui_match_infos.insert(
                        name.to_string(),
                        (m.x + m.w / 2, m.y + m.h / 2, m.w, m.h),
                    );
                    if !ui_matches.iter().any(|n| n == name) {
                        ui_matches.push(name.to_string());
                    }
                } else {
                    ui_match_infos.remove(*name);
                    ui_matches.retain(|n| n != name);
                }
            }
            (ui_matches, ui_match_infos)
        });

        // Tile-hashing: detectar coordenadas absolutas cada N frames.
        //
        // Primary: MinimapMatcher (SSDNormalized template matching). Robusto
        // al anti-aliasing del cliente Tibia 12. Narrow search (~80-160ms)
        // después del primer detect, brute force (~3-4s) solo en cold boot
        // o cada COORDS_REVALIDATE_INTERVAL detecciones para anti-stuck.
        //
        // Nota 2026-04-15: el step 1 dHash (MapIndex::lookup) fue removido
        // del hot path porque NUNCA matcheaba en live (min hamming 14-20 bits
        // vs threshold 3, causado por anti-aliasing).
        //
        // Re-validación periódica (anti stuck-in-false-positive):
        //   Cada COORDS_REVALIDATE_INTERVAL detecciones, Vision fuerza un
        //   brute force full sobre todos los floors. Esto recupera casos
        //   donde el narrow search se quedó pegado a un falso positivo
        //   (ej cold start con char en login screen, transición de piso).
        //
        // Cadencia: detect_interval=15 (30Hz) → 500ms entre detects.
        // REVALIDATE=30 → ~15 seg entre re-validations.
        // Cost: 1 tick spike de ~2-4s cada 15s. Acceptable en debug,
        // pendiente mover a background thread para prod.
        const COORDS_REVALIDATE_INTERVAL: u32 = 30;

        // ── TRACKING HÍBRIDO game_coords ────────────────────────────────────
        //
        // Arquitectura en 2 niveles:
        //   1. **Template matching** (cada `coords_detect_interval` frames =
        //      500ms @ 30Hz): MinimapMatcher establece ground truth absoluta.
        //      Lento (~80-160ms narrow, ~3-4s full) pero preciso hasta 1 tile.
        //   2. **Displacement incremental** (cada frame = 33ms): acumula el
        //      shift del minimap frame-a-frame en pixels. Convierte a tiles
        //      cuando acumula >= ndi_tile_scale pixels en algún axis.
        //
        // Esto permite tracking tile-perfect en tiempo real SIN esperar al
        // próximo template match. Antes del fix, el matcher siempre daba el
        // mismo coord durante ~15s porque el best SSD global no cambiaba con
        // shifts de 1-2 tiles (patches casi idénticos). El cavebot clickeaba
        // al mismo pixel del minimap y el char caminaba sin que el bot
        // supiera que había llegado.
        //
        // El displacement corrige esto: cada frame el minimap se desplaza al
        // mover el char, y acumulamos el delta hasta completar 1 tile,
        // actualizando last_game_coords inmediatamente.
        //
        // Cuando llega un template match fresh, trusteamos absolutamente el
        // nuevo coord (ground truth) y reseteamos el acumulador.

        let game_coords = timed_reader!(self, crate::instrumentation::ReaderId::GameCoords, {
        // PASO 1: actualizar coord incrementalmente con displacement (cada frame).
        if let (Some(last_coord), Some(disp_px)) =
            (self.last_game_coords, minimap_displacement)
        {
            // Ignorar displacements triviales (ruido)
            if disp_px.0 != 0 || disp_px.1 != 0 {
                let (new_coord, new_accum) = game_coords::apply_displacement(
                    last_coord,
                    self.tracked_sub_tile_px,
                    disp_px,
                    self.ndi_tile_scale,
                );
                if new_coord != last_coord {
                    tracing::debug!(
                        "game_coords tracked: {:?} → {:?} via displacement ({}, {})",
                        last_coord, new_coord, disp_px.0, disp_px.1
                    );
                    self.last_game_coords = Some(new_coord);
                }
                self.tracked_sub_tile_px = new_accum;
            }
        }

        // PASO 2: template match periódico (ground truth), override displacement tracking.
        if self.frame_count % self.coords_detect_interval as u64 == 0 {
            if let Some(ref snap) = minimap {
                if !self.minimap_matcher.is_empty() {
                    let force_full = self.last_game_coords.is_none()
                        || self.coords_detects_since_full_search >= COORDS_REVALIDATE_INTERVAL;
                    let t0 = std::time::Instant::now();
                    let detected = self.minimap_matcher.detect(
                        snap,
                        self.ndi_tile_scale,
                        self.last_game_coords,
                        force_full,
                    );
                    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    if force_full {
                        // Reset counter. Log en info (los full searches son raros).
                        self.coords_detects_since_full_search = 0;
                        tracing::info!(
                            "MinimapMatcher full frame={} last={:?} → {:?} ({:.1}ms)",
                            self.frame_count, self.last_game_coords, detected, elapsed_ms
                        );
                    } else {
                        self.coords_detects_since_full_search += 1;
                        tracing::debug!(
                            "MinimapMatcher narrow frame={} last={:?} → {:?} ({:.1}ms)",
                            self.frame_count, self.last_game_coords, detected, elapsed_ms
                        );
                    }
                    // Physical-motion sanity filter (ver game_coords::validate_jump).
                    let detected_filtered = game_coords::validate_jump(
                        self.last_game_coords,
                        detected,
                        force_full,
                    );
                    if detected_filtered.is_none() && detected.is_some() && !force_full {
                        if let (Some(d), Some(l)) = (detected, self.last_game_coords) {
                            tracing::warn!(
                                "MinimapMatcher rejected jump: last={:?} detected={:?} \
                                 (|dx|={}, |dy|={} > {}tiles/500ms). Probable false positive.",
                                l, d,
                                (d.0 - l.0).abs(), (d.1 - l.1).abs(),
                                game_coords::MAX_JUMP_PER_DETECT
                            );
                        }
                    }
                    if detected_filtered.is_some() {
                        // Ground truth: override displacement tracking.
                        self.last_game_coords = detected_filtered;
                        self.tracked_sub_tile_px = (0, 0);
                    }
                }
            }
        }
        self.last_game_coords
        }); // end timed_reader GameCoords

        // ── Stuck detection: game_coords stale + char intentando caminar ─
        //
        // Si el char está en combate, paused, o parado, es normal que
        // game_coords no cambie. PERO si is_moving=true (minimap viene
        // shifting = el char camina) AND game_coords NO actualiza por N
        // segundos, hay un problema: matcher stuck en false positive, o el
        // char está caminando contra una pared, o el path está bloqueado.
        //
        // Threshold: 1800 ticks = 60 seg a 30 Hz. Lo suficientemente largo
        // para absorber pausas normales de combate + transiciones de piso.
        //
        // Side-effect: log warn una sola vez por incidente (reset al
        // recuperarse). NO fuerza safety pause — es informativo solo.
        const COORDS_STALE_THRESHOLD_TICKS: u32 = 1800;

        if game_coords.is_some() && game_coords != self.prev_game_coords {
            // Coord cambió → reset
            self.game_coords_stale_ticks = 0;
            self.prev_game_coords = game_coords;
            if self.reported_coords_stale {
                tracing::info!(
                    "game_coords stale recovered: new coord {:?}",
                    game_coords
                );
                self.reported_coords_stale = false;
            }
        } else if game_coords.is_some() {
            // Mismo coord que antes: stale si is_moving.
            self.game_coords_stale_ticks = self.game_coords_stale_ticks.saturating_add(1);
            if !self.reported_coords_stale
                && self.game_coords_stale_ticks > COORDS_STALE_THRESHOLD_TICKS
                && is_moving == Some(true)
            {
                tracing::warn!(
                    "game_coords stale: {} ticks sin cambio (~{}s) pero is_moving=true. \
                     Posibles causas: matcher stuck, char bloqueado, path roto. \
                     coord actual: {:?}",
                    self.game_coords_stale_ticks,
                    self.game_coords_stale_ticks / 30,
                    game_coords,
                );
                self.reported_coords_stale = true;
            }
        }

        // Inventory: contar items + leer stack counts via OCR cada N frames.
        // Si la cadencia no aplica este tick, el reader queda en 0 µs y se
        // skipea el costo de read (cache `last_inventory_*` se reusa).
        timed_reader!(self, crate::instrumentation::ReaderId::Inventory, {
            if let Some(ref reader) = self.inventory_reader {
                if self.frame_count % self.inventory_detect_interval as u64 == 0 {
                    let reading = reader.read_with_stacks_ml(frame, self.ml_reader.as_mut());
                    self.last_inventory_counts = reading.slot_counts;
                    self.last_inventory_stacks = reading.stack_totals;
                    self.last_inventory_slots  = reading.slots;
                }
            }
        });
        let inventory_counts = self.last_inventory_counts.clone();
        let inventory_stacks = self.last_inventory_stacks.clone();
        let inventory_slots  = self.last_inventory_slots.clone();

        // Drift status del tracker — poblado por `adjust_roi()` calls previos
        // en este tick (HP, mana, battle, status, etc. todos pasan por
        // `self.tracker.adjust_roi` que invoca `window_offset`). Si ningún
        // `adjust_roi` corrió (p.ej. todas las ROIs son None en esta config),
        // `drift_status()` devuelve el valor del último tick — aceptable
        // como fallback degradado.
        let anchor_drift = self.tracker.drift_status();

        Perception {
            vitals,
            battle,
            conditions,
            minimap,
            target_active,
            target_hits,
            loot_sparkles,
            ui_matches,
            ui_match_infos,
            captured_at: Some(frame.captured_at),
            frame_tick,
            minimap_diff,
            is_moving,
            minimap_displacement,
            game_coords,
            inventory_counts,
            inventory_stacks,
            inventory_slots,
            anchor_drift,
        }
    }

    pub fn is_calibrated(&self) -> bool {
        self.calibration.is_usable()
    }
}
