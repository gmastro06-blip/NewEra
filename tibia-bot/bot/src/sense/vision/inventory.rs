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
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // public API, used by tests + downstream consumers
pub struct InventoryReading {
    pub slot_counts:  HashMap<String, u32>,
    pub stack_totals: HashMap<String, u32>,
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
            // Usar threshold per-template si está en el TOML, else default.
            let threshold = thresholds.get(&name).copied().unwrap_or(MATCH_THRESHOLD);
            let threshold_note = if (threshold - MATCH_THRESHOLD).abs() > f32::EPSILON {
                format!(" threshold={:.3}", threshold)
            } else {
                String::new()
            };
            tracing::info!(
                "InventoryReader: template '{}' cargado ({}×{}){}",
                name, template.width(), template.height(), threshold_note
            );
            self.templates.push(ItemTemplate { name, template, threshold });
        }
    }

    /// Setea los ROIs de los slots del inventario a escanear.
    pub fn set_slots(&mut self, slots: Vec<RoiDef>) {
        self.slots = slots;
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
            let Some(patch_bgra) = crop_bgra(frame, Roi::new(slot.x, slot.y, slot.w, slot.h)) else {
                continue;
            };
            let luma = bgra_to_luma(&patch_bgra, slot.w, slot.h);
            let Some(slot_img) = GrayImage::from_raw(slot.w, slot.h, luma) else { continue };

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
    #[allow(dead_code)] // public API
    pub fn read_with_stacks(&self, frame: &Frame) -> InventoryReading {
        let mut slot_counts: HashMap<String, u32> = HashMap::new();
        let mut stack_totals: HashMap<String, u32> = HashMap::new();
        if self.is_empty() {
            return InventoryReading { slot_counts, stack_totals };
        }
        // 1. Collect: match por slot, guardar scores crudos + stacks por template.
        //    Usamos Vec<(score, stack)> para poder aplicar NMS después manteniendo
        //    la correspondencia score↔stack (ambos se filtran al descartar el slot).
        let mut data_per_template: HashMap<String, Vec<(f32, u32)>> = HashMap::new();
        for slot in &self.slots {
            let Some(patch_bgra) = crop_bgra(frame, Roi::new(slot.x, slot.y, slot.w, slot.h)) else {
                continue;
            };
            let luma = bgra_to_luma(&patch_bgra, slot.w, slot.h);
            let Some(slot_img) = GrayImage::from_raw(slot.w, slot.h, luma) else { continue };

            if let Some((score, name)) = best_match_for_slot(&slot_img, &self.templates) {
                let stack = if !self.digit_templates.is_empty() {
                    super::inventory_ocr::read_slot_count(&slot_img, &self.digit_templates)
                        .unwrap_or(1)
                } else {
                    1
                };
                data_per_template.entry(name).or_default().push((score, stack));
            }
        }
        // 2. NMS cross-slot: por template, filtrar slots fuera del gap del top score.
        for (name, mut data) in data_per_template {
            if data.is_empty() {
                continue;
            }
            data.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let top_score = data[0].0;
            let kept: Vec<(f32, u32)> = data.into_iter()
                .filter(|(s, _)| *s >= top_score - NMS_SCORE_GAP)
                .collect();
            slot_counts.insert(name.clone(), kept.len() as u32);
            stack_totals.insert(name, kept.iter().map(|(_, s)| *s).sum());
        }
        InventoryReading { slot_counts, stack_totals }
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

    #[test]
    fn reader_empty_with_only_slots() {
        let mut reader = InventoryReader::new();
        reader.set_slots(vec![RoiDef::new(0, 0, 32, 32)]);
        assert!(reader.is_empty()); // sin templates = empty
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
        // Llenar cada pixel con un patrón ligero.
        for i in 0..(w * h) as usize {
            let v = (i % 200) as u8;
            data[i * 4]     = v;       // B
            data[i * 4 + 1] = v + 30;  // G
            data[i * 4 + 2] = v + 60;  // R
            data[i * 4 + 3] = 255;     // A
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

    #[test]
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
