/// status_icons.rs — Detección de iconos de condición por template matching.
///
/// Los iconos de condición (veneno, quemado, etc.) son pequeños sprites (~11x11 px)
/// que aparecen en la barra de estado de Tibia. Buscamos cada template conocido
/// dentro del ROI de iconos de estado usando SSD normalizado de imageproc.
///
/// Templates: assets/templates/status/<nombre>.png (escala de grises).

use imageproc::template_matching::{match_template, MatchTemplateMethod};
use image::GrayImage;

use crate::sense::frame_buffer::Frame;
use crate::sense::perception::{Condition, StatusConditions};
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::templates::TemplateStore;

/// Umbral máximo de SSD normalizado para considerar que un icono está presente.
/// Ajustar experimentalmente con frame_reference.png.
const MATCH_THRESHOLD: f32 = 0.15;

/// Mapeo de nombre de archivo template → Condition.
/// El nombre debe coincidir con el PNG en assets/templates/status/.
fn template_name_to_condition(name: &str) -> Option<Condition> {
    match name {
        "poisoned"      => Some(Condition::Poisoned),
        "burning"       => Some(Condition::Burning),
        "electrified"   => Some(Condition::Electrified),
        "drowning"      => Some(Condition::Drowning),
        "freezing"      => Some(Condition::Freezing),
        "dazzled"       => Some(Condition::Dazzled),
        "cursed"        => Some(Condition::Cursed),
        "bleeding"      => Some(Condition::Bleeding),
        "haste"         => Some(Condition::Haste),
        "protection"    => Some(Condition::Protection),
        "strengthened"  => Some(Condition::Strengthened),
        "infight"       => Some(Condition::InFight),
        "hungry"        => Some(Condition::Hungry),
        "drunk"         => Some(Condition::Drunk),
        "magic_shield"  => Some(Condition::MagicShield),
        "slowed"        => Some(Condition::SlowedDown),
        _               => None,
    }
}

/// Detecta qué condiciones están activas buscando cada template en el ROI de iconos.
pub fn read_status_icons(
    frame:     &Frame,
    icons_roi: RoiDef,
    templates: &TemplateStore,
) -> StatusConditions {
    if templates.is_empty() {
        return StatusConditions::default();
    }

    // Extraer el parche del ROI como imagen en escala de grises.
    let patch = extract_gray_from_frame(frame, icons_roi);
    if patch.width() == 0 || patch.height() == 0 {
        return StatusConditions::default();
    }

    let mut active = Vec::new();

    for (name, template) in &templates.templates {
        let condition = match template_name_to_condition(name) {
            Some(c) => c,
            None    => continue,
        };

        if template.width() > patch.width() || template.height() > patch.height() {
            continue;
        }

        if template_present(&patch, template, MATCH_THRESHOLD) {
            active.push(condition);
        }
    }

    StatusConditions { active }
}

/// Retorna true si `template` está presente en `patch` con score ≤ threshold.
fn template_present(patch: &GrayImage, template: &GrayImage, threshold: f32) -> bool {
    let scores = match_template(patch, template, MatchTemplateMethod::SumOfSquaredErrorsNormalized);
    scores
        .pixels()
        .any(|px| px[0] <= threshold)
}

/// Extrae un parche BGRA del frame como GrayImage.
/// Duplicado local para evitar pub en anchors (sería circular).
fn extract_gray_from_frame(frame: &Frame, roi: RoiDef) -> GrayImage {
    let stride = frame.width as usize * 4;
    let w = roi.w.min(frame.width.saturating_sub(roi.x));
    let h = roi.h.min(frame.height.saturating_sub(roi.y));
    if w == 0 || h == 0 { return GrayImage::new(0, 0); }

    let mut img = GrayImage::new(w, h);
    for row in 0..h {
        for col in 0..w {
            let off = (roi.y + row) as usize * stride + (roi.x + col) as usize * 4;
            if off + 2 < frame.data.len() {
                let b = frame.data[off]     as u32;
                let g = frame.data[off + 1] as u32;
                let r = frame.data[off + 2] as u32;
                let lum = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
                img.put_pixel(col, row, image::Luma([lum]));
            }
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_known_conditions_have_template_name() {
        let conditions = [
            "poisoned", "burning", "electrified", "drowning",
            "freezing", "dazzled", "cursed", "bleeding",
            "haste", "protection", "strengthened", "infight",
            "hungry", "drunk", "magic_shield", "slowed",
        ];
        for name in &conditions {
            assert!(
                template_name_to_condition(name).is_some(),
                "Condición sin mapeo: {}", name
            );
        }
    }

    #[test]
    fn unknown_template_name_returns_none() {
        assert!(template_name_to_condition("nonexistent").is_none());
    }

    #[test]
    fn empty_template_store_returns_no_conditions() {
        use std::time::Instant;
        let frame = crate::sense::frame_buffer::Frame {
            width: 100, height: 100,
            data: vec![128u8; 100 * 100 * 4],
            captured_at: Instant::now(),
        };
        let store = TemplateStore {
            templates: std::collections::HashMap::new(),
            base_dir:  std::path::PathBuf::new(),
        };
        let roi = RoiDef::new(0, 0, 100, 20);
        let conditions = read_status_icons(&frame, roi, &store);
        assert!(conditions.active.is_empty());
    }

    // ── Helper: construye un frame sintético con un template stampado ────
    //
    // Los tests siguientes usan templates en memoria (sin leer PNGs de disco)
    // para verificar el pipeline de detección de principio a fin.

    fn make_blank_frame(w: u32, h: u32, fill: u8) -> crate::sense::frame_buffer::Frame {
        crate::sense::frame_buffer::Frame {
            width: w,
            height: h,
            data: vec![fill; (w * h * 4) as usize],
            captured_at: std::time::Instant::now(),
        }
    }

    /// Crea un template 11×11 con un patrón distintivo según `seed` (0-255).
    fn synthetic_template(seed: u8) -> GrayImage {
        let mut img = GrayImage::new(11, 11);
        for y in 0..11 {
            for x in 0..11 {
                let v = seed.wrapping_add((x * 7 + y * 13) as u8);
                img.put_pixel(x, y, image::Luma([v]));
            }
        }
        img
    }

    /// Estampa un GrayImage en el frame como BGRA (B=G=R=valor, A=255).
    fn stamp_template_into_frame(
        frame: &mut crate::sense::frame_buffer::Frame,
        tpl: &GrayImage,
        x: u32, y: u32,
    ) {
        let stride = frame.width as usize * 4;
        for ty in 0..tpl.height() {
            for tx in 0..tpl.width() {
                let lum = tpl.get_pixel(tx, ty)[0];
                let off = (y + ty) as usize * stride + (x + tx) as usize * 4;
                if off + 3 < frame.data.len() {
                    frame.data[off]     = lum; // B
                    frame.data[off + 1] = lum; // G
                    frame.data[off + 2] = lum; // R
                    frame.data[off + 3] = 255;
                }
            }
        }
    }

    #[test]
    fn detects_single_template_at_known_position() {
        // Frame 100×100 gris. Estampar template "poisoned" en (20, 5).
        let mut frame = make_blank_frame(100, 100, 128);
        let tpl = synthetic_template(50);
        stamp_template_into_frame(&mut frame, &tpl, 20, 5);

        let mut templates = std::collections::HashMap::new();
        templates.insert("poisoned".to_string(), tpl);
        let store = TemplateStore { templates, base_dir: std::path::PathBuf::new() };

        // ROI que cubre donde estampamos.
        let roi = RoiDef::new(0, 0, 100, 20);
        let result = read_status_icons(&frame, roi, &store);
        assert!(result.active.contains(&Condition::Poisoned),
            "esperaba Poisoned, got {:?}", result.active);
    }

    #[test]
    fn distinct_templates_dont_false_match() {
        // Dos templates muy distintos. Estampar solo "poisoned" en el frame.
        // Verificar que "burning" NO detecta (el matching tiene threshold bajo).
        let mut frame = make_blank_frame(100, 100, 128);
        let tpl_poisoned = synthetic_template(50);
        let tpl_burning = synthetic_template(200); // distinto seed
        stamp_template_into_frame(&mut frame, &tpl_poisoned, 10, 5);

        let mut templates = std::collections::HashMap::new();
        templates.insert("poisoned".to_string(), tpl_poisoned);
        templates.insert("burning".to_string(), tpl_burning);
        let store = TemplateStore { templates, base_dir: std::path::PathBuf::new() };

        let roi = RoiDef::new(0, 0, 100, 20);
        let result = read_status_icons(&frame, roi, &store);
        assert!(result.active.contains(&Condition::Poisoned));
        assert!(!result.active.contains(&Condition::Burning),
            "burning no debe matchear (solo estampamos poisoned)");
    }

    #[test]
    fn roi_outside_frame_returns_empty() {
        let frame = make_blank_frame(50, 50, 100);
        let mut templates = std::collections::HashMap::new();
        templates.insert("poisoned".to_string(), synthetic_template(42));
        let store = TemplateStore { templates, base_dir: std::path::PathBuf::new() };
        // ROI fuera del frame.
        let roi = RoiDef::new(100, 100, 20, 20);
        let result = read_status_icons(&frame, roi, &store);
        assert!(result.active.is_empty());
    }

    #[test]
    fn template_larger_than_patch_is_skipped() {
        // Template 30×30 en un ROI 20×20 → skip sin crashear.
        let frame = make_blank_frame(100, 100, 100);
        let mut big_tpl = GrayImage::new(30, 30);
        for y in 0..30 { for x in 0..30 {
            big_tpl.put_pixel(x, y, image::Luma([50]));
        }}
        let mut templates = std::collections::HashMap::new();
        templates.insert("poisoned".to_string(), big_tpl);
        let store = TemplateStore { templates, base_dir: std::path::PathBuf::new() };
        let roi = RoiDef::new(0, 0, 20, 20);
        let result = read_status_icons(&frame, roi, &store);
        // Sin crash. Puede o no detectar (el patch será 20×20).
        let _ = result;
    }

    #[test]
    fn match_threshold_constant_is_stable() {
        // Regresión: si alguien baja el threshold a 0, casi todo matchearía.
        // Si lo sube a 1.0, nada. Validamos el rango razonable.
        assert!(MATCH_THRESHOLD > 0.0);
        assert!(MATCH_THRESHOLD < 0.5);
    }
}
