//! debug_game_coords — Prueba múltiples scales de downsampling contra el
//! map_index para encontrar empíricamente cuál es el correcto para el minimap
//! NDI del usuario.
//!
//! Uso:
//!   # Primero obtener un frame del bot corriendo:
//!   curl http://localhost:8080/test/grab -o frame.png
//!
//!   # Después correr este debug:
//!   cargo run --release --bin debug_game_coords -- \
//!       --frame frame.png \
//!       --index assets/map_index.bin \
//!       --minimap-roi 1753,4,107,110
//!
//! Output:
//!   Para cada scale de 2 a 8:
//!     - Hash computado
//!     - Si hay match exacto o fuzzy en el index (con posición)
//!
//! Esto permite encontrar empíricamente el ndi_tile_scale correcto sin
//! restart del bot ni guessing.

use std::path::PathBuf;

use tibia_bot::sense::vision::game_coords::{self, MapIndex, PATCH_TILES};
use tibia_bot::sense::perception::MinimapSnapshot;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let frame_path = arg(&args, "--frame")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("--frame <path.png> requerido"))?;

    let index_path = arg(&args, "--index")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/map_index.bin"));

    let roi_str = arg(&args, "--minimap-roi")
        .ok_or_else(|| anyhow::anyhow!("--minimap-roi x,y,w,h requerido"))?;
    let roi = parse_roi(&roi_str)?;

    println!("Loading frame from {}...", frame_path.display());
    let img = image::open(&frame_path)?.to_rgba8();
    let (fw, fh) = (img.width(), img.height());
    println!("  frame: {}×{}", fw, fh);

    println!("Loading map_index from {}...", index_path.display());
    let index = MapIndex::load(&index_path)?;
    println!("  {} patches loaded", index.total_patches);
    println!();

    // Extraer minimap del frame.
    let (rx, ry, rw, rh) = roi;
    if rx + rw > fw || ry + rh > fh {
        anyhow::bail!("ROI out of bounds");
    }

    let mut minimap_data = Vec::with_capacity((rw * rh * 4) as usize);
    for y in 0..rh {
        for x in 0..rw {
            let px = img.get_pixel(rx + x, ry + y);
            minimap_data.extend_from_slice(&px.0);
        }
    }
    let snap = MinimapSnapshot {
        width: rw,
        height: rh,
        data: minimap_data,
    };
    println!("Minimap extracted: {}×{}", rw, rh);
    println!();

    // Probar cada scale de 2 a 8.
    println!("=== Scale sweep ===");
    println!("PATCH_TILES = {}", PATCH_TILES);
    println!();

    for scale in 1..=8u32 {
        let patch_px = PATCH_TILES * scale as usize;
        print!("scale={} (patch={}×{}): ", scale, patch_px, patch_px);

        if rw < patch_px as u32 || rh < patch_px as u32 {
            println!("SKIP (minimap too small: {}×{} < {}×{})", rw, rh, patch_px, patch_px);
            continue;
        }

        match game_coords::detect_position(&snap, &index, scale) {
            Some((x, y, z)) => {
                println!("✓ MATCH! pos=({}, {}, {})", x, y, z);
            }
            None => {
                println!("no match");
            }
        }
    }

    println!();
    println!("=== Hash inspection (scale=5, corner 0,0) ===");
    // Para debug adicional, mostrar el hash exacto computado y buscar sus vecinos
    // hamming en el index.
    let scale = 5u32;
    let patch_px = PATCH_TILES * scale as usize;
    if rw >= patch_px as u32 && rh >= patch_px as u32 {
        // Reuse internal logic by calling the public dhash.
        // Extract the patch manually (duplicating extract_and_downsample logic).
        let mut downsampled = vec![0u8; PATCH_TILES * PATCH_TILES * 4];
        let stride_bytes = rw as usize * 4;
        let n = (scale * scale) as u32;
        for j in 0..PATCH_TILES {
            for i in 0..PATCH_TILES {
                let mut sum = [0u32; 4];
                for dy in 0..scale as usize {
                    for dx in 0..scale as usize {
                        let sx = i * scale as usize + dx;
                        let sy = j * scale as usize + dy;
                        let src_off = sy * stride_bytes + sx * 4;
                        for c in 0..4 {
                            sum[c] += snap.data[src_off + c] as u32;
                        }
                    }
                }
                let dst_off = (j * PATCH_TILES + i) * 4;
                for c in 0..4 {
                    downsampled[dst_off + c] = (sum[c] / n) as u8;
                }
            }
        }

        let hash = game_coords::dhash(&downsampled, PATCH_TILES, PATCH_TILES);
        println!("Computed hash (scale=5, corner 0,0): 0x{:016X}", hash);
        println!("Exact matches: {}", index.lookup_exact(hash).len());

        // Scan ALL index entries to find min hamming distance.
        println!();
        println!("=== Minimum hamming scan across all index entries ===");
        println!("(warning: O(N) over {} hashes, may take a few seconds)", index.total_patches);
        let entries = index.all_entries();
        let mut min_dist = u32::MAX;
        let mut min_pos: Option<game_coords::MapPos> = None;
        let mut count_by_dist = [0u32; 65];
        for (h, positions) in entries.iter() {
            let d = game_coords::hamming(hash, *h);
            count_by_dist[d as usize] += positions.len() as u32;
            if d < min_dist {
                min_dist = d;
                min_pos = positions.first().copied();
            }
        }
        println!("Min hamming distance: {}", min_dist);
        if let Some(p) = min_pos {
            println!("Best candidate: ({}, {}, {})", p.x, p.y, p.z);
        }
        println!("Distribution of hamming distances (first 20 bins):");
        for d in 0..20 {
            if count_by_dist[d] > 0 {
                println!("  d={}: {} patches", d, count_by_dist[d]);
            }
        }

        // Try same with different scales too
        println!();
        println!("=== Min hamming for different scales ===");
        // Test different inner positions at scale=1 to avoid edge noise
        println!();
        println!("=== Inner positions at scale=1 ===");
        for (off_x, off_y) in [(0, 0), (5, 5), (10, 10), (15, 15), (20, 20), (30, 30),
                                 (40, 40), (50, 50), (5, 0), (0, 5), (20, 10), (10, 20)] {
            if off_x + 16 > rw as usize || off_y + 16 > rh as usize {
                continue;
            }
            let mut p = vec![0u8; 16 * 16 * 4];
            for j in 0..16 {
                for i in 0..16 {
                    let sx = off_x + i;
                    let sy = off_y + j;
                    let src = sy * stride_bytes + sx * 4;
                    let dst = (j * 16 + i) * 4;
                    p[dst..dst+4].copy_from_slice(&snap.data[src..src+4]);
                }
            }
            let h = game_coords::dhash(&p, 16, 16);
            let mut mind = u32::MAX;
            let mut minpos: Option<game_coords::MapPos> = None;
            for (eh, positions) in entries.iter() {
                let d = game_coords::hamming(h, *eh);
                if d < mind {
                    mind = d;
                    minpos = positions.first().copied();
                }
            }
            println!("  ({},{}): hash=0x{:016X}, min_d={}, best={:?}",
                off_x, off_y, h, mind,
                minpos.map(|p| (p.x, p.y, p.z)));
        }

        for test_scale in 1..=7u32 {
            let patch_px = PATCH_TILES * test_scale as usize;
            if rw < patch_px as u32 || rh < patch_px as u32 {
                continue;
            }
            let scale_u = test_scale as usize;
            let mut ds = vec![0u8; PATCH_TILES * PATCH_TILES * 4];
            let n2 = (test_scale * test_scale) as u32;
            for j in 0..PATCH_TILES {
                for i in 0..PATCH_TILES {
                    let mut sum = [0u32; 4];
                    for dy in 0..scale_u {
                        for dx in 0..scale_u {
                            let sx = i * scale_u + dx;
                            let sy = j * scale_u + dy;
                            let src_off = sy * stride_bytes + sx * 4;
                            for c in 0..4 {
                                sum[c] += snap.data[src_off + c] as u32;
                            }
                        }
                    }
                    let dst_off = (j * PATCH_TILES + i) * 4;
                    for c in 0..4 {
                        ds[dst_off + c] = (sum[c] / n2) as u8;
                    }
                }
            }
            let h_scale = game_coords::dhash(&ds, PATCH_TILES, PATCH_TILES);
            let mut min_d = u32::MAX;
            for (h, _) in entries.iter() {
                let d = game_coords::hamming(h_scale, *h);
                if d < min_d { min_d = d; }
            }
            println!("  scale={} → hash=0x{:016X}, min_hamming={}", test_scale, h_scale, min_d);
        }
    }

    Ok(())
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn parse_roi(s: &str) -> anyhow::Result<(u32, u32, u32, u32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        anyhow::bail!("--minimap-roi requires x,y,w,h");
    }
    Ok((parts[0].parse()?, parts[1].parse()?, parts[2].parse()?, parts[3].parse()?))
}
