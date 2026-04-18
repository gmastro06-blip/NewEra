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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use image::GrayImage;
use imageproc::template_matching::{match_template, MatchTemplateMethod};
use tracing::warn;

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::RoiDef;

const SUBMIT_INTERVAL: Duration = Duration::from_millis(500);

/// TTL de un match en el cache async. Previene flapping vacío↔match cuando
/// el UI parpadea (ej: NPC greeting window cerrándose brevemente entre
/// ciclos async de 500ms). Debe ser ≥ cycle_time del bg_worker para que
/// el match no expire entre 2 ciclos consecutivos.
///
/// 2026-04-18: subido de 2s → 5s porque los templates grandes (stow_menu
/// 1020×800 ROI, depot_chest 370×500 ROI) hacen cycle_time ~2.5s. Con
/// TTL=2s el entry expiraba ANTES del próximo drain → cache siempre vacío
/// en steady state aunque el bag SÍ matcheaba cada ciclo.
/// TODO: cuando se paralelice el worker por template, bajar a 1s.
const STICKY_TTL: Duration = Duration::from_millis(5000);

// ── Background thread ─────────────────────────────────────────────────────────

struct UiPatch {
    name:        String,
    patch:       GrayImage,
    /// Origin del ROI en frame coords. `None` = patch es el frame completo.
    /// Usado por bg_worker para devolver coord frame-absoluta del match.
    roi_origin:  Option<(u32, u32)>,
}

struct DetectJob {
    patches: Vec<UiPatch>,
}

struct BgTemplate {
    name:      String,
    template:  GrayImage,
    threshold: f32,
}

/// Match devuelto por el background worker: nombre + coord centro FRAME-ABSOLUTO
/// (incluye el offset del ROI si se usó) + dimensiones del template.
#[derive(Debug, Clone)]
pub struct UiMatchInfo {
    pub name:       String,
    /// Centro X del match en coord frame-absoluta (para click directo).
    pub center_x:   u32,
    pub center_y:   u32,
    pub template_w: u32,
    pub template_h: u32,
}

fn bg_worker(
    templates: Vec<BgTemplate>,
    job_rx:    Receiver<DetectJob>,
    result_tx: Sender<Vec<UiMatchInfo>>,
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
            // Encontrar la POSICIÓN del best (top-left en coord del patch).
            let (rw, _rh) = result.dimensions();
            let mut best_score = f32::MAX;
            let mut best_idx   = 0usize;
            for (i, &p) in result.iter().enumerate() {
                if p < best_score {
                    best_score = p;
                    best_idx = i;
                }
            }
            // DEBUG: log de cada match attempt con score best vs threshold.
            // Ayuda a diagnosticar "template matchea sync pero async retorna vacío".
            tracing::debug!(
                "UiDetector bg_worker: '{}' best_score={:.4} threshold={:.4} \
                 patch={}x{} tpl={}x{} roi_origin={:?}",
                patch.name, best_score, tpl.threshold,
                patch.patch.width(), patch.patch.height(),
                tpl.template.width(), tpl.template.height(),
                patch.roi_origin
            );
            if best_score <= tpl.threshold {
                let patch_x = (best_idx as u32) % rw;
                let patch_y = (best_idx as u32) / rw;
                let tw = tpl.template.width();
                let th = tpl.template.height();
                // Sumar offset del ROI (si hay) para obtener coord frame-absoluta.
                let (roi_off_x, roi_off_y) = patch.roi_origin.unwrap_or((0, 0));
                let center_x = roi_off_x + patch_x + tw / 2;
                let center_y = roi_off_y + patch_y + th / 2;
                tracing::info!(
                    "UiDetector bg_worker MATCH: '{}' score={:.4} center=({},{}) tw={}",
                    patch.name, best_score, center_x, center_y, tw
                );
                found.push(UiMatchInfo {
                    name:       patch.name.clone(),
                    center_x,
                    center_y,
                    template_w: tw,
                    template_h: th,
                });
            }
        }
        let n = found.len();
        match result_tx.try_send(found) {
            Ok(()) => tracing::info!("UiDetector bg_worker: sent {} matches to main", n),
            Err(e) => tracing::warn!("UiDetector bg_worker: try_send FAILED ({}), {} matches LOST", e, n),
        }
    }
}

// ── UiDetector ────────────────────────────────────────────────────────────────

/// Priority policy de cada template.
///
/// `Always` — el template se incluye en cada background cycle. Úsese para
/// elementos que el bot necesita detectar pasivamente sin que un step explícito
/// lo pida (ej bag icon chico). Coste debe ser bajo.
///
/// `OnDemand` — el template solo se incluye si el caller lo activó via
/// `request_on_demand(name)`. Para templates caros (stow_menu, depot_chest)
/// que dominarían el cycle time (medido 2026-04-18: stow_menu=20.9s,
/// depot_chest=1.97s vs Always npc_trade_bag=10ms). Sin esta separación el
/// bg_worker nunca completa un cycle en steady state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UiPriority {
    Always,
    OnDemand,
}

/// Classifier hardcoded: qué templates van siempre vs solo on-demand.
/// Llamado al cargar el directorio de templates. Los que no estén listados
/// como Always caen en OnDemand por default — criterio conservador para
/// evitar que un template nuevo grande degrade el cycle sin que nos demos
/// cuenta.
///
/// 2026-04-18: bench baseline muestra:
///   - npc_trade_bag (34×34, ROI 180×100)     →  10.5 ms  → Always
///   - depot_chest  (169×239, ROI 370×500)    →   1.97 s  → OnDemand
///   - stow_menu    (215×219, ROI 1020×800)   →  20.9 s   → OnDemand
///   - npc_trade    (595×374 > ROI 420×650)   →    skip   → OnDemand (sin efecto)
fn classify_template(name: &str) -> UiPriority {
    match name {
        "npc_trade_bag" => UiPriority::Always,
        _               => UiPriority::OnDemand,
    }
}

struct UiTemplate {
    name:     String,
    template: GrayImage,
    roi:      Option<RoiDef>,
    priority: UiPriority,
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
    result_rx:      Option<Receiver<Vec<UiMatchInfo>>>,
    /// Matches del último background job. Hasta ~500ms stale.
    /// Key = template name, Value = match info (centro + dims).
    /// Entradas expiran tras STICKY_TTL sin ser re-confirmadas (ver tick()).
    last_matches:   HashMap<String, UiMatchInfo>,
    /// Timestamp de la última vez que bg_worker confirmó cada template.
    /// Usado por el TTL anti-flapping de last_matches.
    last_seen:      HashMap<String, Instant>,
    last_submitted: Option<Instant>,
    /// Templates OnDemand actualmente activos. El caller (cavebot runner)
    /// llama `request_on_demand(name)` al entrar a un step que los necesita
    /// y `release_on_demand(name)` al salir. Solo los listados aquí + los
    /// Always se incluyen en el próximo background cycle.
    on_demand_active: HashSet<String>,
}

impl UiDetector {
    pub fn new(threshold: f32) -> Self {
        Self {
            templates:        Vec::new(),
            threshold,
            job_tx:           None,
            result_rx:        None,
            last_matches:     HashMap::new(),
            last_seen:        HashMap::new(),
            last_submitted:   None,
            on_demand_active: HashSet::new(),
        }
    }

    /// Carga todos los PNG del directorio dado. Lanza el background thread.
    /// `rois` mapea nombre → área de búsqueda.
    pub fn load_dir(&mut self, dir: &Path, rois: &HashMap<String, RoiDef>) {
        self.templates.clear();
        self.job_tx    = None;
        self.result_rx = None;
        self.last_submitted = None;
        self.last_matches.clear();
        self.last_seen.clear();
        // `on_demand_active` se preserva: reload no debería cancelar un step
        // que ya pidió un template (ej hot-reload del cavebot durante stow).

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
            let priority = classify_template(&name);
            tracing::info!(
                "UiDetector: '{}' cargado ({}×{}), roi={:?}, priority={:?}",
                name, template.width(), template.height(), roi, priority
            );
            self.templates.push(UiTemplate { name, template, roi, priority });
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
        let (result_tx, result_rx) = bounded::<Vec<UiMatchInfo>>(2);

        std::thread::Builder::new()
            .name("ui-detector".into())
            .spawn(move || bg_worker(bg_templates, job_rx, result_tx))
            .expect("No se pudo lanzar ui-detector thread");

        self.job_tx    = Some(job_tx);
        self.result_rx = Some(result_rx);
    }

    pub fn is_empty(&self) -> bool { self.templates.is_empty() }
    pub fn len(&self) -> usize     { self.templates.len() }

    /// Nombres de los templates matcheados en el último background job.
    /// Hasta ~500ms stale. Uso clásico: `ctx.ui_matches` en cavebot conditions.
    pub fn last_matches(&self) -> Vec<String> {
        self.last_matches.keys().cloned().collect()
    }

    /// Info completa del último match de un template (centro + dims) si existe.
    /// Usada por OpenNpcTrade para click genérico en el bag icon sin hardcodear
    /// coords por NPC.
    #[allow(dead_code)] // consumido por el cavebot runner via ctx
    pub fn last_match_info(&self, name: &str) -> Option<UiMatchInfo> {
        self.last_matches.get(name).cloned()
    }

    /// Todos los matches (snapshot) para exponer via ctx.
    pub fn last_matches_map(&self) -> HashMap<String, UiMatchInfo> {
        self.last_matches.clone()
    }

    /// Llamar una vez por tick. **Nunca bloquea.**
    ///
    /// Drena resultados pendientes y envía un nuevo job cada ~500ms.
    pub fn tick(&mut self, frame: &Frame) {
        let (Some(job_tx), Some(result_rx)) = (&self.job_tx, &self.result_rx) else {
            return;
        };

        // Drenar resultados con stickiness anti-flapping.
        //
        // 2026-04-18: bug diagnosticado — antes `last_matches = r.into_iter()...`
        // sobrescribía TODO el HashMap cada ciclo async. Cuando un template NO
        // matcheaba en un ciclo (ej: UI brevemente no visible o score encima
        // de threshold por un frame), el cache se vaciaba, y el próximo ciclo
        // con match real volvía a popular. Entre medio, cualquier consumer del
        // `ctx.ui_match_infos` veía vacío y fallaba.
        //
        // Fix: stickiness. Cada template tiene su propio TTL (STICKY_TTL).
        // Ciclos que NO encuentran un template no borran la entrada — solo
        // dejan que expire por tiempo. Así un match score=0.004 persiste
        // STICKY_TTL aunque varios ciclos async próximos reporten vacío.
        let now = Instant::now();
        loop {
            match result_rx.try_recv() {
                Ok(r) => {
                    tracing::info!(
                        "UiDetector drain: received {} matches; last_matches size before={}",
                        r.len(), self.last_matches.len()
                    );
                    for m in r {
                        let name = m.name.clone();
                        tracing::info!("UiDetector drain: insert '{}' at ({},{})", name, m.center_x, m.center_y);
                        self.last_matches.insert(name.clone(), m);
                        self.last_seen.insert(name, now);
                    }
                }
                Err(TryRecvError::Empty)        => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        // Expirar entradas viejas (última aparición > STICKY_TTL atrás).
        self.last_matches.retain(|name, _| {
            self.last_seen.get(name)
                .map(|t| now.duration_since(*t) < STICKY_TTL)
                .unwrap_or(false)
        });

        // Enviar nuevo job si el intervalo venció
        let now = Instant::now();
        let should_submit = self.last_submitted
            .map(|last| now.duration_since(last) >= SUBMIT_INTERVAL)
            .unwrap_or(true);

        if should_submit {
            // Filtrado por priority:
            //   - Always → siempre incluido
            //   - OnDemand → solo si `on_demand_active` lo contiene
            // Sin filtro, el bg_worker tarda ~23s por ciclo (baseline 2026-04-18)
            // procesando templates que el step actual no necesita.
            let patches: Vec<UiPatch> = self.templates.iter()
                .filter(|tpl| match tpl.priority {
                    UiPriority::Always   => true,
                    UiPriority::OnDemand => self.on_demand_active.contains(&tpl.name),
                })
                .filter_map(|tpl| {
                    let (patch, roi_origin) = if let Some(roi) = tpl.roi {
                        (crop_to_gray(frame, roi)?, Some((roi.x, roi.y)))
                    } else {
                        (frame_to_gray(frame), None)
                    };
                    Some(UiPatch { name: tpl.name.clone(), patch, roi_origin })
                })
                .collect();

            if !patches.is_empty() {
                let n = patches.len();
                match job_tx.try_send(DetectJob { patches }) {
                    Ok(()) => tracing::debug!(
                        "UiDetector submit: sent job with {} patches (on_demand_active={:?})",
                        n, self.on_demand_active
                    ),
                    Err(e) => tracing::warn!("UiDetector submit: try_send FAILED ({}), {} patches DROPPED", e, n),
                }
            }
            self.last_submitted = Some(now);
        }
    }

    // ── OnDemand API ──────────────────────────────────────────────────────────
    //
    // Activar/desactivar templates caros según el step actual del cavebot.
    // El runner llama `request_on_demand` al entrar a un step que necesita el
    // template y `release_on_demand` al salir. Sin esto, templates como
    // stow_menu (20.9s por match) dominarían el cycle siempre.

    /// Activa un template OnDemand para que se incluya en los próximos cycles.
    /// No-op si el template es Always o si no está cargado. Idempotente.
    pub fn request_on_demand(&mut self, name: &str) {
        // No insertar nombres de templates que no existen — evita leak del set.
        let known = self.templates.iter().any(|t| t.name == name);
        if !known {
            tracing::debug!("UiDetector::request_on_demand: template '{}' no cargado, ignorando", name);
            return;
        }
        let added = self.on_demand_active.insert(name.to_string());
        if added {
            tracing::info!("UiDetector: on_demand ACTIVATED '{}' (active set: {:?})", name, self.on_demand_active);
        }
    }

    /// Desactiva un template OnDemand. No-op si no estaba activo.
    /// También limpia su entrada del cache para que un lectura posterior
    /// no encuentre datos stale del periodo en que estaba activo.
    pub fn release_on_demand(&mut self, name: &str) {
        let removed = self.on_demand_active.remove(name);
        if removed {
            // Purge cache: otherwise el consumer podría leer un match viejo
            // cuando el UI ya se cerró. Anti-click-en-fantasma.
            self.last_matches.remove(name);
            self.last_seen.remove(name);
            tracing::info!("UiDetector: on_demand RELEASED '{}' (active set: {:?})", name, self.on_demand_active);
        }
    }

    /// Sincroniza el set de OnDemand activos con una lista "deseada".
    /// Útil para el game loop que cada tick recibe `cavebot.required_ui_templates()`
    /// y quiere activar las nuevas + desactivar las que ya no están en la lista.
    /// Idempotente: llamarlo con el mismo set dos veces no tiene efectos visibles.
    pub fn sync_on_demand(&mut self, desired: &[&str]) {
        let desired_set: HashSet<String> = desired.iter().map(|s| s.to_string()).collect();
        // Liberar los que ya no están en `desired`.
        let to_release: Vec<String> = self.on_demand_active
            .iter().filter(|n| !desired_set.contains(*n)).cloned().collect();
        for name in to_release {
            self.release_on_demand(&name);
        }
        // Activar los nuevos.
        for name in desired {
            self.request_on_demand(name);
        }
    }

    /// Snapshot del set activo. Útil para debug endpoints (`/fsm/debug`).
    #[allow(dead_code)]
    pub fn on_demand_active(&self) -> Vec<String> {
        self.on_demand_active.iter().cloned().collect()
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

    // ── Priority split (Always / OnDemand) ────────────────────────────────────

    #[test]
    fn classify_template_defaults_to_on_demand() {
        // Templates desconocidos → OnDemand (conservador — evita que un nuevo
        // template grande degrade el cycle sin aviso).
        assert_eq!(classify_template("arbitrary_name"), UiPriority::OnDemand);
        assert_eq!(classify_template(""), UiPriority::OnDemand);
        // El bag icon chico es Always (10 ms de matching, vale la pena tenerlo
        // siempre caliente para OpenNpcTrade).
        assert_eq!(classify_template("npc_trade_bag"), UiPriority::Always);
    }

    #[test]
    fn request_on_demand_ignores_unknown_templates() {
        let (dir, _guard) = make_template_dir("square", 10, 255);
        let mut det = UiDetector::new(0.15);
        det.load_dir(&dir, &HashMap::new());

        det.request_on_demand("not_a_template");
        // El set queda vacío — evitamos leak de nombres inventados.
        assert!(det.on_demand_active().is_empty());
    }

    #[test]
    fn request_release_on_demand_toggles_active_set() {
        let (dir, _guard) = make_template_dir("square", 10, 255);
        let mut det = UiDetector::new(0.15);
        det.load_dir(&dir, &HashMap::new());

        // El template "square" es OnDemand (classify_template default).
        det.request_on_demand("square");
        assert_eq!(det.on_demand_active(), vec!["square".to_string()]);

        // request idempotente — no duplica.
        det.request_on_demand("square");
        assert_eq!(det.on_demand_active().len(), 1);

        det.release_on_demand("square");
        assert!(det.on_demand_active().is_empty());
    }

    #[test]
    fn release_on_demand_purges_cached_match() {
        // Simular un match cached para "square" y verificar que release lo borra.
        // Un caller podría leer last_matches tras release y encontrar el match
        // stale del step anterior → click fantasma. release debe purgar.
        let (dir, _guard) = make_template_dir("square", 10, 255);
        let mut det = UiDetector::new(0.15);
        det.load_dir(&dir, &HashMap::new());
        det.request_on_demand("square");

        // Simular que el bg_worker reportó un match insertándolo directamente.
        det.last_matches.insert("square".into(), UiMatchInfo {
            name: "square".into(),
            center_x: 50, center_y: 50, template_w: 10, template_h: 10,
        });
        det.last_seen.insert("square".into(), Instant::now());
        assert!(det.last_matches.contains_key("square"));

        det.release_on_demand("square");
        assert!(!det.last_matches.contains_key("square"),
            "release debe purgar el cache para evitar datos stale");
    }

    #[test]
    fn sync_on_demand_adds_and_removes_to_match_desired() {
        // Setup: cargamos tres templates OnDemand en el mismo dir.
        let (dir, _guard) = make_template_dir("alpha", 10, 255);
        // Añadir beta y gamma al mismo dir manualmente.
        let mut img_b = GrayImage::new(10, 10);
        for y in 0..10 { for x in 0..10 { img_b.put_pixel(x, y, image::Luma([100])); } }
        img_b.save(dir.join("beta.png")).unwrap();
        let mut img_g = GrayImage::new(10, 10);
        for y in 0..10 { for x in 0..10 { img_g.put_pixel(x, y, image::Luma([200])); } }
        img_g.save(dir.join("gamma.png")).unwrap();

        let mut det = UiDetector::new(0.15);
        det.load_dir(&dir, &HashMap::new());
        assert_eq!(det.len(), 3);

        // Activar alpha + beta.
        det.sync_on_demand(&["alpha", "beta"]);
        let active: std::collections::HashSet<String> =
            det.on_demand_active().into_iter().collect();
        assert!(active.contains("alpha"));
        assert!(active.contains("beta"));
        assert!(!active.contains("gamma"));

        // Cambiar a beta + gamma → alpha debe quedar fuera.
        det.sync_on_demand(&["beta", "gamma"]);
        let active: std::collections::HashSet<String> =
            det.on_demand_active().into_iter().collect();
        assert!(!active.contains("alpha"));
        assert!(active.contains("beta"));
        assert!(active.contains("gamma"));

        // Limpiar.
        det.sync_on_demand(&[]);
        assert!(det.on_demand_active().is_empty());
    }
}
