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
//! ```

use std::path::PathBuf;

use tibia_bot::pathfinding::{find_path, simplify_path, Overrides, WalkabilityGrid};

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

    if !grid.is_walkable(from.0, from.1, from.2) {
        anyhow::bail!(
            "start {:?} no es walkable (cost={:?})",
            from,
            grid.cost(from.0, from.1, from.2)
        );
    }
    if !grid.is_walkable(to.0, to.1, to.2) {
        anyhow::bail!(
            "goal {:?} no es walkable (cost={:?})",
            to,
            grid.cost(to.0, to.1, to.2)
        );
    }

    let start = std::time::Instant::now();
    let path = find_path(from, to, &grid)
        .ok_or_else(|| anyhow::anyhow!("no hay path posible entre {:?} y {:?}", from, to))?;
    let elapsed = start.elapsed();

    let floor_changes = count_floor_changes(&path.tiles);
    println!(
        "Path found: {} tiles, cost {}, nodes expanded {}, {} floor changes, in {:?}",
        path.tiles.len(),
        path.total_cost,
        path.nodes_expanded,
        floor_changes,
        elapsed
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

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
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
}
