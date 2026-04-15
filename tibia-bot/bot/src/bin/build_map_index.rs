/// build_map_index — Genera el índice de tile-hashes para posicionamiento absoluto.
///
/// Lee los archivos Minimap_Color_*.png del directorio de minimapas de Tibia
/// y construye un HashMap<dHash, Vec<MapPos>> serializado con bincode.
///
/// Opcionalmente también construye la grilla de walkability leyendo los
/// `Minimap_WaypointCost_*.png` del mismo directorio.
///
/// Uso:
///   cargo run --release --bin build_map_index -- \
///       --map-dir "C:/Users/.../Tibia/minimap" \
///       --output assets/map_index.bin \
///       --walkability assets/walkability.bin \
///       --floors 6,7,8

use std::path::PathBuf;
use std::time::Instant;

use tibia_bot::pathfinding::WalkabilityGrid;
use tibia_bot::sense::vision::game_coords::{self, MapIndex};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let map_dir = args.iter()
        .position(|a| a == "--map-dir")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .expect("uso: --map-dir <path>");

    let output = args.iter()
        .position(|a| a == "--output")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/map_index.bin"));

    let walkability_output = args.iter()
        .position(|a| a == "--walkability")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

    let floors: Vec<i32> = args.iter()
        .position(|a| a == "--floors")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.split(',').filter_map(|f| f.trim().parse().ok()).collect())
        .unwrap_or_default(); // vacío = todos

    println!("Map dir:  {}", map_dir.display());
    println!("Output:   {}", output.display());
    println!("Floors:   {}", if floors.is_empty() { "all".to_string() } else { format!("{:?}", floors) });

    let start = Instant::now();
    let mut index = MapIndex::new();
    let mut files_processed = 0u32;

    for entry in std::fs::read_dir(&map_dir)? {
        let entry = entry?;
        let filename = entry.file_name();
        let filename_str = filename.to_string_lossy();

        let Some((fx, fy, fz)) = game_coords::parse_minimap_filename(&filename_str) else {
            continue;
        };

        if !floors.is_empty() && !floors.contains(&fz) {
            continue;
        }

        match game_coords::index_minimap_png(&entry.path(), fx, fy, fz, &mut index) {
            Ok(count) => {
                files_processed += 1;
                if files_processed % 100 == 0 {
                    print!("\rProcessed {} files, {} patches...", files_processed, index.total_patches);
                }
                let _ = count;
            }
            Err(e) => {
                eprintln!("Warning: {}: {}", entry.path().display(), e);
            }
        }
    }

    println!("\rDone: {} files, {} patches in {:.1}s",
        files_processed, index.total_patches, start.elapsed().as_secs_f64());

    index.save(&output)?;
    let file_size = std::fs::metadata(&output)?.len();
    println!("Saved: {} ({:.1} MB)", output.display(), file_size as f64 / 1_048_576.0);

    if let Some(walk_path) = walkability_output {
        println!("\nBuilding walkability grid from Minimap_WaypointCost_*.png...");
        let walk_start = Instant::now();
        let grid = WalkabilityGrid::load_from_dir(&map_dir, &floors)?;
        println!(
            "Walkability: {} tiles from {} files in {:.1}s",
            grid.len(),
            grid.files_loaded,
            walk_start.elapsed().as_secs_f64()
        );
        grid.save(&walk_path)?;
        let walk_size = std::fs::metadata(&walk_path)?.len();
        println!(
            "Saved: {} ({:.1} MB)",
            walk_path.display(),
            walk_size as f64 / 1_048_576.0
        );
    }

    Ok(())
}
