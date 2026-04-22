//! recorder.rs — MetricsRecorder JSONL para sesiones de captura.
//!
//! Escribe un TickMetrics por línea cuando está activo. Off por default
//! (cero cost). Activable via HTTP endpoint para grabar 5+ minutos de
//! ejecución y analizar offline qué etapa domina, dónde aparecen overruns,
//! correlación entre flags y eventos.
//!
//! ## Ownership
//!
//! El `MetricsRecorder` vive dentro de un `RwLock<Option<MetricsRecorder>>`
//! en `MetricsRegistry` para permitir start/stop dinámico vía HTTP sin
//! reiniciar el bot. El game loop lockea brevemente (write) cada tick:
//!
//!   if let Some(rec) = registry.recorder.write().as_mut() {
//!       rec.write_line(&m);
//!   }
//!
//! El cost del lock cuando recorder=None es ~30 ns (un atomic check del
//! RwLock). Cuando activo, ~5 µs por write. Negligible vs budget 33 ms.
//!
//! ## Formato
//!
//! Una línea JSON por tick. Esquema = TickMetrics serializado.
//! No header, no metadata extra — el reader infiere desde el shape.
//! Análisis offline: línea-a-línea con jq, pandas, o el bin
//! `analyze_metrics` (TODO siguiente sesión).
//!
//! ## Rotación + flush
//!
//! - Append-only (OpenOptions::create.append). Sobrevive reinicios del bot.
//! - Flush cada N ticks (default 30 = 1s) para no perder más de 1s de data
//!   en crash.
//! - No rotación automática. Para sesiones largas (>1h), rotar manualmente
//!   o via cron. JSONL de 1h @ 30 Hz × 300 bytes ≈ 32 MB.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use super::tick::TickMetrics;

/// Escritor JSONL de TickMetrics. None hasta que se llama `start()`.
pub struct MetricsRecorder {
    writer:        BufWriter<File>,
    path:          PathBuf,
    flush_every:   u32,
    since_flush:   u32,
    written_lines: u64,
    /// Buffer de string reusable para serialize → reduce allocs.
    /// `serde_json::to_writer` directo al BufWriter es más rápido que
    /// to_string + write, pero ambos son OK.
    scratch:       String,
}

impl MetricsRecorder {
    /// Crea el recorder y abre el archivo para append. Falla si no se puede
    /// crear/abrir el path (permisos, disk full, etc.).
    pub fn start(path: PathBuf, flush_every: u32) -> std::io::Result<Self> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            writer:        BufWriter::with_capacity(64 * 1024, f),
            path,
            flush_every:   flush_every.max(1),
            since_flush:   0,
            written_lines: 0,
            scratch:       String::with_capacity(512),
        })
    }

    /// Escribe una línea. ~5 µs típico (serialize + write a buffer).
    /// Cualquier error de I/O se loguea pero NO propaga — no queremos
    /// abortar el game loop por un disk full transient.
    pub fn write_line(&mut self, m: &TickMetrics) {
        self.scratch.clear();
        if let Err(e) = serde_json::to_writer(unsafe {
            // SAFETY: we treat the String as a Vec<u8> writer and only push
            // valid UTF-8 (serde_json output is always UTF-8).
            self.scratch.as_mut_vec()
        }, m) {
            tracing::warn!("MetricsRecorder: serialize error: {}", e);
            return;
        }
        self.scratch.push('\n');

        if let Err(e) = self.writer.write_all(self.scratch.as_bytes()) {
            tracing::warn!("MetricsRecorder: write error: {} (path={})",
                           e, self.path.display());
            return;
        }
        self.written_lines += 1;
        self.since_flush += 1;
        if self.since_flush >= self.flush_every {
            if let Err(e) = self.writer.flush() {
                tracing::warn!("MetricsRecorder: flush error: {}", e);
            }
            self.since_flush = 0;
        }
    }

    pub fn written_lines(&self) -> u64 { self.written_lines }
    pub fn path(&self) -> &std::path::Path { &self.path }

    /// Flush explícito antes de drop. El Drop hace BufWriter::flush() pero
    /// silenciosamente — llamar esto si quieres saber del error.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl Drop for MetricsRecorder {
    fn drop(&mut self) {
        let _ = self.writer.flush();
        tracing::info!(
            "MetricsRecorder cerrado: {} líneas escritas a '{}'",
            self.written_lines, self.path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrumentation::tick::ActionKindTag;

    fn dummy(tick: u64) -> TickMetrics {
        TickMetrics {
            tick,
            tick_total_us: 18_000,
            last_action_kind: ActionKindTag::Heal,
            ..Default::default()
        }
    }

    #[test]
    fn writes_one_line_per_record() {
        let dir = tempdir();
        let path = dir.join("metrics.jsonl");
        let mut rec = MetricsRecorder::start(path.clone(), 10).expect("start");
        rec.write_line(&dummy(1));
        rec.write_line(&dummy(2));
        rec.write_line(&dummy(3));
        assert_eq!(rec.written_lines(), 3);
        rec.flush().expect("flush");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        // Cada línea es JSON parseable.
        for l in &lines {
            let _v: serde_json::Value = serde_json::from_str(l).expect("valid json");
        }
    }

    #[test]
    fn appends_to_existing_file() {
        let dir = tempdir();
        let path = dir.join("metrics.jsonl");
        {
            let mut rec = MetricsRecorder::start(path.clone(), 1).expect("start");
            rec.write_line(&dummy(1));
        }
        // Reabrir y agregar más.
        let mut rec = MetricsRecorder::start(path.clone(), 1).expect("restart");
        rec.write_line(&dummy(2));
        rec.flush().expect("flush");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn flush_every_n_writes() {
        let dir = tempdir();
        let path = dir.join("metrics.jsonl");
        // flush_every=3 — los primeros 2 writes quedan en buffer, el 3ro flush.
        let mut rec = MetricsRecorder::start(path.clone(), 3).expect("start");
        rec.write_line(&dummy(1));
        rec.write_line(&dummy(2));
        // En este punto el buffer aún no está flushed (tamaño BufWriter 64KB
        // es mucho mayor que 2 lineas de ~250 bytes). El conteo via re-read:
        let pre = std::fs::read_to_string(&path).unwrap_or_default();
        // Buffer puede o no haberse flushed dependiendo del SO y BufWriter
        // internals. Lo que SÍ podemos verificar: tras 3 writes el flush se
        // dispara automáticamente.
        rec.write_line(&dummy(3));
        // Tras el 3er write, el flush_every=3 dispara, y el archivo tiene
        // las 3 líneas.
        let post = std::fs::read_to_string(&path).unwrap();
        assert!(post.lines().count() >= 3);
        let _ = pre;
    }

    #[test]
    fn start_fails_for_invalid_path() {
        // Path que NO se puede crear (depende del SO). Usamos un path con
        // un caracter inválido en Windows + colocamos en una dir inexistente.
        let bad_path = std::path::PathBuf::from("/nonexistent_dir_xyz_99999/metrics.jsonl");
        let r = MetricsRecorder::start(bad_path, 10);
        assert!(r.is_err(), "esperado error abriendo path inválido");
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("tibia-bot-metrics-test-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap().as_nanos()));
        std::fs::create_dir_all(&p).expect("mkdir tempdir");
        p
    }
}
