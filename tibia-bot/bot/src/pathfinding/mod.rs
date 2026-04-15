//! pathfinding — A* multi-floor para navegación por tile usando los
//! WaypointCost PNGs de Tibia.
//!
//! Los archivos `Minimap_WaypointCost_{x}_{y}_{z}.png` contienen la grilla
//! de walkability/friction del juego. Cada pixel = 1 tile. El valor del
//! pixel (mode P palette) representa la fricción:
//!
//! - 90-213: tile walkable con cost = valor (dark = fast, light = slow)
//! - 255: tile unwalkable (yellow = wall)
//!
//! ## Multi-floor
//!
//! A* soporta pathfinding entre pisos automáticamente. Los "portales" entre
//! pisos (stairs, ramps, ropes, holes) se detectan por coincidencia vertical
//! (tile walkable en `z` y `z-1`) y se marcan como transiciones. A* usa
//! 6-conectividad cuando el tile actual es una transición: 4 vecinos
//! horizontales + 2 vecinos verticales.
//!
//! Para rope/ladder/hole que el auto-detect no ve, usar [`Overrides`] con
//! un archivo TOML:
//!
//! ```toml
//! # assets/pathfinding_overrides.toml
//! add = [
//!   # rope spot en Thais: (32350, 32200, 7) ↔ (32350, 32200, 6)
//!   [32350, 32200, 7],
//!   [32350, 32200, 6],
//! ]
//! remove = [
//!   # bridge en Carlin: falso positivo del auto-detect
//!   [32000, 31600, 7],
//!   [32000, 31600, 8],
//! ]
//! ```
//!
//! Este módulo expone:
//!
//! - [`WalkabilityGrid::load_from_dir`] — carga PNGs + auto-detecta transiciones
//! - [`find_path`] — A* multi-floor entre 2 coords absolutas
//! - [`simplify_path`] — reduce path a corners (incluye cambios de piso)
//! - [`Overrides`] — add/remove transiciones manuales desde TOML
//!
//! El CLI `path_finder` bin usa todo esto para generar node sequences para
//! scripts de cavebot.

pub mod astar;
pub mod overrides;
pub mod walkability;

pub use astar::{find_path, simplify_path, FLOOR_CHANGE_PENALTY};
pub use overrides::Overrides;
pub use walkability::{TileCost, WalkabilityGrid};
