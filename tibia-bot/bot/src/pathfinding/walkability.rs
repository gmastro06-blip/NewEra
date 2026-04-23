//! walkability — Carga y almacena la grilla de walkability/friction de Tibia.
//!
//! Los archivos `Minimap_WaypointCost_{x}_{y}_{z}.png` son mode P (palette),
//! 256×256 pixels. Cada pixel = 1 tile. El índice de palette representa
//! la fricción:
//!
//! - **90-213**: tile walkable con cost = valor (0x5A..0xD5). Valores bajos
//!   son terrenos rápidos (tierra, piedra), altos son lentos (agua, arena).
//! - **255**: tile unwalkable (pared, obstáculo fijo, amarillo).
//!
//! Este módulo expone:
//!
//! - [`TileCost`] — alias para `u8` (90-213 walkable, 255 wall).
//! - [`WalkabilityGrid`] — HashMap sparse `(x,y,z) → TileCost`.
//! - [`WalkabilityGrid::load_from_dir`] — carga todos los PNGs de un directorio.
//! - [`WalkabilityGrid::save`] / `load` — serialización con bincode.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use image::GenericImageView;
use serde::{Deserialize, Serialize};

/// Cost de un tile. 90-213 = walkable, 255 = wall/obstáculo.
pub type TileCost = u8;

/// Umbral sobre el cual un tile se considera no transitable.
/// Tibia usa 255 para walls amarillas; anything below 250 es walkable.
pub const WALL_THRESHOLD: u8 = 250;

/// Magic header para WalkabilityGrid on-disk (postcard format). Primeros 4
/// bytes del archivo. Distingue postcard de bincode legacy sin ambigüedad.
const WALKABILITY_MAGIC: &[u8; 4] = b"TBWG"; // "Tibia Bot Walkability Grid"
/// Version byte tras el magic. Bump al cambiar schema.
const WALKABILITY_VERSION: u8 = 1;

/// Grilla sparse de walkability por tile absoluto `(x, y, z)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WalkabilityGrid {
    /// Mapa de coordenada absoluta → cost (0-255).
    pub tiles: HashMap<(i32, i32, i32), TileCost>,
    /// Tiles marcados como portales de transición entre pisos. Una transición
    /// es bidireccional: si `(x,y,z)` y `(x,y,z-1)` están ambos en este set,
    /// A* permitirá moverse entre ellos (con un penalty por cambio de piso).
    ///
    /// Auto-detectadas vía [`detect_transitions`] por coincidencia vertical
    /// (tile walkable en 2 pisos adyacentes). Override manual vía
    /// [`add_transition`] / [`remove_transition`] para corregir falsos
    /// positivos (bridges) o añadir rope/ladder/hole.
    ///
    /// [`detect_transitions`]: Self::detect_transitions
    /// [`add_transition`]: Self::add_transition
    /// [`remove_transition`]: Self::remove_transition
    #[serde(default)]
    pub transitions: HashSet<(i32, i32, i32)>,
    /// Número de archivos PNG procesados durante la construcción.
    pub files_loaded: u32,
}

impl WalkabilityGrid {
    /// Crea una grilla vacía.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parsea el filename de un PNG de WaypointCost.
    /// Formato: `Minimap_WaypointCost_{x}_{y}_{z}.png`
    pub fn parse_filename(filename: &str) -> Option<(i32, i32, i32)> {
        let stem = filename.strip_suffix(".png")?;
        let parts: Vec<&str> = stem.split('_').collect();
        if parts.len() != 5 || parts[0] != "Minimap" || parts[1] != "WaypointCost" {
            return None;
        }
        let x: i32 = parts[2].parse().ok()?;
        let y: i32 = parts[3].parse().ok()?;
        let z: i32 = parts[4].parse().ok()?;
        Some((x, y, z))
    }

    /// Añade los 256×256 tiles de un PNG de WaypointCost a la grilla.
    ///
    /// `(fx, fy, fz)` es la coord del tile top-left del PNG.
    /// Skip de tiles "unexplored" (ya que los archivos de Tibia solo contienen
    /// pixels con cost). Si no se puede leer como palette, usa luminance del
    /// RGB como fallback.
    pub fn load_png(&mut self, path: &Path, fx: i32, fy: i32, fz: i32) -> Result<u32> {
        let img = image::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let (w, h) = img.dimensions();
        if w != 256 || h != 256 {
            anyhow::bail!("expected 256x256, got {}x{} in {}", w, h, path.display());
        }

        let rgba = img.to_rgba8();
        let mut count = 0u32;

        for py in 0..256u32 {
            for px in 0..256u32 {
                let pixel = rgba.get_pixel(px, py);
                let [r, g, b, a] = pixel.0;

                // Skip transparent (unexplored) pixels.
                if a == 0 {
                    continue;
                }

                // Yellow wall (idx 255 → rgb (255, 255, 0)).
                if r == 255 && g == 255 && b == 0 {
                    let tx = fx + px as i32;
                    let ty = fy + py as i32;
                    self.tiles.insert((tx, ty, fz), 255);
                    count += 1;
                    continue;
                }

                // Walkable: greyscale with cost value (R == G == B in idx 90-213).
                // Some tiles pueden tener anti-aliasing sutil, así que chequeamos
                // que el valor esté en el rango conocido.
                if r == g && g == b && (90..=213).contains(&r) {
                    let tx = fx + px as i32;
                    let ty = fy + py as i32;
                    self.tiles.insert((tx, ty, fz), r);
                    count += 1;
                }
                // Otros colores (magenta unexplored, etc.) se ignoran.
            }
        }

        self.files_loaded += 1;
        Ok(count)
    }

    /// Carga todos los `Minimap_WaypointCost_*.png` de un directorio.
    /// Si `floors` no está vacío, solo carga esos niveles.
    ///
    /// Tras cargar, ejecuta [`detect_transitions`](Self::detect_transitions)
    /// automáticamente para habilitar pathfinding multi-floor.
    pub fn load_from_dir<P: AsRef<Path>>(dir: P, floors: &[i32]) -> Result<Self> {
        let mut grid = Self::new();
        let dir = dir.as_ref();

        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("failed to read dir {}", dir.display()))?
        {
            let entry = entry?;
            let filename = entry.file_name();
            let filename_str = filename.to_string_lossy();

            let Some((fx, fy, fz)) = Self::parse_filename(&filename_str) else {
                continue;
            };

            if !floors.is_empty() && !floors.contains(&fz) {
                continue;
            }

            if let Err(e) = grid.load_png(&entry.path(), fx, fy, fz) {
                eprintln!("Warning: {}: {}", entry.path().display(), e);
            }
        }

        let n = grid.detect_transitions();
        eprintln!(
            "Detected {} vertical transitions ({} tiles marked)",
            n,
            grid.transitions.len()
        );

        Ok(grid)
    }

    /// Auto-detecta transiciones entre pisos por coincidencia vertical.
    ///
    /// Para cada tile walkable `(x,y,z)`, si `(x,y,z-1)` también es walkable,
    /// ambos se marcan como transiciones (el par se considera un stair/ramp).
    ///
    /// Retorna el número de pares detectados.
    ///
    /// **Limitación conocida**: genera falsos positivos en puentes (tile
    /// walkable en ambos pisos pero físicamente no conectados). Corregir
    /// con [`remove_transition`](Self::remove_transition) para áreas
    /// conocidas.
    ///
    /// **No detecta**: rope/ladder/shovel (transiciones que requieren item-use).
    /// Añadir manualmente con [`add_transition`](Self::add_transition).
    pub fn detect_transitions(&mut self) -> u32 {
        let mut count = 0u32;
        // Clonamos keys para evitar borrow mutable durante iteración.
        let keys: Vec<_> = self.tiles.keys().copied().collect();
        for (x, y, z) in keys {
            if self.is_walkable(x, y, z) && self.is_walkable(x, y, z - 1) {
                self.transitions.insert((x, y, z));
                self.transitions.insert((x, y, z - 1));
                count += 1;
            }
        }
        count
    }

    /// True si el tile está marcado como transición vertical.
    pub fn is_transition(&self, x: i32, y: i32, z: i32) -> bool {
        self.transitions.contains(&(x, y, z))
    }

    /// Añade una transición manual (útil para rope/ladder/teleport).
    /// Debe llamarse en ambos endpoints para que A* los conecte.
    pub fn add_transition(&mut self, x: i32, y: i32, z: i32) {
        self.transitions.insert((x, y, z));
    }

    /// Elimina una transición (corrige falsos positivos como puentes).
    /// Retorna true si el tile estaba marcado.
    pub fn remove_transition(&mut self, x: i32, y: i32, z: i32) -> bool {
        self.transitions.remove(&(x, y, z))
    }

    /// Número total de tiles marcados como transición.
    pub fn transitions_count(&self) -> usize {
        self.transitions.len()
    }

    /// Serializa la grilla a un archivo usando postcard.
    ///
    /// Migración 2026-04-23: bincode 1.3 → postcard (bincode unmaintained,
    /// RUSTSEC-2023-0074). Los `.bin` pre-migración se siguen cargando por
    /// el fallback de `load()`.
    ///
    /// Formato on-disk: `[MAGIC (4) | VERSION (1) | postcard bytes...]`. El
    /// magic distingue postcard de bincode legacy sin ambigüedad (postcard
    /// acepta bytes arbitrarios como parsing "success" silent-corrupt).
    ///
    /// Nota: `to_allocvec` mantiene todo el buffer en memoria (~230 MB
    /// para el walkability completo). Bincode hacía lo mismo internamente
    /// con `BufWriter`, así que el footprint es equivalente.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let payload = postcard::to_allocvec(self)
            .with_context(|| format!("postcard serialize WalkabilityGrid: {}", path.display()))?;
        let mut data = Vec::with_capacity(5 + payload.len());
        data.extend_from_slice(WALKABILITY_MAGIC);
        data.push(WALKABILITY_VERSION);
        data.extend_from_slice(&payload);
        std::fs::write(path, data)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// Carga una grilla serializada.
    ///
    /// Si los primeros 5 bytes son `[TBWG, version]` → postcard. Si no →
    /// fallback bincode legacy con warning. Cuando ya no queden archivos
    /// legacy se puede eliminar el fallback y la dep `bincode`.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let data = std::fs::read(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if data.len() >= 5 && &data[..4] == WALKABILITY_MAGIC {
            let version = data[4];
            if version != WALKABILITY_VERSION {
                anyhow::bail!(
                    "WalkabilityGrid '{}': versión on-disk {} no soportada (esperaba {})",
                    path.display(), version, WALKABILITY_VERSION
                );
            }
            return postcard::from_bytes::<Self>(&data[5..])
                .with_context(|| format!(
                    "WalkabilityGrid '{}': postcard decode falló", path.display()
                ));
        }
        // Sin magic → formato legacy bincode.
        let grid: Self = bincode::deserialize(&data)
            .with_context(|| format!(
                "WalkabilityGrid '{}': ni postcard (magic mismatch) ni bincode legacy",
                path.display()
            ))?;
        tracing::warn!(
            "WalkabilityGrid '{}': cargado en formato bincode legacy. \
             Regenerar con `build_map_index --walkability ...` para \
             migrar a postcard.",
            path.display()
        );
        Ok(grid)
    }

    /// Retorna true si el tile existe en el grid y es transitable.
    pub fn is_walkable(&self, x: i32, y: i32, z: i32) -> bool {
        match self.tiles.get(&(x, y, z)) {
            Some(&cost) => cost < WALL_THRESHOLD,
            None => false,
        }
    }

    /// Retorna el cost del tile (0-255), o `None` si no existe.
    pub fn cost(&self, x: i32, y: i32, z: i32) -> Option<TileCost> {
        self.tiles.get(&(x, y, z)).copied()
    }

    /// Número total de tiles almacenados.
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    /// True si la grilla está vacía.
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Añade manualmente un tile (útil para tests).
    pub fn set_tile(&mut self, x: i32, y: i32, z: i32, cost: TileCost) {
        self.tiles.insert((x, y, z), cost);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_filename() {
        let r = WalkabilityGrid::parse_filename("Minimap_WaypointCost_32000_31488_7.png");
        assert_eq!(r, Some((32000, 31488, 7)));
    }

    #[test]
    fn parse_invalid_filename() {
        assert_eq!(WalkabilityGrid::parse_filename("random.png"), None);
        assert_eq!(
            WalkabilityGrid::parse_filename("Minimap_Color_32000_31488_7.png"),
            None
        );
        assert_eq!(WalkabilityGrid::parse_filename("not_a_png.txt"), None);
    }

    #[test]
    fn walkability_api() {
        let mut g = WalkabilityGrid::new();
        assert!(g.is_empty());

        g.set_tile(10, 10, 7, 100); // walkable
        g.set_tile(11, 10, 7, 255); // wall

        assert_eq!(g.len(), 2);
        assert!(g.is_walkable(10, 10, 7));
        assert!(!g.is_walkable(11, 10, 7));
        assert!(!g.is_walkable(12, 10, 7)); // not in grid
        assert_eq!(g.cost(10, 10, 7), Some(100));
        assert_eq!(g.cost(99, 99, 7), None);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let mut g = WalkabilityGrid::new();
        g.set_tile(100, 200, 7, 90);
        g.set_tile(101, 200, 7, 150);
        g.set_tile(102, 200, 7, 255);
        g.add_transition(100, 200, 7);
        g.add_transition(100, 200, 6);
        g.files_loaded = 3;

        let tmp = std::env::temp_dir().join("walkability_roundtrip_test.bin");
        g.save(&tmp).unwrap();

        let loaded = WalkabilityGrid::load(&tmp).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.files_loaded, 3);
        assert_eq!(loaded.cost(100, 200, 7), Some(90));
        assert_eq!(loaded.cost(101, 200, 7), Some(150));
        assert_eq!(loaded.cost(102, 200, 7), Some(255));
        assert!(loaded.is_transition(100, 200, 7));
        assert!(loaded.is_transition(100, 200, 6));
        assert_eq!(loaded.transitions_count(), 2);

        std::fs::remove_file(&tmp).ok();
    }

    /// Compat: `load()` debe aceptar `.bin` escritos con bincode legacy
    /// (formato pre-migración 2026-04-23). Escribimos un grid serializado
    /// en bincode y verificamos que el loader moderno lo parsea vía fallback.
    #[test]
    fn load_accepts_legacy_bincode() {
        let mut g = WalkabilityGrid::new();
        g.set_tile(50, 60, 7, 50);
        g.set_tile(51, 60, 7, 200);
        g.add_transition(50, 60, 7);
        g.files_loaded = 1;

        // Escribir en formato legacy (bincode).
        let legacy_bytes = bincode::serialize(&g).unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "walkability_legacy_{}.bin", std::process::id()
        ));
        std::fs::write(&tmp, &legacy_bytes).unwrap();

        // Load via el método moderno — fallback bincode debe parsear OK.
        let loaded = WalkabilityGrid::load(&tmp).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.cost(50, 60, 7), Some(50));
        assert_eq!(loaded.cost(51, 60, 7), Some(200));
        assert!(loaded.is_transition(50, 60, 7));
        assert_eq!(loaded.files_loaded, 1);

        std::fs::remove_file(&tmp).ok();
    }

    /// Validación empírica: cargar el `walkability.bin` real del proyecto
    /// (formato bincode legacy, ~146 MB). `#[ignore]` porque depende del
    /// asset + es verificación manual.
    ///
    /// Run: `cargo test --release --lib load_real_walkability_bin -- --ignored --nocapture`
    #[test]
    #[ignore = "requires assets/walkability.bin checked into repo; run with --ignored"]
    fn load_real_walkability_bin() {
        let candidates = [
            std::path::Path::new("../assets/walkability.bin"),
            std::path::Path::new("assets/walkability.bin"),
        ];
        let path = candidates.iter().find(|p| p.exists())
            .expect("no se encontró walkability.bin en ../assets ni en assets");
        eprintln!("cargando {} (puede tardar ~1-2s @ 146 MB)", path.display());
        let t0 = std::time::Instant::now();
        let grid = WalkabilityGrid::load(path).expect("load real walkability.bin falló");
        let dt_ms = t0.elapsed().as_millis();
        assert!(!grid.is_empty(), "walkability real no debería estar vacío");
        eprintln!("OK — {} tiles, {} transitions, load time {} ms",
                  grid.len(), grid.transitions_count(), dt_ms);
    }

    #[test]
    fn detect_transitions_marks_stacked_walkable() {
        let mut g = WalkabilityGrid::new();
        // Stacked walkable (ambos pisos) → transición
        g.set_tile(10, 10, 7, 100);
        g.set_tile(10, 10, 6, 100);
        // No stacked (solo z=7)
        g.set_tile(20, 20, 7, 100);
        // Stacked pero el de abajo es wall
        g.set_tile(30, 30, 7, 100);
        g.set_tile(30, 30, 6, 255);

        let n = g.detect_transitions();
        assert_eq!(n, 1); // 1 par (10,10)
        assert!(g.is_transition(10, 10, 7));
        assert!(g.is_transition(10, 10, 6));
        assert!(!g.is_transition(20, 20, 7));
        assert!(!g.is_transition(30, 30, 7));
        assert!(!g.is_transition(30, 30, 6));
    }

    #[test]
    fn remove_transition_corrects_false_positive() {
        let mut g = WalkabilityGrid::new();
        g.set_tile(50, 50, 7, 100);
        g.set_tile(50, 50, 6, 100);
        g.detect_transitions();
        assert!(g.is_transition(50, 50, 7));

        assert!(g.remove_transition(50, 50, 7));
        assert!(!g.is_transition(50, 50, 7));
        assert!(!g.remove_transition(50, 50, 7)); // ya no estaba
    }

    #[test]
    fn add_transition_supports_manual_rope_entry() {
        let mut g = WalkabilityGrid::new();
        g.set_tile(100, 100, 7, 100);
        g.set_tile(100, 100, 6, 100);
        // No llamamos detect_transitions — simulamos un rope que el auto-detect
        // no vio porque el piso inferior estaba con cost alto o algo.
        g.add_transition(100, 100, 7);
        g.add_transition(100, 100, 6);
        assert!(g.is_transition(100, 100, 7));
        assert!(g.is_transition(100, 100, 6));
    }
}
