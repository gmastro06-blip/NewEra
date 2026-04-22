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
        let mut counts: HashMap<String, u32> = HashMap::new();
        if self.is_empty() {
            return counts;
        }
        for slot in &self.slots {
            // Extraer pixels del slot (BGRA) y convertir a luma8 para matching.
            let Some(patch_bgra) = crop_bgra(frame, Roi::new(slot.x, slot.y, slot.w, slot.h)) else {
                continue;
            };
            let luma = bgra_to_luma(&patch_bgra, slot.w, slot.h);
            let slot_img = GrayImage::from_raw(slot.w, slot.h, luma);
            let Some(slot_img) = slot_img else { continue };

            // Match cada template contra este slot — encontrar el MEJOR
            // (no el primero alfabético). Esto evita que templates parecidos
            // ganen por orden de iteración.
            // Guarda (score, name, threshold) del best match para comparar
            // con su threshold específico — no con el global.
            let mut best: Option<(f32, &str, f32)> = None;
            for tpl in &self.templates {
                if slot_img.width() < tpl.template.width()
                    || slot_img.height() < tpl.template.height()
                {
                    continue;
                }
                let result = match_template(
                    &slot_img,
                    &tpl.template,
                    MatchTemplateMethod::CrossCorrelationNormalized,
                );
                // CCORR_NORMED: **mayor = mejor**, buscamos el max.
                let score = result.iter().cloned().fold(f32::MIN, f32::max);
                let current_best_score = best.map_or(f32::MIN, |(s, _, _)| s);
                if score > current_best_score {
                    best = Some((score, tpl.name.as_str(), tpl.threshold));
                }
            }
            if let Some((score, name, threshold)) = best {
                if score >= threshold {
                    *counts.entry(name.to_string()).or_insert(0) += 1;
                }
            }
        }
        counts
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
        for slot in &self.slots {
            let Some(patch_bgra) = crop_bgra(frame, Roi::new(slot.x, slot.y, slot.w, slot.h)) else {
                continue;
            };
            let luma = bgra_to_luma(&patch_bgra, slot.w, slot.h);
            let Some(slot_img) = GrayImage::from_raw(slot.w, slot.h, luma) else { continue };

            // Best-match (no first-match) para evitar bias alfabético.
            // Guarda threshold per-template del best match.
            let mut best: Option<(f32, &str, f32)> = None;
            for tpl in &self.templates {
                if slot_img.width() < tpl.template.width()
                    || slot_img.height() < tpl.template.height()
                {
                    continue;
                }
                let result = match_template(
                    &slot_img, &tpl.template,
                    MatchTemplateMethod::CrossCorrelationNormalized,
                );
                let score = result.iter().cloned().fold(f32::MIN, f32::max);
                let current_best_score = best.map_or(f32::MIN, |(s, _, _)| s);
                if score > current_best_score {
                    best = Some((score, tpl.name.as_str(), tpl.threshold));
                }
            }
            if let Some((score, name, threshold)) = best {
                if score >= threshold {
                    let name_string = name.to_string();
                    *slot_counts.entry(name_string.clone()).or_insert(0) += 1;
                    let stack = if !self.digit_templates.is_empty() {
                        super::inventory_ocr::read_slot_count(&slot_img, &self.digit_templates)
                            .unwrap_or(1)
                    } else {
                        1
                    };
                    *stack_totals.entry(name_string).or_insert(0) += stack;
                }
            }
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
