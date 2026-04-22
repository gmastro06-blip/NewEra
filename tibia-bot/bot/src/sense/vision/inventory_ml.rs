//! inventory_ml.rs — ML classifier para inventory slots (Fase 2.5 — scaffold).
//!
//! Reemplaza el matcher SSE pixel-by-pixel por un classifier CNN entrenado
//! con `ml/train_inventory_classifier.py` y exportado a ONNX.
//!
//! ## Estado actual: scaffold sin runtime ort
//!
//! La carga e inferencia con `ort` (ONNX Runtime) NO está implementada en
//! este commit. El `MlInventoryReader` puede:
//!
//! 1. **Cargar el archivo de clases** (JSON) — funcional.
//! 2. **Validar la existencia del modelo ONNX** — funcional.
//! 3. **Inferir** — devuelve `None` con warn (placeholder).
//!
//! El consumer (`InventoryReader`) cae al fallback SSE matcher cuando
//! `infer_slot()` devuelve None, garantizando comportamiento equivalente
//! al previo a Fase 2.5.
//!
//! ## Por qué scaffold + no full impl
//!
//! 1. **No hay modelo entrenado**: el dataset capture (Fase 2.2) y el
//!    training pipeline (Fase 2.4) están listos pero el usuario aún no
//!    ejecutó una sesión live de captura. Sin .onnx real, integrar `ort`
//!    es código no-testeable end-to-end.
//! 2. **Costo de dependency**: `ort = "2.0"` añade ~10 MB al binary y
//!    requiere ONNX Runtime native lib en el sistema. Mejor commitear
//!    cuando sirve para algo concreto.
//! 3. **Path forward documentado**: ver "TODO INTEGRATION" abajo, listo
//!    para retomar cuando haya modelo.
//!
//! ## TODO INTEGRATION (próxima sesión cuando haya modelo)
//!
//! 1. Add deps: `ort = "2.0"` (o última estable) + `ndarray = "0.15"`.
//! 2. En `MlInventoryReader::load()`:
//!    ```rust
//!    let session = ort::Session::builder()?
//!        .with_optimization_level(ort::GraphOptimizationLevel::Level3)?
//!        .commit_from_file(&self.model_path)?;
//!    self.session = Some(session);
//!    ```
//! 3. En `infer_slot()`:
//!    - Convertir `&GrayImage` 32×32 → `Array4<f32>` (1×3×32×32) con
//!      replicación L→RGB y normalización [0,1].
//!    - Run inference: `session.run(ort::inputs!["input" => tensor]?)?`
//!    - Extract logits, apply softmax, take argmax + confidence.
//!    - Return `Some((class_name, confidence))` si confidence ≥ threshold,
//!      else None.
//! 4. Tests con modelo dummy ONNX (script Python para crear identity model).

use std::path::{Path, PathBuf};

use image::GrayImage;
use serde::Deserialize;

/// Reader ML para clasificación de inventory slots.
///
/// **Estado**: scaffold. `infer_slot()` siempre devuelve None hasta wire de
/// `ort` runtime. El consumer hace fallback a SSE matcher.
pub struct MlInventoryReader {
    /// Path del modelo ONNX (validado al construir).
    model_path:           PathBuf,
    /// Clases conocidas (cargadas del classes.json). Idx en este Vec
    /// corresponde al output del modelo.
    classes:              Vec<String>,
    /// Confidence threshold mínimo para aceptar predicción.
    confidence_threshold: f32,
    /// `true` si modelo + clases cargaron OK (aunque inference no esté wired).
    ready:                bool,
}

#[derive(Deserialize)]
struct ClassesFile {
    classes:    Vec<String>,
    #[serde(default)]
    #[allow(dead_code)] // descriptor input shape, validación futura
    input_size: Option<Vec<u32>>,
}

impl MlInventoryReader {
    /// Crea un reader vacío e inhábil (placeholder).
    pub fn new_empty() -> Self {
        Self {
            model_path:           PathBuf::new(),
            classes:              Vec::new(),
            confidence_threshold: 0.0,
            ready:                false,
        }
    }

    /// Intenta cargar modelo + clases. Si falla, devuelve un reader inhábil
    /// (`is_ready() = false`) que no rompe el caller.
    pub fn load(model_path: &Path, classes_path: &Path, confidence_threshold: f32) -> Self {
        if !model_path.exists() {
            tracing::warn!(
                "MlInventoryReader: modelo ONNX no existe: '{}'. Cae a fallback SSE.",
                model_path.display()
            );
            return Self::new_empty();
        }
        if !classes_path.exists() {
            tracing::warn!(
                "MlInventoryReader: classes.json no existe: '{}'. Cae a fallback SSE.",
                classes_path.display()
            );
            return Self::new_empty();
        }
        let classes = match Self::load_classes(classes_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "MlInventoryReader: error cargando classes.json '{}': {}. Cae a fallback SSE.",
                    classes_path.display(), e
                );
                return Self::new_empty();
            }
        };
        if classes.is_empty() {
            tracing::warn!(
                "MlInventoryReader: classes.json '{}' vacío. Cae a fallback SSE.",
                classes_path.display()
            );
            return Self::new_empty();
        }

        // TODO INTEGRATION: cargar ort::Session aquí.
        // Por ahora marcamos ready=true para indicar que la config es válida,
        // pero `infer_slot()` igual devuelve None hasta que ort esté wired.
        tracing::info!(
            "MlInventoryReader: scaffold cargado (modelo='{}', {} clases, threshold={:.2}). \
             Inference NOT WIRED — fallback SSE activo.",
            model_path.display(), classes.len(), confidence_threshold
        );
        Self {
            model_path:           model_path.to_path_buf(),
            classes,
            confidence_threshold,
            ready: true,
        }
    }

    fn load_classes(path: &Path) -> Result<Vec<String>, String> {
        let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let parsed: ClassesFile = serde_json::from_str(&content).map_err(|e| e.to_string())?;
        Ok(parsed.classes)
    }

    /// `true` si modelo + classes cargaron OK. NO implica que inference funciona
    /// (todavía es scaffold) — el consumer debe checkar el resultado de `infer_slot`.
    #[allow(dead_code)] // public API
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Devuelve la lista de clases conocidas. Vacío si reader inhábil.
    #[allow(dead_code)] // public API
    pub fn classes(&self) -> &[String] {
        &self.classes
    }

    /// Infiere la clase de un slot 32×32. Devuelve `Some((class_name, confidence))`
    /// si confidence ≥ threshold, o `None` si:
    /// - reader inhábil
    /// - inference no wired aún (estado actual)
    /// - confidence bajo el threshold
    /// - error en runtime
    #[allow(dead_code)] // wired desde inventory.rs cuando ort esté integrado
    pub fn infer_slot(&self, _slot: &GrayImage) -> Option<(String, f32)> {
        if !self.ready {
            return None;
        }
        // TODO INTEGRATION: ort inference aquí.
        // 1. Convertir slot luma 32×32 → ndarray (1, 3, 32, 32) con
        //    R=G=B=luma/255.0 (replicar canales).
        // 2. session.run(inputs![...])
        // 3. extract logits, softmax, argmax + confidence
        // 4. Si confidence >= self.confidence_threshold:
        //      Some((self.classes[argmax].clone(), confidence))
        //    Else None.
        None
    }
}

impl Default for MlInventoryReader {
    fn default() -> Self { Self::new_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(suffix: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("inv_ml_test_{}_{}", suffix, n))
    }

    #[test]
    fn new_empty_is_not_ready() {
        let r = MlInventoryReader::new_empty();
        assert!(!r.is_ready());
        assert!(r.classes().is_empty());
    }

    #[test]
    fn load_missing_model_returns_inhabil() {
        let dir = unique_test_dir("missing_model");
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("nonexistent.onnx");
        let classes = dir.join("classes.json");
        std::fs::write(&classes, r#"{"classes":["a","b"]}"#).unwrap();
        let r = MlInventoryReader::load(&model, &classes, 0.8);
        assert!(!r.is_ready(), "modelo missing debe dar reader inhábil");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_classes_returns_inhabil() {
        let dir = unique_test_dir("missing_classes");
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.onnx");
        std::fs::write(&model, b"fake onnx bytes").unwrap();  // archivo existe pero contenido fake
        let classes = dir.join("nonexistent.json");
        let r = MlInventoryReader::load(&model, &classes, 0.8);
        assert!(!r.is_ready());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_valid_files_marks_ready() {
        let dir = unique_test_dir("valid");
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.onnx");
        let classes = dir.join("classes.json");
        std::fs::write(&model, b"placeholder onnx").unwrap();
        std::fs::write(&classes,
            r#"{"classes":["vial","golden_backpack","empty"],"input_size":[3,32,32]}"#).unwrap();
        let r = MlInventoryReader::load(&model, &classes, 0.85);
        assert!(r.is_ready());
        assert_eq!(r.classes(), &["vial", "golden_backpack", "empty"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_classes_with_empty_array_inhabil() {
        let dir = unique_test_dir("empty_classes");
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.onnx");
        let classes = dir.join("classes.json");
        std::fs::write(&model, b"x").unwrap();
        std::fs::write(&classes, r#"{"classes":[]}"#).unwrap();
        let r = MlInventoryReader::load(&model, &classes, 0.8);
        assert!(!r.is_ready(), "classes vacío debe dar inhábil");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_malformed_classes_json_inhabil() {
        let dir = unique_test_dir("malformed");
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.onnx");
        let classes = dir.join("classes.json");
        std::fs::write(&model, b"x").unwrap();
        std::fs::write(&classes, "not valid json").unwrap();
        let r = MlInventoryReader::load(&model, &classes, 0.8);
        assert!(!r.is_ready());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn infer_slot_returns_none_until_ort_wired() {
        let dir = unique_test_dir("infer_stub");
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.onnx");
        let classes = dir.join("classes.json");
        std::fs::write(&model, b"x").unwrap();
        std::fs::write(&classes, r#"{"classes":["a"]}"#).unwrap();
        let r = MlInventoryReader::load(&model, &classes, 0.5);
        assert!(r.is_ready());
        let slot = GrayImage::new(32, 32);
        // Stub: hasta wire de ort, siempre None aunque ready=true.
        assert_eq!(r.infer_slot(&slot), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
