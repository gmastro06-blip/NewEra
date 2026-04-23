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
use crate::sense::vision::calibration::{AnchorDef, AnchorRole, RoiDef};

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

/// Estado de salud granular per-anchor. Complementa `fail_counts` (binary
/// healthy/lost) con estados intermedios observables desde el análisis
/// post-mortem + health system.
///
/// Transiciones:
/// - `Healthy` → `Degraded`: 3 scores consecutivos por encima del
///   `noise_floor_score` (p95 de los últimos N reads).
/// - `Degraded` → `Healthy`: 3 scores consecutivos dentro del noise floor.
/// - `Any` → `Lost`: fail_counts > max_fails (legado existente).
/// - `Lost` → `Recovering`: al submit un full-frame recovery job.
/// - `Recovering` → `Healthy`: match exitoso tras recovery.
///
/// Suspicious se marca externamente por el cluster offset cuando el anchor
/// deriva geométricamente (no encaja en el cluster). Se resetea al próximo
/// match coherente.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default,
         serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AnchorHealth {
    /// Matches consistentes, score dentro del ruido histórico esperado.
    #[default]
    Healthy    = 0,
    /// Match válido pero score elevado vs noise floor — el template
    /// encaja pero con menos confianza. Pre-warning de un failure.
    Degraded   = 1,
    /// Anchor matchea pero el offset diverge del cluster dominante.
    /// Probable false positive local — excluido del cálculo de offset
    /// por `cluster_offset`.
    Suspicious = 2,
    /// fail_counts > max_fails. matches[i] = None, cae al fallback
    /// last_good si existe + dentro de TTL.
    Lost       = 3,
    /// Full-frame recovery job enviado, esperando outcome. Estado
    /// transitorio entre Lost y (Healthy | Lost si el recovery falla).
    Recovering = 4,
}

impl AnchorHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            AnchorHealth::Healthy    => "healthy",
            AnchorHealth::Degraded   => "degraded",
            AnchorHealth::Suspicious => "suspicious",
            AnchorHealth::Lost       => "lost",
            AnchorHealth::Recovering => "recovering",
        }
    }

    pub fn is_usable(self) -> bool {
        matches!(self, AnchorHealth::Healthy | AnchorHealth::Degraded)
    }
}

/// Config runtime de la state machine de AnchorHealth.
/// Hardcoded porque son valores operacionales defensivos; expose cuando
/// validación live indique necesidad de tuning.
///
/// Ventana del ring `recent_scores`: 30 reads. Con `refresh_interval=500ms`
/// = 15s de histórico. Suficiente para detectar degradación progresiva
/// sin acumular scores muy viejos que distorsionen el noise floor.
const NOISE_FLOOR_WINDOW: usize = 30;
/// Percentil para calcular noise floor: p95 de los últimos N scores.
/// Un score que supere `noise_floor * DEGRADED_MULTIPLIER` cuenta como bad.
const DEGRADED_MULTIPLIER: f32 = 1.30;
/// Transiciones requieren N scores consecutivos para cambiar estado.
/// Filtra transients que sino disparan cambios espurios.
const HEALTH_STREAK_REQUIRED: u32 = 3;

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

/// TTL del `last_good`: si el ultimo match valido tiene mas que esto, el
/// fallback degradado deja de ser confiable. La ventana real puede haber
/// derivado mucho desde entonces — usar offset stale arriesga aplicar ROIs
/// fuera de pantalla. 10 segundos es generoso para Tibia (la ventana no se
/// mueve en hunt activo) pero corto para detectar cambios reales (resize,
/// minimize/restore, redibujado de UI).
const LAST_GOOD_TTL: Duration = Duration::from_secs(10);

#[derive(Default)]
struct AnchorShared {
    /// Current match (None si el anchor está actualmente lost).
    matches:     Vec<Option<AnchorMatch>>,
    /// Contador de fallos consecutivos para declarar "lost".
    fail_counts: Vec<u32>,
    /// Último match bueno conocido, usado como fallback si `matches[i]` es
    /// None. Nunca se borra — persiste entre ciclos lost/recovered.
    last_good:   Vec<Option<AnchorMatch>>,
    /// Timestamp del último update a `last_good[i]`. Usado para descartar
    /// fallbacks demasiado viejos (LAST_GOOD_TTL). `None` significa que el
    /// anchor nunca tuvo match exitoso desde startup.
    last_good_at: Vec<Option<Instant>>,
    /// Ring de los últimos `NOISE_FLOOR_WINDOW` scores de cada anchor.
    /// Cap fijo → cuando se llena, se descarta el más viejo (FIFO).
    /// Usado para calcular noise_floor (p95) y detectar degradación.
    recent_scores: Vec<std::collections::VecDeque<f32>>,
    /// Estado granular per-anchor. Default Healthy hasta que scores
    /// acumulados indiquen lo contrario. Mutado por el drain loop de
    /// `tick` tras cada outcome.
    health: Vec<AnchorHealth>,
    /// Streak counter para transiciones de health. Se resetea al cambiar
    /// de dirección (e.g., Healthy→Degraded candidate encuentra un score
    /// bueno → reset).
    health_streak: Vec<u32>,
    /// Noise floor cacheado (p95 de recent_scores). Recalculado al push
    /// de un score nuevo.
    noise_floor: Vec<f32>,
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
        s.last_good_at.push(None);
        s.recent_scores.push(std::collections::VecDeque::with_capacity(NOISE_FLOOR_WINDOW));
        s.health.push(AnchorHealth::Healthy);
        s.health_streak.push(0);
        s.noise_floor.push(0.30); // default = config.max_score típico
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
                        let was_lost_or_recovering = matches!(
                            s.health.get(idx).copied().unwrap_or_default(),
                            AnchorHealth::Lost | AnchorHealth::Recovering
                        );
                        if let Some(m) = outcome.m {
                            // Match exitoso — actualizar matches + last_good.
                            if s.fail_counts[idx] > self.config.max_fails {
                                tracing::info!(
                                    "Anchor #{}: recovered after lost state", idx
                                );
                            }
                            s.matches[idx]      = Some(m);
                            s.last_good[idx]    = Some(m);
                            s.last_good_at[idx] = Some(Instant::now());
                            s.fail_counts[idx]  = 0;

                            // ── AnchorHealth: ring + noise floor + transición ──
                            // SSE score: lower = better. Push a la ventana.
                            if let Some(ring) = s.recent_scores.get_mut(idx) {
                                if ring.len() >= NOISE_FLOOR_WINDOW {
                                    ring.pop_front();
                                }
                                ring.push_back(m.score);
                            }
                            let floor = compute_noise_floor_p95(
                                s.recent_scores.get(idx).map(|r| r.iter()),
                            );
                            if let Some(f) = s.noise_floor.get_mut(idx) {
                                *f = floor;
                            }
                            // Transición de health.
                            let new_health = decide_health_on_match(
                                s.health.get(idx).copied().unwrap_or_default(),
                                s.health_streak.get(idx).copied().unwrap_or(0),
                                m.score,
                                floor,
                                was_lost_or_recovering,
                            );
                            update_health_state(&mut s, idx, new_health);
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
                                update_health_state(&mut s, idx, AnchorHealth::Lost);
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
                        // Transición Lost → Recovering: esperando outcome del job.
                        let mut s = self.shared.write();
                        update_health_state(&mut s, idx, AnchorHealth::Recovering);
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
    /// Los matches lost usan `last_good` como fallback degradado SOLO si su
    /// `last_good_at` está dentro de `LAST_GOOD_TTL`. Last_good viejo se
    /// descarta — preferimos AllLost (pausa) que aplicar offset stale a las
    /// ROIs (puede dar coords fuera del frame y crashear lectores).
    ///
    /// **Failover primary/fallback**: si ≥1 primary está matcheando,
    /// los fallback anchors se IGNORAN (cluster solo sobre primaries).
    /// Si CERO primaries matchean pero ≥1 fallback sí → usa fallbacks
    /// con log INFO. Esto da tolerancia a degradación de templates sin
    /// sacrificar precisión en operación normal.
    pub fn window_offset(&self) -> (i32, i32) {
        let now = Instant::now();
        let s = self.shared.read();
        // Recolectar valid matches con su role (indexado por slot).
        let valid_by_role: Vec<(usize, AnchorMatch)> = s.matches.iter()
            .zip(s.last_good.iter())
            .zip(s.last_good_at.iter())
            .zip(s.fail_counts.iter())
            .enumerate()
            .filter_map(|(idx, (((m, lg), lg_at), &f))| {
                let matched = if f <= self.config.max_fails {
                    *m
                } else {
                    Self::filter_stale_fallback(*lg, *lg_at, now)
                };
                matched.map(|m| (idx, m))
            })
            .collect();
        drop(s);

        // Separar por role via self.anchors[idx].def.role.
        let (primaries, fallbacks): (Vec<AnchorMatch>, Vec<AnchorMatch>) =
            valid_by_role.into_iter().fold(
                (Vec::new(), Vec::new()),
                |(mut p, mut f), (idx, m)| {
                    match self.anchors.get(idx).map(|a| a.def.role) {
                        Some(AnchorRole::Primary) | None => p.push(m),
                        Some(AnchorRole::Fallback)       => f.push(m),
                    }
                    (p, f)
                },
            );

        // Prefer primaries. Usa fallbacks solo si CERO primaries matchean.
        let (valid, used_fallback) = if !primaries.is_empty() {
            (primaries, false)
        } else if !fallbacks.is_empty() {
            (fallbacks, true)
        } else {
            (Vec::new(), false)
        };

        if used_fallback {
            // Log throttled por AtomicBool/counter — por ahora uno por call.
            // En producción el log se queda en WARN porque es degraded state.
            tracing::warn!(
                "AnchorTracker: primaries all lost, using {} fallback anchor(s)",
                valid.len()
            );
        }

        // AllLost: anchors configurados pero ninguno válido en ningún role.
        let (offset, status) = if valid.is_empty() && !self.anchors.is_empty() {
            ((0, 0), DriftStatus::AllLost)
        } else {
            Self::cluster_offset(&valid)
        };
        self.last_drift.store(status.as_u8(), Ordering::Relaxed);
        offset
    }

    /// Pure helper: retorna `last_good` solo si su timestamp está dentro de
    /// `LAST_GOOD_TTL` desde `now`. Sin timestamp (anchor nunca matcheó) →
    /// None. Stale (timestamp >TTL atrás) → None + log warn.
    fn filter_stale_fallback(
        last_good: Option<AnchorMatch>,
        last_good_at: Option<Instant>,
        now: Instant,
    ) -> Option<AnchorMatch> {
        match (last_good, last_good_at) {
            (Some(m), Some(t)) if now.duration_since(t) <= LAST_GOOD_TTL => Some(m),
            (Some(_), Some(t)) => {
                tracing::warn!(
                    "AnchorTracker: last_good stale ({:.1}s > TTL {:.1}s) — descartando",
                    now.duration_since(t).as_secs_f32(),
                    LAST_GOOD_TTL.as_secs_f32()
                );
                None
            }
            _ => None,
        }
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

        // Score-weighted average: matches con score más bajo (mejor SSE)
        // dominan el promedio. weight = 1/(score+ε) para evitar div0.
        // Si todos los scores son similares, equivale al avg simple.
        // Si uno es notablemente peor (p.ej. 0.28 vs 0.05), pesa ~6× menos.
        const SCORE_WEIGHT_EPS: f32 = 0.01;
        let mut sum_w  = 0.0f32;
        let mut sum_wx = 0.0f32;
        let mut sum_wy = 0.0f32;
        for m in &cluster {
            let w = 1.0 / (m.score + SCORE_WEIGHT_EPS);
            sum_w  += w;
            sum_wx += m.offset_x as f32 * w;
            sum_wy += m.offset_y as f32 * w;
        }
        let ox = (sum_wx / sum_w).round() as i32;
        let oy = (sum_wy / sum_w).round() as i32;
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

    /// Snapshot del health state per-anchor (item post-live: AnchorHealth
    /// enum extended). Mantiene el orden de registration — `snapshot[i]`
    /// corresponde a `self.anchors[i]`.
    ///
    /// Campos por entry: (name, health, noise_floor, last_score).
    /// Usado por HealthSystem como input granular + HTTP `/vision/anchors`.
    pub fn anchor_health_snapshot(&self) -> Vec<AnchorHealthSnapshot> {
        let s = self.shared.read();
        self.anchors.iter().enumerate().map(|(idx, slot)| {
            AnchorHealthSnapshot {
                name:        slot.def.name.clone(),
                health:      s.health.get(idx).copied().unwrap_or_default(),
                noise_floor: s.noise_floor.get(idx).copied().unwrap_or(0.30),
                last_score:  s.matches.get(idx)
                    .and_then(|m| m.map(|am| am.score)),
                fail_count:  s.fail_counts.get(idx).copied().unwrap_or(0),
                samples_in_window: s.recent_scores.get(idx)
                    .map(|r| r.len()).unwrap_or(0),
            }
        }).collect()
    }
}

/// Snapshot per-anchor para consumers (HealthSystem, HTTP).
/// Public struct serializable para JSONL recorder.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnchorHealthSnapshot {
    pub name:        String,
    pub health:      AnchorHealth,
    pub noise_floor: f32,
    #[serde(default)]
    pub last_score:  Option<f32>,
    #[serde(default)]
    pub fail_count:  u32,
    #[serde(default)]
    pub samples_in_window: usize,
}

// ── AnchorHealth state machine helpers ──────────────────────────────────────

/// Calcula p95 del iterador de scores. Retorna `0.30` (default max_score)
/// si el iterador es vacío o None. Scores SSE: lower = better; p95 del
/// ruido = el 95to percentile → 5% peores scores observados.
fn compute_noise_floor_p95<'a, I>(scores: Option<I>) -> f32
where
    I: Iterator<Item = &'a f32>,
{
    let Some(iter) = scores else { return 0.30; };
    let mut vals: Vec<f32> = iter.copied().collect();
    if vals.is_empty() { return 0.30; }
    // Sort ascending (scores crecen con peor match en SSE).
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((vals.len() as f32) * 0.95) as usize;
    vals[idx.min(vals.len() - 1)]
}

/// Decide el estado siguiente basado en el score actual vs noise floor +
/// streak previa. Llamado sólo tras un match exitoso (outcome.m = Some).
fn decide_health_on_match(
    current: AnchorHealth,
    streak: u32,
    score: f32,
    noise_floor: f32,
    was_lost_or_recovering: bool,
) -> AnchorHealth {
    // Recovery inmediato desde Lost/Recovering: match exitoso tras
    // recovery → directo a Healthy sin esperar streak.
    if was_lost_or_recovering {
        return AnchorHealth::Healthy;
    }
    // Score "bad" = supera noise floor × multiplier.
    let threshold_bad = noise_floor * DEGRADED_MULTIPLIER;
    let is_bad = score > threshold_bad;

    match (current, is_bad) {
        (AnchorHealth::Healthy, false) => AnchorHealth::Healthy,
        (AnchorHealth::Healthy, true) => {
            // Streak de bad reads; si llega al required, degrada.
            if streak + 1 >= HEALTH_STREAK_REQUIRED {
                AnchorHealth::Degraded
            } else {
                AnchorHealth::Healthy
            }
        }
        (AnchorHealth::Degraded, true) => AnchorHealth::Degraded,
        (AnchorHealth::Degraded, false) => {
            // Streak de good reads; promueve cuando se alcanza required.
            if streak + 1 >= HEALTH_STREAK_REQUIRED {
                AnchorHealth::Healthy
            } else {
                AnchorHealth::Degraded
            }
        }
        (AnchorHealth::Suspicious, _) => {
            // Suspicious se limpia externamente por cluster_offset. Mientras,
            // mantener. (Match exitoso solo no refuta la divergencia geom.)
            AnchorHealth::Suspicious
        }
        (AnchorHealth::Lost | AnchorHealth::Recovering, _) => AnchorHealth::Healthy,
    }
}

/// Actualiza `health[idx]` + manejo de streak counter.
/// Llamar desde el drain loop u otros paths que cambian health state.
fn update_health_state(s: &mut AnchorShared, idx: usize, new_health: AnchorHealth) {
    let prev = s.health.get(idx).copied().unwrap_or_default();
    if idx < s.health.len() {
        if prev == new_health {
            // Streak de "mantener estado" cuenta (useful para transiciones
            // con required N consecutivos).
            s.health_streak[idx] = s.health_streak[idx].saturating_add(1);
        } else {
            // Transición real → reset streak + log.
            s.health[idx] = new_health;
            s.health_streak[idx] = 1;
            tracing::debug!(
                "Anchor #{}: health {:?} → {:?}",
                idx, prev, new_health
            );
        }
    }
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

    // ── AnchorHealth pure helpers (no tracker) ────────────────────────

    #[test]
    fn noise_floor_empty_is_default() {
        let v: Vec<f32> = vec![];
        let floor = compute_noise_floor_p95(Some(v.iter()));
        assert!((floor - 0.30).abs() < 0.001);
    }

    #[test]
    fn noise_floor_single_sample() {
        let v = vec![0.10f32];
        let floor = compute_noise_floor_p95(Some(v.iter()));
        assert!((floor - 0.10).abs() < 0.001);
    }

    #[test]
    fn noise_floor_p95_ordered() {
        // 100 scores rango 0.05..0.25; p95 cae cerca del valor en posición 95.
        let v: Vec<f32> = (0..100).map(|i| 0.05 + (i as f32) * 0.002).collect();
        let floor = compute_noise_floor_p95(Some(v.iter()));
        // p95 ≈ 0.05 + 95*0.002 = 0.24.
        assert!(floor > 0.23 && floor < 0.25, "floor={}", floor);
    }

    #[test]
    fn decide_health_recovery_from_lost_is_immediate() {
        // Match exitoso desde Lost → Healthy directo, sin streak.
        let h = decide_health_on_match(
            AnchorHealth::Lost, 0, 0.05, 0.30, true,
        );
        assert_eq!(h, AnchorHealth::Healthy);
    }

    #[test]
    fn decide_health_healthy_stays_with_good_score() {
        let h = decide_health_on_match(
            AnchorHealth::Healthy, 5, 0.05, 0.10, false,
        );
        assert_eq!(h, AnchorHealth::Healthy);
    }

    #[test]
    fn decide_health_healthy_to_degraded_requires_streak() {
        // Score bad (0.15 > noise 0.10 × 1.30 = 0.13) con streak 0, 1 → aún Healthy.
        // Al streak 2 (prev) + 1 (este) = 3 → Degraded.
        let h = decide_health_on_match(
            AnchorHealth::Healthy, 0, 0.15, 0.10, false,
        );
        assert_eq!(h, AnchorHealth::Healthy);  // 1 bad, streak llegará a 1.
        let h = decide_health_on_match(
            AnchorHealth::Healthy, 1, 0.15, 0.10, false,
        );
        assert_eq!(h, AnchorHealth::Healthy);  // 2 bad, streak llegará a 2.
        let h = decide_health_on_match(
            AnchorHealth::Healthy, 2, 0.15, 0.10, false,
        );
        assert_eq!(h, AnchorHealth::Degraded);  // 3 bad, cruza threshold.
    }

    #[test]
    fn decide_health_degraded_to_healthy_requires_streak() {
        // Good score desde Degraded con streak <required → se mantiene.
        let h = decide_health_on_match(
            AnchorHealth::Degraded, 0, 0.05, 0.10, false,
        );
        assert_eq!(h, AnchorHealth::Degraded);
        let h = decide_health_on_match(
            AnchorHealth::Degraded, 2, 0.05, 0.10, false,
        );
        assert_eq!(h, AnchorHealth::Healthy);
    }

    #[test]
    fn decide_health_suspicious_persists() {
        // Suspicious no se limpia automáticamente por match exitoso.
        let h = decide_health_on_match(
            AnchorHealth::Suspicious, 5, 0.05, 0.10, false,
        );
        assert_eq!(h, AnchorHealth::Suspicious);
    }

    #[test]
    fn anchor_health_is_usable_only_healthy_and_degraded() {
        assert!(AnchorHealth::Healthy.is_usable());
        assert!(AnchorHealth::Degraded.is_usable());
        assert!(!AnchorHealth::Suspicious.is_usable());
        assert!(!AnchorHealth::Lost.is_usable());
        assert!(!AnchorHealth::Recovering.is_usable());
    }

    #[test]
    fn anchor_health_snapshot_returns_initial_state() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        let snap = t.anchor_health_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].health, AnchorHealth::Healthy);
        assert_eq!(snap[0].fail_count, 0);
        assert_eq!(snap[0].samples_in_window, 0);
        assert!(snap[0].last_score.is_none());
    }

    #[test]
    fn anchor_health_snapshot_serializes_to_json() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        let snap = t.anchor_health_snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(json.contains("\"health\":\"healthy\""));
        assert!(json.contains("\"noise_floor\""));
    }

    fn make_tracker() -> AnchorTracker {
        AnchorTracker::new(AnchorConfig::default())
    }

    fn add_dummy_anchor(t: &mut AnchorTracker) {
        add_anchor_with_role(t, AnchorRole::Primary);
    }

    fn add_anchor_with_role(t: &mut AnchorTracker, role: AnchorRole) {
        let def = AnchorDef {
            name: format!("test_{}", match role {
                AnchorRole::Primary  => "primary",
                AnchorRole::Fallback => "fallback",
            }),
            template_path: "test.png".into(),
            expected_roi: RoiDef { x: 10, y: 10, w: 50, h: 50 },
            role,
        };
        t.add(def, None);
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
            s.last_good_at[0] = Some(Instant::now()); // fresco (dentro de TTL)
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
    /// los combina y el status es Ok. Score-weighted con scores iguales
    /// equivale a avg simple, redondeado al entero más cercano (no truncado
    /// como la integer-division previa). avg(5,6)=5.5→6, avg(2,1)=1.5→2.
    #[test]
    fn geometric_check_averages_consistent_pair() {
        let matches = vec![test_match(5, 2), test_match(6, 1)];
        let (offset, status) = AnchorTracker::cluster_offset(&matches);
        assert_eq!(offset, (6, 2));
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
            s.last_good_at[0] = Some(Instant::now()); // dentro de TTL
            s.fail_counts[0] = t.config.max_fails + 2; // lost
        }
        let _ = t.window_offset();
        assert_eq!(t.drift_status(), DriftStatus::Ok);
    }

    // ── filter_stale_fallback (TTL del last_good) ─────────────────────

    #[test]
    fn stale_filter_returns_none_when_no_timestamp() {
        let now = Instant::now();
        // Sin timestamp (anchor nunca matcheó) → no hay last_good utilizable.
        assert!(AnchorTracker::filter_stale_fallback(
            Some(test_match(5, 5)), None, now,
        ).is_none());
    }

    #[test]
    fn stale_filter_returns_none_when_no_match() {
        let now = Instant::now();
        // Sin last_good (None) → None, irrespective de timestamp.
        let t = now - std::time::Duration::from_secs(1);
        assert!(AnchorTracker::filter_stale_fallback(None, Some(t), now).is_none());
    }

    #[test]
    fn stale_filter_passes_recent_fallback() {
        let now = Instant::now();
        // 5s atrás está dentro del TTL (10s) → pass-through.
        let t = now - std::time::Duration::from_secs(5);
        let m = AnchorTracker::filter_stale_fallback(
            Some(test_match(7, -3)), Some(t), now,
        ).expect("recent last_good debe pasar");
        assert_eq!((m.offset_x, m.offset_y), (7, -3));
    }

    #[test]
    fn stale_filter_drops_old_fallback() {
        let now = Instant::now();
        // 11s atrás supera el TTL (10s) → descarta.
        let t = now - std::time::Duration::from_secs(11);
        assert!(AnchorTracker::filter_stale_fallback(
            Some(test_match(7, -3)), Some(t), now,
        ).is_none());
    }

    // ── Score-weighted cluster offset ─────────────────────────────────

    fn match_with_score(ox: i32, oy: i32, score: f32) -> AnchorMatch {
        AnchorMatch {
            found_x: 0, found_y: 0, score,
            offset_x: ox, offset_y: oy,
        }
    }

    #[test]
    fn weighted_cluster_prefers_low_score_match() {
        // 2 matches dentro del cluster geom tolerance.
        // Match A: offset=(0,0), score=0.05 (excelente).
        // Match B: offset=(10,10), score=0.25 (mediocre).
        // Avg simple = (5, 5). Weighted: A pesa ~5× más → cerca de (2, 2).
        let matches = vec![
            match_with_score(0, 0, 0.05),
            match_with_score(10, 10, 0.25),
        ];
        let ((ox, oy), status) = AnchorTracker::cluster_offset(&matches);
        assert_eq!(status, DriftStatus::Ok);
        // Weighted hacia (0,0) — esperamos algo significativamente menor que (5,5).
        assert!(ox < 4 && oy < 4, "weighted debe sesgar hacia low-score; got ({}, {})", ox, oy);
        assert!(ox > 0 && oy > 0, "no debe colapsar a (0,0); got ({}, {})", ox, oy);
    }

    #[test]
    fn weighted_cluster_equal_scores_equals_simple_avg() {
        // Mismos scores → weighted == avg simple. Prueba retrocompat.
        let matches = vec![
            match_with_score(4, 0, 0.05),
            match_with_score(6, 2, 0.05),
        ];
        let ((ox, oy), _) = AnchorTracker::cluster_offset(&matches);
        assert_eq!((ox, oy), (5, 1));
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

    // ── Failover primary/fallback ─────────────────────────────────────

    #[test]
    fn failover_uses_primary_when_available() {
        let mut t = make_tracker();
        add_anchor_with_role(&mut t, AnchorRole::Primary);
        add_anchor_with_role(&mut t, AnchorRole::Fallback);
        {
            let mut s = t.shared.write();
            s.matches[0] = Some(test_match(5, 5));
            s.last_good[0] = Some(test_match(5, 5));
            s.last_good_at[0] = Some(Instant::now());
            // Fallback también matchea pero en posición distinta → debería ignorarse.
            s.matches[1] = Some(test_match(100, 100));
            s.last_good[1] = Some(test_match(100, 100));
            s.last_good_at[1] = Some(Instant::now());
        }
        // Solo el primary (5,5) cuenta.
        assert_eq!(t.window_offset(), (5, 5));
    }

    #[test]
    fn failover_uses_fallback_when_primaries_lost() {
        let mut t = make_tracker();
        add_anchor_with_role(&mut t, AnchorRole::Primary);
        add_anchor_with_role(&mut t, AnchorRole::Fallback);
        {
            let mut s = t.shared.write();
            // Primary lost, sin last_good.
            s.matches[0] = None;
            s.last_good[0] = None;
            s.last_good_at[0] = None;
            s.fail_counts[0] = t.config.max_fails + 5;
            // Fallback matchea fresco.
            s.matches[1] = Some(test_match(7, 9));
            s.last_good[1] = Some(test_match(7, 9));
            s.last_good_at[1] = Some(Instant::now());
        }
        assert_eq!(t.window_offset(), (7, 9));
        // Status debe ser Ok — el fallback es valid.
        assert_eq!(t.drift_status(), DriftStatus::Ok);
    }

    #[test]
    fn failover_all_lost_both_roles_returns_zero() {
        let mut t = make_tracker();
        add_anchor_with_role(&mut t, AnchorRole::Primary);
        add_anchor_with_role(&mut t, AnchorRole::Fallback);
        {
            let mut s = t.shared.write();
            // Ninguno matchea ni tiene last_good.
            for i in 0..2 {
                s.matches[i] = None;
                s.last_good[i] = None;
                s.last_good_at[i] = None;
                s.fail_counts[i] = t.config.max_fails + 1;
            }
        }
        assert_eq!(t.window_offset(), (0, 0));
        assert_eq!(t.drift_status(), DriftStatus::AllLost);
    }

    #[test]
    fn failover_fallback_only_ignored_when_primary_has_last_good() {
        // Primary lost pero con last_good fresco dentro de TTL → cuenta como
        // primary valid, fallback se ignora.
        let mut t = make_tracker();
        add_anchor_with_role(&mut t, AnchorRole::Primary);
        add_anchor_with_role(&mut t, AnchorRole::Fallback);
        {
            let mut s = t.shared.write();
            s.matches[0] = None; // lost
            s.last_good[0] = Some(test_match(3, 3));
            s.last_good_at[0] = Some(Instant::now());
            s.fail_counts[0] = t.config.max_fails + 2;
            // Fallback fresco en otra posición.
            s.matches[1] = Some(test_match(50, 50));
            s.last_good[1] = Some(test_match(50, 50));
            s.last_good_at[1] = Some(Instant::now());
        }
        // Usa el last_good del primary (3,3), ignora fallback.
        assert_eq!(t.window_offset(), (3, 3));
    }

    #[test]
    fn failover_two_fallbacks_cluster_when_primaries_lost() {
        // Config realista: 1 primary muerto + 2 fallbacks coherentes.
        let mut t = make_tracker();
        add_anchor_with_role(&mut t, AnchorRole::Primary);
        add_anchor_with_role(&mut t, AnchorRole::Fallback);
        add_anchor_with_role(&mut t, AnchorRole::Fallback);
        {
            let mut s = t.shared.write();
            s.matches[0] = None;
            s.last_good[0] = None;
            s.last_good_at[0] = None;
            s.fail_counts[0] = t.config.max_fails + 10;
            // 2 fallbacks coherentes → cluster interno.
            s.matches[1] = Some(test_match(4, 0));
            s.last_good[1] = Some(test_match(4, 0));
            s.last_good_at[1] = Some(Instant::now());
            s.matches[2] = Some(test_match(6, 2));
            s.last_good[2] = Some(test_match(6, 2));
            s.last_good_at[2] = Some(Instant::now());
        }
        // Avg weighted (scores iguales) = (5, 1).
        assert_eq!(t.window_offset(), (5, 1));
    }

    /// Tras recovery (match nuevo tras ser lost), last_good se actualiza.
    /// Este test simula el outcome processing del drain loop.
    #[test]
    fn recovery_updates_last_good() {
        let mut t = make_tracker();
        add_dummy_anchor(&mut t);
        // Estado inicial: lost con last_good obsoleto pero dentro de TTL.
        {
            let mut s = t.shared.write();
            s.matches[0] = None;
            s.last_good[0] = Some(test_match(1, 1));
            s.last_good_at[0] = Some(Instant::now());
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
            s.last_good_at[0] = Some(Instant::now());
            s.fail_counts[0] = 0;
        }
        // Ahora el offset refleja el nuevo match.
        assert_eq!(t.window_offset(), (10, 20));
    }
}
