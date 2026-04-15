/// hp_mana.rs — Lectura de HP y mana por muestreo de píxeles.
///
/// Estrategia: las barras de HP (verde) y mana (azul) en Tibia tienen un color
/// distintivo. Muestreamos píxeles a lo largo de la barra para contar cuántos
/// son del color esperado. La proporción de píxeles "llenos" vs total da el ratio.
///
/// No se hace OCR — solo análisis de color, lo que es robusto y rápido.

use crate::sense::frame_buffer::Frame;
use crate::sense::perception::VitalBar;
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::color::{is_bar_filled, is_hp_green, is_mana_blue};
use crate::sense::vision::crop::{count_pixels, Roi};

/// Lee la barra de HP del frame.
/// Retorna None si el ROI no cabe en el frame.
#[allow(dead_code)] // extension point: single-phase HP reading (reemplazado por read_hp_by_edge)
pub fn read_hp(frame: &Frame, roi: RoiDef) -> Option<VitalBar> {
    read_bar(frame, roi, is_hp_green)
}

/// Lee la barra de mana del frame.
#[allow(dead_code)] // extension point: single-phase mana reading (reemplazado por read_mana_by_edge)
pub fn read_mana(frame: &Frame, roi: RoiDef) -> Option<VitalBar> {
    read_bar(frame, roi, is_mana_blue)
}

/// Lee una barra vital genérica contando los píxeles que satisfacen `pred`.
#[allow(dead_code)]
fn read_bar<F>(frame: &Frame, roi: RoiDef, pred: F) -> Option<VitalBar>
where
    F: Fn(&[u8]) -> bool,
{
    let r = Roi::new(roi.x, roi.y, roi.w, roi.h);
    if !r.fits_in(frame.width, frame.height) {
        return None;
    }
    let total_px = r.pixel_count() as u32;
    if total_px == 0 {
        return None;
    }
    let filled_px = count_pixels(frame, r, pred);
    Some(VitalBar::new(filled_px, total_px))
}

/// Cuenta columnas con al menos `min_hits` píxeles del color esperado.
/// NO busca el "borde" de la barra — cuenta el total de columnas activas.
///
/// Ventajas sobre el algoritmo de borde:
/// - Inmune a texto superpuesto ("1500/1500"): el texto no cubre TODOS los
///   píxeles de una columna, entonces la columna igual cuenta.
/// - Inmune a artefactos de compresión NDI al inicio de la barra: si los
///   primeros píxeles son artefactos, las columnas intermedias siguen contando.
/// - Error predecible: el texto puede tapar ~45px de 386 → subreporte ~11%
///   máximo, pero estable (no produce 0% ni valores falsos).
///
/// Para decisiones de FSM ("hp < 50%") este error es aceptable.
pub fn read_bar_by_count<F>(
    frame:    &Frame,
    roi:      RoiDef,
    pred:     F,
    min_hits: u32,
) -> Option<VitalBar>
where
    F: Fn(&[u8]) -> bool,
{
    let r = Roi::new(roi.x, roi.y, roi.w, roi.h);
    if !r.fits_in(frame.width, frame.height) || roi.w == 0 || roi.h == 0 {
        return None;
    }

    let mut filled_cols = 0u32;
    for col_x in 0..roi.w {
        let col_roi = Roi::new(roi.x + col_x, roi.y, 1, roi.h);
        if count_pixels(frame, col_roi, &pred) >= min_hits {
            filled_cols += 1;
        }
    }

    Some(VitalBar::new(filled_cols, roi.w))
}

/// Lee HP usando crominancia: inmune al cambio de color verde→amarillo→rojo.
/// Cuenta columnas con ≥1 píxel de alta saturación (no-gris-fondo).
pub fn read_hp_by_edge(frame: &Frame, roi: RoiDef) -> Option<VitalBar> {
    read_bar_by_count(frame, roi, is_bar_filled, 1)
}

/// Lee mana usando crominancia: inmune a variaciones de saturación del azul.
pub fn read_mana_by_edge(frame: &Frame, roi: RoiDef) -> Option<VitalBar> {
    read_bar_by_count(frame, roi, is_bar_filled, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Crea un frame con la barra de HP (verde puro) llenada hasta `fill_w` columnas.
    fn make_hp_frame(bar_w: u32, bar_h: u32, fill_w: u32) -> (Frame, RoiDef) {
        let width  = 200u32;
        let height = 50u32;
        let bar_x  = 10u32;
        let bar_y  = 20u32;
        let mut data = vec![50u8; (width * height * 4) as usize]; // fondo gris

        // Pintar barra verde
        let stride = width as usize * 4;
        for row in 0..bar_h {
            for col in 0..fill_w {
                let off = (bar_y + row) as usize * stride + (bar_x + col) as usize * 4;
                // BGRA: B=30, G=200, R=20, A=255 → verde HP
                data[off]     = 30;
                data[off + 1] = 200;
                data[off + 2] = 20;
                data[off + 3] = 255;
            }
            // Resto de la barra: fondo gris (ya inicializado)
        }

        let frame = Frame { width, height, data, captured_at: Instant::now() };
        let roi   = RoiDef::new(bar_x, bar_y, bar_w, bar_h);
        (frame, roi)
    }

    #[test]
    fn full_hp_bar() {
        let (frame, roi) = make_hp_frame(100, 8, 100);
        let bar = read_hp(&frame, roi).unwrap();
        // Todos los píxeles son verde → ratio muy alto (>0.9)
        assert!(bar.ratio > 0.9, "ratio = {}", bar.ratio);
    }

    #[test]
    fn half_hp_bar() {
        let (frame, roi) = make_hp_frame(100, 8, 50);
        let bar = read_hp(&frame, roi).unwrap();
        // ~50% del área debería ser verde
        assert!(bar.ratio > 0.4 && bar.ratio < 0.6, "ratio = {}", bar.ratio);
    }

    #[test]
    fn empty_hp_bar() {
        let (frame, roi) = make_hp_frame(100, 8, 0);
        let bar = read_hp(&frame, roi).unwrap();
        assert_eq!(bar.filled_px, 0);
        assert!(bar.ratio < 0.01, "ratio = {}", bar.ratio);
    }

    #[test]
    fn out_of_bounds_roi_returns_none() {
        let (frame, _) = make_hp_frame(100, 8, 50);
        let bad_roi = RoiDef::new(190, 45, 100, 8); // excede dimensiones
        assert!(read_hp(&frame, bad_roi).is_none());
    }

    #[test]
    fn critical_threshold() {
        let (frame, roi) = make_hp_frame(100, 8, 25);
        let bar = read_hp(&frame, roi).unwrap();
        assert!(bar.is_critical(0.30), "ratio = {}", bar.ratio);
        assert!(!bar.is_critical(0.20), "ratio = {}", bar.ratio);
    }

    #[test]
    fn edge_algorithm_half_bar() {
        let (frame, roi) = make_hp_frame(100, 8, 50);
        let bar = read_hp_by_edge(&frame, roi).unwrap();
        // El borde debería detectar ~50 columnas llenas de 100
        assert!(bar.ratio > 0.4 && bar.ratio < 0.6, "edge ratio = {}", bar.ratio);
    }

    #[test]
    fn vital_bar_ratio_clamped() {
        let bar = VitalBar::new(0, 0);
        assert_eq!(bar.ratio, 0.0);
        let bar = VitalBar::new(100, 100);
        assert!((bar.ratio - 1.0).abs() < 1e-6);
    }

}
