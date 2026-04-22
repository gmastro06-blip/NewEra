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
//! ## Diseño
//!
//! - **Sincrónico**: corre en game loop thread, escribe directamente a disco.
//!   PNG encoding ~1-3ms por crop 32×32. 16 slots × ~2ms = 32ms — por eso
//!   el `interval_ticks` default es 15 (cada ~500ms a 30 Hz).
//! - **Append-safe**: si `dir/manifest.csv` existe, append. Resuma sesión
//!   anterior en vez de pisar.
//! - **Errores silenciosos**: si disco lleno o IO falla, loggea warn y
//!   se desactiva. No mata el tick loop.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use image::{ImageBuffer, Luma, Rgb};

use crate::sense::frame_buffer::Frame;
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::crop::{crop_bgra, Roi};

/// Recorder de crops de inventory slots para dataset ML.
pub struct DatasetRecorder {
    /// Directorio raíz del dataset (contendrá manifest.csv + crops/).
    output_dir:     PathBuf,
    /// Slots a capturar (típicamente del calibration `inventory_backpack_strip`).
    slots:          Vec<RoiDef>,
    /// Cada cuántos ticks captura. >= 1.
    interval_ticks: u32,
    /// Etiqueta opcional para el manifest (ej. "abdendriel_wasps_session").
    tag:            String,
    /// Manifest CSV writer (BufWriter sobre File).
    manifest:       Option<BufWriter<File>>,
    /// Contador secuencial de ticks capturados (para nombre del PNG).
    seq:            u64,
    /// Total crops escritos.
    total_crops:    u64,
    /// Último tick procesado (para respetar interval).
    last_tick:      Option<u64>,
}

impl DatasetRecorder {
    /// Crea un recorder. Genera `output_dir/crops/` si no existe.
    /// El manifest se abre en append mode. Si falla creación de dirs o file,
    /// el recorder queda inhábil (`is_enabled() = false`).
    pub fn new(
        output_dir:     PathBuf,
        slots:          Vec<RoiDef>,
        interval_ticks: u32,
        tag:            String,
    ) -> Self {
        let interval = interval_ticks.max(1);
        let mut recorder = Self {
            output_dir: output_dir.clone(),
            slots,
            interval_ticks: interval,
            tag,
            manifest: None,
            seq: 0,
            total_crops: 0,
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
        match OpenOptions::new()
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
                tracing::info!(
                    "DatasetRecorder: capturando a '{}' (interval={} ticks, tag='{}', {} slots)",
                    output_dir.display(), interval, recorder.tag, recorder.slots.len()
                );
                recorder.manifest = Some(w);
            }
            Err(e) => {
                tracing::warn!(
                    "DatasetRecorder: no se pudo abrir manifest.csv '{}': {}",
                    manifest_path.display(), e
                );
            }
        }
        recorder
    }

    /// `true` si el recorder está activo y puede grabar.
    pub fn is_enabled(&self) -> bool {
        self.manifest.is_some() && !self.slots.is_empty()
    }

    /// Total de crops escritos hasta ahora.
    #[allow(dead_code)]
    pub fn total_crops(&self) -> u64 {
        self.total_crops
    }

    /// Procesa un frame en el tick dado. Captura cada slot como PNG y agrega
    /// entrada al manifest. Respeta `interval_ticks`.
    /// Devuelve número de crops escritos en este tick.
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
        let mut written = 0u32;
        let captured_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let crops_dir = self.output_dir.join("crops");
        for (slot_idx, slot) in self.slots.iter().enumerate() {
            let Some(bgra) = crop_bgra(frame, Roi::new(slot.x, slot.y, slot.w, slot.h)) else {
                continue;
            };
            let filename = format!("{:04}_t{:06}_s{:02}.png", self.seq, tick, slot_idx);
            let path = crops_dir.join(&filename);
            if let Err(e) = save_bgra_as_png(&bgra, slot.w, slot.h, &path) {
                tracing::warn!(
                    "DatasetRecorder: falló guardar {}: {}", filename, e
                );
                continue;
            }
            // Manifest entry: label vacío inicialmente, lo agrega tool de etiquetado.
            if let Some(w) = self.manifest.as_mut() {
                let _ = writeln!(w,
                    "{},{},{},{},{},{},",
                    filename, tick, slot_idx, frame_id, captured_ms, self.tag
                );
            }
            written += 1;
        }
        if written > 0 {
            self.total_crops += written as u64;
            self.seq += 1;
            // Flush periódicamente (cada 30 capturas).
            if self.seq % 30 == 0 {
                if let Some(w) = self.manifest.as_mut() {
                    let _ = w.flush();
                }
            }
        }
        self.last_tick = Some(tick);
        written
    }

    /// Flushea el manifest writer. Llamar antes de stop o al cerrar.
    pub fn flush(&mut self) {
        if let Some(w) = self.manifest.as_mut() {
            let _ = w.flush();
        }
    }
}

impl Drop for DatasetRecorder {
    fn drop(&mut self) {
        self.flush();
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

    #[test]
    fn recorder_creates_directories_and_manifest() {
        let dir = unique_test_dir("creates_dirs");
        let slots = vec![RoiDef::new(0, 0, 8, 8)];
        let r = DatasetRecorder::new(dir.clone(), slots, 1, "test".into());
        assert!(r.is_enabled());
        assert!(dir.join("crops").exists());
        assert!(dir.join("manifest.csv").exists());
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
        let written = r.capture(&frame, 100, 50);
        assert_eq!(written, 3);
        // Verificar que existen los 3 PNGs.
        let crops_dir = dir.join("crops");
        let entries: Vec<_> = std::fs::read_dir(&crops_dir).unwrap().collect();
        assert_eq!(entries.len(), 3);
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
        let written = r.capture(&frame, 1, 1);
        // Solo el primer slot fits → 1 crop escrito.
        assert_eq!(written, 1);
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
        r.flush();
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
}
