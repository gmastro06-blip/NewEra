//! target.rs — Detección binaria de "char tiene target actual".
//!
//! Tibia muestra una barra de HP + nombre del target actual en un área fija
//! encima del viewport (tamaño variable según el cliente). Cuando el char
//! ataca a algo, esa zona contiene píxeles cromáticos (barra verde/amarilla/
//! roja + texto del nombre del mob). Cuando el char no tiene target, el
//! área está gris (sin píxeles cromáticos).
//!
//! Esto da un signal **binario** y **directo**: `target_active = true | false`.
//! Mucho más confiable que adivinar por count de battle list.
//!
//! ## Por qué esto resuelve el targeting
//!
//! El enfoque previo era: contar mobs en battle list → detectar flancos de
//! bajada → emit PgDown al flanco. Problemas:
//!
//! 1. Count flicker por JPEG noise → flancos falsos → emits redundantes
//! 2. Flancos reales bloqueados por safety_floor → retargets perdidos
//! 3. PgDown rotaba target aunque el char estuviera atacando bien → chaos
//!
//! Con target_active como señal binaria:
//!
//! 1. Si `has_combat && !target_active` → no tengo target pero hay mobs →
//!    emit PgDown para seleccionar uno
//! 2. Si `has_combat && target_active` → ya estoy atacando algo → no emit
//! 3. El detector de battle list sigue siendo necesario para `has_combat`
//!    (saber que hay mobs cerca), pero ya no condiciona el momento del emit.
//!
//! ## Implementación
//!
//! Misma técnica que battle_list: contar píxeles con `is_bar_filled` dentro
//! del ROI. Threshold ajustable. Incluye histéresis ligera (`was_active_prev`)
//! para absorber frames con compresión fuerte que bajen brevemente el count.

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::color::is_bar_filled;
use crate::sense::vision::crop::{count_pixels, Roi};

/// Threshold ALTO: píxeles requeridos para **activar** la detección desde
/// inactive. El target info bar del cliente de Tibia contiene:
/// - Barra de HP del mob (30-80 px cromáticos típicos)
/// - Texto del nombre del mob (5-20 px cromáticos por letra, ej. 60-150 px total)
/// - Posiblemente un icono pequeño
///
/// Un área sin target es gris uniforme: casi 0 píxeles cromáticos.
/// Valor 30 captura mobs con HP bar visible incluso sin el texto ni icono.
pub const TARGET_ACTIVE_THRESHOLD: u32 = 30;

/// Threshold BAJO (sticky): píxeles requeridos para **mantener** target
/// activo si ya lo estaba en el frame anterior. Absorbe frames con fuerte
/// compresión JPEG o con la barra HP momentáneamente no visible (animación
/// de daño, partícula pasando por encima, etc).
pub const TARGET_STICKY_THRESHOLD: u32 = 15;

/// Detector stateful del target. Guarda el estado del frame anterior para
/// aplicar histéresis.
#[derive(Debug, Clone, Default)]
pub struct TargetDetector {
    was_active_prev: bool,
    /// Último conteo de hits — expuesto para /vision/target/debug.
    last_hits:       u32,
}

/// Resultado de una detección. Incluye diagnostics para exponer por HTTP.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // extension point: threshold_used for HTTP debug
pub struct TargetReading {
    pub active: bool,
    pub hits:   u32,
    /// Qué threshold se aplicó (útil para debug).
    pub threshold_used: u32,
}

impl TargetDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lee el ROI del target info bar y decide si hay target activo.
    /// `None` si el ROI no cabe en el frame (vision degradada).
    pub fn read(&mut self, frame: &Frame, roi: RoiDef) -> Option<TargetReading> {
        let scan = Roi::new(roi.x, roi.y, roi.w, roi.h);
        if !scan.fits_in(frame.width, frame.height) {
            return None;
        }
        let hits = count_pixels(frame, scan, is_bar_filled);
        let threshold = if self.was_active_prev {
            TARGET_STICKY_THRESHOLD
        } else {
            TARGET_ACTIVE_THRESHOLD
        };
        let active = hits >= threshold;
        self.was_active_prev = active;
        self.last_hits = hits;
        Some(TargetReading {
            active,
            hits,
            threshold_used: threshold,
        })
    }

    /// Resetea el estado interno. Llamar en resume tras pause o en cambio de
    /// escena brusca (login, death).
    #[allow(dead_code)] // extension point
    pub fn reset(&mut self) {
        self.was_active_prev = false;
        self.last_hits = 0;
    }

    /// Expuesto para diagnóstico HTTP.
    #[allow(dead_code)] // extension point: HTTP diagnostics
    pub fn last_hits(&self) -> u32 {
        self.last_hits
    }

    /// Expuesto para diagnóstico HTTP.
    #[allow(dead_code)] // extension point: HTTP diagnostics
    pub fn was_active_prev(&self) -> bool {
        self.was_active_prev
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn make_empty_frame(w: u32, h: u32) -> Frame {
        Frame {
            width:       w,
            height:      h,
            data:        vec![50u8; (w * h * 4) as usize], // gris neutro
            captured_at: Instant::now(),
        }
    }

    /// Pinta `pixel_count` píxeles cromáticos saturados dentro del ROI,
    /// empezando por la esquina top-left en filas.
    fn paint_chromatic(frame: &mut Frame, roi: RoiDef, pixel_count: u32) {
        let stride = frame.width as usize * 4;
        let mut painted = 0u32;
        'outer: for row in 0..roi.h {
            for col in 0..roi.w {
                if painted >= pixel_count {
                    break 'outer;
                }
                let x = roi.x + col;
                let y = roi.y + row;
                let off = y as usize * stride + x as usize * 4;
                if off + 3 >= frame.data.len() {
                    continue;
                }
                // Verde saturado que is_bar_filled reconoce.
                frame.data[off]     = 0x20;
                frame.data[off + 1] = 0xD8;
                frame.data[off + 2] = 0x20;
                frame.data[off + 3] = 0xFF;
                painted += 1;
            }
        }
    }

    #[test]
    fn empty_area_returns_inactive() {
        let frame = make_empty_frame(400, 200);
        let roi = RoiDef::new(100, 50, 200, 20);
        let mut det = TargetDetector::new();
        let reading = det.read(&frame, roi).expect("fits");
        assert!(!reading.active);
        assert_eq!(reading.hits, 0);
    }

    #[test]
    fn area_with_sufficient_pixels_activates() {
        let mut frame = make_empty_frame(400, 200);
        let roi = RoiDef::new(100, 50, 200, 20);
        // 50 > TARGET_ACTIVE_THRESHOLD (30)
        paint_chromatic(&mut frame, roi, 50);
        let mut det = TargetDetector::new();
        let reading = det.read(&frame, roi).expect("fits");
        assert!(reading.active);
        assert_eq!(reading.hits, 50);
        assert_eq!(reading.threshold_used, TARGET_ACTIVE_THRESHOLD);
    }

    #[test]
    fn sticky_keeps_active_with_reduced_hits() {
        let roi = RoiDef::new(100, 50, 200, 20);
        let mut det = TargetDetector::new();

        // Frame 1: 40 hits → activa.
        let mut frame_hi = make_empty_frame(400, 200);
        paint_chromatic(&mut frame_hi, roi, 40);
        let r1 = det.read(&frame_hi, roi).expect("fits");
        assert!(r1.active);

        // Frame 2: 20 hits (< threshold activo 30 pero >= sticky 15) → sigue activo.
        let mut frame_mid = make_empty_frame(400, 200);
        paint_chromatic(&mut frame_mid, roi, 20);
        let r2 = det.read(&frame_mid, roi).expect("fits");
        assert!(r2.active, "sticky debe mantener con 20 >= 15");
        assert_eq!(r2.threshold_used, TARGET_STICKY_THRESHOLD);
    }

    #[test]
    fn sticky_releases_when_truly_empty() {
        let roi = RoiDef::new(100, 50, 200, 20);
        let mut det = TargetDetector::new();

        // Activar.
        let mut frame_hi = make_empty_frame(400, 200);
        paint_chromatic(&mut frame_hi, roi, 40);
        let _ = det.read(&frame_hi, roi);

        // Frame vacío (0 hits) < sticky 15 → desactiva.
        let frame_empty = make_empty_frame(400, 200);
        let r = det.read(&frame_empty, roi).expect("fits");
        assert!(!r.active);
        assert_eq!(r.hits, 0);
    }

    #[test]
    fn reset_clears_sticky_state() {
        let roi = RoiDef::new(100, 50, 200, 20);
        let mut det = TargetDetector::new();

        // Activar.
        let mut frame_hi = make_empty_frame(400, 200);
        paint_chromatic(&mut frame_hi, roi, 40);
        let _ = det.read(&frame_hi, roi);

        det.reset();
        assert!(!det.was_active_prev());

        // Tras reset, necesita threshold alto para re-activar.
        let mut frame_low = make_empty_frame(400, 200);
        paint_chromatic(&mut frame_low, roi, 20);
        let r = det.read(&frame_low, roi).expect("fits");
        assert!(!r.active, "20 < 30 debe rechazar tras reset");
    }

    #[test]
    fn roi_outside_frame_returns_none() {
        let frame = make_empty_frame(400, 200);
        // ROI que se sale del frame por la derecha.
        let roi = RoiDef::new(350, 50, 200, 20);
        let mut det = TargetDetector::new();
        assert!(det.read(&frame, roi).is_none());
    }
}
