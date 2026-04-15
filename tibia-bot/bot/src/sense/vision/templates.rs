/// templates.rs — Carga templates PNG desde disco para template matching.
///
/// Los templates se cargan una sola vez al iniciar y se convierten a GrayImage
/// para usar con imageproc::template_matching.
///
/// Estructura de directorios esperada:
///   assets/templates/status/     ← iconos de condición (11x11 px aprox.)
///   assets/anchors/              ← templates de ancla (tamaño variable)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::GrayImage;
use tracing::{info, warn};

/// Colección de templates indexados por nombre de archivo (sin extensión).
#[allow(dead_code)] // extension point: base_dir for reload
pub struct TemplateStore {
    pub templates: HashMap<String, GrayImage>,
    pub base_dir:  PathBuf,
}

impl TemplateStore {
    /// Carga todos los archivos PNG de `dir` como templates en escala de grises.
    /// No falla si el directorio no existe — retorna un store vacío con un warning.
    pub fn load_dir(dir: &Path) -> Self {
        let mut store = Self {
            templates: HashMap::new(),
            base_dir:  dir.to_path_buf(),
        };

        if !dir.exists() {
            warn!(
                "Directorio de templates no encontrado: '{}' — template matching deshabilitado",
                dir.display()
            );
            return store;
        }

        let entries = match std::fs::read_dir(dir) {
            Ok(e)  => e,
            Err(e) => {
                warn!("No se pudo leer '{}': {}", dir.display(), e);
                return store;
            }
        };

        let mut count = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None    => continue,
            };
            match load_png_gray(&path) {
                Ok(img) => {
                    store.templates.insert(name, img);
                    count += 1;
                }
                Err(e) => {
                    warn!("No se pudo cargar template '{}': {}", path.display(), e);
                }
            }
        }

        info!("Templates cargados desde '{}': {}", dir.display(), count);
        store
    }

    /// Obtiene un template por nombre (sin extensión .png).
    #[allow(dead_code)] // extension point
    pub fn get(&self, name: &str) -> Option<&GrayImage> {
        self.templates.get(name)
    }

    pub fn is_empty(&self) -> bool { self.templates.is_empty() }
}

/// Carga un PNG y lo convierte a GrayImage.
pub fn load_png_gray(path: &Path) -> Result<GrayImage> {
    let img = image::open(path)
        .with_context(|| format!("No se pudo abrir '{}'", path.display()))?;
    Ok(img.to_luma8())
}
