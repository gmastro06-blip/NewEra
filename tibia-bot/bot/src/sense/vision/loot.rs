//! loot.rs — Detector de "loot sparkles" en el viewport.
//!
//! Tibia pinta un anillo de puntos blancos/brillantes alrededor de corpses
//! que tienen items looteables. El anillo persiste hasta que el corpse
//! se lootea (mediante Quick Loot, Open Next Corpse, o click manual).
//!
//! Este detector cuenta píxeles "blanco puro saturado" en un ROI centrado
//! en el personaje (los 8 tiles adyacentes + el tile del char = área 3×3).
//! Los mobs siempre dejan corpse en el tile donde murieron — y siempre a
//! distancia melee del char — así que el área 3×3 cubre todos los casos.
//!
//! ## Por qué este signal es mejor que kill-decrement
//!
//! | Enfoque | Fiabilidad | Problema |
//! |---|---|---|
//! | `enemy_count` baja → emit loot | Media | Detector puede perder kills. Corpses vacíos también cuentan. |
//! | Sparkles visibles → emit loot | **Alta** | Signal DIRECTA. Zero falsos positivos (solo aparece con loot). Auto-retry si el emit falla. |
//!
//! ## Implementación
//!
//! - Color match: R≈G≈B y todos los tres ≥ 220 (blanco puro saturado)
//! - Diferencia entre canales <15 (excluye amarillos de spells, oranges de fire)
//! - ROI calculada al runtime como `game_viewport.center() ± 1.5 * tile_size`
//!
//! ## Calibración
//!
//! Un sparkle individual son ~10-15 píxeles blancos en un anillo. Threshold
//! sugerido: `sparkles_visible` cuando `count >= 8`. Ajustable según lo que
//! veamos in-game con `/vision/loot/debug`.

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::crop::{count_pixels, Roi};

/// Tamaño estándar de tile en Tibia a zoom default (64×64 px).
/// Configurable via `config.vision.loot_tile_size` si el user tiene zoom distinto.
#[allow(dead_code)] // extension point: configurable tile size
pub const DEFAULT_TILE_SIZE: u32 = 64;

/// Threshold ALTO (armed): píxeles blancos para **activar** el detector.
/// Un sparkle ring tiene ~10-15 px blancos. Con 8 dejamos margen para
/// capturar un sparkle parcialmente oculto.
pub const LOOT_SPARKLE_THRESHOLD: u32 = 8;

/// Threshold BAJO (disarmed): para **desarmar** el detector, sparkles
/// tienen que bajar por debajo de este valor. Implementa hysteresis —
/// sparkles oscilando entre 6-10 por noise NO causa rising edges múltiples.
///
/// Valor elegido conservador: 2 píxeles es prácticamente "frame limpio sin
/// corpses visibles" — deja margen para JPEG noise del NDI stream sin dejar
/// pasar falsos desarmes.
#[allow(dead_code)] // extension point: hysteresis config
pub const LOOT_DISARM_THRESHOLD: u32 = 2;

/// Máximo de emits consecutivos mientras el detector está "armed" sin
/// que los sparkles bajen al threshold de disarm.
///
/// **Ajustado a 1** tras observar in-vivo que Quick Loot (free account)
/// toma todo en una sola pulsación — emits subsiguientes son redundantes
/// y se traducen en "No loot" messages cosméticos.
///
/// Con MAX_EMITS=1:
///   - Rising edge → 1 emit → Quick Loot ejecuta
///   - Sparkles se apagan tras loot real → disarm natural en ≤4s
///   - Si aparecen nuevos corpses después del disarm → nuevo rising edge → 1 emit más
///   - Ratio esperado loots:kills = 1:1 en hunts normales
///
/// Si en algún momento un combate genera más corpses de los que Quick Loot
/// puede limpiar en una pasada, el bot esperará al disarm (4s) y re-armará
/// para el siguiente intento. Máximo tipo "1 emit cada 4s mientras haya loot",
/// que es humanamente razonable.
///
/// Valor histórico: 3. Causaba 3 emits por grupo (incluyendo 2 sustained
/// innecesarios cuando Quick Loot ya había limpiado todo). Ratio 3:1 observado.
#[allow(dead_code)] // extension point: emit budget
pub const LOOT_MAX_EMITS_WHILE_ARMED: u8 = 1;

/// Computa el ROI del área de loot como cuadrado 3×3 tiles centrado en el
/// viewport del juego. Asume que el char siempre está en el centro visual
/// del viewport (comportamiento estándar del cliente Tibia).
///
/// Retorna `None` si el viewport es más pequeño que el área de loot que
/// queríamos (caso degenerado).
pub fn compute_loot_area(viewport: RoiDef, tile_size: u32) -> Option<RoiDef> {
    let area_size = tile_size * 3; // 3×3 tiles
    if viewport.w < area_size || viewport.h < area_size {
        return None;
    }
    // Centro del viewport.
    let cx = viewport.x + viewport.w / 2;
    let cy = viewport.y + viewport.h / 2;
    // Top-left del área 3×3 centrado en cx,cy.
    let half = area_size / 2;
    Some(RoiDef::new(
        cx.saturating_sub(half),
        cy.saturating_sub(half),
        area_size,
        area_size,
    ))
}

/// Cuenta píxeles "blanco puro saturado" dentro del ROI dado. Un píxel es
/// sparkle-like si los 3 canales son muy altos (≥220) y similares entre sí
/// (diff <15), lo cual excluye amarillos/oranges de spells y fuego.
pub fn count_sparkle_pixels(frame: &Frame, roi: RoiDef) -> u32 {
    let scan = Roi::new(roi.x, roi.y, roi.w, roi.h);
    if !scan.fits_in(frame.width, frame.height) {
        return 0;
    }
    count_pixels(frame, scan, is_sparkle_pixel)
}

/// Predicado: `true` si el píxel es "blanco puro saturado" (loot sparkle).
#[inline]
pub fn is_sparkle_pixel(px: &[u8]) -> bool {
    let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
    // Canales todos muy altos.
    if r < 220 || g < 220 || b < 220 {
        return false;
    }
    // Y similares entre sí (blanco puro, no amarillo/naranja).
    (r - g).abs() < 15 && (g - b).abs() < 15 && (r - b).abs() < 15
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn make_frame(w: u32, h: u32) -> Frame {
        Frame {
            width:       w,
            height:      h,
            data:        vec![0x20u8; (w * h * 4) as usize], // fondo oscuro
            captured_at: Instant::now(),
        }
    }

    fn paint_pixel(frame: &mut Frame, x: u32, y: u32, r: u8, g: u8, b: u8) {
        let stride = frame.width as usize * 4;
        let off = y as usize * stride + x as usize * 4;
        if off + 3 < frame.data.len() {
            frame.data[off]     = r;
            frame.data[off + 1] = g;
            frame.data[off + 2] = b;
            frame.data[off + 3] = 0xFF;
        }
    }

    #[test]
    fn sparkle_pixel_white_passes() {
        assert!(is_sparkle_pixel(&[255, 255, 255, 255]));
        assert!(is_sparkle_pixel(&[240, 245, 238, 255]));
        assert!(is_sparkle_pixel(&[220, 220, 220, 255]));
    }

    #[test]
    fn sparkle_pixel_yellow_rejected() {
        // Amarillo saturado (R alto, G alto, B bajo) → no es sparkle.
        assert!(!is_sparkle_pixel(&[255, 255, 100, 255]));
        // Naranja
        assert!(!is_sparkle_pixel(&[255, 180, 80, 255]));
    }

    #[test]
    fn sparkle_pixel_dark_rejected() {
        assert!(!is_sparkle_pixel(&[200, 200, 200, 255]));
        assert!(!is_sparkle_pixel(&[50, 50, 50, 255]));
    }

    #[test]
    fn sparkle_pixel_slightly_tinted_rejected() {
        // Canales demasiado desiguales (diff >= 15).
        assert!(!is_sparkle_pixel(&[250, 220, 250, 255]));
    }

    #[test]
    fn compute_loot_area_centered_on_viewport() {
        // Viewport 967x708 at (388,94) como en calibration real.
        let vp = RoiDef::new(388, 94, 967, 708);
        let area = compute_loot_area(vp, 64).unwrap();
        // Centro viewport = (388+483, 94+354) = (871, 448)
        // area_size = 192, half = 96
        // Top-left = (871-96, 448-96) = (775, 352)
        assert_eq!(area.x, 775);
        assert_eq!(area.y, 352);
        assert_eq!(area.w, 192);
        assert_eq!(area.h, 192);
    }

    #[test]
    fn compute_loot_area_with_custom_tile_size() {
        let vp = RoiDef::new(0, 0, 1000, 1000);
        // tile_size=48 → area_size=144
        let area = compute_loot_area(vp, 48).unwrap();
        assert_eq!(area.w, 144);
        assert_eq!(area.h, 144);
        // Centro = (500, 500), half=72, top-left=(428, 428)
        assert_eq!(area.x, 428);
        assert_eq!(area.y, 428);
    }

    #[test]
    fn compute_loot_area_viewport_too_small_returns_none() {
        let vp = RoiDef::new(0, 0, 100, 100);
        // 100 < 192 → None
        assert!(compute_loot_area(vp, 64).is_none());
    }

    #[test]
    fn count_sparkle_pixels_zero_on_dark_frame() {
        let frame = make_frame(400, 400);
        let roi = RoiDef::new(100, 100, 200, 200);
        assert_eq!(count_sparkle_pixels(&frame, roi), 0);
    }

    #[test]
    fn count_sparkle_pixels_counts_painted_whites() {
        let mut frame = make_frame(400, 400);
        let roi = RoiDef::new(100, 100, 200, 200);
        // Pintar 12 pixeles blancos dentro del ROI.
        for i in 0..12 {
            paint_pixel(&mut frame, 150 + i, 150, 250, 250, 250);
        }
        let count = count_sparkle_pixels(&frame, roi);
        assert_eq!(count, 12);
    }

    #[test]
    fn count_sparkle_pixels_ignores_pixels_outside_roi() {
        let mut frame = make_frame(400, 400);
        let roi = RoiDef::new(100, 100, 50, 50); // ROI pequeño
        // Pintar blancos FUERA del ROI — no deben contarse.
        for i in 0..10 {
            paint_pixel(&mut frame, 200 + i, 200, 250, 250, 250);
        }
        assert_eq!(count_sparkle_pixels(&frame, roi), 0);
    }

    #[test]
    fn count_sparkle_pixels_ignores_yellow_spell_effects() {
        let mut frame = make_frame(400, 400);
        let roi = RoiDef::new(100, 100, 200, 200);
        // Pintar 20 pixeles amarillos (R alto, G alto, B bajo).
        for i in 0..20 {
            paint_pixel(&mut frame, 150 + i, 150, 255, 255, 50);
        }
        // Ninguno debe contar como sparkle.
        assert_eq!(count_sparkle_pixels(&frame, roi), 0);
    }

    #[test]
    fn roi_outside_frame_returns_zero() {
        let frame = make_frame(100, 100);
        // ROI más grande que el frame.
        let roi = RoiDef::new(50, 50, 200, 200);
        assert_eq!(count_sparkle_pixels(&frame, roi), 0);
    }
}
