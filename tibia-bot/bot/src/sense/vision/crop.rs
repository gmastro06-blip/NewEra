/// crop.rs — Helpers para recortar regiones de interés (ROI) de un Frame BGRA.
///
/// Todos los recortes trabajan sobre datos BGRA contiguos en memoria.
/// El stride por fila es `width * 4` bytes (garantizado por el NDI receiver).

use crate::sense::frame_buffer::Frame;

/// Región de interés: posición y dimensiones en píxeles del frame completo.
#[derive(Debug, Clone, Copy)]
pub struct Roi {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Roi {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Comprueba que el ROI cabe dentro de un frame de las dimensiones dadas.
    pub fn fits_in(&self, frame_w: u32, frame_h: u32) -> bool {
        self.x + self.w <= frame_w && self.y + self.h <= frame_h
    }

    #[allow(dead_code)] // extension point
    pub fn pixel_count(&self) -> usize {
        (self.w * self.h) as usize
    }
}

/// Extrae los datos BGRA de un ROI del frame como un Vec<u8> contiguo.
/// Retorna None si el ROI no cabe en el frame.
pub fn crop_bgra(frame: &Frame, roi: Roi) -> Option<Vec<u8>> {
    if !roi.fits_in(frame.width, frame.height) {
        return None;
    }
    let stride = frame.width as usize * 4;
    let row_bytes = roi.w as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * roi.h as usize);
    for row in 0..roi.h as usize {
        let y = roi.y as usize + row;
        let start = y * stride + roi.x as usize * 4;
        out.extend_from_slice(&frame.data[start..start + row_bytes]);
    }
    Some(out)
}

/// Itera sobre los píxeles BGRA de un ROI, llamando `f(x, y, &px[4])` por cada uno.
/// Más eficiente que `crop_bgra` cuando no necesitamos almacenar todos los píxeles.
pub fn iter_roi<F>(frame: &Frame, roi: Roi, mut f: F)
where
    F: FnMut(u32, u32, &[u8]),
{
    if !roi.fits_in(frame.width, frame.height) {
        return;
    }
    let stride = frame.width as usize * 4;
    for row in 0..roi.h {
        let y = roi.y + row;
        for col in 0..roi.w {
            let x = roi.x + col;
            let off = y as usize * stride + x as usize * 4;
            f(col, row, &frame.data[off..off + 4]);
        }
    }
}

/// Cuenta cuántos píxeles en el ROI satisfacen el predicado `pred(&px[4]) -> bool`.
pub fn count_pixels<F>(frame: &Frame, roi: Roi, pred: F) -> u32
where
    F: Fn(&[u8]) -> bool,
{
    let mut count = 0u32;
    iter_roi(frame, roi, |_x, _y, px| {
        if pred(px) {
            count += 1;
        }
    });
    count
}

/// Longest horizontal contiguous run of pixels matching `pred` within the ROI.
///
/// Scans each row independently. For each row, tracks the current run of
/// consecutive matching pixels and updates the global maximum across rows.
///
/// **Uso clave 2026-04-18**: el battle list detector HP-bar usaba total pixel
/// count. Las líneas de texto rojo del NPC chat ("Aelzerand Neeymas: hi"
/// renderizadas en rojo Tibia) tienen MUCHOS pixeles colored pero
/// DISCONTINUOS (gaps entre letras). Un HP bar real es un strip horizontal
/// CONTINUO (≥30 pixeles consecutivos típicamente). Distinguir por run
/// contiguo elimina el false positive.
pub fn longest_horizontal_run<F>(frame: &Frame, roi: Roi, pred: F) -> u32
where
    F: Fn(&[u8]) -> bool,
{
    if !roi.fits_in(frame.width, frame.height) {
        return 0;
    }
    let stride = frame.width as usize * 4;
    let mut max_run = 0u32;
    for row in 0..roi.h {
        let y = (roi.y + row) as usize;
        let base = y * stride;
        let mut current_run = 0u32;
        for col in 0..roi.w {
            let x = (roi.x + col) as usize;
            let off = base + x * 4;
            if off + 3 >= frame.data.len() {
                break;
            }
            let px = &frame.data[off..off + 4];
            if pred(px) {
                current_run += 1;
                if current_run > max_run {
                    max_run = current_run;
                }
            } else {
                current_run = 0;
            }
        }
    }
    max_run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn make_frame(w: u32, h: u32, fill: u8) -> Frame {
        Frame {
            width:       w,
            height:      h,
            data:        vec![fill; (w * h * 4) as usize],
            captured_at: Instant::now(),
        }
    }

    #[test]
    fn crop_fits_in_check() {
        let roi = Roi::new(0, 0, 10, 10);
        assert!(roi.fits_in(10, 10));
        assert!(!roi.fits_in(9, 10));
    }

    #[test]
    fn crop_bgra_returns_correct_size() {
        let frame = make_frame(100, 100, 0xAA);
        let roi = Roi::new(10, 10, 20, 5);
        let data = crop_bgra(&frame, roi).unwrap();
        assert_eq!(data.len(), 20 * 5 * 4);
        assert!(data.iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn crop_bgra_out_of_bounds_returns_none() {
        let frame = make_frame(10, 10, 0);
        let roi = Roi::new(5, 5, 10, 10); // goes out of bounds
        assert!(crop_bgra(&frame, roi).is_none());
    }

    #[test]
    fn count_pixels_all_match() {
        let frame = make_frame(4, 4, 0xFF);
        let roi = Roi::new(0, 0, 4, 4);
        let n = count_pixels(&frame, roi, |px| px[0] == 0xFF);
        assert_eq!(n, 16);
    }

}
