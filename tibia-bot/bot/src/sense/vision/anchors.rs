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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::{AnchorDef, RoiDef};

// ── Tipos públicos ─────────────────────────────────────────────────────────────

/// Resultado del check de consistencia de anchors tras cada tick.
/// Expuesto al FSM para decidir si pausar el bot cuando la ventana deriva
/// inconsistentemente.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftStatus {
    /// Hay ≥ 1 anchor válido y (si ≥ 2) forman cluster coherente.
    /// Bot puede operar con confianza sobre el offset calculado.
    #[default]
    Ok,
    /// ≥ 2 anchors válidos pero offsets divergen más allá de
    /// GEOMETRIC_TOLERANCE_PX. Ningún cluster dominante → offset (0,0) se
    /// aplica por fallback pero las ROIs apuntan a coords no confiables.
    /// El FSM debería pausar el bot con reason `anchors:drift_inconsistent`.
    Inconsistent,
    /// Ningún anchor válido (todos en estado lost o sin config). El tracker
    /// retorna (0, 0) porque no tiene info. Bot no sabe dónde está la
    /// ventana — pausar antes de emitir acciones.
    AllLost,
}

impl DriftStatus {
    fn as_u8(self) -> u8 {
        match self {
            DriftStatus::Ok           => 0,
            DriftStatus::Inconsistent => 1,
            DriftStatus::AllLost      => 2,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            1 => DriftStatus::Inconsistent,
            2 => DriftStatus::AllLost,
            _ => DriftStatus::Ok,
        }
    }
    pub fn is_ok(self) -> bool { matches!(self, DriftStatus::Ok) }
    /// Etiqueta corta para logs/reasons de safety pause.
    pub fn as_str(self) -> &'static str {
        match self {
            DriftStatus::Ok           => "ok",
            DriftStatus::Inconsistent => "inconsistent",
            DriftStatus::AllLost      => "all_lost",
        }
    }
}

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
    /// Último veredicto de consistencia computado por `window_offset`.
    /// Expuesto vía `drift_status()` para que el game loop + FSM decidan
    /// safety pauses. `AtomicU8` para permitir escritura desde `&self`
    /// (contrato de `window_offset(&self) -> (i32, i32)`).
    last_drift: AtomicU8,
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
            last_drift: AtomicU8::new(DriftStatus::Ok.as_u8()),
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

    /// Offset promedio de la ventana con **geometric consistency check** cuando
    /// hay ≥2 anchors. Un anchor con offset inconsistente vs los demás se
    /// descarta como false-positive en lugar de contaminar el promedio.
    ///
    /// Motivación (2026-04-18 — caso real): `sidebar_top` tras full-frame
    /// recovery matcheaba en la battle list panel (x=132) en vez del sidebar
    /// real (x=1700) → offset=-1568px que shift-eaba todas las ROIs al fuera
    /// del frame. Con un segundo anchor independiente (ej minimap_corner),
    /// ambos DEBEN reportar offsets similares (dentro de GEOMETRIC_TOLERANCE_PX).
    /// Si diverge uno, el cluster consistente lo excluye.
    ///
    /// Comportamiento:
    /// - 0 anchors válidos → (0, 0)
    /// - 1 anchor válido → su offset (sin comparación posible; confía en
    ///   `max_score` para evitar matches absurdos)
    /// - ≥2 anchors válidos → geometric filtering: los matches dentro de
    ///   `GEOMETRIC_TOLERANCE_PX` del cluster dominante se promedian; el
    ///   resto se descarta con warning. Si ninguno forma cluster consistente
    ///   (todos divergen), retorna (0, 0) con warning.
    ///
    /// Los matches lost usan `last_good` como fallback degradado.
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
        drop(s);

        // AllLost: anchors configurados pero ninguno con valid ni last_good.
        // Distinto de "sin anchors" (total_anchor_count==0 → no hay concepto
        // de drift, se reporta Ok). Con anchors configurados y cero valid,
        // el offset (0,0) no es confiable → DriftStatus::AllLost para que
        // el FSM decida pausar.
        let (offset, status) = if valid.is_empty() && !self.anchors.is_empty() {
            ((0, 0), DriftStatus::AllLost)
        } else {
            Self::cluster_offset(&valid)
        };
        self.last_drift.store(status.as_u8(), Ordering::Relaxed);
        offset
    }

    /// Veredicto de consistencia del último `window_offset`. Expuesto al game
    /// loop para decidir safety pause cuando el tracker no puede confiar en
    /// la ventana (drift divergente o anchors todos lost).
    ///
    /// IMPORTANTE: `window_offset()` debe haberse llamado al menos una vez
    /// en el tick actual para que este valor esté fresco. El game loop típico
    /// llama `adjust_roi()` (que internamente llama `window_offset()`) antes
    /// de leer el status.
    pub fn drift_status(&self) -> DriftStatus {
        DriftStatus::from_u8(self.last_drift.load(Ordering::Relaxed))
    }

    /// Aplica geometric consistency filtering al set de matches y devuelve
    /// el offset promedio del cluster dominante junto con un `DriftStatus`
    /// que indica si la geometría es coherente o divergente.
    ///
    /// - `valid.is_empty()` → `((0, 0), Ok)` — el caller decide si esto
    ///   representa `AllLost` (hay anchors configurados) o simplemente no
    ///   hay anchors (status Ok genuino).
    /// - `valid.len() == 1` → `(offset, Ok)` — sin punto de comparación,
    ///   se confía en el single match (filtrado upstream por `max_score`).
    /// - `valid.len() ≥ 2` con cluster dominante → `(avg, Ok)`.
    /// - `valid.len() ≥ 2` sin cluster (todos divergen) → `((0, 0), Inconsistent)`.
    ///
    /// Expuesto como método asociado para testabilidad.
    fn cluster_offset(valid: &[AnchorMatch]) -> ((i32, i32), DriftStatus) {
        if valid.is_empty() {
            return ((0, 0), DriftStatus::Ok);
        }
        if valid.len() == 1 {
            return ((valid[0].offset_x, valid[0].offset_y), DriftStatus::Ok);
        }
        const GEOMETRIC_TOLERANCE_PX: i32 = 15;

        // Cluster dominante: el match con más vecinos dentro de tolerance
        // es el "pivote" del cluster. Los vecinos se incluyen en el promedio.
        // Coste O(n²) pero n ≤ 5 en la práctica — negligible.
        let mut best_pivot = 0usize;
        let mut best_count = 0usize;
        for (i, m_i) in valid.iter().enumerate() {
            let count = valid.iter()
                .filter(|m_j| {
                    (m_j.offset_x - m_i.offset_x).abs() <= GEOMETRIC_TOLERANCE_PX
                        && (m_j.offset_y - m_i.offset_y).abs() <= GEOMETRIC_TOLERANCE_PX
                })
                .count();
            if count > best_count {
                best_count  = count;
                best_pivot  = i;
            }
        }

        // Sin cluster de ≥2 → todos los matches divergen entre sí. Ningún
        // anchor confiable → (0, 0) + Inconsistent para que el FSM pause.
        if best_count < 2 {
            tracing::warn!(
                "AnchorTracker: {} anchors inconsistentes (offsets: {:?}), \
                 ninguno forma cluster dentro de ±{}px. Usando (0, 0).",
                valid.len(),
                valid.iter().map(|m| (m.offset_x, m.offset_y)).collect::<Vec<_>>(),
                GEOMETRIC_TOLERANCE_PX
            );
            return ((0, 0), DriftStatus::Inconsistent);
        }

        let pivot = &valid[best_pivot];
        let cluster: Vec<&AnchorMatch> = valid.iter()
            .filter(|m| {
                (m.offset_x - pivot.offset_x).abs() <= GEOMETRIC_TOLERANCE_PX
                    && (m.offset_y - pivot.offset_y).abs() <= GEOMETRIC_TOLERANCE_PX
            })
            .collect();

        if cluster.len() < valid.len() {
            let outliers: Vec<(i32, i32)> = valid.iter()
                .filter(|m| !cluster.iter().any(|c| std::ptr::eq(*c, *m)))
                .map(|m| (m.offset_x, m.offset_y))
                .collect();
            tracing::warn!(
                "AnchorTracker: descartando {} outlier(s) {:?} (cluster pivote=({}, {}))",
                outliers.len(), outliers, pivot.offset_x, pivot.offset_y
            );
        }

        let n  = cluster.len() as i32;
        let ox = cluster.iter().map(|m| m.offset_x).sum::<i32>() / n;
        let oy = cluster.iter().map(|m| m.offset_y).sum::<i32>() / n;
        ((ox, oy), DriftStatus::Ok)
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

    // ── Geometric consistency check (2+ anchors) ──────────────────────────

    /// Con 2 anchors coherentes (offsets dentro de tolerance), el promedio
    /// los combina y el status es Ok.
    #[test]
    fn geometric_check_averages_consistent_pair() {
        let matches = vec![test_match(5, 2), test_match(6, 1)];
        let (offset, status) = AnchorTracker::cluster_offset(&matches);
        assert_eq!(offset, (5, 1));
        assert_eq!(status, DriftStatus::Ok);
    }

    /// Con 2 anchors de offsets divergentes, NINGUNO forma cluster → (0, 0)
    /// + status Inconsistent. Este es el caso real: un anchor tiene
    /// false-positive de -1568px, otro tiene offset correcto de 0.
    #[test]
    fn geometric_check_rejects_divergent_pair() {
        let matches = vec![test_match(-1568, 0), test_match(0, 0)];
        let (offset, status) = AnchorTracker::cluster_offset(&matches);
        assert_eq!(offset, (0, 0));
        assert_eq!(status, DriftStatus::Inconsistent);
    }

    /// Con 3 anchors donde 2 coinciden y 1 es outlier, el outlier se
    /// descarta y los 2 consistentes se promedian. Status = Ok.
    #[test]
    fn geometric_check_drops_outlier_keeps_cluster() {
        let matches = vec![
            test_match(4, 0),
            test_match(6, 2),
            test_match(-1568, 0), // outlier
        ];
        let ((ox, oy), status) = AnchorTracker::cluster_offset(&matches);
        assert_eq!(ox, 5); // avg(4, 6)
        assert_eq!(oy, 1); // avg(0, 2)
        assert_eq!(status, DriftStatus::Ok);
    }

    /// Un solo anchor sigue funcionando — status Ok (sin punto de comparación).
    #[test]
    fn geometric_check_single_anchor_passes_through() {
        let matches = vec![test_match(7, -3)];
        let (offset, status) = AnchorTracker::cluster_offset(&matches);
        assert_eq!(offset, (7, -3));
        assert_eq!(status, DriftStatus::Ok);
    }

    /// Lista vacía → (0, 0), status Ok (el caller decide si es AllLost).
    #[test]
    fn geometric_check_empty_returns_zero() {
        let (offset, status) = AnchorTracker::cluster_offset(&[]);
        assert_eq!(offset, (0, 0));
        assert_eq!(status, DriftStatus::Ok);
    }

    // ── DriftStatus integration via window_offset ─────────────────────────

    /// Sin anchors configurados → drift_status = Ok (no hay drift concept).
    #[test]
    fn drift_status_no_anchors_is_ok() {
        let t = make_tracker();
        let _ = t.window_offset();
        assert_eq!(t.drift_status(), DriftStatus::Ok);
    }

    /// Anchor configurado pero sin match ni last_good → AllLost.
    #[test]
    fn drift_status_all_lost_when_configured_but_no_matches() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        // estado por default: matches=None, last_good=None, fail_counts=0.
        // fail_counts=0 ≤ max_fails, pero matches es None → filtermap devuelve None.
        let _ = t.window_offset();
        assert_eq!(t.drift_status(), DriftStatus::AllLost);
    }

    /// Un match válido → Ok.
    #[test]
    fn drift_status_single_valid_match_is_ok() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        {
            let mut s = t.shared.write();
            s.matches[0] = Some(test_match(5, 2));
            s.last_good[0] = Some(test_match(5, 2));
        }
        let _ = t.window_offset();
        assert_eq!(t.drift_status(), DriftStatus::Ok);
    }

    /// 2 anchors divergentes → Inconsistent propagado desde cluster_offset.
    #[test]
    fn drift_status_inconsistent_pair_propagates() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        add_dummy_anchor(&mut t);
        {
            let mut s = t.shared.write();
            s.matches[0] = Some(test_match(-1568, 0));
            s.last_good[0] = Some(test_match(-1568, 0));
            s.matches[1] = Some(test_match(0, 0));
            s.last_good[1] = Some(test_match(0, 0));
        }
        let _ = t.window_offset();
        assert_eq!(t.drift_status(), DriftStatus::Inconsistent);
    }

    /// Anchor degradado a last_good (match actual es None) sigue contando
    /// como "válido" para DriftStatus — el offset no es fresco pero es
    /// aplicable. Status = Ok (sólo hay un "valid" via fallback).
    #[test]
    fn drift_status_last_good_fallback_counts_as_valid() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        {
            let mut s = t.shared.write();
            s.matches[0] = None;
            s.last_good[0] = Some(test_match(3, 3));
            s.fail_counts[0] = t.config.max_fails + 2; // lost
        }
        let _ = t.window_offset();
        assert_eq!(t.drift_status(), DriftStatus::Ok);
    }

    /// Transición Ok → Inconsistent → Ok se refleja en status cada tick.
    #[test]
    fn drift_status_updates_on_each_call() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        add_dummy_anchor(&mut t);

        // Estado 1: divergentes → Inconsistent.
        {
            let mut s = t.shared.write();
            s.matches[0] = Some(test_match(-100, 0));
            s.last_good[0] = Some(test_match(-100, 0));
            s.matches[1] = Some(test_match(100, 0));
            s.last_good[1] = Some(test_match(100, 0));
        }
        let _ = t.window_offset();
        assert_eq!(t.drift_status(), DriftStatus::Inconsistent);

        // Estado 2: ambos coherentes → Ok.
        {
            let mut s = t.shared.write();
            s.matches[0] = Some(test_match(5, 0));
            s.last_good[0] = Some(test_match(5, 0));
            s.matches[1] = Some(test_match(6, 0));
            s.last_good[1] = Some(test_match(6, 0));
        }
        let _ = t.window_offset();
        assert_eq!(t.drift_status(), DriftStatus::Ok);
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
