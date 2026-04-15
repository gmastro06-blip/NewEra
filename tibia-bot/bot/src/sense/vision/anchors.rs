/// anchors.rs — AnchorTracker con template matching en thread de fondo.
///
/// imageproc::match_template puede tardar 100-600ms en modo debug.
/// Para no bloquear el game loop (presupuesto 33ms), el matching corre en un
/// thread dedicado ("anchor-matcher"). El game loop llama tick() que:
///   1. Drena resultados pendientes del background (non-blocking).
///   2. Envía nuevos jobs vía bounded channel (try_send — descarta si ocupado).
///
/// Consecuencia: los offsets de ancla se actualizan con un retraso de 1 ciclo
/// de matching (~500ms en debug, <5ms en release). Para una ventana fija esto
/// es completamente irrelevante.

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use image::{GrayImage, Luma};
use imageproc::template_matching::{match_template, MatchTemplateMethod};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::{AnchorDef, RoiDef};

// ── Tipos públicos ─────────────────────────────────────────────────────────────

/// Resultado de localizar un ancla en el frame actual.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // extension point: found_x/y/score exposed for diagnostics
pub struct AnchorMatch {
    pub found_x:  u32,
    pub found_y:  u32,
    pub score:    f32,
    pub offset_x: i32,
    pub offset_y: i32,
}

/// Configuración del AnchorTracker.
pub struct AnchorConfig {
    /// Cada cuánto tiempo volver a enviar un job de matching.
    pub refresh_interval: Duration,
    /// Puntuación máxima (normalized SSD ∈ [0, ~2]; 0 = match perfecto).
    pub max_score:        f32,
    /// Tras cuántos fallos consecutivos se marca el anchor como "lost".
    pub max_fails:        u32,
    /// Cada cuánto reintenta recovery con búsqueda full-frame cuando un
    /// anchor está lost. El retry es costoso (match contra frame completo)
    /// por eso se limita a cadencia baja.
    pub lost_retry_interval: Duration,
}

impl Default for AnchorConfig {
    fn default() -> Self {
        Self {
            refresh_interval: Duration::from_millis(500),
            // SumOfSquaredErrorsNormalized: 0.0 = perfecto, ~1.0 = mal match.
            // 0.30 = umbral práctico para frames NDI con compresión.
            // Si el ancla nunca matchea, subir a 0.50 y revisar debug.png.
            max_score:        0.30,
            max_fails:        10,
            lost_retry_interval: Duration::from_secs(30),
        }
    }
}

// ── Mensajes internos entre game loop y background ────────────────────────────

struct MatchJob {
    anchor_idx: usize,
    patch:      GrayImage,
    template:   Arc<GrayImage>,
    search_x:   u32,
    search_y:   u32,
    expected_x: u32,
    expected_y: u32,
    max_score:  f32,
}

struct MatchOutcome {
    anchor_idx: usize,
    m:          Option<AnchorMatch>,
}

// ── Estado compartido (background escribe, game loop lee) ─────────────────────

#[derive(Default)]
struct AnchorShared {
    /// Current match (None si el anchor está actualmente lost).
    matches:     Vec<Option<AnchorMatch>>,
    /// Contador de fallos consecutivos para declarar "lost".
    fail_counts: Vec<u32>,
    /// Último match bueno conocido, usado como fallback si `matches[i]` es
    /// None. Nunca se borra — persiste entre ciclos lost/recovered.
    last_good:   Vec<Option<AnchorMatch>>,
}

// ── Slot de ancla (datos estáticos + scheduling) ──────────────────────────────

struct AnchorSlot {
    def:            AnchorDef,
    template:       Option<Arc<GrayImage>>,
    last_submitted: Option<Instant>,
    /// Última vez que se intentó recovery full-frame (solo cuando lost).
    last_lost_retry: Option<Instant>,
}

// ── AnchorTracker ─────────────────────────────────────────────────────────────

pub struct AnchorTracker {
    config:     AnchorConfig,
    anchors:    Vec<AnchorSlot>,
    shared:     Arc<RwLock<AnchorShared>>,
    job_tx:     Sender<MatchJob>,
    outcome_rx: Receiver<MatchOutcome>,
}

impl AnchorTracker {
    pub fn new(config: AnchorConfig) -> Self {
        // Canal de jobs: capacity 1 — si el background está ocupado, try_send falla
        // y el game loop simplemente no envía (reintenta el próximo tick).
        let (job_tx, job_rx) = bounded::<MatchJob>(1);
        // Canal de resultados: capacity 4 — el background puede tener varios listos.
        let (outcome_tx, outcome_rx) = bounded::<MatchOutcome>(4);

        std::thread::Builder::new()
            .name("anchor-matcher".into())
            .spawn(move || {
                for job in job_rx {
                    let result = bg_find_template(
                        &job.patch, &job.template,
                        job.search_x, job.search_y,
                        job.expected_x, job.expected_y,
                        job.max_score,
                    );
                    let _ = outcome_tx.send(MatchOutcome {
                        anchor_idx: job.anchor_idx,
                        m: result,
                    });
                }
            })
            .expect("No se pudo lanzar anchor-matcher thread");

        Self {
            config,
            anchors:    Vec::new(),
            shared:     Arc::new(RwLock::new(AnchorShared::default())),
            job_tx,
            outcome_rx,
        }
    }

    /// Registra un ancla. Debe llamarse antes del primer tick().
    pub fn add(&mut self, def: AnchorDef, template: Option<GrayImage>) {
        self.anchors.push(AnchorSlot {
            def,
            template:       template.map(Arc::new),
            last_submitted: None,
            last_lost_retry: None,
        });
        let mut s = self.shared.write();
        s.matches.push(None);
        s.fail_counts.push(0);
        s.last_good.push(None);
    }

    /// Llamar una vez por tick desde el game loop. NUNCA bloquea.
    ///
    /// 1. Drena outcomes pendientes del background → actualiza cache compartida.
    /// 2. Para cada ancla lista (throttle vencido): envía MatchJob vía try_send.
    pub fn tick(&mut self, frame: &Frame) {
        // ── 1. Drenar resultados pendientes ───────────────────────────────────
        loop {
            match self.outcome_rx.try_recv() {
                Ok(outcome) => {
                    let idx = outcome.anchor_idx;
                    let mut s = self.shared.write();
                    if idx < s.matches.len() && idx < s.fail_counts.len() {
                        if let Some(m) = outcome.m {
                            // Match exitoso — actualizar matches + last_good.
                            if s.fail_counts[idx] > self.config.max_fails {
                                tracing::info!(
                                    "Anchor #{}: recovered after lost state", idx
                                );
                            }
                            s.matches[idx]   = Some(m);
                            s.last_good[idx] = Some(m);
                            s.fail_counts[idx] = 0;
                        } else {
                            s.fail_counts[idx] += 1;
                            if s.fail_counts[idx] == self.config.max_fails + 1 {
                                tracing::warn!(
                                    "Anchor #{}: lost after {} consecutive fails",
                                    idx, self.config.max_fails
                                );
                            }
                            if s.fail_counts[idx] > self.config.max_fails {
                                s.matches[idx] = None;
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty)        => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // ── 2. Enviar nuevos jobs ──────────────────────────────────────────────
        let now = Instant::now();
        // Snapshot de lost-state para no tomar el lock repetidamente.
        let lost_states: Vec<bool> = {
            let s = self.shared.read();
            s.fail_counts.iter()
                .map(|&f| f > self.config.max_fails)
                .collect()
        };
        for (idx, anchor) in self.anchors.iter_mut().enumerate() {
            let is_lost = lost_states.get(idx).copied().unwrap_or(false);

            // Throttle: anchors sanos usan refresh_interval; anchors lost usan
            // lost_retry_interval (más lento) porque hacen búsqueda full-frame.
            if is_lost {
                if let Some(last) = anchor.last_lost_retry {
                    if now.duration_since(last) < self.config.lost_retry_interval {
                        continue;
                    }
                }
            } else if let Some(last) = anchor.last_submitted {
                if now.duration_since(last) < self.config.refresh_interval {
                    continue;
                }
            }

            let template = match &anchor.template {
                Some(t) => Arc::clone(t),
                None    => continue,
            };

            // Lost → búsqueda full-frame. Healthy → ROI expandido ±30px.
            let search_roi = if is_lost {
                RoiDef { x: 0, y: 0, w: frame.width, h: frame.height }
            } else {
                expand_roi(anchor.def.expected_roi, 30, frame.width, frame.height)
            };
            let patch = extract_gray(frame, search_roi);

            let job = MatchJob {
                anchor_idx: idx,
                patch,
                template,
                search_x:   search_roi.x,
                search_y:   search_roi.y,
                expected_x: anchor.def.expected_roi.x,
                expected_y: anchor.def.expected_roi.y,
                max_score:  self.config.max_score,
            };

            match self.job_tx.try_send(job) {
                Ok(()) => {
                    if is_lost {
                        anchor.last_lost_retry = Some(now);
                        tracing::info!(
                            "Anchor #{}: submitted full-frame recovery job", idx
                        );
                    } else {
                        anchor.last_submitted = Some(now);
                    }
                }
                Err(_) => {
                    // Background ocupado — no actualizar last_* para que el
                    // próximo tick reintente inmediatamente.
                }
            }
        }
    }

    /// Offset promedio de la ventana según todos los anclas con match válido.
    /// Si un anchor está lost pero tiene `last_good`, usa ese offset como
    /// fallback degradado (mejor que (0,0) si la ventana no se movió).
    /// Retorna (0, 0) si ningún anchor tiene match ni last_good.
    pub fn window_offset(&self) -> (i32, i32) {
        let s = self.shared.read();
        let valid: Vec<AnchorMatch> = s.matches.iter()
            .zip(s.last_good.iter())
            .zip(s.fail_counts.iter())
            .filter_map(|((m, lg), &f)| {
                if f <= self.config.max_fails {
                    *m
                } else {
                    // Lost: usar last_good si existe (persistido desde último match).
                    *lg
                }
            })
            .collect();

        if valid.is_empty() {
            return (0, 0);
        }

        let n = valid.len() as i32;
        let ox = valid.iter().map(|m| m.offset_x).sum::<i32>() / n;
        let oy = valid.iter().map(|m| m.offset_y).sum::<i32>() / n;
        (ox, oy)
    }

    /// Ajusta un ROI aplicando el offset calculado.
    pub fn adjust_roi(&self, roi: RoiDef) -> RoiDef {
        let (ox, oy) = self.window_offset();
        RoiDef {
            x: (roi.x as i32 + ox).max(0) as u32,
            y: (roi.y as i32 + oy).max(0) as u32,
            w: roi.w,
            h: roi.h,
        }
    }

    pub fn valid_anchor_count(&self) -> usize {
        let s = self.shared.read();
        s.matches.iter()
            .zip(s.fail_counts.iter())
            .filter(|(m, &f)| m.is_some() && f <= self.config.max_fails)
            .count()
    }

    pub fn total_anchor_count(&self) -> usize { self.anchors.len() }
}

// ── Background: template matching ─────────────────────────────────────────────

fn bg_find_template(
    patch:      &GrayImage,
    template:   &GrayImage,
    search_x:   u32,
    search_y:   u32,
    expected_x: u32,
    expected_y: u32,
    max_score:  f32,
) -> Option<AnchorMatch> {
    if patch.width() < template.width() || patch.height() < template.height() {
        tracing::warn!(
            "Ancla: patch ({}×{}) más chico que template ({}×{}) — skip",
            patch.width(), patch.height(), template.width(), template.height()
        );
        return None;
    }

    let scores = match_template(
        patch,
        template,
        MatchTemplateMethod::SumOfSquaredErrorsNormalized,
    );

    let (mut best_x, mut best_y) = (0u32, 0u32);
    let mut best_score = f32::MAX;
    for (x, y, px) in scores.enumerate_pixels() {
        let s = px[0];
        if s < best_score {
            best_score = s;
            best_x = x;
            best_y = y;
        }
    }

    if best_score > max_score {
        tracing::debug!(
            "Ancla: score={:.4} > umbral={:.2} — LOST. best_pos=({},{})",
            best_score, max_score, search_x + best_x, search_y + best_y
        );
        return None;
    }

    tracing::info!(
        "Ancla: score={:.4} (umbral={:.2}) FOUND best_pos=({},{})",
        best_score, max_score, search_x + best_x, search_y + best_y
    );

    let found_x = search_x + best_x;
    let found_y = search_y + best_y;
    Some(AnchorMatch {
        found_x,
        found_y,
        score:    best_score,
        offset_x: found_x as i32 - expected_x as i32,
        offset_y: found_y as i32 - expected_y as i32,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn expand_roi(roi: RoiDef, margin: u32, fw: u32, fh: u32) -> RoiDef {
    let x  = roi.x.saturating_sub(margin);
    let y  = roi.y.saturating_sub(margin);
    let x2 = (roi.x + roi.w + margin).min(fw);
    let y2 = (roi.y + roi.h + margin).min(fh);
    RoiDef { x, y, w: x2 - x, h: y2 - y }
}

fn extract_gray(frame: &Frame, roi: RoiDef) -> GrayImage {
    let stride = frame.width as usize * 4;
    let mut img = GrayImage::new(roi.w, roi.h);
    for row in 0..roi.h {
        for col in 0..roi.w {
            let off = (roi.y + row) as usize * stride + (roi.x + col) as usize * 4;
            if off + 2 < frame.data.len() {
                let r   = frame.data[off]     as u32;  // RGBA: byte[0]=R
                let g   = frame.data[off + 1] as u32;
                let b   = frame.data[off + 2] as u32;  // RGBA: byte[2]=B
                let lum = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
                img.put_pixel(col, row, Luma([lum]));
            }
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_match(ox: i32, oy: i32) -> AnchorMatch {
        AnchorMatch {
            found_x: 0, found_y: 0, score: 0.05,
            offset_x: ox, offset_y: oy,
        }
    }

    fn make_tracker() -> AnchorTracker {
        AnchorTracker::new(AnchorConfig::default())
    }

    fn add_dummy_anchor(t: &mut AnchorTracker) {
        let def = AnchorDef {
            name: "test".into(),
            template_path: "test.png".into(),
            expected_roi: RoiDef { x: 10, y: 10, w: 50, h: 50 },
        };
        t.add(def, None); // template=None: no jobs will be submitted
    }

    /// Sanity: tracker vacío reporta (0,0) y 0 anchors.
    #[test]
    fn empty_tracker_zero_offset() {
        let t = make_tracker();
        assert_eq!(t.window_offset(), (0, 0));
        assert_eq!(t.total_anchor_count(), 0);
    }

    /// Un match reciente produce el offset correspondiente.
    #[test]
    fn valid_match_yields_offset() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        {
            let mut s = t.shared.write();
            s.matches[0] = Some(test_match(5, -3));
            s.last_good[0] = Some(test_match(5, -3));
            s.fail_counts[0] = 0;
        }
        assert_eq!(t.window_offset(), (5, -3));
    }

    /// Anchor lost con last_good → usa el fallback degradado.
    #[test]
    fn lost_anchor_falls_back_to_last_good() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        {
            let mut s = t.shared.write();
            s.matches[0] = None; // lost
            s.last_good[0] = Some(test_match(7, 2)); // persisted
            s.fail_counts[0] = t.config.max_fails + 5; // lost state
        }
        // Debe usar last_good como fallback.
        assert_eq!(t.window_offset(), (7, 2));
    }

    /// Anchor lost SIN last_good → retorna (0, 0).
    #[test]
    fn lost_anchor_without_last_good_returns_zero() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        {
            let mut s = t.shared.write();
            s.matches[0] = None;
            s.last_good[0] = None;
            s.fail_counts[0] = t.config.max_fails + 1;
        }
        assert_eq!(t.window_offset(), (0, 0));
    }

    /// Tras recovery (match nuevo tras ser lost), last_good se actualiza.
    /// Este test simula el outcome processing del drain loop.
    #[test]
    fn recovery_updates_last_good() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        // Estado inicial: lost con last_good obsoleto.
        {
            let mut s = t.shared.write();
            s.matches[0] = None;
            s.last_good[0] = Some(test_match(1, 1));
            s.fail_counts[0] = t.config.max_fails + 2;
        }
        assert_eq!(t.window_offset(), (1, 1));

        // Simulamos un outcome exitoso (como si el background thread lo hubiera
        // procesado): replicamos la lógica del drain loop de `tick`.
        {
            let mut s = t.shared.write();
            let new_match = test_match(10, 20);
            s.matches[0] = Some(new_match);
            s.last_good[0] = Some(new_match);
            s.fail_counts[0] = 0;
        }
        // Ahora el offset refleja el nuevo match.
        assert_eq!(t.window_offset(), (10, 20));
    }
}
