//! dataset_recorder.rs — Captura crops de inventory slots para entrenamiento ML (Fase 2).
//!
//! Por cada tick (filtrable por `interval_ticks`), extrae los N slots
//! definidos en `inventory_backpack_strip` o `inventory_grid` y los guarda
//! como PNG individuales + un manifest CSV con metadata.
//!
//! ## Output layout
//!
//! ```text
//! <output_dir>/
//!   manifest.csv               # filename,tick,slot_index,frame_id,captured_at_unix_ms
//!   crops/
//!     0000_t00123_s00.png      # 0000=seq | t00123=tick | s00=slot
//!     0000_t00123_s01.png
//!     ...
//! ```
//!
//! ## Workflow
//!
//! 1. Iniciar capture: `POST /dataset/start?dir=path&interval=15&tag=hunt1`
//! 2. Sesión live normal (atacar mobs, abrir backpacks distintos, etc.)
//! 3. Detener: `POST /dataset/stop`
//! 4. Etiquetar: tool externo (Python/Rust) que lee manifest.csv, muestra
//!    cada crop al usuario, agrega columna `label` con la clase asignada.
//! 5. Entrenar: dataset etiquetado → YOLOv8-cls → ONNX export.
//!
//! ## Diseño (post-auditoría 2026-04-20)
//!
//! **Background writer thread** (Fase auditoría #2):
//! - `capture()` en el game loop thread extrae los BGRA bytes (cheap, ~0.5 ms
//!   por crop 32×32) y envía un `CropJob` al channel.
//! - `writer_thread` consume del channel: PNG encode (~1-3 ms) + escritura
//!   a disco (depende del FS cache). Estas operaciones ya no bloquean el tick.
//! - Channel bounded (`CHANNEL_CAPACITY=256`): si el writer se atrasa (disco
//!   lento, flush OS), el main thread incrementa `dropped_crops` counter con
//!   `try_send` en vez de bloquear. Safer: preferimos perder algunos crops
//!   (dataset aún large) a overrun del tick loop.
//! - Manifest CSV se escribe también desde el writer thread para mantener
//!   atomicidad filename-en-manifest ↔ filename-en-disco.
//! - `Drop` del `DatasetRecorder`: dropea el Sender, lo que cierra el
//!   channel. El writer thread termina su loop naturalmente (recv → Err) y
//!   flushea manifest antes de exit. `join()` esperado por el caller.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use image::{ImageBuffer, Luma, Rgb};

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::crop::{crop_bgra, Roi};

/// Capacidad del channel game_loop → writer_thread. Si el writer se queda
/// atrás, `try_send` falla con Full y se incrementa `dropped_crops`.
/// 256 ≈ 16 slots × 16 ticks de buffer, cubre un spike corto del disco.
const CHANNEL_CAPACITY: usize = 256;

/// Job enviado al writer thread con los BGRA bytes + metadata para
/// escribir un crop PNG + una fila del manifest.
struct CropJob {
    filename:    String,
    bgra:        Vec<u8>,
    width:       u32,
    height:      u32,
    tick:        u64,
    slot_index:  u32,
    frame_id:    u64,
    captured_ms: u64,
    tag:         String,
}

/// Recorder de crops de inventory slots para dataset ML (background writer).
pub struct DatasetRecorder {
    /// Directorio raíz del dataset (contendrá manifest.csv + crops/).
    output_dir:     PathBuf,
    /// Slots a capturar (típicamente del calibration `inventory_backpack_strip`).
    slots:          Vec<RoiDef>,
    /// Cada cuántos ticks captura. >= 1.
    interval_ticks: u32,
    /// Etiqueta opcional para el manifest (ej. "abdendriel_wasps_session").
    tag:            String,
    /// Channel tx hacia writer_thread. `None` si recorder inhábil.
    tx:             Option<SyncSender<CropJob>>,
    /// Handle del writer thread para join en Drop.
    worker:         Option<JoinHandle<()>>,
    /// Contador atómico de crops escritos (actualizado por writer thread,
    /// leído por main via `total_crops()`).
    written_counter: Arc<AtomicU64>,
    /// Contador de jobs que no cupieron en el channel (disco lento).
    dropped_counter: Arc<AtomicU64>,
    /// Contador secuencial de ticks capturados (para nombre del PNG).
    seq:            u64,
    /// Último tick procesado (para respetar interval).
    last_tick:      Option<u64>,
}

impl DatasetRecorder {
    /// Crea un recorder. Genera `output_dir/crops/` + spawnea writer thread.
    /// Si falla creación de dirs o file, el recorder queda inhábil
    /// (`is_enabled() = false`) y el thread no se spawnea.
    pub fn new(
        output_dir:     PathBuf,
        slots:          Vec<RoiDef>,
        interval_ticks: u32,
        tag:            String,
    ) -> Self {
        let interval = interval_ticks.max(1);
        let written_counter = Arc::new(AtomicU64::new(0));
        let dropped_counter = Arc::new(AtomicU64::new(0));
        let mut recorder = Self {
            output_dir: output_dir.clone(),
            slots,
            interval_ticks: interval,
            tag: tag.clone(),
            tx: None,
            worker: None,
            written_counter: Arc::clone(&written_counter),
            dropped_counter: Arc::clone(&dropped_counter),
            seq: 0,
            last_tick: None,
        };
        if let Err(e) = std::fs::create_dir_all(output_dir.join("crops")) {
            tracing::warn!(
                "DatasetRecorder: no se pudo crear '{}/crops': {}. Capture deshabilitado.",
                output_dir.display(), e
            );
            return recorder;
        }
        let manifest_path = output_dir.join("manifest.csv");
        let was_new = !manifest_path.exists();
        let manifest = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&manifest_path)
        {
            Ok(f) => {
                let mut w = BufWriter::new(f);
                if was_new {
                    // Header solo si el archivo es nuevo (append-safe).
                    let _ = writeln!(w,
                        "filename,tick,slot_index,frame_id,captured_at_unix_ms,tag,label");
                }
                w
            }
            Err(e) => {
                tracing::warn!(
                    "DatasetRecorder: no se pudo abrir manifest.csv '{}': {}",
                    manifest_path.display(), e
                );
                return recorder;
            }
        };

        // Spawn writer thread. Posee el manifest BufWriter y el output_dir.
        let (tx, rx) = sync_channel::<CropJob>(CHANNEL_CAPACITY);
        let crops_dir = output_dir.join("crops");
        let written = Arc::clone(&written_counter);
        let worker = std::thread::Builder::new()
            .name("dataset-writer".into())
            .spawn(move || {
                let mut manifest = manifest;
                let mut jobs_since_flush = 0u32;
                while let Ok(job) = rx.recv() {
                    let path = crops_dir.join(&job.filename);
                    if let Err(e) = save_bgra_as_png(&job.bgra, job.width, job.height, &path) {
                        tracing::warn!(
                            "dataset-writer: falló guardar {}: {}", job.filename, e
                        );
                        continue;
                    }
                    let _ = writeln!(
                        manifest,
                        "{},{},{},{},{},{},",
                        job.filename, job.tick, job.slot_index, job.frame_id,
                        job.captured_ms, job.tag,
                    );
                    written.fetch_add(1, Ordering::Relaxed);
                    jobs_since_flush += 1;
                    if jobs_since_flush >= 30 {
                        let _ = manifest.flush();
                        jobs_since_flush = 0;
                    }
                }
                // Channel closed: flush final.
                let _ = manifest.flush();
                tracing::info!("dataset-writer: exit limpio, manifest flusheado");
            })
            .expect("spawn dataset-writer thread");

        tracing::info!(
            "DatasetRecorder: capturando a '{}' (interval={} ticks, tag='{}', \
             {} slots, channel cap={}, background writer)",
            output_dir.display(), interval, tag, recorder.slots.len(), CHANNEL_CAPACITY
        );
        recorder.tx = Some(tx);
        recorder.worker = Some(worker);
        recorder
    }

    /// `true` si el recorder está activo y puede grabar.
    pub fn is_enabled(&self) -> bool {
        self.tx.is_some() && !self.slots.is_empty()
    }

    /// Total de crops escritos a disco hasta ahora (actualizado por writer).
    #[allow(dead_code)]
    pub fn total_crops(&self) -> u64 {
        self.written_counter.load(Ordering::Relaxed)
    }

    /// Total de jobs droppeados por channel full (disco lento / overrun).
    /// Útil para observar backpressure: si sube, el disco no da abasto.
    #[allow(dead_code)]
    pub fn dropped_crops(&self) -> u64 {
        self.dropped_counter.load(Ordering::Relaxed)
    }

    /// Procesa un frame en el tick dado. Extrae los BGRA de cada slot en el
    /// thread actual (cheap) y envía jobs al writer thread.
    /// Respeta `interval_ticks`.
    /// Devuelve número de jobs enqueued en este tick (no el # escritos, que
    /// se cuenta asíncronamente via `total_crops()`).
    pub fn capture(&mut self, frame: &Frame, tick: u64, frame_id: u64) -> u32 {
        if !self.is_enabled() {
            return 0;
        }
        // Respeta el intervalo: skip si no hay enough gap desde el último tick.
        if let Some(last) = self.last_tick {
            if tick.saturating_sub(last) < self.interval_ticks as u64 {
                return 0;
            }
        }
        let Some(tx) = self.tx.as_ref() else { return 0; };
        let mut enqueued = 0u32;
        let captured_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        for (slot_idx, slot) in self.slots.iter().enumerate() {
            let Some(bgra) = crop_bgra(frame, Roi::new(slot.x, slot.y, slot.w, slot.h)) else {
                continue;
            };
            let filename = format!("{:04}_t{:06}_s{:02}.png", self.seq, tick, slot_idx);
            let job = CropJob {
                filename,
                bgra,
                width:       slot.w,
                height:      slot.h,
                tick,
                slot_index:  slot_idx as u32,
                frame_id,
                captured_ms,
                tag:         self.tag.clone(),
            };
            match tx.try_send(job) {
                Ok(()) => { enqueued += 1; }
                Err(TrySendError::Full(_)) => {
                    // Backpressure: disco o writer no da abasto. Drop silent
                    // pero contamos para observabilidad. No bloqueamos tick.
                    self.dropped_counter.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {
                    // Writer thread crasheó. Desactivar recorder.
                    tracing::warn!("DatasetRecorder: writer thread disconnected");
                    self.tx = None;
                    break;
                }
            }
        }
        if enqueued > 0 {
            self.seq += 1;
        }
        self.last_tick = Some(tick);
        enqueued
    }

    /// Flushea el writer thread. NO-OP externally — el flush periódico
    /// sucede en el worker cada 30 jobs + en Drop. Mantener función por
    /// compat con callers que la esperan.
    #[allow(dead_code)]
    pub fn flush(&mut self) {
        // Best-effort: worker hace flush cada 30 jobs + on disconnect/drop.
    }
}

impl Drop for DatasetRecorder {
    fn drop(&mut self) {
        // Cerrar channel: tx dropped → writer sale del recv loop y flushea manifest.
        self.tx.take();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Guarda BGRA bytes como PNG en disco. Convierte BGRA → RGB para image crate
/// (descarta alpha — los slots de inventory no tienen transparencia útil).
fn save_bgra_as_png(bgra: &[u8], width: u32, height: u32, path: &Path) -> Result<(), String> {
    let n = (width * height) as usize;
    if bgra.len() != n * 4 {
        return Err(format!(
            "BGRA size mismatch: got {} bytes, expected {}", bgra.len(), n * 4
        ));
    }
    // Convertir BGRA → RGB.
    let mut rgb = Vec::with_capacity(n * 3);
    for i in 0..n {
        rgb.push(bgra[i * 4 + 2]); // R (de B en BGRA)
        rgb.push(bgra[i * 4 + 1]); // G
        rgb.push(bgra[i * 4]);     // B (de R en BGRA)
    }
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, rgb)
            .ok_or_else(|| "ImageBuffer::from_raw failed".to_string())?;
    img.save(path).map_err(|e| e.to_string())
}

/// Helper utility: guardar luma como PNG (para tests).
#[allow(dead_code)]
pub fn save_luma_as_png(luma: &[u8], width: u32, height: u32, path: &Path) -> Result<(), String> {
    let img: ImageBuffer<Luma<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, luma.to_vec())
            .ok_or_else(|| "ImageBuffer::from_raw failed".to_string())?;
    img.save(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn make_test_frame(w: u32, h: u32, fill: u8) -> Frame {
        Frame {
            width:       w,
            height:      h,
            data:        vec![fill; (w * h * 4) as usize],
            captured_at: Instant::now(),
        }
    }

    fn unique_test_dir(suffix: &str) -> PathBuf {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("dataset_test_{}_{}", suffix, n))
    }

    /// Espera hasta N ms a que el writer thread haya persistido >= expected
    /// jobs (counter atómico). Permite tests deterministas sin sleep fijo.
    fn wait_for_writes(recorder: &DatasetRecorder, expected: u64, timeout_ms: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(timeout_ms) {
            if recorder.total_crops() >= expected {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn recorder_creates_directories_and_manifest() {
        let dir = unique_test_dir("creates_dirs");
        let slots = vec![RoiDef::new(0, 0, 8, 8)];
        let r = DatasetRecorder::new(dir.clone(), slots, 1, "test".into());
        assert!(r.is_enabled());
        assert!(dir.join("crops").exists());
        assert!(dir.join("manifest.csv").exists());
        drop(r);  // espera writer thread
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_respects_interval() {
        let dir = unique_test_dir("interval");
        let slots = vec![RoiDef::new(0, 0, 8, 8)];
        let mut r = DatasetRecorder::new(dir.clone(), slots, 5, "iv".into());
        let frame = make_test_frame(20, 20, 100);
        // Tick 0 captura (primer call), tick 1-4 skip, tick 5 captura
        assert_eq!(r.capture(&frame, 0, 0), 1);
        for t in 1..5 {
            assert_eq!(r.capture(&frame, t, t), 0, "tick {} debe skip", t);
        }
        assert_eq!(r.capture(&frame, 5, 5), 1);
        drop(r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_writes_one_png_per_slot() {
        let dir = unique_test_dir("multi_slot");
        let slots = vec![
            RoiDef::new(0, 0, 8, 8),
            RoiDef::new(10, 0, 8, 8),
            RoiDef::new(0, 10, 8, 8),
        ];
        let mut r = DatasetRecorder::new(dir.clone(), slots, 1, "multi".into());
        let frame = make_test_frame(20, 20, 200);
        let enqueued = r.capture(&frame, 100, 50);
        assert_eq!(enqueued, 3);
        // Esperar que el writer thread persista.
        assert!(wait_for_writes(&r, 3, 500), "writer thread no persistió 3 crops en 500ms");
        // Verificar que existen los 3 PNGs.
        let crops_dir = dir.join("crops");
        let entries: Vec<_> = std::fs::read_dir(&crops_dir).unwrap().collect();
        assert_eq!(entries.len(), 3);
        drop(r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_skips_oob_slots() {
        let dir = unique_test_dir("oob");
        let slots = vec![
            RoiDef::new(0, 0, 8, 8),         // OK
            RoiDef::new(50, 50, 8, 8),       // OOB en frame 20×20
        ];
        let mut r = DatasetRecorder::new(dir.clone(), slots, 1, "oob".into());
        let frame = make_test_frame(20, 20, 100);
        let enqueued = r.capture(&frame, 1, 1);
        // Solo el primer slot fits → 1 job enqueued.
        assert_eq!(enqueued, 1);
        drop(r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_appends_to_existing_manifest() {
        let dir = unique_test_dir("append");
        let slots = vec![RoiDef::new(0, 0, 8, 8)];
        // Sesión 1
        {
            let mut r = DatasetRecorder::new(dir.clone(), slots.clone(), 1, "s1".into());
            let frame = make_test_frame(20, 20, 100);
            r.capture(&frame, 1, 1);
            // drop dispara join + flush
        }
        // Sesión 2: append, no overwrite
        {
            let mut r = DatasetRecorder::new(dir.clone(), slots.clone(), 1, "s2".into());
            let frame = make_test_frame(20, 20, 200);
            r.capture(&frame, 100, 50);
        }
        let manifest = std::fs::read_to_string(dir.join("manifest.csv")).unwrap();
        let lines: Vec<&str> = manifest.lines().collect();
        // 1 header + 2 entries de capture
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("filename,tick"));
        // Verifica que ambas sesiones contribuyeron al CSV.
        assert!(lines[1].contains(",s1,") || lines[1].contains(",s2,"));
        assert!(lines[2].contains(",s1,") || lines[2].contains(",s2,"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_disabled_when_dir_uncreatable() {
        // Crea un archivo con el nombre que queremos usar como dir → mkdir
        // bajo ese nombre debe fallar porque ya existe como archivo.
        let parent = unique_test_dir("uncreatable_parent");
        std::fs::create_dir_all(&parent).unwrap();
        let conflict_path = parent.join("not_a_dir");
        // Crear FILE con el nombre del que iba a ser nuestro dir.
        std::fs::write(&conflict_path, b"blocking file").unwrap();
        // Try to create recorder en path donde ya hay un archivo no-dir.
        let slots = vec![RoiDef::new(0, 0, 8, 8)];
        let r = DatasetRecorder::new(conflict_path, slots, 1, "fail".into());
        assert!(!r.is_enabled(), "recorder debe estar inhábil si dir no se puede crear");
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn recorder_no_slots_disabled() {
        let dir = unique_test_dir("no_slots");
        let r = DatasetRecorder::new(dir.clone(), vec![], 1, "empty".into());
        assert!(!r.is_enabled());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_csv_has_correct_columns() {
        let dir = unique_test_dir("csv_cols");
        let slots = vec![RoiDef::new(0, 0, 8, 8)];
        let mut r = DatasetRecorder::new(dir.clone(), slots, 1, "test_tag".into());
        let frame = make_test_frame(20, 20, 100);
        r.capture(&frame, 42, 7);
        // Esperar writer.
        assert!(wait_for_writes(&r, 1, 500));
        drop(r);  // force flush via join
        let content = std::fs::read_to_string(dir.join("manifest.csv")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[0], "filename,tick,slot_index,frame_id,captured_at_unix_ms,tag,label");
        let entry = lines[1];
        let cols: Vec<&str> = entry.split(',').collect();
        assert_eq!(cols.len(), 7);
        assert!(cols[0].ends_with(".png"));
        assert_eq!(cols[1], "42");                  // tick
        assert_eq!(cols[2], "0");                   // slot_index
        assert_eq!(cols[3], "7");                   // frame_id
        assert!(cols[4].parse::<u64>().is_ok());    // captured_ms
        assert_eq!(cols[5], "test_tag");            // tag
        assert_eq!(cols[6], "");                    // label vacío
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_does_not_block_on_channel_full() {
        // Sanity check: 16 slots × 20 ticks = 320 jobs > CHANNEL_CAPACITY (256).
        // capture() debe retornar inmediato, jobs en exceso se cuentan como
        // droppeados sin bloquear el tick.
        let dir = unique_test_dir("backpressure");
        let slots: Vec<RoiDef> = (0..16).map(|i| RoiDef::new(i * 10, 0, 8, 8)).collect();
        let mut r = DatasetRecorder::new(dir.clone(), slots, 1, "bp".into());
        let frame = make_test_frame(200, 20, 128);
        let start = std::time::Instant::now();
        for t in 0..20 {
            r.capture(&frame, t, t);
        }
        let elapsed = start.elapsed();
        // 20 ticks × 16 slots = 320 iteraciones. Cada try_send + crop_bgra debería
        // ser <500µs → total <100ms (generoso). Sin bloqueo, idealmente <20ms.
        assert!(elapsed < std::time::Duration::from_millis(500),
            "capture bloqueó por {}ms — esperado <500ms", elapsed.as_millis());
        drop(r);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
