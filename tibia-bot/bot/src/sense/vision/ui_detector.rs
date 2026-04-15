//! ui_detector.rs — Detector genérico de elementos de UI por template matching.
//!
//! Carga todos los PNG de `assets/templates/ui/` al arrancar. Cada tick
//! busca cada template en su ROI configurada (o en el frame completo si no
//! hay ROI). Retorna la lista de nombres de templates que hacen match.
//!
//! ## Uso en Lua
//! ```lua
//! function on_tick(ctx)
//!   if ctx.ui["depot_chest"] then
//!     bot.log("info", "depot chest abierto")
//!   end
//! end
//! ```
//!
//! ## Uso en cavebot
//! ```toml
//! [[step]]
//! kind  = "goto_if"
//! label = "deposit_done"
//! when  = "not:ui_visible(depot_chest)"
//! ```
//!
//! ## Configuración de ROIs (calibration.toml)
//! ```toml
//! [ui_rois]
//! depot_chest = { x = 1200, y = 0, w = 700, h = 500 }
//! stow_menu   = { x = 900,  y = 0, w = 900, h = 800 }
//! ```
//!
//! ## Modelo de ejecución
//!
//! `match_template` sobre ROIs grandes es O(search_area × template_pixels)
//! y puede tardar >20s. Para no bloquear el game loop (33ms/tick), el detector
//! corre en un thread de fondo dedicado ("ui-detector") con el mismo patrón
//! que `PromptDetector` y `AnchorTracker`:
//!
//! - `tick(frame)` extrae parches y envía un `DetectJob` cada ~500ms.
//!   Es no-bloqueante: usa `try_send` y descarta si el background está ocupado.
//! - `last_matches()` retorna el resultado cacheado del último job completado.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use image::GrayImage;
use imageproc::template_matching::{match_template, MatchTemplateMethod};
use tracing::warn;

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::RoiDef;

const SUBMIT_INTERVAL: Duration = Duration::from_millis(500);

// ── Background thread ─────────────────────────────────────────────────────────

struct UiPatch {
    name:  String,
    patch: GrayImage,
}

struct DetectJob {
    patches: Vec<UiPatch>,
}

struct BgTemplate {
    name:      String,
    template:  GrayImage,
    threshold: f32,
}

fn bg_worker(
    templates: Vec<BgTemplate>,
    job_rx:    Receiver<DetectJob>,
    result_tx: Sender<Vec<String>>,
) {
    for job in job_rx {
        let mut found = Vec::new();
        for patch in &job.patches {
            let Some(tpl) = templates.iter().find(|t| t.name == patch.name) else {
                continue;
            };
            if patch.patch.width() < tpl.template.width()
                || patch.patch.height() < tpl.template.height()
            {
                continue;
            }
            let result = match_template(
                &patch.patch,
                &tpl.template,
                MatchTemplateMethod::SumOfSquaredErrorsNormalized,
            );
            let best = result.iter().cloned().fold(f32::MAX, f32::min);
            if best <= tpl.threshold {
                found.push(patch.name.clone());
            }
        }
        let _ = result_tx.try_send(found);
    }
}

// ── UiDetector ────────────────────────────────────────────────────────────────

struct UiTemplate {
    name:     String,
    template: GrayImage,
    roi:      Option<RoiDef>,
}

/// Detector genérico de UI con background thread.
///
/// Llama a `tick(frame)` cada tick del game loop (no bloquea).
/// Lee el resultado con `last_matches()`.
pub struct UiDetector {
    templates:      Vec<UiTemplate>,
    threshold:      f32,
    job_tx:         Option<Sender<DetectJob>>,
    result_rx:      Option<Receiver<Vec<String>>>,
    last_result:    Vec<String>,
    last_submitted: Option<Instant>,
}

impl UiDetector {
    pub fn new(threshold: f32) -> Self {
        Self {
            templates:      Vec::new(),
            threshold,
            job_tx:         None,
            result_rx:      None,
            last_result:    Vec::new(),
            last_submitted: None,
        }
    }

    /// Carga todos los PNG del directorio dado. Lanza el background thread.
    /// `rois` mapea nombre → área de búsqueda.
    pub fn load_dir(&mut self, dir: &Path, rois: &HashMap<String, RoiDef>) {
        self.templates.clear();
        self.job_tx    = None;
        self.result_rx = None;
        self.last_submitted = None;

        if !dir.exists() {
            tracing::info!(
                "UiDetector: directorio '{}' no existe — sin templates de UI",
                dir.display()
            );
            return;
        }

        let entries = match std::fs::read_dir(dir) {
            Ok(e)  => e,
            Err(e) => {
                warn!("UiDetector: no se pudo leer '{}': {}", dir.display(), e);
                return;
            }
        };

        let mut count = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None    => continue,
            };
            let template = match image::open(&path) {
                Ok(img) => img.to_luma8(),
                Err(e)  => {
                    warn!("UiDetector: no se pudo cargar '{}': {}", path.display(), e);
                    continue;
                }
            };
            let roi = rois.get(&name).copied();
            if roi.is_none() {
                warn!(
                    "UiDetector: template '{}' sin ROI — búsqueda en frame completo (lento). \
                     Añade [ui_rois.{}] a calibration.toml.",
                    name, name
                );
            }
            tracing::info!(
                "UiDetector: '{}' cargado ({}×{}), roi={:?}",
                name, template.width(), template.height(), roi
            );
            self.templates.push(UiTemplate { name, template, roi });
            count += 1;
        }

        tracing::info!(
            "UiDetector: {} template(s) cargados desde '{}'",
            count, dir.display()
        );

        if self.templates.is_empty() {
            return;
        }

        // Clonar templates para el background thread.
        let bg_templates: Vec<BgTemplate> = self.templates.iter().map(|t| BgTemplate {
            name:      t.name.clone(),
            template:  t.template.clone(),
            threshold: self.threshold,
        }).collect();

        let (job_tx, job_rx)       = bounded::<DetectJob>(1);
        let (result_tx, result_rx) = bounded::<Vec<String>>(2);

        std::thread::Builder::new()
            .name("ui-detector".into())
            .spawn(move || bg_worker(bg_templates, job_rx, result_tx))
            .expect("No se pudo lanzar ui-detector thread");

        self.job_tx    = Some(job_tx);
        self.result_rx = Some(result_rx);
    }

    pub fn is_empty(&self) -> bool { self.templates.is_empty() }
    pub fn len(&self) -> usize     { self.templates.len() }

    /// Resultado del último job completado. Puede ser hasta ~500ms antiguo.
    /// Vacío si ningún job ha completado todavía.
    pub fn last_matches(&self) -> &[String] {
        &self.last_result
    }

    /// Llamar una vez por tick. **Nunca bloquea.**
    ///
    /// Drena resultados pendientes y envía un nuevo job cada ~500ms.
    pub fn tick(&mut self, frame: &Frame) {
        let (Some(job_tx), Some(result_rx)) = (&self.job_tx, &self.result_rx) else {
            return;
        };

        // Drenar resultados
        loop {
            match result_rx.try_recv() {
                Ok(r)                             => { self.last_result = r; }
                Err(TryRecvError::Empty)          => break,
                Err(TryRecvError::Disconnected)   => break,
            }
        }

        // Enviar nuevo job si el intervalo venció
        let now = Instant::now();
        let should_submit = self.last_submitted
            .map(|last| now.duration_since(last) >= SUBMIT_INTERVAL)
            .unwrap_or(true);

        if should_submit {
            let patches: Vec<UiPatch> = self.templates.iter()
                .filter_map(|tpl| {
                    let patch = if let Some(roi) = tpl.roi {
                        crop_to_gray(frame, roi)?
                    } else {
                        frame_to_gray(frame)
                    };
                    Some(UiPatch { name: tpl.name.clone(), patch })
                })
                .collect();

            if !patches.is_empty() {
                let _ = job_tx.try_send(DetectJob { patches });
            }
            self.last_submitted = Some(now);
        }
    }
}

// ── Helpers de conversión Frame → GrayImage ──────────────────────────────────

fn crop_to_gray(frame: &Frame, roi: RoiDef) -> Option<GrayImage> {
    if roi.x + roi.w > frame.width || roi.y + roi.h > frame.height {
        return None;
    }
    let mut gray  = GrayImage::new(roi.w, roi.h);
    let stride    = frame.width as usize * 4;
    for row in 0..roi.h {
        for col in 0..roi.w {
            let off = (roi.y + row) as usize * stride + (roi.x + col) as usize * 4;
            if off + 2 >= frame.data.len() { return None; }
            let r = frame.data[off]     as u32;
            let g = frame.data[off + 1] as u32;
            let b = frame.data[off + 2] as u32;
            let luma = (299 * r + 587 * g + 114 * b) / 1000;
            gray.put_pixel(col, row, image::Luma([luma as u8]));
        }
    }
    Some(gray)
}

fn frame_to_gray(frame: &Frame) -> GrayImage {
    let mut gray = GrayImage::new(frame.width, frame.height);
    let stride   = frame.width as usize * 4;
    for row in 0..frame.height {
        for col in 0..frame.width {
            let off = row as usize * stride + col as usize * 4;
            let r = frame.data[off]     as u32;
            let g = frame.data[off + 1] as u32;
            let b = frame.data[off + 2] as u32;
            let luma = (299 * r + 587 * g + 114 * b) / 1000;
            gray.put_pixel(col, row, image::Luma([luma as u8]));
        }
    }
    gray
}
