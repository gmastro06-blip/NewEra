//! recorder.rs — Grabador de `PerceptionSnapshot` a JSONL.
//!
//! Usado por F1 (replay tool). Cada tick del game loop puede llamar a
//! `PerceptionRecorder::record(&snapshot)` que escribe una línea JSON al
//! archivo configurado. El replay binary (`replay_perception`) lee ese
//! archivo y puede reinyectar al FSM.
//!
//! ## Diseño
//!
//! - **Sincrónico**: el recorder vive en el game loop thread y escribe
//!   directamente con `BufWriter<File>`. Los writes van a buffer OS.
//! - **Tamaño esperado**: una PerceptionSnapshot JSON es ~500-1500 bytes.
//!   A 30 Hz durante 1h = ~100 MB. Para sesiones largas considerar
//!   `interval_ticks > 1` en config.
//! - **Graceful close**: `Drop` flushea el BufWriter al cerrar el archivo.
//! - **Errores silenciosos**: si falla write (disco lleno), loggea warning
//!   y desactiva el recorder para no matar el tick loop.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use crate::sense::perception::PerceptionSnapshot;

/// Recorder simple de snapshots de perception a JSONL.
pub struct PerceptionRecorder {
    path:           PathBuf,
    writer:         Option<BufWriter<File>>,
    /// Si es > 1, solo graba 1 de cada N ticks. Default = 1 (todos).
    interval_ticks: u32,
    records_written: u64,
    last_recorded_tick: Option<u64>,
}

impl PerceptionRecorder {
    /// Crea un recorder que escribe al path dado. El archivo se crea o
    /// se trunca. Llama a `is_enabled()` para chequear.
    pub fn new(path: PathBuf, interval_ticks: u32) -> Self {
        let writer = match File::create(&path) {
            Ok(f) => {
                tracing::info!(
                    "PerceptionRecorder: grabando a '{}' (interval={} ticks)",
                    path.display(), interval_ticks.max(1)
                );
                Some(BufWriter::new(f))
            }
            Err(e) => {
                tracing::warn!(
                    "PerceptionRecorder: no se pudo crear '{}': {}. Grabación deshabilitada.",
                    path.display(), e
                );
                None
            }
        };
        Self {
            path,
            writer,
            interval_ticks: interval_ticks.max(1),
            records_written: 0,
            last_recorded_tick: None,
        }
    }

    #[allow(dead_code)] // exposed for /recording/status endpoint
    pub fn is_enabled(&self) -> bool {
        self.writer.is_some()
    }

    #[allow(dead_code)] // exposed for /recording/status endpoint
    pub fn records_written(&self) -> u64 {
        self.records_written
    }

    #[allow(dead_code)] // exposed for /recording/status endpoint
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Graba un snapshot. Si el recorder está deshabilitado (error de IO
    /// previo), es un no-op.
    pub fn record(&mut self, snap: &PerceptionSnapshot) {
        // Throttle por interval_ticks.
        if let Some(last) = self.last_recorded_tick {
            if snap.tick.saturating_sub(last) < self.interval_ticks as u64 {
                return;
            }
        }
        let Some(writer) = self.writer.as_mut() else { return };
        match serde_json::to_string(snap) {
            Ok(json) => {
                if let Err(e) = writeln!(writer, "{}", json) {
                    tracing::warn!("PerceptionRecorder: write error: {}. Desactivando.", e);
                    self.writer = None;
                    return;
                }
                self.records_written += 1;
                self.last_recorded_tick = Some(snap.tick);
            }
            Err(e) => {
                tracing::warn!("PerceptionRecorder: serialize error: {}", e);
            }
        }
    }

    /// Fuerza flush del buffer al archivo. Llamar antes de cerrar
    /// o antes de leer el archivo desde otro proceso.
    pub fn flush(&mut self) {
        if let Some(writer) = self.writer.as_mut() {
            if let Err(e) = writer.flush() {
                tracing::warn!("PerceptionRecorder: flush error: {}", e);
            }
        }
    }
}

impl Drop for PerceptionRecorder {
    fn drop(&mut self) {
        self.flush();
        tracing::info!(
            "PerceptionRecorder: cerrado, {} records written a '{}'",
            self.records_written, self.path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn recorder_writes_jsonl() {
        let tmp = std::env::temp_dir().join(format!("tibia_recorder_test_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        {
            let mut rec = PerceptionRecorder::new(tmp.clone(), 1);
            assert!(rec.is_enabled());

            let mut snap = PerceptionSnapshot::default();
            snap.tick = 1;
            snap.hp_ratio = Some(0.8);
            rec.record(&snap);

            snap.tick = 2;
            snap.hp_ratio = Some(0.7);
            rec.record(&snap);

            assert_eq!(rec.records_written(), 2);
            rec.flush();
        }
        // Drop fuerza flush.

        let mut content = String::new();
        std::fs::File::open(&tmp).unwrap().read_to_string(&mut content).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        // Cada linea debe ser JSON parseable.
        let s1: PerceptionSnapshot = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(s1.tick, 1);
        assert_eq!(s1.hp_ratio, Some(0.8));
        let s2: PerceptionSnapshot = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(s2.tick, 2);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn recorder_throttles_by_interval() {
        let tmp = std::env::temp_dir().join(format!("tibia_recorder_interval_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        {
            let mut rec = PerceptionRecorder::new(tmp.clone(), 5);  // cada 5 ticks
            let mut snap = PerceptionSnapshot::default();
            for t in 0..20 {
                snap.tick = t;
                rec.record(&snap);
            }
            // Ticks grabados: 0, 5, 10, 15 → 4 records.
            assert_eq!(rec.records_written(), 4);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn recorder_invalid_path_disabled() {
        let bad = PathBuf::from("/nonexistent_dir_xyz/file.jsonl");
        let mut rec = PerceptionRecorder::new(bad, 1);
        assert!(!rec.is_enabled());
        // record es no-op, no panic.
        rec.record(&PerceptionSnapshot::default());
        assert_eq!(rec.records_written(), 0);
    }
}
