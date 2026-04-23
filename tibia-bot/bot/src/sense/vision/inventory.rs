//! inventory.rs — Reader de inventario por template matching en slots.
//!
//! Dado un set de ROIs 32×32 (slots del backpack) y un directorio de templates
//! `assets/templates/inventory/*.png`, el reader matchea cada template contra
//! cada slot y cuenta cuántos slots matchean por item.
//!
//! Retorna `HashMap<String, u32>` donde key=nombre del template (sin .png) y
//! value=número de slots que matchean.
//!
//! Diseño sincrónico: los matches son baratos (32×32 templates sobre 32×32
//! slots = ~1ms por 100 matches), no necesitamos background thread como
//! `ui_detector`.
//!
//! Uso en cavebot:
//! ```toml
//! [[step]]
//! kind  = "goto_if"
//! when  = "not:has_item(mana_potion, 3)"
//! label = "refill"
//! ```
//!
//! Uso en TOML de calibración:
//! ```toml
//! [[inventory_slot]]
//! x = 1780
//! y = 520
//! w = 32
//! h = 32
//! ```

use std::collections::HashMap;
use std::path::Path;

use image::GrayImage;
use imageproc::template_matching::{match_template, MatchTemplateMethod};
use tracing::warn;

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::crop::{crop_bgra, Roi};

/// Threshold DEFAULT de match para CrossCorrelationNormalized: **mayor = mejor match**.
/// Rango [0.0, 1.0] donde 1.0 es match perfecto.
///
/// CCORR_NORMED es más robusto que SSE a variaciones locales de brightness/overlay
/// (ej: stack count text en diferentes cantidades 27/100/48).
///
/// 0.80 es el threshold empíricamente medido con templates de Tibia wiki.
/// Los templates wiki difieren ligeramente del render real del cliente
/// (anti-aliasing, paleta), lo que limita la precisión del matching.
///
/// **Per-template override**: si `assets/templates/inventory/thresholds.toml`
/// existe, cada template puede definir su propio threshold. Útil para:
/// - Templates sparse (sprite con mucho fondo negro) que producen FPs a 0.80
///   → usar 0.90+ para filtrar
/// - Templates muy específicos donde queremos tolerancia mayor
///
/// Formato del TOML:
/// ```toml
/// white_pearl = 0.95
/// magic_ring  = 0.95
/// dragon_ham  = 0.88
/// ```
pub const MATCH_THRESHOLD: f32 = 0.80;

/// Score gap para Non-Max Suppression **cross-slot** (Fase 1.2).
///
/// Después de asignar el best-match por slot, agrupamos por template name.
/// Para cada grupo, sólo contamos slots cuyo score está dentro de
/// `NMS_SCORE_GAP` del mejor score del grupo.
///
/// **Motivación**: en sesión live 2026-04-20, dragon_ham detectaba 4 slots
/// cuando había 2 reales. Probable causa: 2 slots legítimos con score ~0.92
/// + 2 slots borderline con score ~0.81 (arriba del threshold global 0.80).
/// Con NMS_SCORE_GAP=0.08, los borderline quedan descartados (0.81 < 0.92 - 0.08).
///
/// **Trade-off**: si un slot legítimo tiene score bajo (ej. stack count
/// distorsiona la correlación), se descarta. Por eso el gap es 0.08 — no
/// demasiado estricto pero suficiente para filtrar FPs evidentes.
///
/// NMS sólo aplica si hay ≥2 matches del mismo template. Con 1 match, no
/// hay "grupo" que filtrar.
const NMS_SCORE_GAP: f32 = 0.08;

/// Altura en píxeles del área del stack count en el bottom-right del slot.
/// Usada por `strip_stack_count_corner()` para excluir esa zona del template
/// matching y evitar que el número de unidades distorsione la correlación.
///
/// Tibia renderiza el stack count en bottom-right ~16×8 px (ver
/// `inventory_ocr::OCR_AREA_W=16` y `OCR_AREA_H=8`). Stripping las últimas
/// 8 rows del slot completo excluye el stack count Y la mitad inferior-izq
/// del icon — pequeña pérdida de discriminación pero gran ganancia en
/// robustness ante variación de stack count (ej. template extraído de
/// slot con "3" no debe fallar al matchear slot con "21").
///
/// **Wired al matcher (Fase 1.3)**: `best_match_for_slot()` aplica el strip
/// tanto al template como al slot antes de match. El OCR (`read_with_stacks`)
/// usa el slot completo sin stripping.
pub const STACK_COUNT_HEIGHT_PX: u32 = 8;

/// Densidad mínima aceptable de un template (ratio de pixels con luma > 40).
/// Templates por debajo de este valor son "sparse" — el sprite tiene mucho
/// fondo negro/transparente y tiende a matchear cualquier slot oscuro
/// produciendo false positives.
///
/// **Evidencia empírica (sesión 2026-04-20)**:
/// - `white_pearl.png` wiki: density ~0.08, matcheó 13/16 slots falsos
/// - `magic_ring.png`: density ~0.12, matcheó 5/16 falsos
/// - `npc_trade_bag.png` UI: density ~0.15, matcheó false positive bloqueando
///   battle_list scan (bug crítico)
///
/// Threshold 0.20 (20% pixels densos) filtra esos sprites.
/// Template con density baja NO se rechaza — se carga con threshold
/// auto-boosted a 0.95 (match casi idéntico) para prevenir FPs.
/// User puede overridear via thresholds.toml si sabe lo que hace.
pub const TEMPLATE_DENSITY_MIN: f32 = 0.20;

/// Luma threshold para considerar un pixel "denso" (no-background).
const DENSITY_LUMA_THRESHOLD: u8 = 40;

/// Stage A — empty-slot detection threshold (plan inventory robustez 2026-04-22).
///
/// Un slot vacío muestra solo el background del container (gris oscuro
/// casi uniforme con stddev de luma <12). Un slot con sprite tiene
/// variaciones >30 por los colores del icon + borde.
///
/// `EMPTY_STDDEV_MAX = 20.0` está en el centro del gap, con margen a ambos
/// lados:
/// - Slots vacíos típicos: stddev 8-12 → < 20 → clasificados empty.
/// - Slots con item: stddev 30-60 → ≥ 20 → template match aplica.
///
/// Short-circuit: si un slot pasa como empty, skipeamos extract_slot,
/// template match, NMS, OCR — ~100-300 µs ahorrados por slot vacío.
/// En steady state (inventory backpack con 4-10 slots vacíos), ahorro
/// esperado 40-70% del cost total de read().
pub const EMPTY_STDDEV_MAX: f32 = 20.0;

/// Calcula stddev de luma sobre un `GrayImage`. Single-pass O(n).
/// Usado por Stage A para empty detection (cheap filter antes de template
/// match). Para slot 32×32 = 1024 pixels → ~2 µs en release.
/// Expuesto para consumers que ya tienen un `GrayImage` en mano; el path
/// interno usa `frame_roi_luma_stddev` (evita allocar GrayImage).
#[allow(dead_code)] // público API para consumers externos; tests + bench lo usan.
pub fn luma_stddev(img: &GrayImage) -> f32 {
    let pixels = img.as_raw();
    if pixels.is_empty() {
        return 0.0;
    }
    let n = pixels.len() as f64;
    let sum: u64 = pixels.iter().map(|&p| p as u64).sum();
    let mean = sum as f64 / n;
    let var: f64 = pixels.iter()
        .map(|&p| {
            let d = p as f64 - mean;
            d * d
        })
        .sum::<f64>() / n;
    var.sqrt() as f32
}

/// Versión rápida directa sobre un frame + ROI (sin allocar GrayImage).
/// Itera BGRA pixels, convierte a luma inline y acumula stats.
/// Preferida cuando no se necesita el GrayImage después.
/// ~1.5 µs estimado para slot 32×32.
#[allow(dead_code)] // alternative path — usado si exact_slot no se requiere
pub fn frame_roi_luma_stddev(frame: &Frame, roi: &Roi) -> f32 {
    let stride = frame.width as usize * 4;
    let mut sum: u64 = 0;
    let mut sum_sq: u64 = 0;
    let mut count: u64 = 0;
    for row in 0..roi.h {
        for col in 0..roi.w {
            let off = (roi.y + row) as usize * stride + (roi.x + col) as usize * 4;
            if off + 2 < frame.data.len() {
                // Luma approx = (B + G*2 + R) / 4 en BGRA.
                let b = frame.data[off]     as u64;
                let g = frame.data[off + 1] as u64;
                let r = frame.data[off + 2] as u64;
                let luma = (b + g * 2 + r) / 4;
                sum    += luma;
                sum_sq += luma * luma;
                count  += 1;
            }
        }
    }
    if count == 0 {
        return 0.0;
    }
    let mean = sum as f64 / count as f64;
    let var  = sum_sq as f64 / count as f64 - mean * mean;
    var.max(0.0).sqrt() as f32
}

/// Threshold auto-boosted para templates sparse sin override en TOML.
const SPARSE_AUTO_THRESHOLD: f32 = 0.95;

/// Padding en píxeles que añadimos al slot ROI para permitir matching
/// shift-tolerant (Fase 1.4).
///
/// **Motivación**: en sesión live 2026-04-20 vimos que cambiar `backpack_h`
/// de 68 a 76 (offset acumulado 4-8 px en rows inferiores) cambiaba
/// completamente la detection. Un offset pixel-exact de calibración no es
/// realista — la posición del icon dentro del slot puede variar ±1-2 px
/// por scaling NDI, anti-aliasing, o frame timing.
///
/// **Approach**: extraer un patch `(slot.w + 2*SHIFT_PX) × (slot.h + 2*SHIFT_PX)`
/// alrededor de cada slot ROI. `imageproc::match_template` con sliding window
/// devuelve el max sobre TODAS las posiciones — automáticamente eligiendo el
/// mejor offset dentro de ±SHIFT_PX.
///
/// **Cost**: 5x5 = 25 positions por template (vs 1 sin shift). Templates
/// 32×24 sobre patch 36×28 = 5×5 sliding positions. Total ~25× cost del
/// matching, pero sigue dentro del budget porque imageproc usa FFT
/// para correlation (cost no escala lineal con N positions).
///
/// **Fallback**: si el slot está cerca del edge del frame y no hay espacio
/// para padding, usamos el slot original sin shift tolerance.
pub const SHIFT_TOLERANCE_PX: u32 = 2;

struct ItemTemplate {
    name:      String,
    template:  GrayImage,
    /// Threshold específico para este template. Default = MATCH_THRESHOLD.
    /// Cargado desde `thresholds.toml` en el mismo dir que los PNGs.
    threshold: f32,
}

/// Resultado de leer el inventario con OCR de stack count.
/// `slot_counts` es el número de slots que matchean cada item (lo que ya
/// retornaba `read()`). `stack_totals` es la suma de unidades reales leídas
/// por OCR del stack count en la esquina del slot.
///
/// `slots`: output per-slot con confidence + stage (item #2 del plan
/// inventory robustez 2026-04-22). `Vec` vacío en readers antiguos o
/// inventarios sin slots configurados. Consumers que prefieran el output
/// agregado siguen con `slot_counts` + `stack_totals` — retro-compat total.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // public API, used by tests + downstream consumers
pub struct InventoryReading {
    pub slot_counts:  HashMap<String, u32>,
    pub stack_totals: HashMap<String, u32>,
    pub slots:        Vec<super::inventory_slot::SlotReading>,
}

/// Reader de inventario: template matching por slot.
pub struct InventoryReader {
    templates: Vec<ItemTemplate>,
    slots:     Vec<RoiDef>,
    digit_templates: super::inventory_ocr::DigitTemplates,
}

impl InventoryReader {
    pub fn new() -> Self {
        Self {
            templates: Vec::new(),
            slots:     Vec::new(),
            digit_templates: super::inventory_ocr::DigitTemplates::new(),
        }
    }

    /// Carga templates de dígitos para OCR del stack count.
    /// Si los templates no existen, `read_with_stacks()` retorna stack_totals
    /// fallback al slot count (1 unit per slot).
    #[allow(dead_code)] // user-callable from main.rs
    pub fn load_digit_templates(&mut self, dir: &std::path::Path) -> usize {
        self.digit_templates.load_dir(dir)
    }

    /// Threshold per-template para mapeo de score → confidence.
    /// Retorna `MATCH_THRESHOLD` si el template no existe (no debería
    /// ocurrir porque el name viene del match previo).
    /// Lineal O(n templates) — n ≤ ~70, negligible.
    fn threshold_for(&self, name: &str) -> f32 {
        self.templates
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.threshold)
            .unwrap_or(MATCH_THRESHOLD)
    }

    /// Carga todos los templates PNG del directorio indicado.
    ///
    /// Si existe `<dir>/thresholds.toml`, aplica thresholds per-template.
    /// Templates sin entry en el TOML usan `MATCH_THRESHOLD` como default.
    pub fn load_templates(&mut self, dir: &Path) {
        self.templates.clear();
        if !dir.exists() {
            tracing::info!(
                "InventoryReader: directorio '{}' no existe — sin templates",
                dir.display()
            );
            return;
        }
        // Cargar thresholds per-template (opcional).
        let thresholds = load_thresholds(&dir.join("thresholds.toml"));
        if !thresholds.is_empty() {
            tracing::info!(
                "InventoryReader: {} thresholds per-template cargados de {}",
                thresholds.len(),
                dir.join("thresholds.toml").display()
            );
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                warn!("InventoryReader: no se pudo leer '{}': {}", dir.display(), e);
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let template = match image::open(&path) {
                Ok(img) => img.to_luma8(),
                Err(e) => {
                    warn!("InventoryReader: no se pudo cargar '{}': {}", path.display(), e);
                    continue;
                }
            };
            // Validator (Fase auditoría): density check previene FPs sistémicos
            // de templates sparse. Log WARN si < TEMPLATE_DENSITY_MIN y no hay
            // override en TOML → auto-boost threshold a 0.95.
            let density = template_density(&template);
            let user_override = thresholds.get(&name).copied();
            let threshold = match user_override {
                Some(t) => t,  // user override siempre gana (asume que sabe)
                None if density < TEMPLATE_DENSITY_MIN => {
                    warn!(
                        "InventoryReader: template '{}' density={:.2} < {:.2} (sparse — \
                         riesgo de false positives). Auto-boost threshold → {:.2}. \
                         Override en thresholds.toml si es intencional.",
                        name, density, TEMPLATE_DENSITY_MIN, SPARSE_AUTO_THRESHOLD
                    );
                    SPARSE_AUTO_THRESHOLD
                }
                None => MATCH_THRESHOLD,
            };
            let threshold_note = if (threshold - MATCH_THRESHOLD).abs() > f32::EPSILON {
                let source = if user_override.is_some() { "user" } else { "auto-sparse" };
                format!(" threshold={:.3} ({})", threshold, source)
            } else {
                String::new()
            };
            tracing::info!(
                "InventoryReader: template '{}' cargado ({}×{}) density={:.2}{}",
                name, template.width(), template.height(), density, threshold_note
            );
            self.templates.push(ItemTemplate { name, template, threshold });
        }
    }

    /// Setea los ROIs de los slots del inventario a escanear.
    pub fn set_slots(&mut self, slots: Vec<RoiDef>) {
        self.slots = slots;
    }

    /// Devuelve clone de los slots configurados — útil para consumers
    /// externos (ej. `DatasetRecorder` que captura crops por slot).
    pub fn slots(&self) -> Vec<RoiDef> {
        self.slots.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty() || self.slots.is_empty()
    }

    /// Escanea todos los slots y retorna el conteo de matches por item.
    /// Clave = nombre del template. Valor = número de slots con match.
    ///
    /// Para incluir OCR del stack count, usar `read_with_stacks()` en su lugar.
    /// Esta función se mantiene como API simple para casos sin OCR.
    #[allow(dead_code)]
    pub fn read(&self, frame: &Frame) -> HashMap<String, u32> {
        if self.is_empty() {
            return HashMap::new();
        }
        // 1. Collect: por cada slot el best match que pasa su threshold.
        //    Guardamos scores crudos para aplicar NMS cross-slot después.
        let mut scores_per_template: HashMap<String, Vec<f32>> = HashMap::new();
        for slot in &self.slots {
            let Some((slot_img, _used_shift)) = extract_slot_with_shift_tolerance(frame, slot) else {
                continue;
            };

            if let Some((score, name)) = best_match_for_slot(&slot_img, &self.templates) {
                scores_per_template.entry(name).or_default().push(score);
            }
        }
        // 2. NMS cross-slot: por template, filtrar scores fuera del gap del top.
        apply_nms(scores_per_template)
    }

    /// Versión extendida que también lee el stack count via OCR (M1).
    /// Si `digit_templates` está vacío, `stack_totals` cae al fallback de
    /// `slot_counts` (1 unit per slot).
    ///
    /// **ML delegation (Fase 2.5)**: si `ml_reader` provisto y ready, por cada
    /// slot primero intenta `ml_reader.infer_slot()`. Si devuelve `Some((class,
    /// conf))`, usa esa clase — salta el SSE matcher. Si None, fallback SSE.
    /// Sin feature `ml-runtime`, `infer_slot` siempre None → 100% fallback SSE.
    #[allow(dead_code)] // public API
    pub fn read_with_stacks_ml(
        &self,
        frame:      &Frame,
        ml_reader:  Option<&mut super::inventory_ml::MlInventoryReader>,
    ) -> InventoryReading {
        use super::inventory_slot::{SlotReading, SlotStage};

        let mut slot_counts:  HashMap<String, u32> = HashMap::new();
        let mut stack_totals: HashMap<String, u32> = HashMap::new();
        let mut slots_out:    Vec<SlotReading> = Vec::with_capacity(self.slots.len());

        if self.is_empty() {
            return InventoryReading { slot_counts, stack_totals, slots: slots_out };
        }
        let use_ml = ml_reader.as_ref().map(|r| r.is_ready()).unwrap_or(false);
        let ml_ref_opt = if use_ml { ml_reader } else { None };
        let mut ml_inner = ml_ref_opt;

        for (slot_idx, slot) in self.slots.iter().enumerate() {
            let slot_idx = slot_idx as u32;

            // Stage A: empty-slot short-circuit (plan inventory robustez).
            let stddev = frame_roi_luma_stddev(
                frame,
                &Roi::new(slot.x, slot.y, slot.w, slot.h),
            );
            if stddev < EMPTY_STDDEV_MAX {
                slots_out.push(SlotReading::empty(slot_idx));
                continue;
            }

            let Some((slot_img_padded, _)) = extract_slot_with_shift_tolerance(frame, slot) else {
                // Extract falló (ROI fuera del frame). Marcar como unmatched
                // en vez de skip silencioso — ayuda al debug del JSONL.
                slots_out.push(SlotReading::unmatched(slot_idx));
                continue;
            };
            // Extract slot exact también para OCR + ML (ML requiere 32×32 exact).
            let exact_slot = crop_bgra(frame, Roi::new(slot.x, slot.y, slot.w, slot.h))
                .and_then(|bgra| {
                    let luma = bgra_to_luma(&bgra, slot.w, slot.h);
                    GrayImage::from_raw(slot.w, slot.h, luma)
                });

            // Intento 1: ML classifier (si provisto + ready).
            let ml_match: Option<(String, f32)> = if let (Some(ref mut ml), Some(ref slot_exact)) =
                (ml_inner.as_deref_mut(), exact_slot.as_ref())
            {
                ml.infer_slot(slot_exact)
            } else {
                None
            };

            // Intento 2: fallback SSE (best-match + threshold per-template).
            let sse_match: Option<(f32, String)> = if ml_match.is_none() {
                best_match_for_slot(&slot_img_padded, &self.templates)
            } else {
                None
            };

            // Helper para leer stack via OCR (solo si digit templates cargados).
            let read_stack = |slot_img: Option<&GrayImage>| -> Option<u32> {
                if self.digit_templates.is_empty() {
                    return None;
                }
                slot_img.and_then(|s| {
                    super::inventory_ocr::read_slot_count(s, &self.digit_templates)
                })
            };

            match (ml_match, sse_match) {
                (Some((cls, conf)), _) => {
                    let stack = read_stack(exact_slot.as_ref());
                    *slot_counts.entry(cls.clone()).or_insert(0) += 1;
                    *stack_totals.entry(cls.clone()).or_insert(0) += stack.unwrap_or(1);
                    slots_out.push(SlotReading::ml_classified(
                        slot_idx, cls, conf, stack,
                    ));
                }
                (None, Some((score, name))) => {
                    let threshold = self.threshold_for(&name);
                    let stack = read_stack(exact_slot.as_ref());
                    *slot_counts.entry(name.clone()).or_insert(0) += 1;
                    *stack_totals.entry(name.clone()).or_insert(0) += stack.unwrap_or(1);
                    slots_out.push(SlotReading::matched(
                        slot_idx, name, score, threshold, stack, SlotStage::FullSweep,
                    ));
                }
                (None, None) => {
                    // Slot con contenido pero ningún template matcheó el threshold.
                    // Item nuevo sin template O template similar que no alcanzó.
                    slots_out.push(SlotReading::unmatched(slot_idx));
                }
            }
        }
        // NOTA: NMS cross-slot NO aplica aquí — el orden de matches ML no es
        // comparable con scores SSE. Si se quiere dedup, hacer por separado.
        InventoryReading { slot_counts, stack_totals, slots: slots_out }
    }

    /// Versión extendida que también lee el stack count via OCR (M1).
    /// Si `digit_templates` está vacío, `stack_totals` cae al fallback de
    /// `slot_counts` (1 unit per slot).
    #[allow(dead_code)] // public API
    pub fn read_with_stacks(&self, frame: &Frame) -> InventoryReading {
        use super::inventory_slot::{SlotReading, SlotStage};

        // Estado transitorio por slot, resuelto a `SlotReading` tras NMS.
        enum SlotRaw {
            Empty,
            Unmatched,
            Pending { name: String, score: f32, stack: Option<u32> },
        }

        let mut slot_counts: HashMap<String, u32> = HashMap::new();
        let mut stack_totals: HashMap<String, u32> = HashMap::new();
        let mut slots_raw: Vec<SlotRaw> = Vec::with_capacity(self.slots.len());
        if self.is_empty() {
            return InventoryReading {
                slot_counts, stack_totals, slots: Vec::new(),
            };
        }

        // Pase 1: recolectar estado per-slot preservando índice.
        // Usamos también `matches_by_template` para el NMS posterior.
        let mut matches_by_template: HashMap<String, Vec<(usize, f32)>> = HashMap::new();
        for (slot_idx, slot) in self.slots.iter().enumerate() {
            // Stage A.
            let stddev = frame_roi_luma_stddev(
                frame,
                &Roi::new(slot.x, slot.y, slot.w, slot.h),
            );
            if stddev < EMPTY_STDDEV_MAX {
                slots_raw.push(SlotRaw::Empty);
                continue;
            }

            let Some((slot_img_padded, _used_shift)) = extract_slot_with_shift_tolerance(frame, slot) else {
                slots_raw.push(SlotRaw::Unmatched);
                continue;
            };

            if let Some((score, name)) = best_match_for_slot(&slot_img_padded, &self.templates) {
                // OCR sobre el slot ORIGINAL (sin padding).
                let stack: Option<u32> = if !self.digit_templates.is_empty() {
                    crop_bgra(frame, Roi::new(slot.x, slot.y, slot.w, slot.h))
                        .and_then(|bgra| {
                            let luma = bgra_to_luma(&bgra, slot.w, slot.h);
                            GrayImage::from_raw(slot.w, slot.h, luma)
                        })
                        .and_then(|exact_slot| {
                            super::inventory_ocr::read_slot_count(&exact_slot, &self.digit_templates)
                        })
                } else {
                    None
                };
                matches_by_template.entry(name.clone())
                    .or_default()
                    .push((slot_idx, score));
                slots_raw.push(SlotRaw::Pending { name, score, stack });
            } else {
                slots_raw.push(SlotRaw::Unmatched);
            }
        }

        // Pase 2: NMS cross-slot por template → set de slot_idx que SOBREVIVEN.
        let mut survivors: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for (name, mut data) in matches_by_template {
            if data.is_empty() {
                continue;
            }
            data.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let top_score = data[0].1;
            let kept: Vec<(usize, f32)> = data.into_iter()
                .filter(|(_, s)| *s >= top_score - NMS_SCORE_GAP)
                .collect();
            let count = kept.len() as u32;
            slot_counts.insert(name.clone(), count);
            // stack_totals se reconstruye tras saber qué slots sobreviven.
            let _ = name;
            for (idx, _) in kept {
                survivors.insert(idx);
            }
        }

        // Pase 3: construir `slots` output + `stack_totals` agregado.
        let mut slots_out: Vec<SlotReading> = Vec::with_capacity(slots_raw.len());
        for (slot_idx, raw) in slots_raw.into_iter().enumerate() {
            let slot_idx_u32 = slot_idx as u32;
            let reading = match raw {
                SlotRaw::Empty     => SlotReading::empty(slot_idx_u32),
                SlotRaw::Unmatched => SlotReading::unmatched(slot_idx_u32),
                SlotRaw::Pending { name, score, stack } => {
                    if survivors.contains(&slot_idx) {
                        let threshold = self.threshold_for(&name);
                        *stack_totals.entry(name.clone()).or_insert(0) += stack.unwrap_or(1);
                        SlotReading::matched(
                            slot_idx_u32, name, score, threshold, stack, SlotStage::FullSweep,
                        )
                    } else {
                        // Descartado por NMS — el match original existía pero
                        // otro slot con score superior lo absorbió como FP.
                        // Reportar como unmatched en el output per-slot.
                        SlotReading::unmatched(slot_idx_u32)
                    }
                }
            };
            slots_out.push(reading);
        }
        InventoryReading { slot_counts, stack_totals, slots: slots_out }
    }
}

impl Default for InventoryReader {
    fn default() -> Self { Self::new() }
}

/// Carga thresholds per-template desde un TOML simple clave=valor.
/// Formato esperado:
/// ```toml
/// white_pearl = 0.95
/// magic_ring  = 0.95
/// dragon_ham  = 0.88
/// ```
/// Claves son nombres de templates (sin .png). Valores son f32 en [0.0, 1.0].
/// Retorna map vacío si el archivo no existe o falla el parseo.
fn load_thresholds(path: &Path) -> HashMap<String, f32> {
    let mut out = HashMap::new();
    if !path.exists() {
        return out;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "InventoryReader: thresholds.toml existe pero no se pudo leer ({}): {}",
                path.display(), e
            );
            return out;
        }
    };
    // Parser muy simple: una clave=valor por línea, comentarios con #, trim.
    for (line_no, raw) in content.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            warn!("thresholds.toml línea {}: formato inválido (sin '='): '{}'",
                  line_no + 1, raw);
            continue;
        };
        let key = k.trim().to_string();
        let val_str = v.trim();
        match val_str.parse::<f32>() {
            Ok(val) if (0.0..=1.0).contains(&val) => {
                out.insert(key, val);
            }
            Ok(val) => {
                warn!(
                    "thresholds.toml línea {}: valor fuera de rango [0,1] para '{}' = {}",
                    line_no + 1, key, val
                );
            }
            Err(e) => {
                warn!(
                    "thresholds.toml línea {}: valor no-numérico para '{}': '{}' ({})",
                    line_no + 1, key, val_str, e
                );
            }
        }
    }
    out
}

/// Extrae el patch BGRA de un slot, intentando primero con padding
/// `SHIFT_TOLERANCE_PX` en cada lado (para shift-tolerant matching). Si
/// el slot está cerca del edge del frame y el padding cae fuera, fallback
/// al slot original sin padding. Devuelve el GrayImage convertido + bool
/// `used_shift_tolerance` para diagnóstico.
fn extract_slot_with_shift_tolerance(
    frame: &Frame,
    slot:  &RoiDef,
) -> Option<(GrayImage, bool)> {
    // Intento 1: slot + padding SHIFT_TOLERANCE_PX en cada lado.
    let pad = SHIFT_TOLERANCE_PX;
    if slot.x >= pad && slot.y >= pad {
        let padded_roi = Roi::new(
            slot.x - pad,
            slot.y - pad,
            slot.w + 2 * pad,
            slot.h + 2 * pad,
        );
        if let Some(patch_bgra) = crop_bgra(frame, padded_roi) {
            let luma = bgra_to_luma(&patch_bgra, padded_roi.w, padded_roi.h);
            if let Some(img) = GrayImage::from_raw(padded_roi.w, padded_roi.h, luma) {
                return Some((img, true));
            }
        }
    }
    // Fallback: slot exacto sin padding (cerca de edge).
    let patch_bgra = crop_bgra(frame, Roi::new(slot.x, slot.y, slot.w, slot.h))?;
    let luma = bgra_to_luma(&patch_bgra, slot.w, slot.h);
    let img = GrayImage::from_raw(slot.w, slot.h, luma)?;
    Some((img, false))
}

/// Strippea las últimas `STACK_COUNT_HEIGHT_PX` rows del bottom del slot/template.
/// El resultado es una nueva GrayImage de width × (height - STACK_COUNT_HEIGHT_PX).
/// Si la altura es menor o igual a STACK_COUNT_HEIGHT_PX, devuelve el slot
/// original sin modificar (no hay nada que strippear).
fn strip_stack_count_corner(img: &GrayImage) -> GrayImage {
    let w = img.width();
    let h = img.height();
    if h <= STACK_COUNT_HEIGHT_PX {
        return img.clone();
    }
    let new_h = h - STACK_COUNT_HEIGHT_PX;
    let mut stripped = GrayImage::new(w, new_h);
    for y in 0..new_h {
        for x in 0..w {
            stripped.put_pixel(x, y, *img.get_pixel(x, y));
        }
    }
    stripped
}

/// Encuentra el template con mayor score para un slot dado, aplicando el
/// threshold per-template del ganador. Devuelve `Some((score, name))` sólo
/// si el score pasa el threshold del template ganador, o `None` si no.
///
/// **Stack count stripping (Fase 1.3)**: tanto el slot como cada template son
/// strippeados de las últimas STACK_COUNT_HEIGHT_PX rows ANTES del matching.
/// Esto evita que el número de unidades del stack distorsione la correlación
/// (un template extraído de slot con "3" debe poder matchear un slot con "21").
/// El OCR del stack count se hace separadamente sobre el slot completo.
///
/// Nota importante: el threshold se compara contra el ganador, no contra
/// cada candidato. Si el best_match es `dragon_ham` con threshold 0.88 y
/// score 0.87, se rechaza — aunque otros templates hayan pasado su threshold
/// más laxo con scores menores. Esto evita que un template "casi matchea" y
/// otro "débilmente matchea" compitan y el ganador "casi" quede contado.
fn best_match_for_slot(
    slot_img: &GrayImage,
    templates: &[ItemTemplate],
) -> Option<(f32, String)> {
    // Strip stack count corner del slot UNA sola vez (no por template).
    let slot_stripped = strip_stack_count_corner(slot_img);
    let mut best: Option<(f32, &str, f32)> = None;
    for tpl in templates {
        let tpl_stripped = strip_stack_count_corner(&tpl.template);
        if slot_stripped.width() < tpl_stripped.width()
            || slot_stripped.height() < tpl_stripped.height()
        {
            continue;
        }
        let result = match_template(
            &slot_stripped,
            &tpl_stripped,
            MatchTemplateMethod::CrossCorrelationNormalized,
        );
        let score = result.iter().cloned().fold(f32::MIN, f32::max);
        let current_best_score = best.map_or(f32::MIN, |(s, _, _)| s);
        if score > current_best_score {
            best = Some((score, tpl.name.as_str(), tpl.threshold));
        }
    }
    match best {
        Some((score, name, threshold)) if score >= threshold => {
            Some((score, name.to_string()))
        }
        _ => None,
    }
}

/// Aplica Non-Max Suppression cross-slot sobre los scores agrupados por template.
/// Para cada template: ordena scores desc, quita los que estén fuera del
/// `NMS_SCORE_GAP` del top. Retorna count final por template.
fn apply_nms(scores_per_template: HashMap<String, Vec<f32>>) -> HashMap<String, u32> {
    let mut out: HashMap<String, u32> = HashMap::new();
    for (name, mut scores) in scores_per_template {
        if scores.is_empty() {
            continue;
        }
        scores.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let top_score = scores[0];
        let kept = scores.iter().filter(|&&s| s >= top_score - NMS_SCORE_GAP).count();
        out.insert(name, kept as u32);
    }
    out
}

/// Computa la densidad del template: ratio de pixels con luma > DENSITY_LUMA_THRESHOLD.
/// Templates sparse (mayormente fondo negro/transparente) dan density baja y son
/// prone a false positives — ver `TEMPLATE_DENSITY_MIN` para contexto.
pub fn template_density(img: &GrayImage) -> f32 {
    let total = (img.width() * img.height()) as f32;
    if total == 0.0 {
        return 0.0;
    }
    let dense = img.pixels().filter(|p| p.0[0] > DENSITY_LUMA_THRESHOLD).count() as f32;
    dense / total
}

/// Convierte BGRA a luma (Y = 0.299R + 0.587G + 0.114B).
fn bgra_to_luma(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let b = data[i * 4] as f32;
        let g = data[i * 4 + 1] as f32;
        let r = data[i * 4 + 2] as f32;
        out.push((0.114 * b + 0.587 * g + 0.299 * r) as u8);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_empty_by_default() {
        let reader = InventoryReader::new();
        assert!(reader.is_empty());
    }

    // ── Stage A: empty slot detection (luma stddev) ────────────────────

    fn gray_image_with_value(w: u32, h: u32, v: u8) -> GrayImage {
        let mut img = GrayImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, image::Luma([v]));
            }
        }
        img
    }

    fn gray_image_with_noise(w: u32, h: u32, amplitude: u8) -> GrayImage {
        let mut img = GrayImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                // Pseudo-random via mixing x+y. Amplitude escala variación.
                let base = ((x * 17 + y * 31) % 256) as u8;
                let scaled = (base as u32 * amplitude as u32 / 255) as u8;
                img.put_pixel(x, y, image::Luma([scaled]));
            }
        }
        img
    }

    #[test]
    fn luma_stddev_zero_for_uniform_image() {
        let img = gray_image_with_value(32, 32, 42);
        assert!(luma_stddev(&img) < 0.01);
    }

    #[test]
    fn luma_stddev_zero_for_empty_image() {
        let img = GrayImage::new(0, 0);
        assert_eq!(luma_stddev(&img), 0.0);
    }

    #[test]
    fn luma_stddev_high_for_noisy_image() {
        // amplitude=255 genera pattern con stddev muy alto.
        let img = gray_image_with_noise(32, 32, 255);
        let s = luma_stddev(&img);
        assert!(s > 30.0, "got {}", s);
    }

    #[test]
    fn luma_stddev_below_threshold_for_near_uniform() {
        // Uniform with tiny noise (amplitude 20) → stddev ~8, < 20 threshold.
        let img = gray_image_with_noise(32, 32, 20);
        let s = luma_stddev(&img);
        assert!(s < EMPTY_STDDEV_MAX,
            "near-uniform stddev={} should be < EMPTY_STDDEV_MAX={}",
            s, EMPTY_STDDEV_MAX);
    }

    #[test]
    fn luma_stddev_above_threshold_for_content() {
        // Sprite-like: stronger variation → stddev > 20.
        let img = gray_image_with_noise(32, 32, 200);
        let s = luma_stddev(&img);
        assert!(s > EMPTY_STDDEV_MAX,
            "sprite-like stddev={} should be > EMPTY_STDDEV_MAX={}",
            s, EMPTY_STDDEV_MAX);
    }

    #[test]
    fn frame_roi_stddev_matches_gray_stddev() {
        // Generar frame BGRA con patrón conocido, verificar que
        // frame_roi_luma_stddev devuelve algo cercano a luma_stddev del slot.
        let w = 64u32;
        let h = 64u32;
        let mut data = vec![0u8; (w * h * 4) as usize];
        for i in 0..(w * h) as usize {
            let v = ((i % 200) as u8).saturating_mul(1);
            data[i * 4]     = v;
            data[i * 4 + 1] = v;
            data[i * 4 + 2] = v;
            data[i * 4 + 3] = 255;
        }
        let frame = crate::sense::frame_buffer::Frame {
            width: w, height: h, data,
            captured_at: std::time::Instant::now(),
        };
        let roi = Roi::new(16, 16, 32, 32);
        let s_frame = frame_roi_luma_stddev(&frame, &roi);
        // Stddev debe ser >0 (hay variación en el patrón).
        assert!(s_frame > 5.0, "got {}", s_frame);
    }

    #[test]
    fn frame_roi_stddev_zero_for_uniform_bgra() {
        let w = 64u32;
        let h = 64u32;
        let data = vec![100u8; (w * h * 4) as usize]; // all pixels identical
        let frame = crate::sense::frame_buffer::Frame {
            width: w, height: h, data,
            captured_at: std::time::Instant::now(),
        };
        let roi = Roi::new(16, 16, 32, 32);
        assert!(frame_roi_luma_stddev(&frame, &roi) < 0.01);
    }

    #[test]
    fn reader_empty_with_only_slots() {
        let mut reader = InventoryReader::new();
        reader.set_slots(vec![RoiDef::new(0, 0, 32, 32)]);
        assert!(reader.is_empty()); // sin templates = empty
    }

    // ── Item #2: per-slot output integration ─────────────────────────────

    #[test]
    fn read_returns_empty_slots_vec_when_unconfigured() {
        // Sin templates → InventoryReading.slots vacío.
        let reader = InventoryReader::new();
        let frame = crate::sense::frame_buffer::Frame {
            width: 100, height: 100, data: vec![50u8; 100 * 100 * 4],
            captured_at: std::time::Instant::now(),
        };
        let reading = reader.read_with_stacks(&frame);
        assert!(reading.slots.is_empty());
    }

    #[test]
    fn read_ml_variant_emits_empty_slot_reading_for_uniform_frame() {
        use crate::sense::vision::inventory_slot::SlotStage;

        // Frame uniforme → todos los slots luma stddev ~0 → Stage A Empty.
        let mut reader = InventoryReader::new();
        reader.templates.push(ItemTemplate {
            name: "dummy".into(),
            template: make_template(42),
            threshold: MATCH_THRESHOLD,
        });
        reader.set_slots(vec![
            RoiDef::new(10, 10, 32, 32),
            RoiDef::new(50, 10, 32, 32),
        ]);

        let frame = crate::sense::frame_buffer::Frame {
            width: 200, height: 200,
            data: vec![100u8; 200 * 200 * 4],  // uniforme → stddev ~0
            captured_at: std::time::Instant::now(),
        };
        let reading = reader.read_with_stacks_ml(&frame, None);
        assert_eq!(reading.slots.len(), 2);
        for slot in &reading.slots {
            assert_eq!(slot.stage, SlotStage::Empty);
            assert!(slot.item.is_none());
            assert_eq!(slot.confidence, 1.0);
            assert!(slot.raw_score.is_none());
        }
        // Aggregate HashMaps vacíos también.
        assert!(reading.slot_counts.is_empty());
        assert!(reading.stack_totals.is_empty());
    }

    #[test]
    fn read_ml_variant_preserves_slot_idx_order() {
        // Confirma que slot_idx en Vec<SlotReading> matches el orden de
        // configuración del reader.set_slots. Importante para consumers
        // que indexan por slot_idx.
        let mut reader = InventoryReader::new();
        reader.templates.push(ItemTemplate {
            name: "dummy".into(),
            template: make_template(42),
            threshold: MATCH_THRESHOLD,
        });
        reader.set_slots(vec![
            RoiDef::new(0,   0, 32, 32),
            RoiDef::new(50,  0, 32, 32),
            RoiDef::new(100, 0, 32, 32),
        ]);

        let frame = crate::sense::frame_buffer::Frame {
            width: 200, height: 100,
            data: vec![50u8; 200 * 100 * 4],
            captured_at: std::time::Instant::now(),
        };
        let reading = reader.read_with_stacks_ml(&frame, None);
        assert_eq!(reading.slots.len(), 3);
        for (i, slot) in reading.slots.iter().enumerate() {
            assert_eq!(slot.slot_idx, i as u32);
        }
    }

    #[test]
    fn threshold_for_returns_override_or_default() {
        let mut reader = InventoryReader::new();
        reader.templates.push(ItemTemplate {
            name: "white_pearl".into(),
            template: make_template(0),
            threshold: 0.95,  // override
        });
        reader.templates.push(ItemTemplate {
            name: "mana_potion".into(),
            template: make_template(0),
            threshold: MATCH_THRESHOLD,
        });
        assert!((reader.threshold_for("white_pearl") - 0.95).abs() < 0.001);
        assert!((reader.threshold_for("mana_potion") - MATCH_THRESHOLD).abs() < 0.001);
        // Template inexistente → MATCH_THRESHOLD default.
        assert!((reader.threshold_for("nonexistent") - MATCH_THRESHOLD).abs() < 0.001);
    }

    #[test]
    fn bgra_to_luma_produces_expected_size() {
        let bgra = vec![100u8, 150, 200, 255, 50, 75, 100, 255];
        let luma = bgra_to_luma(&bgra, 2, 1);
        assert_eq!(luma.len(), 2);
    }

    // ── Per-template thresholds (Fase 1) ──────────────────────────────

    #[test]
    fn load_thresholds_nonexistent_returns_empty() {
        let path = std::path::PathBuf::from("/tmp/doesnotexist-thresholds-xyz.toml");
        let out = load_thresholds(&path);
        assert!(out.is_empty());
    }

    #[test]
    fn load_thresholds_parses_valid_toml() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_thresholds_valid.toml");
        std::fs::write(&path,
            "# comment\nwhite_pearl = 0.95\n\nmagic_ring = 0.9  # inline comment\ndragon_ham=0.85\n"
        ).unwrap();
        let out = load_thresholds(&path);
        assert_eq!(out.get("white_pearl"), Some(&0.95));
        assert_eq!(out.get("magic_ring"), Some(&0.9));
        assert_eq!(out.get("dragon_ham"), Some(&0.85));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_thresholds_rejects_out_of_range() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_thresholds_oor.toml");
        std::fs::write(&path, "bad = 2.5\ngood = 0.5\nneg = -0.1\n").unwrap();
        let out = load_thresholds(&path);
        assert_eq!(out.get("good"), Some(&0.5));
        assert!(out.get("bad").is_none(), "valor >1 debe rechazarse");
        assert!(out.get("neg").is_none(), "valor <0 debe rechazarse");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_thresholds_rejects_non_numeric() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_thresholds_nan.toml");
        std::fs::write(&path, "bad = abc\ngood = 0.3\n").unwrap();
        let out = load_thresholds(&path);
        assert_eq!(out.get("good"), Some(&0.3));
        assert!(out.get("bad").is_none());
        let _ = std::fs::remove_file(&path);
    }

    // ── NMS cross-slot (Fase 1.2) ──────────────────────────────────────

    #[test]
    fn apply_nms_single_score_keeps_it() {
        let mut input = HashMap::new();
        input.insert("dragon_ham".to_string(), vec![0.85]);
        let out = apply_nms(input);
        assert_eq!(out.get("dragon_ham"), Some(&1));
    }

    #[test]
    fn apply_nms_all_scores_within_gap_keeps_all() {
        // Scores [0.93, 0.90, 0.88] — todos within NMS_SCORE_GAP=0.08 del top=0.93
        // (0.93 - 0.08 = 0.85, todos ≥ 0.85)
        let mut input = HashMap::new();
        input.insert("dragon_ham".to_string(), vec![0.93, 0.90, 0.88]);
        let out = apply_nms(input);
        assert_eq!(out.get("dragon_ham"), Some(&3));
    }

    #[test]
    fn apply_nms_filters_borderline_scores() {
        // Scores [0.93, 0.85, 0.82, 0.81] — top=0.93, cutoff=0.85
        // Solo los ≥ 0.85 se mantienen → [0.93, 0.85] = 2
        let mut input = HashMap::new();
        input.insert("dragon_ham".to_string(), vec![0.93, 0.85, 0.82, 0.81]);
        let out = apply_nms(input);
        assert_eq!(out.get("dragon_ham"), Some(&2),
            "0.81 y 0.82 < 0.85 (0.93 - 0.08), deben filtrarse");
    }

    #[test]
    fn apply_nms_multiple_templates_independent() {
        let mut input = HashMap::new();
        input.insert("vial".to_string(),       vec![0.95, 0.92]);
        input.insert("dragon_ham".to_string(), vec![0.90, 0.80]);
        let out = apply_nms(input);
        // vial: ambos within 0.08 de 0.95 → 2
        assert_eq!(out.get("vial"), Some(&2));
        // dragon_ham: 0.80 < 0.90 - 0.08 = 0.82 → sólo 1
        assert_eq!(out.get("dragon_ham"), Some(&1));
    }

    #[test]
    fn apply_nms_empty_input_returns_empty() {
        let out = apply_nms(HashMap::new());
        assert!(out.is_empty());
    }

    // ── Stack count stripping (Fase 1.3) ──────────────────────────────

    #[test]
    fn strip_stack_count_removes_bottom_rows() {
        // Slot 32×32 → debería quedar 32×24
        let img = GrayImage::new(32, 32);
        let stripped = strip_stack_count_corner(&img);
        assert_eq!(stripped.width(), 32);
        assert_eq!(stripped.height(), 32 - STACK_COUNT_HEIGHT_PX);
    }

    #[test]
    fn strip_stack_count_preserves_top_pixels() {
        // Llenar top 24 rows con valor 100, bottom 8 con 200.
        let mut img = GrayImage::new(32, 32);
        for y in 0..32 {
            let v = if y < 24 { 100u8 } else { 200u8 };
            for x in 0..32 {
                img.put_pixel(x, y, image::Luma([v]));
            }
        }
        let stripped = strip_stack_count_corner(&img);
        // Top pixel preservado.
        assert_eq!(stripped.get_pixel(0, 0).0[0], 100);
        // Bottom pixel del stripped (era y=23 del original) preservado.
        assert_eq!(stripped.get_pixel(0, 23).0[0], 100);
    }

    // ── Shift-tolerant slot extraction (Fase 1.4) ─────────────────────

    fn make_test_frame(w: u32, h: u32, fill: u8) -> Frame {
        Frame {
            width:       w,
            height:      h,
            data:        vec![fill; (w * h * 4) as usize],
            captured_at: std::time::Instant::now(),
        }
    }

    #[test]
    fn extract_slot_uses_padding_when_room() {
        // Slot en el centro del frame: hay room para padding.
        let frame = make_test_frame(200, 200, 128);
        let slot = RoiDef::new(50, 50, 32, 32);
        let (img, used_shift) = extract_slot_with_shift_tolerance(&frame, &slot).unwrap();
        assert!(used_shift, "Slot en centro debe usar padding");
        assert_eq!(img.width(), 32 + 2 * SHIFT_TOLERANCE_PX);
        assert_eq!(img.height(), 32 + 2 * SHIFT_TOLERANCE_PX);
    }

    #[test]
    fn extract_slot_falls_back_at_top_left_corner() {
        // Slot en (0, 0): no hay room para padding hacia arriba/izq.
        let frame = make_test_frame(200, 200, 128);
        let slot = RoiDef::new(0, 0, 32, 32);
        let (img, used_shift) = extract_slot_with_shift_tolerance(&frame, &slot).unwrap();
        assert!(!used_shift, "Slot en (0,0) debe fallback a sin padding");
        assert_eq!(img.width(), 32);
        assert_eq!(img.height(), 32);
    }

    #[test]
    fn extract_slot_falls_back_when_padded_exceeds_right_edge() {
        // Slot cerca del right edge del frame.
        let frame = make_test_frame(50, 50, 128);
        // Slot 32x32 en (16, 16) → padded sería (14, 14) 36×36 = (14..50). OK!
        // Pero (18, 18) 32×32 → padded (16,16) 36×36 = (16..52) — fuera por 2.
        let slot = RoiDef::new(18, 18, 32, 32);
        let (img, used_shift) = extract_slot_with_shift_tolerance(&frame, &slot).unwrap();
        assert!(!used_shift, "padded fuera del frame debe fallback");
        assert_eq!(img.width(), 32);
    }

    #[test]
    fn strip_stack_count_too_small_returns_clone() {
        // Si height ≤ STACK_COUNT_HEIGHT_PX (8), no se strippea.
        let img = GrayImage::new(32, 8);
        let stripped = strip_stack_count_corner(&img);
        assert_eq!(stripped.height(), 8); // sin cambio
        let img_smaller = GrayImage::new(32, 5);
        let stripped_smaller = strip_stack_count_corner(&img_smaller);
        assert_eq!(stripped_smaller.height(), 5);
    }

    // ── Template density validator (auditoría) ────────────────────────

    #[test]
    fn template_density_all_black_returns_zero() {
        let img = GrayImage::new(32, 32);  // all pixels = 0
        assert_eq!(template_density(&img), 0.0);
    }

    #[test]
    fn template_density_all_white_returns_one() {
        let mut img = GrayImage::new(32, 32);
        for p in img.pixels_mut() { p.0[0] = 255; }
        assert!((template_density(&img) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn template_density_sparse_sprite_below_threshold() {
        // Simula sprite sparse tipo white_pearl: solo 10% pixels densos.
        let mut img = GrayImage::new(32, 32);
        let total = 32 * 32;
        let dense_count = total / 10;  // 10%
        for (i, p) in img.pixels_mut().enumerate() {
            p.0[0] = if i < dense_count as usize { 255 } else { 0 };
        }
        let d = template_density(&img);
        assert!(d < TEMPLATE_DENSITY_MIN, "expected sparse, got density={}", d);
    }

    #[test]
    fn template_density_dense_sprite_above_threshold() {
        // Simula sprite denso tipo gold_coin strip: 80% pixels densos.
        let mut img = GrayImage::new(32, 32);
        let total = 32 * 32;
        let dense_count = (total * 8) / 10;
        for (i, p) in img.pixels_mut().enumerate() {
            p.0[0] = if i < dense_count as usize { 200 } else { 0 };
        }
        let d = template_density(&img);
        assert!(d >= TEMPLATE_DENSITY_MIN, "expected dense, got density={}", d);
    }

    #[test]
    fn template_density_boundary_luma_40() {
        // Pixels con luma == 40 NO cuentan (threshold es >40 strict).
        let mut img = GrayImage::new(10, 10);
        for p in img.pixels_mut() { p.0[0] = 40; }
        assert_eq!(template_density(&img), 0.0);

        // Luma 41 sí cuenta.
        let mut img2 = GrayImage::new(10, 10);
        for p in img2.pixels_mut() { p.0[0] = 41; }
        assert!((template_density(&img2) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn load_thresholds_handles_malformed_lines() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_thresholds_malformed.toml");
        std::fs::write(&path, "this is not key=value\nvalid = 0.7\n").unwrap();
        let out = load_thresholds(&path);
        // "this is not key=value" splitea en key="this is not key", val="value" → reject non-numeric
        // "valid = 0.7" → OK
        assert_eq!(out.get("valid"), Some(&0.7));
        let _ = std::fs::remove_file(&path);
    }

    // ── M3: Performance benchmark ────────────────────────────────────────

    /// Construye un frame BGRA 1920×1080 con un patrón distintivo en una
    /// región específica (donde estamos pintando el slot).
    fn make_perf_frame() -> crate::sense::frame_buffer::Frame {
        let w = 1920u32;
        let h = 1080u32;
        let mut data = vec![0u8; (w * h * 4) as usize];
        // Llenar cada pixel con un patrón ligero. Usamos saturating_add para
        // evitar overflow cuando v se acerca a 255 (199 + 60 desbordaba u8).
        for i in 0..(w * h) as usize {
            let v = (i % 200) as u8;
            data[i * 4]     = v;                       // B
            data[i * 4 + 1] = v.saturating_add(30);    // G
            data[i * 4 + 2] = v.saturating_add(60);    // R
            data[i * 4 + 3] = 255;                     // A
        }
        crate::sense::frame_buffer::Frame {
            width: w,
            height: h,
            data,
            captured_at: std::time::Instant::now(),
        }
    }

    fn make_template(seed: u8) -> image::GrayImage {
        let mut img = image::GrayImage::new(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                let v = seed.wrapping_add((x * 5 + y * 7) as u8);
                img.put_pixel(x, y, image::Luma([v]));
            }
        }
        img
    }

    /// Bench-style guard: el target <50 ms aplica al binario release. En
    /// debug, `match_template` × 400 matches puede tardar 200-300 ms (la
    /// medición empírica reporta ~239 ms en el dev box). `#[ignore]` evita
    /// fallar el test default — correr con `cargo test --release -- --ignored`
    /// para validar el budget cuando interesa.
    #[test]
    #[ignore = "perf-only: usar `cargo test --release -- --ignored` para verificar budget"]
    fn read_with_realistic_load_under_budget() {
        // 20 templates × 20 slots = 400 matches por read().
        // Budget 30Hz: 33ms/tick. detect_interval=15 → ~500ms entre reads.
        // Target conservador: read() < 50ms (10× margen sobre budget esperado ~5ms).
        let mut reader = InventoryReader::new();

        // 20 templates sintéticos.
        for i in 0..20 {
            reader.templates.push(ItemTemplate {
                name: format!("item_{}", i),
                template: make_template(i * 12),
                threshold: MATCH_THRESHOLD,
            });
        }

        // 20 slots en grid 4×5, top-right del frame.
        let mut slots = Vec::new();
        for row in 0..5u32 {
            for col in 0..4u32 {
                slots.push(RoiDef {
                    x: 1760 + col * 34,
                    y: 420 + row * 34,
                    w: 32,
                    h: 32,
                });
            }
        }
        reader.set_slots(slots);
        assert!(!reader.is_empty());

        let frame = make_perf_frame();

        // Warmup.
        let _ = reader.read(&frame);

        // Bench: 100 reads consecutivos.
        let n = 100;
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            let _ = reader.read(&frame);
        }
        let elapsed = t0.elapsed();
        let per_read_ms = elapsed.as_secs_f64() * 1000.0 / n as f64;

        eprintln!(
            "InventoryReader::read() perf: {:.2}ms/call ({} reads en {:.0}ms, {} matches/read)",
            per_read_ms,
            n,
            elapsed.as_secs_f64() * 1000.0,
            20 * 20,
        );

        // Target: <50ms por read en debug builds. En release suele ser <10ms.
        // Si supera 50ms, la cadencia detect_interval=15 ya no cabe en el tick budget.
        assert!(
            per_read_ms < 50.0,
            "InventoryReader::read() demasiado lento: {:.2}ms (target <50ms)",
            per_read_ms
        );
    }
}
