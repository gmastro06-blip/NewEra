//! prompts.rs — Detección de pantallas que requieren intervención humana.
//!
//! Cuando el cliente de Tibia muestra ciertas pantallas (login, death screen,
//! captcha, event dialog...), un bot que sigue ejecutando comandos sin
//! responder a la pantalla es **trivialmente detectable**. Este módulo
//! detecta esas pantallas vía template matching contra ROIs configuradas
//! y le dice al bot que **se pause** (no intenta auto-responder — eso
//! también es detectable).
//!
//! ## Filosofía
//!
//! Cada prompt tiene:
//! - Una **ROI** (región del frame donde aparece) definida en calibration.toml
//! - Un **template PNG** (imagen de referencia) en `assets/templates/prompts/`
//!
//! El detector hace template matching simple (SSD normalizado) y retorna
//! la primera coincidencia por encima del umbral.
//!
//! ## Lista de prompts soportados
//!
//! Tibia tiene **tres pantallas/modals que bloquean al bot** (verificado
//! contra tibia.fandom.com + documentación oficial):
//!
//! - `login`: pantalla de entrada al cliente. Aparece tras cualquier
//!   disconnect, crash, logout, kick por inactividad (15 min), server save
//!   diario (10:00 CET), o cierre manual del client.
//! - `char_select`: lista de personajes. Aparece tras login exitoso **y**
//!   tras la muerte del personaje (Tibia no tiene una "death screen"
//!   propia; muerto → character select directamente).
//! - `npc_trade`: modal buy/sell de NPC shopkeepers (pociones, runas,
//!   supplies). Se abre al decir "hi" → "trade" al NPC. Mientras está
//!   abierta, el personaje NO puede caminar — si el bot espera moverse
//!   pero la window sigue abierta, se queda bloqueado.
//!
//! Notas importantes de diseño:
//! - **Tibia NO usa captchas.** BattleEye (desde feb 2017) es kernel-level
//!   anti-cheat, no un prompt visual.
//! - Inactivity kick, server save kick y disconnect terminan todos en
//!   el mismo `login` screen, así que no hay prompts separados para ellos.
//! - **Deposit gold / withdraw NO son prompts** — son conversaciones por
//!   texto en el console con el NPC banker. No bloquean la UI.
//! - Depot chest / containers son ventanas no-modales, tampoco son prompts.
//! - Party invites / player trade requests son popups flotantes que se
//!   resuelven con ESC — no los cubrimos.
//!
//! ## Templates requeridos (mantenidos por el usuario)
//!
//! El usuario debe proveer los PNGs via capturas manuales:
//! - `assets/templates/prompts/login.png`
//! - `assets/templates/prompts/char_select.png`
//! - `assets/templates/prompts/npc_trade.png`
//!
//! Si un template falta, ese prompt específico no se detecta (graceful
//! degradation). Si ningún template existe, el módulo es no-op.
//!
//! ## Modelo de ejecución
//!
//! `match_template` sobre ROIs grandes (ej. 900×800 con template 739×427)
//! tarda ~20s en Windows. Para no bloquear el game loop (33ms/tick),
//! el detector corre en un thread de fondo dedicado ("prompt-detector"):
//!
//! - Main thread: extrae los parches del frame y envía un `DetectJob`
//!   vía canal bounded(1). Si el canal está lleno (background ocupado),
//!   el envío se descarta y se reintenta en el próximo intervalo.
//! - Background thread: recibe el job, corre `match_template` para cada
//!   template en orden, devuelve `Option<PromptKind>` al canal de resultado.
//! - Main thread: `tick()` drena el canal de resultado y retorna
//!   `Some(result)` cuando hay un resultado nuevo, `None` si sigue pendiente.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use image::GrayImage;
use imageproc::template_matching::{match_template, MatchTemplateMethod};

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::RoiDef;

// Intervalo mínimo entre envíos al background (500 ms ≈ 15 ticks a 30 Hz).
const SUBMIT_INTERVAL: Duration = Duration::from_millis(500);

/// Tipo de prompt detectado — se propaga a `safety_pause_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptKind {
    /// Modal buy/sell de shopkeeper NPC — bloquea el movimiento mientras
    /// está abierto. Normalmente lo cierra el script Lua tras terminar la
    /// compra; si no lo hace, este detector lo señaliza.
    NpcTrade,
    /// Pantalla de login (post-disconnect/crash/server save a las 10:00 CET).
    /// El bot nunca auto-responde — el operador debe intervenir manualmente.
    Login,
    /// Pantalla de character select (post-login o post-death). Igual que
    /// Login, requiere intervención manual.
    CharSelect,
}

impl PromptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptKind::NpcTrade   => "prompt:npc_trade",
            PromptKind::Login      => "prompt:login",
            PromptKind::CharSelect => "prompt:char_select",
        }
    }

    pub fn template_filename(self) -> &'static str {
        match self {
            PromptKind::NpcTrade   => "npc_trade.png",
            PromptKind::Login      => "login.png",
            PromptKind::CharSelect => "char_select.png",
        }
    }
}

/// Una entrada de template con su ROI de búsqueda y la imagen de referencia.
/// Solo se usa en el main thread para extraer parches del frame.
pub struct PromptTemplate {
    pub kind:     PromptKind,
    pub roi:      RoiDef,
    pub template: GrayImage,
}

// ── Background thread ─────────────────────────────────────────────────────────

/// Parche pre-extraído del frame para un template concreto.
struct DetectPatch {
    kind:  PromptKind,
    patch: GrayImage,
}

/// Job enviado al background: lista de parches para esta ronda de detección.
struct DetectJob {
    patches: Vec<DetectPatch>,
}

/// Template que vive en el background thread (clon de la imagen, sin ROI).
struct BgTemplate {
    kind:      PromptKind,
    template:  GrayImage,
    threshold: f32,
}

/// Loop del background thread. Sale cuando `job_rx` se cierra (Sender dropped).
fn bg_worker(
    templates: Vec<BgTemplate>,
    job_rx:    Receiver<DetectJob>,
    result_tx: Sender<Option<PromptKind>>,
) {
    for job in job_rx {
        let result = run_detect(&job.patches, &templates);
        // Si el canal de resultados está lleno (main thread no está
        // drenando), simplemente descartamos — el siguiente job traerá
        // el resultado actualizado.
        let _ = result_tx.try_send(result);
    }
}

fn run_detect(patches: &[DetectPatch], templates: &[BgTemplate]) -> Option<PromptKind> {
    for patch in patches {
        let Some(tpl) = templates.iter().find(|t| t.kind == patch.kind) else {
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
            return Some(patch.kind);
        }
    }
    None
}

// ── PromptDetector ────────────────────────────────────────────────────────────

/// Detector de prompts con background thread.
///
/// El main thread llama a `tick(frame)` cada tick del game loop:
/// - Drena resultados del background (`Some(result)` cuando hay uno nuevo).
/// - Envía un nuevo job al background si el intervalo de 500ms ha expirado.
pub struct PromptDetector {
    /// Templates en el main thread (para extraer parches del frame).
    templates:      Vec<PromptTemplate>,
    threshold:      f32,
    /// Canales hacia/desde el background thread (None si no hay templates).
    job_tx:         Option<Sender<DetectJob>>,
    result_rx:      Option<Receiver<Option<PromptKind>>>,
    last_submitted: Option<Instant>,
}

impl PromptDetector {
    /// Construye vacío. `load_from_dir` añade templates y lanza el worker.
    pub fn new(threshold: f32) -> Self {
        Self {
            templates:      Vec::new(),
            threshold,
            job_tx:         None,
            result_rx:      None,
            last_submitted: None,
        }
    }

    /// Carga los templates PNG desde `templates_dir` y lanza el background
    /// thread. Si ya había templates cargados, los reemplaza (el thread
    /// anterior se cierra al soltar los canales).
    ///
    /// Los templates que faltan se ignoran silenciosamente.
    pub fn load_from_dir(
        &mut self,
        templates_dir: &Path,
        rois: &HashMap<PromptKind, RoiDef>,
    ) {
        self.templates.clear();
        // Soltar canales → el thread anterior recibe Disconnected y sale.
        self.job_tx    = None;
        self.result_rx = None;
        self.last_submitted = None;

        for kind in [PromptKind::NpcTrade, PromptKind::Login, PromptKind::CharSelect] {
            let Some(roi) = rois.get(&kind).copied() else {
                continue;
            };
            let path = templates_dir.join(kind.template_filename());
            if !path.exists() {
                continue;
            }
            match image::open(&path) {
                Ok(img) => {
                    let gray = img.to_luma8();
                    tracing::info!(
                        "PromptDetector: template '{}' cargado ({}x{})",
                        path.display(), gray.width(), gray.height()
                    );
                    self.templates.push(PromptTemplate { kind, roi, template: gray });
                }
                Err(e) => {
                    tracing::warn!(
                        "PromptDetector: no se pudo cargar '{}': {}",
                        path.display(), e
                    );
                }
            }
        }

        if self.templates.is_empty() {
            return;
        }

        // Clonar templates para el background thread.
        let bg_templates: Vec<BgTemplate> = self.templates.iter().map(|t| BgTemplate {
            kind:      t.kind,
            template:  t.template.clone(),
            threshold: self.threshold,
        }).collect();

        let (job_tx, job_rx)     = bounded::<DetectJob>(1);
        let (result_tx, result_rx) = bounded::<Option<PromptKind>>(2);

        std::thread::Builder::new()
            .name("prompt-detector".into())
            .spawn(move || bg_worker(bg_templates, job_rx, result_tx))
            .expect("No se pudo lanzar prompt-detector thread");

        self.job_tx    = Some(job_tx);
        self.result_rx = Some(result_rx);
    }

    /// ¿Hay al menos un template cargado?
    pub fn is_loaded(&self) -> bool {
        !self.templates.is_empty()
    }

    pub fn template_count(&self) -> usize {
        self.templates.len()
    }

    /// Llamar una vez por tick desde el game loop. **Nunca bloquea.**
    ///
    /// 1. Drena el canal de resultados del background thread.
    /// 2. Si el intervalo de 500ms venció, extrae parches del frame y
    ///    envía un nuevo `DetectJob` al background (descarta si está ocupado).
    ///
    /// Retorna:
    /// - `Some(Some(kind))` — background completó y detectó un prompt.
    /// - `Some(None)`       — background completó y no detectó nada.
    /// - `None`             — todavía sin resultado nuevo este ciclo.
    pub fn tick(&mut self, frame: &Frame) -> Option<Option<PromptKind>> {
        let (Some(job_tx), Some(result_rx)) = (&self.job_tx, &self.result_rx) else {
            return None;
        };

        // ── 1. Drenar resultado pendiente ─────────────────────────────────────
        let mut new_result: Option<Option<PromptKind>> = None;
        loop {
            match result_rx.try_recv() {
                Ok(r)                             => { new_result = Some(r); }
                Err(TryRecvError::Empty)          => break,
                Err(TryRecvError::Disconnected)   => break,
            }
        }

        // ── 2. Enviar nuevo job si el intervalo venció ────────────────────────
        let now = Instant::now();
        let should_submit = self.last_submitted
            .map(|last| now.duration_since(last) >= SUBMIT_INTERVAL)
            .unwrap_or(true);

        if should_submit {
            let patches: Vec<DetectPatch> = self.templates.iter()
                .filter_map(|tpl| {
                    crop_to_gray(frame, tpl.roi).map(|patch| DetectPatch {
                        kind: tpl.kind,
                        patch,
                    })
                })
                .collect();

            if !patches.is_empty() {
                let _ = job_tx.try_send(DetectJob { patches });
            }
            // Actualizar siempre (éxito o canal lleno) para no re-intentar
            // cada tick mientras el background está ocupado.
            self.last_submitted = Some(now);
        }

        new_result
    }

    /// Versión síncrona — solo para tests y diagnóstico.
    /// En el game loop usar `tick()`.
    #[allow(dead_code)] // extension point: sync diagnostics
    fn detect(&self, frame: &Frame) -> Option<PromptKind> {
        for tpl in &self.templates {
            if self.matches_sync(frame, tpl) {
                return Some(tpl.kind);
            }
        }
        None
    }

    #[allow(dead_code)]
    fn matches_sync(&self, frame: &Frame, tpl: &PromptTemplate) -> bool {
        let Some(roi_gray) = crop_to_gray(frame, tpl.roi) else {
            return false;
        };
        if tpl.template.width() > roi_gray.width()
            || tpl.template.height() > roi_gray.height()
        {
            return false;
        }
        let result = match_template(
            &roi_gray,
            &tpl.template,
            MatchTemplateMethod::SumOfSquaredErrorsNormalized,
        );
        let best = result.iter().cloned().fold(f32::MAX, f32::min);
        best <= self.threshold
    }
}

/// Convierte un rectángulo del frame RGBA a GrayImage.
fn crop_to_gray(frame: &Frame, roi: RoiDef) -> Option<GrayImage> {
    if roi.x + roi.w > frame.width || roi.y + roi.h > frame.height {
        return None;
    }
    let mut gray = GrayImage::new(roi.w, roi.h);
    let stride = frame.width as usize * 4;
    for row in 0..roi.h {
        for col in 0..roi.w {
            let off = (roi.y + row) as usize * stride + (roi.x + col) as usize * 4;
            if off + 3 >= frame.data.len() { return None; }
            // Luma estándar de RGB: 0.299R + 0.587G + 0.114B
            let r = frame.data[off]     as u32;
            let g = frame.data[off + 1] as u32;
            let b = frame.data[off + 2] as u32;
            let luma = (299 * r + 587 * g + 114 * b) / 1000;
            gray.put_pixel(col, row, image::Luma([luma as u8]));
        }
    }
    Some(gray)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn test_frame(w: u32, h: u32, fill: u8) -> Frame {
        Frame {
            width: w,
            height: h,
            data: vec![fill; (w * h * 4) as usize],
            captured_at: Instant::now(),
        }
    }

    #[test]
    fn prompt_kind_as_str_stable() {
        assert_eq!(PromptKind::NpcTrade.as_str(),   "prompt:npc_trade");
        assert_eq!(PromptKind::Login.as_str(),      "prompt:login");
        assert_eq!(PromptKind::CharSelect.as_str(), "prompt:char_select");
    }

    #[test]
    fn prompt_kind_template_filenames() {
        assert_eq!(PromptKind::NpcTrade.template_filename(),   "npc_trade.png");
        assert_eq!(PromptKind::Login.template_filename(),      "login.png");
        assert_eq!(PromptKind::CharSelect.template_filename(), "char_select.png");
    }

    #[test]
    fn empty_detector_returns_none() {
        let det = PromptDetector::new(0.20);
        assert!(!det.is_loaded());
        let frame = test_frame(100, 100, 128);
        assert_eq!(det.detect(&frame), None);
    }

    #[test]
    fn crop_to_gray_extracts_roi() {
        let frame = test_frame(100, 100, 0); // negro
        let roi = RoiDef { x: 10, y: 10, w: 20, h: 20 };
        let gray = crop_to_gray(&frame, roi).unwrap();
        assert_eq!(gray.width(), 20);
        assert_eq!(gray.height(), 20);
        for px in gray.pixels() {
            assert_eq!(px[0], 0);
        }
    }

    #[test]
    fn crop_out_of_bounds_returns_none() {
        let frame = test_frame(50, 50, 128);
        let roi = RoiDef { x: 40, y: 40, w: 20, h: 20 }; // 40+20 > 50
        assert!(crop_to_gray(&frame, roi).is_none());
    }

    #[test]
    fn detector_with_matching_template_returns_kind() {
        // Construimos un frame con un parche uniforme de color en una ROI,
        // y un template del mismo tamaño y color → match perfecto.
        let mut frame = test_frame(200, 200, 50);
        let roi = RoiDef { x: 50, y: 50, w: 30, h: 30 };
        // "Pintamos" el ROI en el frame con luma 200.
        let stride = frame.width as usize * 4;
        for row in 0..roi.h {
            for col in 0..roi.w {
                let off = (roi.y + row) as usize * stride + (roi.x + col) as usize * 4;
                frame.data[off]     = 200;
                frame.data[off + 1] = 200;
                frame.data[off + 2] = 200;
            }
        }
        // Template igual: 30x30 con luma 200.
        let mut template = GrayImage::new(30, 30);
        for p in template.pixels_mut() {
            *p = image::Luma([200]);
        }
        let mut det = PromptDetector::new(0.10);
        det.templates.push(PromptTemplate {
            kind: PromptKind::NpcTrade,
            roi,
            template,
        });
        assert_eq!(det.detect(&frame), Some(PromptKind::NpcTrade));
    }

    #[test]
    fn detector_with_non_matching_template_returns_none() {
        // Frame con fondo 50, template con luma 200 → no match.
        let frame = test_frame(200, 200, 50);
        let mut template = GrayImage::new(30, 30);
        for p in template.pixels_mut() {
            *p = image::Luma([200]);
        }
        let mut det = PromptDetector::new(0.10);
        det.templates.push(PromptTemplate {
            kind: PromptKind::NpcTrade,
            roi: RoiDef { x: 0, y: 0, w: 100, h: 100 },
            template,
        });
        assert_eq!(det.detect(&frame), None);
    }
}
