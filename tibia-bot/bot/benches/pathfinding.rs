//! benches/pathfinding.rs — baseline de performance del A* multi-floor.
//!
//! Mide `find_path` en 3 escenarios representativos:
//!
//! 1. `open_grid_100` — 100×100 all-walkable, mismo piso. Path simple
//!    Manhattan. Representa el caso dominante en un hunt rectangular.
//! 2. `maze_50` — 50×50 con walls formando un maze serpenteante. Forzar al
//!    A* a expandir muchos nodos. Representa ciudades con pillars/buildings.
//! 3. `multi_floor_3z` — 40×40 en 3 pisos (z=6,7,8) con transiciones en 4
//!    puntos. Representa hunts con stairs/ropes.
//!
//! ## Ejecución
//!
//! ```bash
//! cargo bench --bench pathfinding -- --save-baseline pathfinding_2026_04_19
//! cargo bench --bench pathfinding -- --baseline pathfinding_2026_04_19
//! ```

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tibia_bot::pathfinding::{find_path, WalkabilityGrid};

fn open_grid(size: i32) -> WalkabilityGrid {
    let mut g = WalkabilityGrid::new();
    for x in 0..size {
        for y in 0..size {
            g.set_tile(x, y, 7, 100);
        }
    }
    g
}

fn maze_50() -> WalkabilityGrid {
    // 50×50 con walls serpentinas. Fuerza detours. Misma técnica que maze
    // generation simple: walls verticales alternando abiertas arriba/abajo.
    let mut g = WalkabilityGrid::new();
    for x in 0..50 {
        for y in 0..50 {
            g.set_tile(x, y, 7, 100);
        }
    }
    // Walls verticales cada 5 columnas, dejando un gap de 1 tile.
    for col in (5i32..50).step_by(5) {
        let gap_row: i32 = if col % 10 == 0 { 0 } else { 49 };
        for row in 0i32..50 {
            if row != gap_row {
                g.set_tile(col, row, 7, 255);
            }
        }
    }
    g
}

fn multi_floor_grid() -> WalkabilityGrid {
    let mut g = WalkabilityGrid::new();
    for z in 6..=8 {
        for x in 0..40 {
            for y in 0..40 {
                g.set_tile(x, y, z, 100);
            }
        }
    }
    // 4 puntos de transición: esquinas (5,5), (5,35), (35,5), (35,35)
    for &(x, y) in &[(5, 5), (5, 35), (35, 5), (35, 35)] {
        for z in 6..=8 {
            g.add_transition(x, y, z);
        }
    }
    g
}

fn bench_open_grid(c: &mut Criterion) {
    let grid = open_grid(100);
    let mut group = c.benchmark_group("astar");
    group.sample_size(20);

    group.bench_function("open_grid_100_corner_to_corner", |b| {
        b.iter(|| {
            let p = find_path(
                black_box((0, 0, 7)),
                black_box((99, 99, 7)),
                black_box(&grid),
            );
            assert!(p.is_some());
        })
    });

    group.bench_function("open_grid_100_diagonal_half", |b| {
        b.iter(|| {
            let p = find_path(
                black_box((10, 10, 7)),
                black_box((50, 50, 7)),
                black_box(&grid),
            );
            assert!(p.is_some());
        })
    });

    group.finish();
}

fn bench_maze(c: &mut Criterion) {
    let grid = maze_50();
    let mut group = c.benchmark_group("astar");
    group.sample_size(20);

    group.bench_function("maze_50_serpentine", |b| {
        b.iter(|| {
            let p = find_path(
                black_box((0, 0, 7)),
                black_box((49, 49, 7)),
                black_box(&grid),
            );
            assert!(p.is_some(), "maze path debería existir");
        })
    });

    group.finish();
}

fn bench_multi_floor(c: &mut Criterion) {
    let grid = multi_floor_grid();
    let mut group = c.benchmark_group("astar");
    group.sample_size(20);

    group.bench_function("multi_floor_3z_cross_floor", |b| {
        b.iter(|| {
            let p = find_path(
                black_box((0, 0, 8)),
                black_box((39, 39, 6)),
                black_box(&grid),
            );
            assert!(p.is_some(), "cross-floor path debería existir");
        })
    });

    group.finish();
}

criterion_group!(benches, bench_open_grid, bench_maze, bench_multi_floor);
criterion_main!(benches);
