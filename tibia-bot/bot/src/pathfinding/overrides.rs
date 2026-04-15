//! overrides — Add/remove manual de transiciones entre pisos.
//!
//! El auto-detect de [`WalkabilityGrid::detect_transitions`] solo captura
//! stairs/ramps (tiles walkable en pisos adyacentes con mismo `(x,y)`).
//! Para:
//!
//! - **Ropes** (`use rope` en spot marcado): rara vez el piso superior
//!   es walkable directamente — el auto-detect no lo ve.
//! - **Holes** (`use shovel` o walk-in): el tile del hole puede ser
//!   walkable solo en un piso (se cae al usar).
//! - **Ladders** (`use` en ladder): similar a rope.
//! - **Falsos positivos**: bridges y rooftops donde ambos pisos son
//!   walkable pero NO están físicamente conectados.
//!
//! ## Formato
//!
//! ```toml
//! # Pares de coords que se añaden como transiciones (ambos endpoints).
//! # Cada entry debe venir en pares [x, y, z] — A* solo conecta si ambos
//! # endpoints están marcados.
//! add = [
//!   [32350, 32200, 7],
//!   [32350, 32200, 6],
//! ]
//!
//! # Tiles que se desmarcan del auto-detect (corrige falsos positivos).
//! remove = [
//!   [32000, 31600, 7],
//!   [32000, 31600, 8],
//! ]
//! ```
//!
//! ## Uso
//!
//! ```ignore
//! let mut grid = WalkabilityGrid::load_from_dir(&map_dir, &[])?;
//! let overrides = Overrides::load("assets/pathfinding_overrides.toml")?;
//! overrides.apply(&mut grid);
//! ```

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::walkability::WalkabilityGrid;

/// Overrides cargados desde un archivo TOML.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Overrides {
    /// Tiles a añadir al set de transiciones.
    #[serde(default)]
    pub add: Vec<(i32, i32, i32)>,
    /// Tiles a quitar del set de transiciones.
    #[serde(default)]
    pub remove: Vec<(i32, i32, i32)>,
}

impl Overrides {
    /// Carga un archivo TOML de overrides. Si no existe, retorna `Overrides::default()`.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let overrides: Overrides = toml::from_str(&content)
            .with_context(|| format!("failed to parse TOML {}", path.display()))?;
        Ok(overrides)
    }

    /// Aplica los overrides a una grilla. Retorna `(added, removed)`.
    pub fn apply(&self, grid: &mut WalkabilityGrid) -> (u32, u32) {
        let mut added = 0u32;
        let mut removed = 0u32;
        for &(x, y, z) in &self.add {
            if !grid.is_transition(x, y, z) {
                added += 1;
            }
            grid.add_transition(x, y, z);
        }
        for &(x, y, z) in &self.remove {
            if grid.remove_transition(x, y, z) {
                removed += 1;
            }
        }
        (added, removed)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_nonexistent_returns_default() {
        let o = Overrides::load("nonexistent.toml").unwrap();
        assert!(o.add.is_empty());
        assert!(o.remove.is_empty());
    }

    #[test]
    fn parse_and_apply_toml() {
        let toml_str = r#"
            add = [
                [100, 200, 7],
                [100, 200, 6],
            ]
            remove = [
                [50, 50, 7],
            ]
        "#;
        let o: Overrides = toml::from_str(toml_str).unwrap();
        assert_eq!(o.add.len(), 2);
        assert_eq!(o.remove.len(), 1);

        let mut g = WalkabilityGrid::new();
        g.set_tile(100, 200, 7, 100);
        g.set_tile(100, 200, 6, 100);
        g.set_tile(50, 50, 7, 100);
        g.add_transition(50, 50, 7); // simulamos falso positivo del auto-detect

        let (added, removed) = o.apply(&mut g);
        assert_eq!(added, 2);
        assert_eq!(removed, 1);
        assert!(g.is_transition(100, 200, 7));
        assert!(g.is_transition(100, 200, 6));
        assert!(!g.is_transition(50, 50, 7));
    }

    #[test]
    fn load_and_apply_roundtrip() {
        let tmp = std::env::temp_dir().join("pathfinding_overrides_test.toml");
        std::fs::write(
            &tmp,
            "add = [[10, 20, 7], [10, 20, 6]]\nremove = []\n",
        )
        .unwrap();

        let o = Overrides::load(&tmp).unwrap();
        assert_eq!(o.add.len(), 2);

        let mut g = WalkabilityGrid::new();
        g.set_tile(10, 20, 7, 100);
        g.set_tile(10, 20, 6, 100);
        o.apply(&mut g);
        assert!(g.is_transition(10, 20, 7));
        assert!(g.is_transition(10, 20, 6));

        std::fs::remove_file(&tmp).ok();
    }
}
