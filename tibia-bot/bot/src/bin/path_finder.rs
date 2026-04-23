//! path_finder — CLI para calcular rutas A* multi-floor entre 2 coords absolutas.
//!
//! Lee un `walkability.bin` pre-generado y calcula el path más corto entre
//! dos coordenadas de tile. Soporta pathfinding multi-floor automáticamente
//! (stairs/ramps auto-detectados). Puede imprimir el path, simplificarlo
//! a corners, o exportarlo como snippet de cavebot script.
//!
//! ## Uso
//!
//! ```bash
//! # Path simple same-floor
//! cargo run --release --bin path_finder -- \
//!     --walkability assets/walkability.bin \
//!     --from 32015,32212,7 \
//!     --to   32100,32300,7
//!
//! # Multi-floor con overrides (rope/hole manuales)
//! cargo run --release --bin path_finder -- \
//!     --walkability assets/walkability.bin \
//!     --overrides assets/pathfinding_overrides.toml \
//!     --from 32015,32212,7 \
//!     --to   32200,32400,6
//!
//! # Simplificado con export
//! cargo run --release --bin path_finder -- \
//!     --walkability assets/walkability.bin \
//!     --from 32015,32212,7 \
//!     --to   32100,32300,7 \
//!     --simplify \
//!     --output path_snippet.toml
//!
//! # Path largo con via waypoints (4 hunts failing — el user corta
//! # manualmente un path largo en 2-3 segmentos cortos que A* resuelve)
//! cargo run --release --bin path_finder -- \
//!     --walkability assets/walkability.bin \
//!     --from 32015,32212,7 \
//!     --via  32200,32300,7 \
//!     --via  32350,32380,7 \
//!     --to   32500,32500,7 \
//!     --simplify --output orcs_path.toml
//!
//! # Path con límite de iters custom (default 100k; subir a 1M para
//! # distancias largas en terreno complejo sin segmentar)
//! cargo run --release --bin path_finder -- \
//!     --walkability assets/walkability.bin \
//!     --from 32015,32212,7 \
//!     --to   32500,32500,7 \
//!     --max-iters 1000000
//! ```

use std::path::PathBuf;

use tibia_bot::pathfinding::{
    find_path_with_limit, simplify_path, DEFAULT_MAX_ITERS, Overrides, WalkabilityGrid,
};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let walkability = arg(&args, "--walkability")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("--walkability <path.bin> requerido"))?;

    let from = arg(&args, "--from")
        .and_then(|s| parse_coord(&s))
        .ok_or_else(|| anyhow::anyhow!("--from X,Y,Z requerido"))?;

    let to = arg(&args, "--to")
        .and_then(|s| parse_coord(&s))
        .ok_or_else(|| anyhow::anyhow!("--to X,Y,Z requerido"))?;

    let simplify = args.iter().any(|a| a == "--simplify");
    let output = arg(&args, "--output").map(PathBuf::from);
    let overrides_path = arg(&args, "--overrides").map(PathBuf::from);
    let nearest_walkable_radius: i32 = arg(&args, "--nearest-walkable")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let max_iters: usize = arg(&args, "--max-iters")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_ITERS);
    // --via X,Y,Z repeatable: segmenta el path en tramos from→via1→via2→…→to.
    // Ayuda para distancias largas donde A* monolítico excede max_iters.
    let vias: Vec<Tile> = collect_args(&args, "--via")
        .iter()
        .filter_map(|s| parse_coord(s))
        .collect();

    println!("Loading walkability from {}...", walkability.display());
    let mut grid = WalkabilityGrid::load(&walkability)?;
    println!(
        "Loaded {} tiles, {} transitions, from {} files",
        grid.len(),
        grid.transitions_count(),
        grid.files_loaded
    );

    if let Some(ref op) = overrides_path {
        let overrides = Overrides::load(op)?;
        let (added, removed) = overrides.apply(&mut grid);
        println!(
            "Applied overrides from {}: +{} transitions, -{} transitions",
            op.display(),
            added,
            removed
        );
    }
    println!();

    let from = remap_if_needed(&grid, from, "start", nearest_walkable_radius)?;
    let to = remap_if_needed(&grid, to, "goal", nearest_walkable_radius)?;

    // Remap vias (puede que el user pase una coord 1 tile off). Usa el mismo
    // radio que start/goal para coherencia.
    let mut waypoints: Vec<Tile> = Vec::with_capacity(2 + vias.len());
    waypoints.push(from);
    for (i, via) in vias.iter().enumerate() {
        let remapped = remap_if_needed(&grid, *via, &format!("via[{}]", i), nearest_walkable_radius)?;
        waypoints.push(remapped);
    }
    waypoints.push(to);

    if !vias.is_empty() {
        println!("Segmented path: {} segment(s) via {} waypoint(s)",
                 waypoints.len() - 1, vias.len());
    }

    let start = std::time::Instant::now();
    let path = find_path_segmented(&waypoints, &grid, max_iters)?;
    let elapsed = start.elapsed();

    let floor_changes = count_floor_changes(&path.tiles);
    println!(
        "Path found: {} tiles, cost {}, nodes expanded {}, {} floor changes, in {:?} (max_iters={})",
        path.tiles.len(),
        path.total_cost,
        path.nodes_expanded,
        floor_changes,
        elapsed,
        max_iters
    );

    if floor_changes > 0 {
        println!("Floor transitions:");
        for (from_t, to_t) in find_floor_transitions(&path.tiles) {
            let kind = if to_t.2 < from_t.2 { "up" } else { "down" };
            println!(
                "  ({}, {}, {}) → ({}, {}, {}) [{}]",
                from_t.0, from_t.1, from_t.2, to_t.0, to_t.1, to_t.2, kind
            );
        }
    }

    let final_path = if simplify {
        let s = simplify_path(&path.tiles);
        println!("Simplified to {} waypoints.", s.len());
        s
    } else {
        path.tiles
    };

    if let Some(out_path) = output {
        let snippet = render_toml_snippet(&final_path);
        std::fs::write(&out_path, &snippet)?;
        println!("\nWrote cavebot snippet to {}", out_path.display());
    } else {
        println!("\nPath:");
        for (i, tile) in final_path.iter().enumerate() {
            println!("  [{:3}] ({}, {}, {})", i, tile.0, tile.1, tile.2);
        }
        println!("\n─── As cavebot snippet ───");
        println!("{}", render_toml_snippet(&final_path));
    }

    Ok(())
}

/// Valida que `coord` sea walkable. Si no lo es y `radius > 0`, busca el tile
/// walkable más cercano en un cubo de lado `2*radius+1` centrado en `coord`
/// (distancia Chebyshev creciente) y loguea el remap. Si `radius == 0` o no
/// encuentra walkable, bail out con el mismo mensaje que antes.
fn remap_if_needed(
    grid: &WalkabilityGrid,
    coord: (i32, i32, i32),
    label: &str,
    radius: i32,
) -> anyhow::Result<(i32, i32, i32)> {
    if grid.is_walkable(coord.0, coord.1, coord.2) {
        return Ok(coord);
    }
    if radius <= 0 {
        anyhow::bail!(
            "{} {:?} no es walkable (cost={:?})",
            label,
            coord,
            grid.cost(coord.0, coord.1, coord.2)
        );
    }
    match nearest_walkable(grid, coord, radius) {
        Some(remapped) => {
            let d = chebyshev(coord, remapped);
            println!(
                "  {} remapped: {:?} → {:?} (distance={} tiles)",
                label, coord, remapped, d
            );
            Ok(remapped)
        }
        None => anyhow::bail!(
            "{} {:?} no es walkable y no hay tile walkable dentro de radio {} (cost={:?})",
            label,
            coord,
            radius,
            grid.cost(coord.0, coord.1, coord.2)
        ),
    }
}

/// BFS esférico por Chebyshev distance: itera shell por shell (d=1,2,…,radius),
/// devuelve el **primer** tile walkable encontrado en la shell actual.
///
/// **Orden de iteración** (determinista, importa para reproducibilidad):
/// por cada shell `r`, itera `dz` outer → `dy` middle → `dx` inner, todos
/// de `-r` a `+r`. El primer `(dx, dy, dz)` con `|dx|=r ∨ |dy|=r ∨ |dz|=r`
/// (condición de shell) cuyo tile es walkable se devuelve inmediatamente.
///
/// **Semántica Chebyshev, NO Euclidean**: dentro de la misma shell `r`, un
/// tile en la esquina (distancia Chebyshev `r`, Euclidean `r·√3`) tiene la
/// misma prioridad que uno en la cara (Chebyshev `r`, Euclidean `r`). Si
/// necesitás "Euclidean nearest", hay que ordenar candidatos dentro de cada
/// shell por `dx²+dy²+dz²` antes de devolver.
fn nearest_walkable(
    grid: &WalkabilityGrid,
    center: (i32, i32, i32),
    radius: i32,
) -> Option<(i32, i32, i32)> {
    nearest_walkable_with(center, radius, |x, y, z| grid.is_walkable(x, y, z))
}

/// Versión pura de `nearest_walkable` que toma un predicado en vez de un grid.
/// Permite tests unit sin construir `WalkabilityGrid`.
fn nearest_walkable_with<F: Fn(i32, i32, i32) -> bool>(
    center: (i32, i32, i32),
    radius: i32,
    is_walkable: F,
) -> Option<(i32, i32, i32)> {
    for r in 1..=radius {
        for dz in -r..=r {
            for dy in -r..=r {
                for dx in -r..=r {
                    // Solo shell (al menos una dimensión toca el borde)
                    if dx.abs() != r && dy.abs() != r && dz.abs() != r {
                        continue;
                    }
                    let x = center.0 + dx;
                    let y = center.1 + dy;
                    let z = center.2 + dz;
                    if is_walkable(x, y, z) {
                        return Some((x, y, z));
                    }
                }
            }
        }
    }
    None
}

fn chebyshev(a: (i32, i32, i32), b: (i32, i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs()).max((a.2 - b.2).abs())
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Recolecta TODOS los valores de un flag repeatable (ej. `--via a --via b`).
/// Útil para `--via` que puede aparecer múltiples veces. Los pares dangling
/// (flag sin valor subsecuente) se ignoran silenciosamente.
fn collect_args(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a == name {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
            }
        }
    }
    out
}

/// Busca un path segmentado entre múltiples waypoints consecutivos. Corre A*
/// en cada par `(waypoints[i], waypoints[i+1])` con el mismo `max_iters` y
/// concatena los tiles resultantes, deduplicando el tile compartido entre
/// segmentos (goal de uno = start del siguiente).
///
/// Retorna Err si cualquier segmento falla. El mensaje indica qué par fue.
fn find_path_segmented(
    waypoints: &[Tile],
    grid: &WalkabilityGrid,
    max_iters: usize,
) -> anyhow::Result<tibia_bot::pathfinding::astar::Path> {
    use tibia_bot::pathfinding::astar::Path;

    if waypoints.len() < 2 {
        anyhow::bail!("find_path_segmented: necesita al menos 2 waypoints");
    }

    let mut all_tiles: Vec<Tile> = Vec::new();
    let mut total_cost: u64 = 0;
    let mut total_nodes: usize = 0;

    for (i, pair) in waypoints.windows(2).enumerate() {
        let a = pair[0];
        let b = pair[1];
        let seg = find_path_with_limit(a, b, grid, max_iters).ok_or_else(|| {
            anyhow::anyhow!(
                "segmento {} ({:?} → {:?}) sin path (max_iters={})",
                i, a, b, max_iters
            )
        })?;
        total_cost += seg.total_cost;
        total_nodes += seg.nodes_expanded;
        if all_tiles.is_empty() {
            all_tiles.extend(seg.tiles);
        } else {
            // Skip primer tile (duplica el goal del segmento anterior).
            all_tiles.extend(seg.tiles.into_iter().skip(1));
        }
    }

    Ok(Path {
        tiles: all_tiles,
        total_cost,
        nodes_expanded: total_nodes,
    })
}

fn parse_coord(s: &str) -> Option<(i32, i32, i32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let x: i32 = parts[0].trim().parse().ok()?;
    let y: i32 = parts[1].trim().parse().ok()?;
    let z: i32 = parts[2].trim().parse().ok()?;
    Some((x, y, z))
}

type Tile = (i32, i32, i32);
type FloorTransition = (Tile, Tile);

fn count_floor_changes(path: &[Tile]) -> usize {
    path.windows(2).filter(|w| w[0].2 != w[1].2).count()
}

fn find_floor_transitions(path: &[Tile]) -> Vec<FloorTransition> {
    path.windows(2)
        .filter(|w| w[0].2 != w[1].2)
        .map(|w| (w[0], w[1]))
        .collect()
}

fn render_toml_snippet(path: &[(i32, i32, i32)]) -> String {
    let mut out = String::new();
    out.push_str("# Generated by path_finder\n");
    // Inicializamos prev_z con el piso del start para que el PRIMER step
    // emitido detecte correctamente un cambio de piso desde path[0].
    let mut prev_z: Option<i32> = path.first().map(|t| t.2);
    for (i, tile) in path.iter().enumerate().skip(1) {
        // Comentario cuando hay cambio de piso para que el usuario sepa
        // que tiene que haber stair/rope/hole ahí.
        if let Some(pz) = prev_z {
            if pz != tile.2 {
                out.push_str(&format!(
                    "# floor change from z={} to z={} (stair/ramp/rope expected)\n",
                    pz, tile.2
                ));
            }
        }
        out.push_str(&format!(
            "[[step]]\nkind = \"node\"\nx = {}\ny = {}\nz = {}\nmax_wait_ms = 30000\n",
            tile.0, tile.1, tile.2
        ));
        if i < path.len() - 1 {
            out.push('\n');
        }
        prev_z = Some(tile.2);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_coord_valid() {
        assert_eq!(parse_coord("32015,32212,7"), Some((32015, 32212, 7)));
        assert_eq!(parse_coord("-1,-2,3"), Some((-1, -2, 3)));
        assert_eq!(parse_coord(" 10 , 20 , 7 "), Some((10, 20, 7)));
    }

    #[test]
    fn parse_coord_invalid() {
        assert_eq!(parse_coord("32015,32212"), None);
        assert_eq!(parse_coord("abc,def,g"), None);
        assert_eq!(parse_coord(""), None);
    }

    #[test]
    fn render_snippet_contains_steps() {
        let path = vec![(10, 10, 7), (10, 20, 7), (20, 20, 7)];
        let s = render_toml_snippet(&path);
        assert!(s.contains("x = 10"));
        assert!(s.contains("y = 20"));
        assert!(s.contains("x = 20"));
        assert!(s.contains("kind = \"node\""));
    }

    #[test]
    fn count_and_find_floor_changes() {
        let path = vec![
            (0, 0, 7),
            (0, 1, 7),
            (0, 1, 6),
            (0, 2, 6),
            (0, 2, 5),
        ];
        assert_eq!(count_floor_changes(&path), 2);
        let transitions = find_floor_transitions(&path);
        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[0], ((0, 1, 7), (0, 1, 6)));
        assert_eq!(transitions[1], ((0, 2, 6), (0, 2, 5)));
    }

    #[test]
    fn render_snippet_with_floor_change_adds_comment() {
        let path = vec![(10, 10, 7), (10, 10, 6), (10, 11, 6)];
        let s = render_toml_snippet(&path);
        assert!(s.contains("floor change from z=7 to z=6"));
    }

    #[test]
    fn chebyshev_returns_max_axis_delta() {
        assert_eq!(chebyshev((0, 0, 0), (3, 1, 2)), 3);
        assert_eq!(chebyshev((0, 0, 0), (0, 5, 2)), 5);
        assert_eq!(chebyshev((10, 10, 7), (10, 10, 7)), 0);
        assert_eq!(chebyshev((5, 5, 5), (-5, 5, 5)), 10);
    }

    // ─── nearest_walkable_with edge cases ─────────────────────────────

    #[test]
    fn nearest_walkable_radius_zero_returns_none() {
        // radius=0: el loop `1..=0` está vacío, nunca itera.
        let result = nearest_walkable_with((10, 10, 7), 0, |_, _, _| true);
        assert_eq!(result, None);
    }

    #[test]
    fn nearest_walkable_all_unwalkable_returns_none() {
        // Predicado siempre falso: no importa el radius, devuelve None.
        let result = nearest_walkable_with((10, 10, 7), 5, |_, _, _| false);
        assert_eq!(result, None);
    }

    #[test]
    fn nearest_walkable_finds_tile_at_shell_one() {
        // Solo (11, 10, 7) walkable: está a distancia Chebyshev = 1.
        let result = nearest_walkable_with((10, 10, 7), 5, |x, y, z| {
            (x, y, z) == (11, 10, 7)
        });
        let found = result.expect("debería encontrar walkable");
        assert_eq!(chebyshev((10, 10, 7), found), 1);
    }

    #[test]
    fn nearest_walkable_finds_tile_at_radius_boundary() {
        // Walkable solo en (15, 10, 7): distancia Chebyshev = 5, radius = 5.
        let result = nearest_walkable_with((10, 10, 7), 5, |x, y, z| {
            (x, y, z) == (15, 10, 7)
        });
        assert_eq!(result, Some((15, 10, 7)));
    }

    #[test]
    fn nearest_walkable_respects_radius_limit() {
        // Walkable en (16, 10, 7): fuera del radio 5 (distancia = 6).
        let result = nearest_walkable_with((10, 10, 7), 5, |x, y, z| {
            (x, y, z) == (16, 10, 7)
        });
        assert_eq!(result, None);
    }

    #[test]
    fn nearest_walkable_iteration_order_is_dz_dy_dx() {
        // Dos tiles walkable en shell r=1:
        //   A = (10, 10, 6)  → dz=-1, dy=0, dx=0
        //   B = (10, 10, 8)  → dz=+1, dy=0, dx=0
        // El orden dz outer → dy → dx significa dz=-1 se visita ANTES que
        // dz=+1, así que debe devolver A.
        let result = nearest_walkable_with((10, 10, 7), 3, |x, y, z| {
            (x, y, z) == (10, 10, 6) || (x, y, z) == (10, 10, 8)
        });
        assert_eq!(result, Some((10, 10, 6)));
    }

    #[test]
    fn nearest_walkable_prefers_inner_shell_over_outer() {
        // Walkable en shell r=3 (coord (13, 10, 7)) y shell r=1 (coord (11, 10, 7)).
        // Debe devolver el de shell r=1 porque itera shells crecientes.
        let result = nearest_walkable_with((10, 10, 7), 5, |x, y, z| {
            (x, y, z) == (11, 10, 7) || (x, y, z) == (13, 10, 7)
        });
        assert_eq!(result, Some((11, 10, 7)));
    }

    // ─── --via / --max-iters integration ──────────────────────────────

    #[test]
    fn collect_args_returns_all_repeats() {
        let args = vec![
            "bin".into(),
            "--via".into(), "1,2,7".into(),
            "--foo".into(),
            "--via".into(), "3,4,7".into(),
            "--via".into(), "5,6,7".into(),
        ];
        let out = collect_args(&args, "--via");
        assert_eq!(out, vec!["1,2,7", "3,4,7", "5,6,7"]);
    }

    #[test]
    fn collect_args_ignores_dangling_flag() {
        // `--via` al final sin valor no debe panicar ni agregar entry.
        let args = vec!["bin".into(), "--via".into(), "1,2,7".into(), "--via".into()];
        let out = collect_args(&args, "--via");
        assert_eq!(out, vec!["1,2,7"]);
    }

    #[test]
    fn segmented_path_concatenates_without_duplicate_tiles() {
        use tibia_bot::pathfinding::WalkabilityGrid;
        // Grid 10×1 en z=7, todo walkable.
        let mut g = WalkabilityGrid::new();
        for x in 0..10 {
            g.set_tile(x, 0, 7, 100);
        }

        // Path directo 0→9 (sin vias).
        let direct = find_path_segmented(&[(0, 0, 7), (9, 0, 7)], &g, 10_000).unwrap();
        assert_eq!(direct.tiles.len(), 10);
        assert_eq!(direct.tiles.first(), Some(&(0, 0, 7)));
        assert_eq!(direct.tiles.last(), Some(&(9, 0, 7)));

        // Path segmentado 0→4→9 (1 via). Debe tener los mismos 10 tiles sin
        // duplicar (4,0,7).
        let segmented = find_path_segmented(&[(0, 0, 7), (4, 0, 7), (9, 0, 7)], &g, 10_000).unwrap();
        assert_eq!(segmented.tiles.len(), 10, "no debe duplicar tile en seam");
        assert_eq!(segmented.tiles.first(), Some(&(0, 0, 7)));
        assert_eq!(segmented.tiles.last(), Some(&(9, 0, 7)));
        // El via debe aparecer exactamente una vez.
        let via_count = segmented.tiles.iter().filter(|t| **t == (4, 0, 7)).count();
        assert_eq!(via_count, 1);
    }

    #[test]
    fn segmented_path_aggregates_cost_and_nodes() {
        use tibia_bot::pathfinding::WalkabilityGrid;
        let mut g = WalkabilityGrid::new();
        for x in 0..10 {
            g.set_tile(x, 0, 7, 100);
        }

        let direct = find_path_segmented(&[(0, 0, 7), (9, 0, 7)], &g, 10_000).unwrap();
        let segmented = find_path_segmented(&[(0, 0, 7), (4, 0, 7), (9, 0, 7)], &g, 10_000).unwrap();

        // El cost total debe ser igual (misma distancia Manhattan).
        assert_eq!(direct.total_cost, segmented.total_cost);
        // nodes_expanded: segmented >= direct (dos A* separados tienen algo
        // de overhead pero menos expansion por segmento).
        assert!(segmented.nodes_expanded > 0);
    }

    #[test]
    fn segmented_path_fails_if_any_segment_impossible() {
        use tibia_bot::pathfinding::WalkabilityGrid;
        // Grid: (0,0) y (9,0) walkable, pero gap de walls entre (2..5, 0) bloquea
        // la primera mitad. La segunda mitad (5..9) es open.
        let mut g = WalkabilityGrid::new();
        for x in 0..10 {
            g.set_tile(x, 0, 7, 100);
        }
        for x in 2..5 {
            g.set_tile(x, 0, 7, 255); // wall
        }
        // (4, 0, 7) es wall → via inalcanzable.
        let result = find_path_segmented(&[(0, 0, 7), (4, 0, 7), (9, 0, 7)], &g, 10_000);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        // El error debe identificar qué segmento falló.
        assert!(msg.contains("segmento") || msg.contains("sin path"),
            "unexpected error: {}", msg);
    }

    #[test]
    fn nearest_walkable_chebyshev_vs_euclidean_semantic() {
        // Dentro de shell r=2:
        //   A = (12, 10, 7)  → Chebyshev=2, Euclidean=2.0  (en cara)
        //   B = (12, 12, 7)  → Chebyshev=2, Euclidean=~2.83 (en esquina)
        // Con semántica Chebyshev (la actual), devuelve el PRIMERO por orden de
        // iteración, no el Euclidean-más-cercano. Iteración dz=0, dy ∈ [-2,2],
        // dx ∈ [-2,2]: visita dy=-2 primero. (10, 8, 7)? solo si es walkable.
        // Aquí A tiene dy=0 (shell solo si |dx|=2 → sí, dx=+2). B tiene dy=+2.
        // Orden: dy=-2 (no walkable) → dy=-1 (no walkable) → dy=0, dx=-2 (no) →
        // dx=+2 = A (walkable). Devuelve A.
        let result = nearest_walkable_with((10, 10, 7), 5, |x, y, z| {
            (x, y, z) == (12, 10, 7) || (x, y, z) == (12, 12, 7)
        });
        // Cualquiera de los dos es legítimo como Chebyshev=2, pero la impl
        // actual devuelve A por el orden de iteración dy outer.
        assert_eq!(result, Some((12, 10, 7)));
    }
}
