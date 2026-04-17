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

/// Resultado de un match on-demand (API síncrona).
///
/// Coords son frame-absolutas (incluyen offset de ROI si se usó una).
///
/// **Extension point**: los consumidores actuales son tests + eventualmente
/// `StepVerify` si se wirea match_now() como alternativa al cached
/// `ctx.ui_matches` (que tiene hasta 500ms de staleness). Ver ADR-003
/// para el rationale de por qué la versión cached es aceptable para MVP.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub struct MatchResult {
    /// Best SSD-normalized score. Lower = better match. Typical threshold ≤ 0.15.
    pub score: f32,
    /// Top-left (x, y) of the best match in *frame coordinates* (not ROI-relative).
    pub x: u32,
    pub y: u32,
    /// Width/height of the template that matched.
    pub w: u32,
    pub h: u32,
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

    // ── API síncrona on-demand (Fase 2A) ──────────────────────────────────────
    //
    // Estos métodos corren template-matching INLINE en el thread del caller
    // (no van al background worker). Útiles para verificar postcondiciones
    // después de una acción del cavebot. Coste esperado: 5–30 ms por llamada
    // según tamaño de ROI y template.

    /// Runs template matching INLINE on the caller thread. Does NOT go through
    /// the background worker. Uses the ROI configured for the template (or full
    /// frame if no ROI). Returns `Some(MatchResult)` if the best score ≤ threshold,
    /// `None` otherwise (not found or template not loaded).
    ///
    /// Typical caller: cavebot runner verifying a step's postcondition.
    /// Expected cost: 5–30 ms on an 800×600 ROI vs a 50×50 template.
    ///
    /// **Status**: API disponible pero sin consumidor activo. StepVerify
    /// usa el cached `ctx.ui_matches` (async 500ms) por default; si algún
    /// caso live requiere latencia <200ms, wirear acá.
    #[allow(dead_code)]
    pub fn match_now(&self, frame: &Frame, template_name: &str) -> Option<MatchResult> {
        let tpl = self.templates.iter().find(|t| t.name == template_name)?;
        let (search, offset_x, offset_y) = if let Some(roi) = tpl.roi {
            (crop_to_gray(frame, roi)?, roi.x, roi.y)
        } else {
            (frame_to_gray(frame), 0, 0)
        };
        best_match(&search, &tpl.template, offset_x, offset_y, self.threshold)
    }

    /// Runs template matching in a specific ROI override (not the configured one).
    /// Used when a step wants to verify a template appears in a tight box — e.g.
    /// "confirm that 'buy' button is visible at (100, 200) ± 10 px" you pass
    /// roi={x:90, y:190, w:template.w+20, h:template.h+20}.
    #[allow(dead_code)]
    pub fn match_in_roi(
        &self,
        frame: &Frame,
        template_name: &str,
        roi: RoiDef,
    ) -> Option<MatchResult> {
        let tpl = self.templates.iter().find(|t| t.name == template_name)?;
        let search = crop_to_gray(frame, roi)?;
        best_match(&search, &tpl.template, roi.x, roi.y, self.threshold)
    }

    /// Convenience wrapper: returns true if `match_now` found a match with
    /// score ≤ self.threshold AND the match center is within `tolerance` px of
    /// (expected_x, expected_y). Useful for "button at this exact spot" checks.
    #[allow(dead_code)]
    pub fn match_at_point(
        &self,
        frame: &Frame,
        template_name: &str,
        expected_x: u32,
        expected_y: u32,
        tolerance: u32,
    ) -> bool {
        let Some(m) = self.match_now(frame, template_name) else {
            return false;
        };
        let cx = m.x + m.w / 2;
        let cy = m.y + m.h / 2;
        let dx = cx.abs_diff(expected_x);
        let dy = cy.abs_diff(expected_y);
        dx <= tolerance && dy <= tolerance
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

/// Busca el mejor match de `template` dentro de `search` y retorna
/// `Some(MatchResult)` con coords frame-absolutas (search_offset_{x,y} + offset
/// local) si el score es ≤ threshold. Usado por la API síncrona.
#[allow(dead_code)]
fn best_match(
    search:        &GrayImage,
    template:      &GrayImage,
    search_offset_x: u32,
    search_offset_y: u32,
    threshold:     f32,
) -> Option<MatchResult> {
    let tw = template.width();
    let th = template.height();
    if search.width() < tw || search.height() < th {
        return None;
    }
    let result = match_template(
        search,
        template,
        MatchTemplateMethod::SumOfSquaredErrorsNormalized,
    );
    let result_w = result.width();
    // `result` es ImageBuffer<Luma<f32>>; iter() va fila por fila.
    let mut best_idx   = 0usize;
    let mut best_score = f32::MAX;
    for (idx, px) in result.iter().enumerate() {
        if *px < best_score {
            best_score = *px;
            best_idx   = idx;
        }
    }
    if best_score > threshold {
        return None;
    }
    let local_x = (best_idx as u32) % result_w;
    let local_y = (best_idx as u32) / result_w;
    Some(MatchResult {
        score: best_score,
        x: search_offset_x + local_x,
        y: search_offset_y + local_y,
        w: tw,
        h: th,
    })
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    /// Helper: build a 100×100 RGBA frame filled with `bg`, then paint a
    /// 10×10 square of `fg` at (sx, sy).
    fn make_frame(w: u32, h: u32, bg: u8, fg: u8, sx: u32, sy: u32, sq: u32) -> Frame {
        let mut data = vec![bg; (w as usize) * (h as usize) * 4];
        // canal alpha a 255 en todo el buffer
        for i in 0..(w as usize) * (h as usize) {
            data[i * 4 + 3] = 255;
        }
        for row in sy..(sy + sq) {
            for col in sx..(sx + sq) {
                let off = (row as usize) * (w as usize) * 4 + (col as usize) * 4;
                data[off]     = fg;
                data[off + 1] = fg;
                data[off + 2] = fg;
                data[off + 3] = 255;
            }
        }
        Frame {
            width:       w,
            height:      h,
            data,
            captured_at: Instant::now(),
        }
    }

    /// Guard: removes a directory on drop so a failed test doesn't leave junk.
    struct TmpDirGuard(std::path::PathBuf);
    impl Drop for TmpDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Crea un directorio tmp único, escribe un template PNG (gris `fg` sobre
    /// fondo negro) de tamaño `size × size` y lo asocia con `name`.
    /// Devuelve el path al directorio y el guard que lo limpia al drop.
    fn make_template_dir(
        name: &str,
        size: u32,
        fg: u8,
    ) -> (std::path::PathBuf, TmpDirGuard) {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("ui_detector_test_{}_{}", std::process::id(), seq));
        std::fs::create_dir_all(&dir).unwrap();
        let mut img = GrayImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                img.put_pixel(x, y, image::Luma([fg]));
            }
        }
        img.save(dir.join(format!("{}.png", name))).unwrap();
        let guard = TmpDirGuard(dir.clone());
        (dir, guard)
    }

    #[test]
    fn match_now_finds_square_at_expected_coords() {
        // Square de 10×10 blanco en (30, 40) sobre fondo negro.
        let frame = make_frame(100, 100, 0, 255, 30, 40, 10);
        let (dir, _guard) = make_template_dir("square", 10, 255);

        let mut det = UiDetector::new(0.15);
        det.load_dir(&dir, &HashMap::new());
        assert_eq!(det.len(), 1);

        let m = det.match_now(&frame, "square").expect("match_now debería encontrar");
        assert_eq!(m.x, 30);
        assert_eq!(m.y, 40);
        assert_eq!(m.w, 10);
        assert_eq!(m.h, 10);
        assert!(m.score <= 0.15, "score {} fuera del threshold", m.score);
    }

    #[test]
    fn match_in_roi_returns_frame_absolute_coords() {
        let frame = make_frame(100, 100, 0, 255, 30, 40, 10);
        let (dir, _guard) = make_template_dir("square", 10, 255);

        let mut det = UiDetector::new(0.15);
        det.load_dir(&dir, &HashMap::new());

        // ROI ajustada alrededor del square: (25, 35) con 20×20 lo envuelve.
        let roi = RoiDef { x: 25, y: 35, w: 20, h: 20 };
        let m = det.match_in_roi(&frame, "square", roi).expect("match en ROI");
        // Coords deben ser frame-absolutas (30, 40), no ROI-relativas (5, 5).
        assert_eq!(m.x, 30, "x debe ser frame-absolute");
        assert_eq!(m.y, 40, "y debe ser frame-absolute");
    }

    #[test]
    fn match_at_point_respects_tolerance() {
        let frame = make_frame(100, 100, 0, 255, 30, 40, 10);
        let (dir, _guard) = make_template_dir("square", 10, 255);

        let mut det = UiDetector::new(0.15);
        det.load_dir(&dir, &HashMap::new());

        // Centro real: (35, 45). Con tolerance=2 debe aceptar (34, 46).
        assert!(det.match_at_point(&frame, "square", 34, 46, 2));
        // Con tolerance=2 debe rechazar (50, 60) — demasiado lejos.
        assert!(!det.match_at_point(&frame, "square", 50, 60, 2));
        // Template inexistente → false siempre.
        assert!(!det.match_at_point(&frame, "no_such_tpl", 35, 45, 2));
    }

    #[test]
    fn match_now_returns_none_for_unloaded_template() {
        let frame = make_frame(100, 100, 0, 255, 30, 40, 10);
        let (dir, _guard) = make_template_dir("square", 10, 255);

        let mut det = UiDetector::new(0.15);
        det.load_dir(&dir, &HashMap::new());

        assert!(det.match_now(&frame, "ghost").is_none());
    }

    #[test]
    fn match_now_returns_none_when_score_above_threshold() {
        // Frame SIN el square (todo negro). El template blanco 10×10 no va a
        // matchear bajo threshold=0.05 contra un frame uniforme negro.
        let frame = make_frame(100, 100, 0, 0, 0, 0, 0);
        let (dir, _guard) = make_template_dir("square", 10, 255);

        let mut det = UiDetector::new(0.05);
        det.load_dir(&dir, &HashMap::new());

        assert!(
            det.match_now(&frame, "square").is_none(),
            "frame sin el patrón no debe matchear bajo threshold estricto"
        );
    }
}
