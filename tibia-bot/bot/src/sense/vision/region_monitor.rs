//! region_monitor.rs — Framework genérico de "diff per región" (Fase 1.5).
//!
//! Permite registrar regiones del frame con un threshold y, en cada tick,
//! obtener un evento de cambio cuando el contenido de la región difiere
//! del snapshot anterior por más del threshold configurado.
//!
//! ## Motivación (plan híbrido del usuario, paso 4 del diagrama "Comparación
//! con referencia")
//!
//! El bot ya hace `minimap_diff` (frame vs prev_frame) para detectar movimiento.
//! Este módulo generaliza ese patrón a CUALQUIER región — útil para:
//! - Detectar aparición/desaparición de UI sin template específico
//! - Trigger genérico "algo cambió" para custom logic
//! - Watchdog de regiones críticas (battle list, status bar)
//!
//! ## Uso
//!
//! ```rust,ignore
//! use tibia_bot::sense::vision::region_monitor::{RegionMonitor, RegionDiff};
//! use tibia_bot::sense::vision::calibration::RoiDef;
//!
//! let mut monitor = RegionMonitor::new();
//! monitor.add_region("battle_list", RoiDef::new(2, 45, 171, 200), 0.05);
//! monitor.add_region("hp_bar",      RoiDef::new(188, 5, 637, 25), 0.10);
//!
//! // En cada tick:
//! let changes = monitor.tick(&frame, tick_number);
//! for change in changes {
//!     if change.above_threshold {
//!         println!("Cambio detectado en {}: {:.2}%",
//!                  change.name, change.change_ratio * 100.0);
//!     }
//! }
//! ```
//!
//! ## Diseño
//!
//! - **Métrica de diff**: L1 normalizado (suma de |Δ| por canal / max posible).
//!   Robusto a iluminación uniforme. Para diff color-aware, agregar variante.
//! - **Snapshot**: BGRA bytes. Memoria por región = roi.w * roi.h * 4 bytes.
//!   Para minimap 107×110 = ~47 KB. Para battle list 171×997 = ~680 KB.
//! - **NO wired al main loop**: framework disponible para consumers que lo
//!   necesiten. Wire específico es responsabilidad del consumer.

use std::collections::HashMap;

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::crop::{crop_bgra, Roi};

/// Una región monitorizada con su ROI, threshold y último snapshot.
struct MonitoredRegion {
    roi:               RoiDef,
    /// Threshold de cambio en [0.0, 1.0]. 0.05 = ≥5% de diff dispara evento.
    threshold:         f32,
    /// Último snapshot capturado (BGRA bytes). None hasta el primer tick.
    prev_snapshot:     Option<Vec<u8>>,
    /// Tick del último cambio detectado (para diagnóstico).
    last_change_tick:  Option<u64>,
}

/// Resultado de un tick de monitoring para una región.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // public API
pub struct RegionDiff {
    /// Nombre de la región (clave del registro).
    pub name:             String,
    /// Ratio de cambio en [0.0, 1.0]. 0.0 = idéntico al prev. 1.0 = totalmente distinto.
    pub change_ratio:     f32,
    /// `true` si change_ratio ≥ threshold configurado.
    pub above_threshold:  bool,
    /// `true` si fue el primer tick para esta región (sin snapshot previo).
    pub first_tick:       bool,
}

/// Monitor genérico de cambios per región.
pub struct RegionMonitor {
    regions: HashMap<String, MonitoredRegion>,
}

impl RegionMonitor {
    pub fn new() -> Self {
        Self { regions: HashMap::new() }
    }

    /// Registra una región para monitoreo.
    /// `threshold` en [0.0, 1.0]. Valores típicos:
    /// - 0.01-0.05: muy sensible (detecta cualquier movimiento sutil)
    /// - 0.05-0.15: medio (detecta cambios de UI, mob entrando al panel)
    /// - 0.20+: solo cambios grandes (escena entera cambió)
    #[allow(dead_code)] // public API
    pub fn add_region(&mut self, name: &str, roi: RoiDef, threshold: f32) {
        let clamped = threshold.clamp(0.0, 1.0);
        self.regions.insert(
            name.to_string(),
            MonitoredRegion {
                roi,
                threshold: clamped,
                prev_snapshot: None,
                last_change_tick: None,
            },
        );
    }

    /// Quita una región del monitoreo.
    #[allow(dead_code)] // public API
    pub fn remove_region(&mut self, name: &str) -> bool {
        self.regions.remove(name).is_some()
    }

    /// Devuelve la lista de nombres de regiones registradas.
    #[allow(dead_code)]
    pub fn region_names(&self) -> Vec<String> {
        self.regions.keys().cloned().collect()
    }

    /// Devuelve el tick del último cambio para una región (o None si nunca cambió).
    #[allow(dead_code)]
    pub fn last_change_tick(&self, name: &str) -> Option<u64> {
        self.regions.get(name).and_then(|r| r.last_change_tick)
    }

    /// Procesa un tick: para cada región, captura snapshot y compara con el
    /// previo. Devuelve un `RegionDiff` por región registrada.
    /// `current_tick` se usa solo para logging/diagnóstico (`last_change_tick`).
    #[allow(dead_code)] // public API — no wired aún
    pub fn tick(&mut self, frame: &Frame, current_tick: u64) -> Vec<RegionDiff> {
        let mut results = Vec::with_capacity(self.regions.len());
        for (name, region) in self.regions.iter_mut() {
            let crop_roi = Roi::new(region.roi.x, region.roi.y, region.roi.w, region.roi.h);
            let Some(snapshot) = crop_bgra(frame, crop_roi) else {
                // ROI fuera del frame — skip silenciosamente.
                continue;
            };
            let first_tick = region.prev_snapshot.is_none();
            let change_ratio = if let Some(prev) = &region.prev_snapshot {
                l1_diff_normalized(prev, &snapshot)
            } else {
                0.0  // primer tick: no hay con qué comparar
            };
            let above_threshold = !first_tick && change_ratio >= region.threshold;
            if above_threshold {
                region.last_change_tick = Some(current_tick);
            }
            region.prev_snapshot = Some(snapshot);
            results.push(RegionDiff {
                name: name.clone(),
                change_ratio,
                above_threshold,
                first_tick,
            });
        }
        results
    }

    /// Reset del snapshot de una región (forzando que el próximo tick sea
    /// "first_tick" = true). Útil después de cambio brusco de escena
    /// (login, death, teleport).
    #[allow(dead_code)]
    pub fn reset_region(&mut self, name: &str) -> bool {
        if let Some(region) = self.regions.get_mut(name) {
            region.prev_snapshot = None;
            true
        } else {
            false
        }
    }

    /// Reset de todas las regiones simultáneamente.
    #[allow(dead_code)]
    pub fn reset_all(&mut self) {
        for region in self.regions.values_mut() {
            region.prev_snapshot = None;
        }
    }
}

impl Default for RegionMonitor {
    fn default() -> Self { Self::new() }
}

/// L1 diff normalizado entre dos buffers BGRA del mismo tamaño.
/// Retorna ratio en [0.0, 1.0]: 0.0 = idénticos, 1.0 = máxima diferencia posible.
/// Si los tamaños difieren, retorna 1.0 (cambio total — usualmente bug).
fn l1_diff_normalized(a: &[u8], b: &[u8]) -> f32 {
    if a.len() != b.len() {
        return 1.0;
    }
    if a.is_empty() {
        return 0.0;
    }
    let mut sum: u64 = 0;
    for (px_a, px_b) in a.iter().zip(b.iter()) {
        sum += (*px_a as i32 - *px_b as i32).unsigned_abs() as u64;
    }
    // Max possible diff = bytes * 255
    let max = a.len() as u64 * 255;
    (sum as f32) / (max as f32)
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
    fn l1_diff_identical_returns_zero() {
        let a = vec![100u8; 100];
        let b = vec![100u8; 100];
        assert_eq!(l1_diff_normalized(&a, &b), 0.0);
    }

    #[test]
    fn l1_diff_max_difference_returns_one() {
        let a = vec![0u8; 100];
        let b = vec![255u8; 100];
        assert!((l1_diff_normalized(&a, &b) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn l1_diff_half_difference_returns_half() {
        let a = vec![0u8; 100];
        let b: Vec<u8> = (0..100).map(|_| 127u8).collect();
        let r = l1_diff_normalized(&a, &b);
        // 127/255 ≈ 0.498
        assert!(r > 0.49 && r < 0.51, "got {}", r);
    }

    #[test]
    fn l1_diff_empty_returns_zero() {
        let a: Vec<u8> = vec![];
        let b: Vec<u8> = vec![];
        assert_eq!(l1_diff_normalized(&a, &b), 0.0);
    }

    #[test]
    fn l1_diff_size_mismatch_returns_one() {
        let a = vec![100u8; 100];
        let b = vec![100u8; 50];
        assert_eq!(l1_diff_normalized(&a, &b), 1.0);
    }

    #[test]
    fn monitor_first_tick_is_flagged() {
        let mut monitor = RegionMonitor::new();
        monitor.add_region("test", RoiDef::new(0, 0, 10, 10), 0.05);
        let frame = make_frame(20, 20, 128);
        let diffs = monitor.tick(&frame, 1);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].first_tick);
        assert!(!diffs[0].above_threshold);
        assert_eq!(diffs[0].change_ratio, 0.0);
    }

    #[test]
    fn monitor_no_change_below_threshold() {
        let mut monitor = RegionMonitor::new();
        monitor.add_region("test", RoiDef::new(0, 0, 10, 10), 0.05);
        let frame = make_frame(20, 20, 128);
        // Tick 1: snapshot inicial
        monitor.tick(&frame, 1);
        // Tick 2: mismo frame → no change
        let diffs = monitor.tick(&frame, 2);
        assert!(!diffs[0].above_threshold);
        assert_eq!(diffs[0].change_ratio, 0.0);
    }

    #[test]
    fn monitor_change_above_threshold_fires() {
        let mut monitor = RegionMonitor::new();
        monitor.add_region("test", RoiDef::new(0, 0, 10, 10), 0.05);
        let frame_a = make_frame(20, 20, 0);
        let frame_b = make_frame(20, 20, 255);
        monitor.tick(&frame_a, 1);
        let diffs = monitor.tick(&frame_b, 2);
        assert!(diffs[0].above_threshold);
        assert!(diffs[0].change_ratio > 0.5);
        assert_eq!(monitor.last_change_tick("test"), Some(2));
    }

    #[test]
    fn monitor_change_below_threshold_does_not_fire() {
        let mut monitor = RegionMonitor::new();
        monitor.add_region("test", RoiDef::new(0, 0, 10, 10), 0.5);  // threshold alto
        let frame_a = make_frame(20, 20, 100);
        let frame_b = make_frame(20, 20, 110);  // small change
        monitor.tick(&frame_a, 1);
        let diffs = monitor.tick(&frame_b, 2);
        assert!(!diffs[0].above_threshold);
        assert!(diffs[0].change_ratio < 0.1);
    }

    #[test]
    fn monitor_reset_region_resets_snapshot() {
        let mut monitor = RegionMonitor::new();
        monitor.add_region("test", RoiDef::new(0, 0, 10, 10), 0.05);
        let frame = make_frame(20, 20, 128);
        monitor.tick(&frame, 1);
        // Reset → próximo tick debería ser first_tick again
        assert!(monitor.reset_region("test"));
        let diffs = monitor.tick(&frame, 2);
        assert!(diffs[0].first_tick);
    }

    #[test]
    fn monitor_remove_region() {
        let mut monitor = RegionMonitor::new();
        monitor.add_region("test", RoiDef::new(0, 0, 10, 10), 0.05);
        assert!(monitor.remove_region("test"));
        assert!(!monitor.remove_region("nonexistent"));
        let frame = make_frame(20, 20, 128);
        let diffs = monitor.tick(&frame, 1);
        assert!(diffs.is_empty());
    }

    #[test]
    fn monitor_threshold_clamped() {
        let mut monitor = RegionMonitor::new();
        monitor.add_region("over", RoiDef::new(0, 0, 10, 10), 1.5);
        monitor.add_region("under", RoiDef::new(0, 0, 10, 10), -0.3);
        // No exposed accessor para threshold, validar via behaviour:
        // "over" 1.0 nunca dispara (max diff es 1.0); "under" 0.0 siempre dispara con cualquier change.
        let frame_a = make_frame(20, 20, 100);
        let frame_b = make_frame(20, 20, 105);
        monitor.tick(&frame_a, 1);
        let diffs = monitor.tick(&frame_b, 2);
        let over = diffs.iter().find(|d| d.name == "over").unwrap();
        let under = diffs.iter().find(|d| d.name == "under").unwrap();
        // Diff es ~5/255 ≈ 0.02. over (1.0): no dispara. under (0.0): dispara.
        assert!(!over.above_threshold);
        assert!(under.above_threshold);
    }

    #[test]
    fn monitor_handles_roi_outside_frame() {
        let mut monitor = RegionMonitor::new();
        monitor.add_region("oob", RoiDef::new(100, 100, 50, 50), 0.05);
        let frame = make_frame(20, 20, 128);  // demasiado pequeño
        let diffs = monitor.tick(&frame, 1);
        // Region skipped silenciosamente, no panic.
        assert!(diffs.is_empty());
    }

    #[test]
    fn monitor_multiple_regions_independent() {
        let mut monitor = RegionMonitor::new();
        monitor.add_region("a", RoiDef::new(0, 0, 5, 5), 0.05);
        monitor.add_region("b", RoiDef::new(10, 10, 5, 5), 0.5);
        let frame_a = make_frame(20, 20, 0);
        monitor.tick(&frame_a, 1);
        let frame_b = make_frame(20, 20, 255);
        let diffs = monitor.tick(&frame_b, 2);
        assert_eq!(diffs.len(), 2);
        // Both should detect change
        for d in &diffs {
            assert!(d.above_threshold, "region {} debe disparar", d.name);
        }
    }
}
