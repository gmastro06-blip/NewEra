//! registry.rs — MetricsRegistry: dueño de histogramas + ArcSwap del último tick.
//!
//! Patrón de ownership:
//! - `Arc<MetricsRegistry>` clonado a BotLoop, bridge thread, HTTP handlers.
//! - Los histogramas son AtomicU64 internamente — multi-writer safe.
//! - El ArcSwap<TickMetrics> permite reads lock-free desde HTTP.
//! - Los rolling windows (CircularU32) viven dentro de un struct interior
//!   que solo se accede desde el game loop thread (single writer).
//!
//! BotLoop hace `metrics.record_tick(m)` al final de cada tick — punto único
//! de actualización de histogramas + windows + ArcSwap publish.
//!
//! El bridge thread llama `metrics.record_action_ack(kind, rtt, ok)` cuando
//! recibe el ACK del Arduino — actualiza histograma de RTT cross-thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use arc_swap::ArcSwap;
use parking_lot::{Mutex, RwLock};

use super::histogram::LatencyHistogram;
use super::recorder::MetricsRecorder;
use super::tick::{ActionKindTag, ReaderId, TickMetrics, TickFlags};
use super::window::CircularU32;

/// Capacidad para el window de jitter — 1 segundo @ 30 Hz.
pub const JITTER_WINDOW_TICKS: usize = 30;
/// Capacidad para el window de FPS estable — 10 segundos @ 30 Hz.
pub const FPS_WINDOW_TICKS: usize = 300;

/// Registry global de métricas. Una instancia única por proceso, compartida
/// vía `Arc`.
pub struct MetricsRegistry {
    // ── Histogramas (multi-writer atomic) ────────────────────────────────
    pub frame_age:        LatencyHistogram,
    pub vision_total:     LatencyHistogram,
    pub filter:           LatencyHistogram,
    pub fsm:              LatencyHistogram,
    pub dispatch:         LatencyHistogram,
    pub state_write:      LatencyHistogram,
    pub tick_total:       LatencyHistogram,
    pub action_rtt:       LatencyHistogram,
    pub e2e_capture_emit: LatencyHistogram,

    /// Per-reader vision cost. Indexado por `ReaderId as usize`.
    pub vision_readers:   [LatencyHistogram; ReaderId::COUNT],

    // ── Counters (Prometheus-style) ──────────────────────────────────────
    pub ticks_total:      AtomicU64,
    pub ticks_overrun:    AtomicU64,
    pub frame_seq_gaps:   AtomicU64,

    // ── Inventory per-slot metrics (item #5 plan robustez 2026-04-22) ────
    // Observaciones acumuladas por tick. Cada entry SlotReading incrementa:
    // - _observed: siempre (total slots × ticks)
    // - _empty / _matched / _unmatched: según stage del SlotReading
    // - _with_stable: si stable_item.is_some() (filter propagó majority)
    // Confidence histogram: sólo para stage FullSweep/MlClassified/CachedHit
    // con confidence > 0 (skipea Empty que siempre es 1.0 trivial).
    pub inventory_slots_observed:     AtomicU64,
    pub inventory_slots_empty:        AtomicU64,
    pub inventory_slots_matched:      AtomicU64,
    pub inventory_slots_unmatched:    AtomicU64,
    pub inventory_slots_with_stable:  AtomicU64,
    /// Confidence distribution. Units: confidence × 10000 (basis points),
    /// reusa LatencyHistogram (buckets exponenciales base 500 us).
    /// Bucket 0 → confidence < 0.05 (rechazos borderline).
    /// Bucket ~8 → confidence > 0.95 (matches fuertes).
    /// Cadencia: inventory_detect_interval ~15 ticks → el mismo cache se
    /// re-ingesta en ticks "dormidos" → multiplicador fijo × 15. Normalizar
    /// dividiendo por `inventory_slots_observed / slot_count` si se quiere
    /// distribución por read real.
    pub inventory_slot_confidence:    LatencyHistogram,

    /// `actions_emitted[kind as usize]` y `actions_acked` — separados por kind.
    pub actions_emitted:  [AtomicU64; 8],
    pub actions_acked:    [AtomicU64; 8],
    pub actions_failed:   [AtomicU64; 8],

    // ── Snapshot del último tick (lock-free read) ────────────────────────
    pub last_tick:        ArcSwap<TickMetrics>,

    // ── Rolling windows (single-writer game loop) ────────────────────────
    /// Los windows necesitan &mut. Como el registry vive detrás de Arc,
    /// usamos parking_lot::Mutex para serializar acceso. Game loop es el
    /// único writer; HTTP handlers leen vía snapshot que copia.
    /// Mutex porque RwLock sería ineficiente para writes 30 Hz dominantes.
    windows:              Mutex<Windows>,

    // ── Frame seq tracking ───────────────────────────────────────────────
    last_frame_seq:       AtomicU64,

    // ── Recorder JSONL opcional ──────────────────────────────────────────
    /// `None` = recorder inactivo (zero cost en record_tick).
    /// `Some` = activo, cada record_tick escribe una línea.
    /// RwLock para permitir start/stop dinámico desde HTTP. El game loop
    /// hace read() para chequear si está activo (~30 ns sin contención).
    recorder:             RwLock<Option<MetricsRecorder>>,
}

struct Windows {
    /// tick_total_us — para jitter (stddev sobre 1s).
    tick_jitter:    CircularU32<JITTER_WINDOW_TICKS>,
    /// tick_total_us — para FPS estable (10s).
    tick_long:      CircularU32<FPS_WINDOW_TICKS>,
    /// vision_total_us — para detección de degradation trend.
    vision_jitter:  CircularU32<JITTER_WINDOW_TICKS>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordingStatus {
    pub active:        bool,
    pub path:          Option<String>,
    pub written_lines: u64,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct WindowsSnapshot {
    pub jitter_us:           u32,
    pub tick_mean_us_short:  u32,
    pub tick_mean_us_long:   u32,
    pub tick_max_us_long:    u32,
    pub vision_mean_us:      u32,
    pub vision_jitter_us:    u32,
    pub samples_short:       usize,
    pub samples_long:        usize,
}

impl Default for MetricsRegistry {
    fn default() -> Self { Self::new() }
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self {
            frame_age:        LatencyHistogram::new(),
            vision_total:     LatencyHistogram::new(),
            filter:           LatencyHistogram::new(),
            fsm:              LatencyHistogram::new(),
            dispatch:         LatencyHistogram::new(),
            state_write:      LatencyHistogram::new(),
            tick_total:       LatencyHistogram::new(),
            action_rtt:       LatencyHistogram::new(),
            e2e_capture_emit: LatencyHistogram::new(),
            vision_readers: [
                LatencyHistogram::new(), LatencyHistogram::new(), LatencyHistogram::new(),
                LatencyHistogram::new(), LatencyHistogram::new(), LatencyHistogram::new(),
                LatencyHistogram::new(), LatencyHistogram::new(), LatencyHistogram::new(),
                LatencyHistogram::new(), LatencyHistogram::new(), LatencyHistogram::new(),
            ],
            ticks_total:      AtomicU64::new(0),
            ticks_overrun:    AtomicU64::new(0),
            frame_seq_gaps:   AtomicU64::new(0),
            inventory_slots_observed:    AtomicU64::new(0),
            inventory_slots_empty:       AtomicU64::new(0),
            inventory_slots_matched:     AtomicU64::new(0),
            inventory_slots_unmatched:   AtomicU64::new(0),
            inventory_slots_with_stable: AtomicU64::new(0),
            inventory_slot_confidence:   LatencyHistogram::new(),
            actions_emitted:  Default::default(),
            actions_acked:    Default::default(),
            actions_failed:   Default::default(),
            last_tick:        ArcSwap::from_pointee(TickMetrics::default()),
            windows: Mutex::new(Windows {
                tick_jitter:   CircularU32::new(),
                tick_long:     CircularU32::new(),
                vision_jitter: CircularU32::new(),
            }),
            last_frame_seq:   AtomicU64::new(0),
            recorder:         RwLock::new(None),
        }
    }

    /// Activa el recorder JSONL. Sustituye cualquier sesión previa
    /// (drop del MetricsRecorder anterior dispara flush + close).
    /// Devuelve error si no se puede crear el archivo.
    pub fn start_recording(
        &self,
        path: std::path::PathBuf,
        flush_every: u32,
    ) -> std::io::Result<()> {
        let rec = MetricsRecorder::start(path, flush_every)?;
        *self.recorder.write() = Some(rec);
        Ok(())
    }

    /// Para la grabación, drop del recorder dispara flush + close.
    /// Devuelve líneas escritas en la sesión que se cierra.
    pub fn stop_recording(&self) -> u64 {
        let mut guard = self.recorder.write();
        let n = guard.as_ref().map(|r| r.written_lines()).unwrap_or(0);
        *guard = None;
        n
    }

    /// Estado del recorder para HTTP `/instrumentation/recording_status`.
    pub fn recording_status(&self) -> RecordingStatus {
        let guard = self.recorder.read();
        match guard.as_ref() {
            Some(r) => RecordingStatus {
                active:        true,
                path:          Some(r.path().display().to_string()),
                written_lines: r.written_lines(),
            },
            None => RecordingStatus {
                active:        false,
                path:          None,
                written_lines: 0,
            },
        }
    }

    /// Llamado desde el game loop al final de cada tick. Punto único de
    /// actualización de histogramas + windows + publish del snapshot.
    ///
    /// Costo medido empírico (bench `instrumentation_record_tick_steady_state`):
    /// **302 ns/call** en release. 9 µs/seg @ 30 Hz = 0.0003% CPU. Negligible.
    pub fn record_tick(&self, m: TickMetrics) {
        // ── Histogramas ────────────────────────────────────────────────────
        self.frame_age.record_us(m.frame_age_us);
        self.vision_total.record_us(m.vision_total_us);
        self.filter.record_us(m.filter_us);
        self.fsm.record_us(m.fsm_us);
        self.dispatch.record_us(m.dispatch_us);
        self.state_write.record_us(m.state_write_us);
        self.tick_total.record_us(m.tick_total_us);
        self.e2e_capture_emit.record_us(m.e2e_capture_to_emit_us());

        for (i, us) in m.vision_per_reader_us.iter().enumerate() {
            if *us > 0 {
                self.vision_readers[i].record_us(*us as u32);
            }
        }

        // ── Counters ───────────────────────────────────────────────────────
        self.ticks_total.fetch_add(1, Ordering::Relaxed);
        if m.flags.contains(TickFlags::TICK_OVERRUN) {
            self.ticks_overrun.fetch_add(1, Ordering::Relaxed);
        }

        // Frame seq gap detection — sólo si tenemos seq válida (frame_seq>0).
        if m.frame_seq > 0 {
            let prev = self.last_frame_seq.swap(m.frame_seq, Ordering::Relaxed);
            if prev > 0 && m.frame_seq > prev + 1 {
                self.frame_seq_gaps.fetch_add(1, Ordering::Relaxed);
            }
        }

        let kind_idx = m.last_action_kind as usize;
        if kind_idx != 0 && kind_idx < self.actions_emitted.len() {
            self.actions_emitted[kind_idx].fetch_add(1, Ordering::Relaxed);
        }

        // ── Windows (mutex single-writer) ─────────────────────────────────
        {
            let mut w = self.windows.lock();
            w.tick_jitter.push(m.tick_total_us);
            w.tick_long.push(m.tick_total_us);
            w.vision_jitter.push(m.vision_total_us);
        }

        // ── ArcSwap publish — lock-free para readers HTTP ─────────────────
        self.last_tick.store(Arc::new(m));

        // ── Recorder JSONL si activo ──────────────────────────────────────
        // Cuando inactivo (None): RwLock read es ~30 ns sin contención.
        // Cuando activo: write lock + serialize + I/O ~5 µs (escrito como
        // mut porque write_line es &mut self). Game loop es el único writer
        // del recorder, así que el contention es solo con start/stop HTTP.
        let mut guard = self.recorder.write();
        if let Some(rec) = guard.as_mut() {
            rec.write_line(&m);
        }
    }

    /// Llamado desde el bridge thread cuando llega ACK del Arduino. Mide
    /// round-trip y actualiza counters por kind.
    pub fn record_action_ack(&self, kind: ActionKindTag, rtt_us: u32, ok: bool) {
        let i = kind as usize;
        if i >= self.actions_acked.len() { return; }
        if ok {
            self.actions_acked[i].fetch_add(1, Ordering::Relaxed);
            self.action_rtt.record_us(rtt_us);
        } else {
            self.actions_failed[i].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot del último tick. Lock-free (ArcSwap load).
    pub fn last_tick_snapshot(&self) -> Arc<TickMetrics> {
        self.last_tick.load_full()
    }

    /// Ingesta observaciones per-slot al finalizar el tick (item #5 plan
    /// robustez). Llamado por BotLoop tras record_tick si hay
    /// `perception.inventory_slots` poblado.
    ///
    /// Incrementa counters por stage + alimenta histograma de confidence
    /// (en basis points 0..10000). Cost ~100 ns × N slots. Con N=16:
    /// ~1.6 µs. Se ingesta cada tick (incluso ticks "dormidos" que reusan
    /// el cache del último read real) — ver comentario en el registry.
    pub fn ingest_inventory_slots(
        &self,
        slots: &[crate::sense::vision::inventory_slot::SlotReading],
    ) {
        use crate::sense::vision::inventory_slot::SlotStage;
        for s in slots {
            self.inventory_slots_observed.fetch_add(1, Ordering::Relaxed);
            match s.stage {
                SlotStage::Empty => {
                    self.inventory_slots_empty.fetch_add(1, Ordering::Relaxed);
                }
                SlotStage::CachedHit | SlotStage::FullSweep | SlotStage::MlClassified => {
                    if s.item.is_some() {
                        self.inventory_slots_matched.fetch_add(1, Ordering::Relaxed);
                        // Confidence en basis points [0..10000]. Histograma
                        // interpreta como "µs" pero es solo un bucket index.
                        let bp = (s.confidence * 10_000.0).clamp(0.0, 10_000.0) as u32;
                        self.inventory_slot_confidence.record_us(bp);
                    } else {
                        // Stage FullSweep con item=None → unmatched real.
                        self.inventory_slots_unmatched.fetch_add(1, Ordering::Relaxed);
                    }
                }
                SlotStage::Unknown => {
                    // No contar — estado transitorio, no debería emitirse.
                }
            }
            if s.stable_item.is_some() {
                self.inventory_slots_with_stable.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Snapshot agregado de los rolling windows (jitter, FPS, vision).
    /// Toma el mutex brevemente para copiar stats.
    pub fn windows_snapshot(&self) -> WindowsSnapshot {
        let w = self.windows.lock();
        WindowsSnapshot {
            jitter_us:          w.tick_jitter.stddev(),
            tick_mean_us_short: w.tick_jitter.mean(),
            tick_mean_us_long:  w.tick_long.mean(),
            tick_max_us_long:   w.tick_long.max(),
            vision_mean_us:     w.vision_jitter.mean(),
            vision_jitter_us:   w.vision_jitter.stddev(),
            samples_short:      w.tick_jitter.len(),
            samples_long:       w.tick_long.len(),
        }
    }

    /// Tasa de éxito de acciones por kind, computada a partir de counters.
    /// Retorna None si no hay actions emitted del kind.
    pub fn action_success_rate(&self, kind: ActionKindTag) -> Option<f32> {
        let i = kind as usize;
        if i >= self.actions_emitted.len() { return None; }
        let emitted = self.actions_emitted[i].load(Ordering::Relaxed);
        if emitted == 0 { return None; }
        let acked = self.actions_acked[i].load(Ordering::Relaxed);
        Some(acked as f32 / emitted as f32)
    }

    /// FPS efectivo derivado del rolling window de 10s.
    /// Si el window no está lleno, usa los samples disponibles.
    pub fn measured_fps(&self) -> f32 {
        let snap = self.windows_snapshot();
        if snap.samples_long == 0 || snap.tick_mean_us_long == 0 {
            return 0.0;
        }
        // Si cada tick consume tick_mean_us_long, FPS efectivo = 1e6 / mean.
        // Esto refleja capacity, no cadence real (que está limitada por NDI).
        // Para cadence real necesitamos timestamps; aproximación suficiente
        // para detectar caídas de capacity.
        1_000_000.0 / snap.tick_mean_us_long as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_tick(tick: u64) -> TickMetrics {
        TickMetrics {
            tick,
            frame_seq: tick,
            frame_age_us: 80_000,
            vision_total_us: 10_000,
            filter_us: 300,
            fsm_us: 200,
            dispatch_us: 100,
            tick_total_us: 15_000,
            last_action_kind: ActionKindTag::None,
            ..Default::default()
        }
    }

    #[test]
    fn record_tick_updates_histograms() {
        let r = MetricsRegistry::new();
        r.record_tick(dummy_tick(1));
        assert_eq!(r.tick_total.count(), 1);
        assert_eq!(r.frame_age.count(), 1);
        assert_eq!(r.ticks_total.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_tick_publishes_snapshot() {
        let r = MetricsRegistry::new();
        r.record_tick(dummy_tick(42));
        let snap = r.last_tick_snapshot();
        assert_eq!(snap.tick, 42);
    }

    #[test]
    fn frame_seq_gap_detected() {
        let r = MetricsRegistry::new();
        r.record_tick(TickMetrics { frame_seq: 1, ..Default::default() });
        r.record_tick(TickMetrics { frame_seq: 2, ..Default::default() });
        assert_eq!(r.frame_seq_gaps.load(Ordering::Relaxed), 0);
        // Gap: salto de 2 a 5 → 1 gap detectado.
        r.record_tick(TickMetrics { frame_seq: 5, ..Default::default() });
        assert_eq!(r.frame_seq_gaps.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn frame_seq_zero_does_not_count_gap() {
        let r = MetricsRegistry::new();
        // frame_seq=0 → reader sin frame_id wire (Objetivo 1 pendiente).
        // No debería contar como gap.
        r.record_tick(TickMetrics { frame_seq: 0, ..Default::default() });
        r.record_tick(TickMetrics { frame_seq: 0, ..Default::default() });
        assert_eq!(r.frame_seq_gaps.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn tick_overrun_flag_increments_counter() {
        let r = MetricsRegistry::new();
        let mut m = dummy_tick(1);
        m.flags = TickFlags::TICK_OVERRUN;
        r.record_tick(m);
        assert_eq!(r.ticks_overrun.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn action_emitted_counter_per_kind() {
        let r = MetricsRegistry::new();
        let mut m = dummy_tick(1);
        m.last_action_kind = ActionKindTag::Heal;
        r.record_tick(m);
        m.last_action_kind = ActionKindTag::Click;
        m.tick = 2;
        r.record_tick(m);
        assert_eq!(r.actions_emitted[ActionKindTag::Heal as usize].load(Ordering::Relaxed), 1);
        assert_eq!(r.actions_emitted[ActionKindTag::Click as usize].load(Ordering::Relaxed), 1);
        assert_eq!(r.actions_emitted[ActionKindTag::Attack as usize].load(Ordering::Relaxed), 0);
    }

    #[test]
    fn action_ack_updates_rtt_histogram_and_counter() {
        let r = MetricsRegistry::new();
        r.record_action_ack(ActionKindTag::Heal, 8000, true);
        assert_eq!(r.actions_acked[ActionKindTag::Heal as usize].load(Ordering::Relaxed), 1);
        assert_eq!(r.action_rtt.count(), 1);
        assert_eq!(r.action_rtt.mean_us(), 8000);
    }

    #[test]
    fn action_ack_failure_does_not_record_rtt() {
        let r = MetricsRegistry::new();
        r.record_action_ack(ActionKindTag::Heal, 50_000, false);
        assert_eq!(r.actions_failed[ActionKindTag::Heal as usize].load(Ordering::Relaxed), 1);
        assert_eq!(r.actions_acked[ActionKindTag::Heal as usize].load(Ordering::Relaxed), 0);
        // RTT histograma NO se actualiza para failures (latencia inútil de medir).
        assert_eq!(r.action_rtt.count(), 0);
    }

    #[test]
    fn action_success_rate_computes_correctly() {
        let r = MetricsRegistry::new();
        // 4 emitted, 3 acked, 1 failed → rate = 3/4 = 0.75.
        for _ in 0..4 {
            let mut m = dummy_tick(1);
            m.last_action_kind = ActionKindTag::Heal;
            r.record_tick(m);
        }
        r.record_action_ack(ActionKindTag::Heal, 1000, true);
        r.record_action_ack(ActionKindTag::Heal, 1100, true);
        r.record_action_ack(ActionKindTag::Heal, 1200, true);
        r.record_action_ack(ActionKindTag::Heal, 50_000, false);

        let rate = r.action_success_rate(ActionKindTag::Heal).unwrap();
        assert!((rate - 0.75).abs() < 0.01, "got {}", rate);
    }

    #[test]
    fn action_success_rate_none_when_no_emit() {
        let r = MetricsRegistry::new();
        assert!(r.action_success_rate(ActionKindTag::Heal).is_none());
    }

    #[test]
    fn windows_snapshot_reflects_pushes() {
        let r = MetricsRegistry::new();
        for i in 0..30 {
            let mut m = dummy_tick(i);
            m.tick_total_us = 15_000 + (i as u32 * 100);
            r.record_tick(m);
        }
        let snap = r.windows_snapshot();
        assert_eq!(snap.samples_short, 30);
        // Mean debería estar ~16450 (15000 + avg de 0..30 * 100 = 15000 + 1450).
        assert!(snap.tick_mean_us_short > 15_000);
        assert!(snap.tick_mean_us_short < 17_000);
        assert!(snap.jitter_us > 0); // hay varianza
    }

    #[test]
    fn measured_fps_reasonable_for_steady_state() {
        let r = MetricsRegistry::new();
        // 30 ticks de 33 ms = capacity de ~30 fps.
        for _ in 0..30 {
            let mut m = dummy_tick(0);
            m.tick_total_us = 33_000;
            r.record_tick(m);
        }
        let fps = r.measured_fps();
        // 1e6 / 33000 ≈ 30.3
        assert!(fps > 29.0 && fps < 31.0, "fps={}", fps);
    }

    #[test]
    fn measured_fps_zero_when_no_samples() {
        let r = MetricsRegistry::new();
        assert_eq!(r.measured_fps(), 0.0);
    }

    #[test]
    fn registry_arc_shared_across_threads() {
        use std::thread;
        let r = Arc::new(MetricsRegistry::new());
        let mut handles = vec![];
        for i in 0..4 {
            let r2 = Arc::clone(&r);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let mut m = dummy_tick(i);
                    r2.record_tick(m);
                    r2.record_action_ack(ActionKindTag::Heal, 1000, true);
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
        // 4 threads × 100 ticks = 400 records.
        assert_eq!(r.tick_total.count(), 400);
        assert_eq!(r.action_rtt.count(), 400);
    }

    // ── Inventory per-slot metrics (item #5 plan robustez) ────────────

    #[test]
    fn ingest_inventory_slots_increments_counters_by_stage() {
        use crate::sense::vision::inventory_slot::{SlotReading, SlotStage};
        let r = MetricsRegistry::new();
        let slots = vec![
            SlotReading::empty(0),
            SlotReading::empty(1),
            SlotReading::matched(
                2, "mana_potion".into(), 0.92, 0.80, Some(47),
                SlotStage::FullSweep,
            ),
            SlotReading::unmatched(3),
        ];
        r.ingest_inventory_slots(&slots);
        assert_eq!(r.inventory_slots_observed.load(Ordering::Relaxed), 4);
        assert_eq!(r.inventory_slots_empty.load(Ordering::Relaxed), 2);
        assert_eq!(r.inventory_slots_matched.load(Ordering::Relaxed), 1);
        assert_eq!(r.inventory_slots_unmatched.load(Ordering::Relaxed), 1);
        assert_eq!(r.inventory_slots_with_stable.load(Ordering::Relaxed), 0);
        // Confidence histogram: solo 1 sample (la slot matched).
        assert_eq!(r.inventory_slot_confidence.count(), 1);
    }

    #[test]
    fn ingest_counts_stable_item_when_filter_populated() {
        use crate::sense::vision::inventory_slot::{SlotReading, SlotStage};
        let r = MetricsRegistry::new();
        let mut s = SlotReading::matched(
            0, "X".into(), 0.85, 0.80, None, SlotStage::FullSweep,
        );
        s.stable_item = Some("X".into());  // filter activo
        r.ingest_inventory_slots(&[s]);
        assert_eq!(r.inventory_slots_with_stable.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ingest_empty_slots_zero_confidence_samples() {
        use crate::sense::vision::inventory_slot::SlotReading;
        let r = MetricsRegistry::new();
        // Slot Empty tiene confidence 1.0 por default pero NO se ingesta al
        // histograma (es trivial; solo matched slots contribuyen).
        r.ingest_inventory_slots(&[SlotReading::empty(0), SlotReading::empty(1)]);
        assert_eq!(r.inventory_slot_confidence.count(), 0);
        assert_eq!(r.inventory_slots_empty.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn ingest_empty_slots_vec_is_noop() {
        let r = MetricsRegistry::new();
        r.ingest_inventory_slots(&[]);
        assert_eq!(r.inventory_slots_observed.load(Ordering::Relaxed), 0);
    }
}
