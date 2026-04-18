//! astar — A* pathfinding sobre una [`WalkabilityGrid`].
//!
//! **Implementación**: delega el core a `pathfinding::directed::astar::astar`
//! (crate batería-incluida). Preserva las 2 features del wrapper histórico:
//!   - `max_iters` — aborta la búsqueda si se expanden > N nodos.
//!   - `nodes_expanded` — counter devuelto en `Path` para profiling.
//!
//! Ambos se implementan envolviendo el callback `successors` con un
//! `Cell<usize>` compartido que cuenta invocaciones y, cuando supera
//! `max_iters`, retorna `[]` — eso hace que el algoritmo termine con `None`.
//!
//! **Seis-conectividad**: 4 vecinos en el mismo piso (N/S/E/W) + 2 vecinos
//! verticales (Z±1) cuando ambos tiles están marcados como transición
//! (stair/ramp/rope). Esto permite pathfinding multi-floor automático.
//!
//! ## Heurística
//!
//! Manhattan 3D admisible: `(|dx|+|dy|)*MIN_COST + |dz|*MIN_FLOOR_COST`
//! donde `MIN_COST=90` y `MIN_FLOOR_COST=590` (`MIN_COST + FLOOR_PENALTY`).
//!
//! ## Cost por edge
//!
//! Usa el cost del tile destino (no el source). Esto permite que terrenos
//! lentos (cost alto) sean penalizados al moverse *hacia* ellos. Los paths
//! prefieren caminos rápidos (dark tiles, ~90) sobre lentos (~213).
//!
//! Los cambios de piso reciben un **penalty fijo** `FLOOR_CHANGE_PENALTY=500`
//! adicional al cost del tile destino. Evita que A* use stairs gratuitamente
//! cuando un path horizontal más largo es comparable.
//!
//! ## Límites
//!
//! - `max_iters` = 100_000 nodos expandidos (default).
//! - Si se excede, retorna `None`.

use std::cell::Cell;

use super::walkability::{WalkabilityGrid, WALL_THRESHOLD};

/// Máximo número de nodos expandidos antes de abortar la búsqueda.
pub const DEFAULT_MAX_ITERS: usize = 100_000;

/// Cost adicional al cambiar de piso (sobre el cost del tile destino).
pub const FLOOR_CHANGE_PENALTY: u64 = 500;

/// Cost mínimo de un tile walkable (0x5A = 90).
const MIN_TILE_COST: u64 = 90;

/// Resultado de un path exitoso.
#[derive(Debug, Clone)]
pub struct Path {
    /// Lista ordenada de tiles desde `start` hasta `goal` (ambos incluidos).
    pub tiles: Vec<(i32, i32, i32)>,
    /// Cost total acumulado (suma de costs de los tiles, excluyendo start).
    pub total_cost: u64,
    /// Nodos expandidos durante la búsqueda (útil para profiling).
    pub nodes_expanded: usize,
}

/// Heurística 3D admisible: distancia mínima posible entre 2 tiles.
///
/// Para ser admisible, nunca debe sobreestimar el cost real. El cost mínimo
/// por move horizontal es `MIN_TILE_COST=90`, y por cambio de piso es
/// `MIN_TILE_COST + FLOOR_CHANGE_PENALTY = 590`.
fn heuristic(a: (i32, i32, i32), b: (i32, i32, i32)) -> u64 {
    let dx = (a.0 - b.0).unsigned_abs() as u64;
    let dy = (a.1 - b.1).unsigned_abs() as u64;
    let dz = (a.2 - b.2).unsigned_abs() as u64;
    (dx + dy) * MIN_TILE_COST + dz * (MIN_TILE_COST + FLOOR_CHANGE_PENALTY)
}

/// Computa los vecinos transitables de un tile con sus costs.
///
/// 4-connectivity en el mismo piso + 2 verticales cuando ambos extremos
/// están marcados como transición. Los walls (cost ≥ WALL_THRESHOLD) y los
/// tiles desconocidos (cost = None) se filtran. El cost de un edge incluye
/// `FLOOR_CHANGE_PENALTY` si cruza pisos.
fn successors(
    tile: (i32, i32, i32),
    grid: &WalkabilityGrid,
) -> Vec<((i32, i32, i32), u64)> {
    let mut neighbors: Vec<(i32, i32, i32)> = vec![
        (tile.0, tile.1 - 1, tile.2),
        (tile.0, tile.1 + 1, tile.2),
        (tile.0 - 1, tile.1, tile.2),
        (tile.0 + 1, tile.1, tile.2),
    ];
    if grid.is_transition(tile.0, tile.1, tile.2) {
        let up   = (tile.0, tile.1, tile.2 - 1);
        let down = (tile.0, tile.1, tile.2 + 1);
        if grid.is_transition(up.0, up.1, up.2)     { neighbors.push(up);   }
        if grid.is_transition(down.0, down.1, down.2) { neighbors.push(down); }
    }
    neighbors.into_iter()
        .filter_map(|next| {
            let cost = grid.cost(next.0, next.1, next.2)?;
            if cost >= WALL_THRESHOLD { return None; }
            let move_cost = if next.2 != tile.2 {
                cost as u64 + FLOOR_CHANGE_PENALTY
            } else {
                cost as u64
            };
            Some((next, move_cost))
        })
        .collect()
}

/// Busca un path entre `start` y `goal` en la grilla. Retorna `None` si
/// no hay path posible o se excede `DEFAULT_MAX_ITERS`.
pub fn find_path(
    start: (i32, i32, i32),
    goal: (i32, i32, i32),
    grid: &WalkabilityGrid,
) -> Option<Path> {
    find_path_with_limit(start, goal, grid, DEFAULT_MAX_ITERS)
}

/// Como [`find_path`] pero con límite de iteraciones configurable.
pub fn find_path_with_limit(
    start: (i32, i32, i32),
    goal: (i32, i32, i32),
    grid: &WalkabilityGrid,
    max_iters: usize,
) -> Option<Path> {
    // Corner case: start == goal.
    if start == goal {
        return Some(Path {
            tiles: vec![start],
            total_cost: 0,
            nodes_expanded: 0,
        });
    }

    // Start y goal deben ser transitables.
    if !grid.is_walkable(start.0, start.1, start.2) {
        return None;
    }
    if !grid.is_walkable(goal.0, goal.1, goal.2) {
        return None;
    }

    // Counter compartido entre el successors callback y el post-process.
    // `Cell` permite mutación interior sin exclusive borrow — necesario porque
    // el callback se pasa como `FnMut` pero queremos leer el counter tras
    // la llamada a astar.
    let nodes_expanded = Cell::new(0usize);
    let aborted        = Cell::new(false);

    let result = pathfinding::directed::astar::astar(
        &start,
        |&tile| {
            // Si ya abortamos por max_iters, devolver vacío → astar termina None.
            if aborted.get() {
                return Vec::new();
            }
            nodes_expanded.set(nodes_expanded.get() + 1);
            if nodes_expanded.get() >= max_iters {
                aborted.set(true);
                return Vec::new();
            }
            successors(tile, grid)
        },
        |&tile| heuristic(tile, goal),
        |&tile| tile == goal,
    );

    if aborted.get() {
        return None;
    }

    result.map(|(tiles, total_cost)| Path {
        tiles,
        total_cost,
        nodes_expanded: nodes_expanded.get(),
    })
}

/// Reduce un path contiguo a sus corners (cambios de dirección en x, y o z).
///
/// Ejemplo:
/// ```text
/// Input:  [(10,10,7), (10,11,7), (10,12,7), (10,13,7), (11,13,7), (12,13,7)]
/// Output: [(10,10,7),                       (10,13,7),            (12,13,7)]
/// ```
///
/// Cambios de piso (Z) también se emiten como corners para asegurar que el
/// cavebot no "saltée" un stair/ramp. El start y goal siempre se preservan.
/// Útil para convertir un path denso en una lista compacta de waypoints
/// para cavebot `node` steps.
pub fn simplify_path(path: &[(i32, i32, i32)]) -> Vec<(i32, i32, i32)> {
    if path.len() <= 2 {
        return path.to_vec();
    }

    let mut out = Vec::with_capacity(path.len() / 4 + 2);
    out.push(path[0]);

    let mut prev_dir: Option<(i32, i32, i32)> = None;
    for i in 1..path.len() {
        let curr = path[i];
        let prev = path[i - 1];
        let dir = (
            (curr.0 - prev.0).signum(),
            (curr.1 - prev.1).signum(),
            (curr.2 - prev.2).signum(),
        );

        match prev_dir {
            Some(pd) if pd == dir => {
                // mismo sentido: no emit corner
            }
            _ => {
                // cambio de dirección: emit el tile previo como corner
                // (excepto si ya es el start)
                if i > 1 && out.last() != Some(&prev) {
                    out.push(prev);
                }
            }
        }
        prev_dir = Some(dir);
    }

    // Siempre incluir el último tile.
    let last = *path.last().unwrap();
    if out.last() != Some(&last) {
        out.push(last);
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathfinding::walkability::WalkabilityGrid;

    /// Crea una grilla 10×10 todo walkable en z=7.
    fn open_grid(size: i32) -> WalkabilityGrid {
        let mut g = WalkabilityGrid::new();
        for x in 0..size {
            for y in 0..size {
                g.set_tile(x, y, 7, 100);
            }
        }
        g
    }

    #[test]
    fn same_start_and_goal_returns_single_tile() {
        let g = open_grid(5);
        let p = find_path((2, 2, 7), (2, 2, 7), &g).unwrap();
        assert_eq!(p.tiles, vec![(2, 2, 7)]);
        assert_eq!(p.total_cost, 0);
    }

    #[test]
    fn straight_line_path_in_open_grid() {
        let g = open_grid(10);
        let p = find_path((0, 0, 7), (0, 5, 7), &g).unwrap();
        assert_eq!(p.tiles.first(), Some(&(0, 0, 7)));
        assert_eq!(p.tiles.last(), Some(&(0, 5, 7)));
        // Manhattan distance = 5, cada step cost 100 → total 500.
        assert_eq!(p.total_cost, 500);
        assert_eq!(p.tiles.len(), 6);
    }

    #[test]
    fn diagonal_path_uses_manhattan_steps() {
        let g = open_grid(10);
        let p = find_path((0, 0, 7), (3, 3, 7), &g).unwrap();
        // 4-connected: 6 steps min (3 derecha + 3 abajo).
        assert_eq!(p.tiles.len(), 7);
        assert_eq!(p.total_cost, 600);
    }

    #[test]
    fn wall_forces_detour() {
        let mut g = open_grid(10);
        // Wall vertical en x=5, y=0..8 (dejando un gap en y=8,9).
        for y in 0..8 {
            g.set_tile(5, y, 7, 255);
        }
        let p = find_path((0, 0, 7), (9, 0, 7), &g).unwrap();
        // Debe rodear por abajo: 9 steps derecha + 8 abajo + 8 arriba = 25
        // como mínimo. Solo verificamos que llega y no pasa por el wall.
        assert_eq!(p.tiles.first(), Some(&(0, 0, 7)));
        assert_eq!(p.tiles.last(), Some(&(9, 0, 7)));
        for tile in &p.tiles {
            if tile.0 == 5 && tile.1 < 8 {
                panic!("path crosses wall at {:?}", tile);
            }
        }
    }

    #[test]
    fn unreachable_goal_returns_none() {
        let mut g = open_grid(5);
        // Cerramos el goal con walls por todos lados.
        g.set_tile(4, 3, 7, 255);
        g.set_tile(3, 4, 7, 255);
        g.set_tile(4, 5, 7, 255);
        g.set_tile(5, 4, 7, 255);
        // Goal (4,4) está rodeado.
        let p = find_path((0, 0, 7), (4, 4, 7), &g);
        // Goal (4,4) es walkable pero inalcanzable → find_path la encuentra
        // como su propio "islote" de 1 tile → None.
        // En una grilla 5×5 donde se cierran los 4 vecinos, el algoritmo
        // explora todo y no lo encuentra.
        assert!(p.is_none());
    }

    #[test]
    fn start_not_walkable_returns_none() {
        let mut g = open_grid(5);
        g.set_tile(0, 0, 7, 255);
        let p = find_path((0, 0, 7), (3, 3, 7), &g);
        assert!(p.is_none());
    }

    #[test]
    fn goal_on_nonexistent_floor_returns_none() {
        let g = open_grid(5);
        // z=8 tiles no existen → goal no walkable → None.
        let p = find_path((0, 0, 7), (2, 2, 8), &g);
        assert!(p.is_none());
    }

    #[test]
    fn different_floor_without_transition_returns_none() {
        let mut g = WalkabilityGrid::new();
        // Ambos pisos totalmente walkable pero SIN transiciones marcadas.
        for x in 0..5 {
            for y in 0..5 {
                g.set_tile(x, y, 7, 100);
                g.set_tile(x, y, 6, 100);
            }
        }
        // No detect_transitions ni add_transition.
        let p = find_path((0, 0, 7), (4, 4, 6), &g);
        assert!(p.is_none());
    }

    #[test]
    fn multi_floor_path_via_single_stair() {
        let mut g = WalkabilityGrid::new();
        for x in 0..5 {
            for y in 0..5 {
                g.set_tile(x, y, 7, 100);
                g.set_tile(x, y, 6, 100);
            }
        }
        // Solo (2,2) está marcado como transición (stair aislado).
        g.add_transition(2, 2, 7);
        g.add_transition(2, 2, 6);

        let p = find_path((0, 0, 7), (4, 4, 6), &g).unwrap();
        assert_eq!(p.tiles.first(), Some(&(0, 0, 7)));
        assert_eq!(p.tiles.last(), Some(&(4, 4, 6)));
        // Debe pasar por (2,2,7) → (2,2,6)
        assert!(p.tiles.contains(&(2, 2, 7)));
        assert!(p.tiles.contains(&(2, 2, 6)));
        // Total cost incluye FLOOR_CHANGE_PENALTY
        assert!(p.total_cost >= FLOOR_CHANGE_PENALTY);
    }

    #[test]
    fn multi_floor_with_auto_detect_transitions() {
        let mut g = WalkabilityGrid::new();
        for x in 0..3 {
            for y in 0..3 {
                g.set_tile(x, y, 7, 100);
                g.set_tile(x, y, 6, 100);
            }
        }
        // Auto-detect: cada par (x,y,7)+(x,y,6) walkable → transición.
        let n = g.detect_transitions();
        assert_eq!(n, 9);

        let p = find_path((0, 0, 7), (2, 2, 6), &g).unwrap();
        assert_eq!(p.tiles.first(), Some(&(0, 0, 7)));
        assert_eq!(p.tiles.last(), Some(&(2, 2, 6)));
    }

    #[test]
    fn floor_penalty_discourages_unnecessary_floor_change() {
        let mut g = WalkabilityGrid::new();
        // z=7: path directo horizontal (0,0)→(5,0) en 6 tiles (500 cost).
        // z=6: también walkable + transición al comienzo y fin.
        for x in 0..=5 {
            g.set_tile(x, 0, 7, 100);
            g.set_tile(x, 0, 6, 100);
        }
        g.add_transition(0, 0, 7);
        g.add_transition(0, 0, 6);
        g.add_transition(5, 0, 7);
        g.add_transition(5, 0, 6);

        let p = find_path((0, 0, 7), (5, 0, 7), &g).unwrap();
        // Debe preferir el path horizontal (5*100 = 500) sobre
        // bajar/subir (2 * FLOOR_CHANGE_PENALTY + 5*100 = 1500).
        assert_eq!(p.total_cost, 500);
        // Path no cambia de piso.
        for tile in &p.tiles {
            assert_eq!(tile.2, 7);
        }
    }

    #[test]
    fn floor_penalty_accepted_when_necessary() {
        let mut g = WalkabilityGrid::new();
        // z=7: wall bloquea el path horizontal.
        g.set_tile(0, 0, 7, 100);
        g.set_tile(1, 0, 7, 255); // wall
        g.set_tile(2, 0, 7, 100);
        // z=6: libre por debajo.
        g.set_tile(0, 0, 6, 100);
        g.set_tile(1, 0, 6, 100);
        g.set_tile(2, 0, 6, 100);

        g.add_transition(0, 0, 7);
        g.add_transition(0, 0, 6);
        g.add_transition(2, 0, 7);
        g.add_transition(2, 0, 6);

        let p = find_path((0, 0, 7), (2, 0, 7), &g).unwrap();
        // Debe bajar a z=6 y subir: (0,0,7)→(0,0,6)→(1,0,6)→(2,0,6)→(2,0,7)
        assert_eq!(p.tiles.first(), Some(&(0, 0, 7)));
        assert_eq!(p.tiles.last(), Some(&(2, 0, 7)));
        assert!(p.tiles.contains(&(0, 0, 6)));
        assert!(p.tiles.contains(&(2, 0, 6)));
    }

    #[test]
    fn floor_transition_requires_both_endpoints_marked() {
        let mut g = WalkabilityGrid::new();
        for x in 0..3 {
            for y in 0..3 {
                g.set_tile(x, y, 7, 100);
                g.set_tile(x, y, 6, 100);
            }
        }
        // Solo (1,1,7) marcado, NO (1,1,6).
        g.add_transition(1, 1, 7);

        let p = find_path((0, 0, 7), (2, 2, 6), &g);
        // No debería encontrar path porque la transición no está completa.
        assert!(p.is_none());
    }

    #[test]
    fn simplify_detects_z_changes_as_corners() {
        let path = vec![
            (0, 0, 7),
            (0, 1, 7),
            (0, 2, 7),
            (0, 2, 6), // cambio de piso
            (0, 3, 6),
            (0, 4, 6),
        ];
        let s = simplify_path(&path);
        // Start, corner pre-Z, corner post-Z, goal.
        assert_eq!(s, vec![(0, 0, 7), (0, 2, 7), (0, 2, 6), (0, 4, 6)]);
    }

    #[test]
    fn prefers_cheaper_tiles() {
        let mut g = WalkabilityGrid::new();
        // Path A: directo por tiles cost=213 (slow).
        // Path B: desviación por tiles cost=90 (fast).
        // 3×3 grid:
        //   (0,0) start cheap
        //   (0,1) cheap
        //   (0,2) cheap
        //   (1,0) slow     (1,1) slow  (1,2) cheap
        //   (2,0) goal
        // Direct: (0,0)->(1,0)->(2,0) = 213+213 = 426
        // Indirect: (0,0)->(0,1)->(0,2)->(1,2)->(2,2)->(2,1)->(2,0) = 90*6 = 540
        // Direct wins en este caso. Simplificamos:
        for y in 0..3 {
            g.set_tile(0, y, 7, 90);
            g.set_tile(2, y, 7, 90);
        }
        // columna central slow.
        g.set_tile(1, 0, 7, 213);
        g.set_tile(1, 1, 7, 213);
        g.set_tile(1, 2, 7, 213);

        let p = find_path((0, 0, 7), (2, 0, 7), &g).unwrap();
        // El path puede ir (0,0)->(1,0)->(2,0): 213+90 = 303
        // o         (0,0)->(0,1)->(1,1)->(2,1)->(2,0): 90+213+90+90 = 483
        // A* debe elegir el primero.
        assert_eq!(p.total_cost, 303);
    }

    #[test]
    fn simplify_straight_line() {
        let path = vec![(0, 0, 7), (0, 1, 7), (0, 2, 7), (0, 3, 7)];
        let s = simplify_path(&path);
        assert_eq!(s, vec![(0, 0, 7), (0, 3, 7)]);
    }

    #[test]
    fn simplify_l_shape() {
        let path = vec![
            (0, 0, 7),
            (0, 1, 7),
            (0, 2, 7),
            (0, 3, 7),
            (1, 3, 7),
            (2, 3, 7),
        ];
        let s = simplify_path(&path);
        assert_eq!(s, vec![(0, 0, 7), (0, 3, 7), (2, 3, 7)]);
    }

    #[test]
    fn simplify_zigzag() {
        let path = vec![
            (0, 0, 7),
            (1, 0, 7),
            (1, 1, 7),
            (2, 1, 7),
            (2, 2, 7),
        ];
        let s = simplify_path(&path);
        // Cada tile es un corner.
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn simplify_short_path_unchanged() {
        let p1 = vec![(0, 0, 7)];
        assert_eq!(simplify_path(&p1), p1);

        let p2 = vec![(0, 0, 7), (0, 1, 7)];
        assert_eq!(simplify_path(&p2), p2);
    }

    #[test]
    fn max_iters_limit_enforced() {
        let g = open_grid(100);
        // Con límite de 5 iters no puede llegar de (0,0) a (99,99).
        let p = find_path_with_limit((0, 0, 7), (99, 99, 7), &g, 5);
        assert!(p.is_none());
    }
}
