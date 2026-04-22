//! instrumentation/mod.rs — Métricas runtime por tick (sin deps externas).
//!
//! Diseño: ver respuesta del 2026-04-22 al pedido de instrumentación.
//!
//! ## Layout
//!
//! - `histogram` — LatencyHistogram (16 buckets exponenciales, atomic).
//! - `window`    — CircularU32<N> (rolling alloc-free).
//! - `tick`      — TickMetrics struct + flags + ReaderId/ActionKindTag enums.
//! - `registry`  — MetricsRegistry: dueño de histogramas + ArcSwap del último tick.
//! - `recorder`  — JSONL writer activable on-demand.
//!
//! ## Costo per tick (estimado, validar con bench)
//!
//! - Sin recorder: ~2 µs (histograms record + windows push + ArcSwap publish).
//! - Con recorder: ~7 µs (+ serde_json::to_writer + BufWriter::write).
//!
//! Sobre budget 33 ms/tick = 0.006% / 0.02% respectivamente. Negligible.
//!
//! ## Threading
//!
//! - Game loop es único writer de las CircularU32 (sin locks).
//! - LatencyHistogram + AtomicU64 counters son cross-thread safe.
//! - ArcSwap<TickMetrics> permite read lock-free desde HTTP handlers.

pub mod histogram;
pub mod window;
pub mod tick;
pub mod registry;
pub mod recorder;

pub use histogram::{LatencyHistogram, HistogramSnapshot, Percentiles};
pub use window::CircularU32;
pub use tick::{TickMetrics, TickFlags, ReaderId, ActionKindTag};
pub use registry::MetricsRegistry;
pub use recorder::MetricsRecorder;
