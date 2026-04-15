/// find_minimap_ground_truth — Brute force search para encontrar la ubicación
/// real del char comparando el minimap NDI contra TODOS los reference PNGs.
///
/// Usa CCORR_NORMED (robusto a brightness shift) en luma (no RGB) para evitar
/// sensibilidad a color space issues. Retorna el top-5 matches globales
/// ordenados por score.
///
/// Este tool resuelve la pregunta "¿el dHash falla porque el algoritmo no
/// sirve, o porque el char está en un lugar distinto al que creemos?". Si
/// encuentra un match fuerte (score > 0.6), sabemos dónde está el char y
/// podemos usar CCORR como approach alternativo a dHash.
///
/// Uso:
///   cargo run --release --bin find_minimap_ground_truth -- \
///       --frame frame_diag.png \
///       --minimap-roi 1753,4,107,110 \
///       --map-dir assets/minimap/minimap \
///       --floors 6,7,8
///
/// Tiempo estimado: ~10-30 seg (paralelizado con rayon).

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use image::GrayImage;
use imageproc::template_matching::{match_template_parallel, MatchTemplateMethod};

use tibia_bot::sense::vision::game_coords::parse_minimap_filename;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let frame_path = arg(&args, "--frame").context("--frame requerido")?;
    let roi_str = arg(&args, "--minimap-roi").context("--minimap-roi requerido")?;
    let map_dir_str = arg(&args, "--map-dir").context("--map-dir requerido")?;
    let floors_str = arg(&args, "--floors").unwrap_or_else(|| "".to_string());
    let scale: u32 = arg(&args, "--scale")
        .unwrap_or_else(|| "1".to_string())
        .parse()
        .context("--scale debe ser entero")?;

    let (rx, ry, rw, rh) = parse_roi(&roi_str)?;
    let map_dir = PathBuf::from(&map_dir_str);
    let floors: Vec<i32> = if floors_str.is_empty() {
        Vec::new()
    } else {
        floors_str.split(',').filter_map(|s| s.trim().parse().ok()).collect()
    };

    println!("Frame:    {}", frame_path);
    println!("ROI:      ({}, {}) {}×{}", rx, ry, rw, rh);
    println!("Map dir:  {}", map_dir.display());
    println!("Floors:   {}", if floors.is_empty() { "all".to_string() } else { format!("{:?}", floors) });
    println!();

    // 1. Cargar frame y extraer minimap como GrayImage.
    let frame = image::open(&frame_path)
        .with_context(|| format!("abrir frame '{}'", frame_path))?
        .to_rgba8();
    let (fw, fh) = frame.dimensions();
    if rx + rw > fw || ry + rh > fh {
        bail!("ROI fuera del frame ({}x{})", fw, fh);
    }

    // Extract minimap a full resolution (1 px/NDI-pixel) en luma.
    let mut raw_luma = GrayImage::new(rw, rh);
    for y in 0..rh {
        for x in 0..rw {
            let p = frame.get_pixel(rx + x, ry + y).0;
            let luma = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as u8;
            raw_luma.put_pixel(x, y, image::Luma([luma]));
        }
    }

    // Downsample por `scale` via box averaging. El template final queda a
    // 1 px/tile = misma escala que los reference PNGs.
    let template_luma = if scale > 1 {
        let dw = rw / scale;
        let dh = rh / scale;
        let mut down = GrayImage::new(dw, dh);
        for dy in 0..dh {
            for dx in 0..dw {
                let mut sum = 0u32;
                let n = (scale * scale) as u32;
                for by in 0..scale {
                    for bx in 0..scale {
                        let sx = dx * scale + bx;
                        let sy = dy * scale + by;
                        sum += raw_luma.get_pixel(sx, sy).0[0] as u32;
                    }
                }
                down.put_pixel(dx, dy, image::Luma([(sum / n) as u8]));
            }
        }
        down
    } else {
        raw_luma
    };
    let (tw, th) = template_luma.dimensions();
    println!("Template NDI raw: {}×{} luma, downsampled by scale={} → {}×{} (tiles)", rw, rh, scale, tw, th);

    // 2. Listar reference PNGs.
    let mut refs: Vec<(PathBuf, i32, i32, i32)> = Vec::new();
    for entry in std::fs::read_dir(&map_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some((fx, fy, fz)) = parse_minimap_filename(&name_str) else { continue };
        if !floors.is_empty() && !floors.contains(&fz) {
            continue;
        }
        refs.push((entry.path(), fx, fy, fz));
    }
    println!("Reference PNGs a escanear: {}", refs.len());
    println!();

    if refs.is_empty() {
        bail!("no se encontraron reference PNGs en '{}'", map_dir.display());
    }

    // 3. Brute force match_template sobre cada reference.
    // Sequential — match_template_parallel internamente ya usa rayon si el
    // feature está habilitado en imageproc, suficiente paralelización.
    let t0 = Instant::now();
    let results: Vec<(f32, i32, i32, i32, u32, u32)> = refs.iter()
        .enumerate()
        .filter_map(|(idx, (path, fx, fy, fz))| {
            if idx % 100 == 0 && idx > 0 {
                eprint!("\r  scanning {}/{}...", idx, refs.len());
            }
            let img = image::open(path).ok()?.to_luma8();
            let (iw, ih) = img.dimensions();
            if iw < tw || ih < th { return None; }

            // SSDE: menor = mejor. Mide diferencia pixel-a-pixel, robusto
            // contra saturation de CCORR cuando hay regiones de color uniforme.
            // Invertimos el signo para poder ordenar descendente igual que CCORR.
            let result = match_template_parallel(
                &img,
                &template_luma,
                MatchTemplateMethod::SumOfSquaredErrorsNormalized,
            );

            // Encontrar el pixel con MENOR score (mejor match).
            let mut best_score = f32::MAX;
            let mut best_x = 0u32;
            let mut best_y = 0u32;
            for y in 0..result.height() {
                for x in 0..result.width() {
                    let s = result.get_pixel(x, y).0[0];
                    if s < best_score {
                        best_score = s;
                        best_x = x;
                        best_y = y;
                    }
                }
            }

            // Retornar NEGATIVO para que el sort descendente ponga los
            // mejores (menores originalmente) primero.
            Some((-best_score, *fx, *fy, *fz, best_x, best_y))
        })
        .collect();

    let elapsed = t0.elapsed();
    println!("Scan completo en {:.1}s ({} matches)", elapsed.as_secs_f64(), results.len());
    println!();

    // 4. Ordenar por score descendente y mostrar top-10.
    let mut sorted = results;
    sorted.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    println!("═══ TOP-10 MATCHES (SSD_NORMED, lower original = better) ═══");
    println!("  ssd      file_x    file_y   z   local_x local_y   → world_x world_y");
    for (i, (neg_score, fx, fy, fz, lx, ly)) in sorted.iter().take(10).enumerate() {
        let ssd = -neg_score; // des-invertir
        let world_x = fx + *lx as i32 + (tw / 2) as i32;
        let world_y = fy + *ly as i32 + (th / 2) as i32;
        let marker = if i == 0 { "→" } else { " " };
        println!(
            "{} {:.4}   {:>6}    {:>6}   {}   {:>3}     {:>3}      {:>6}  {:>6}",
            marker, ssd, fx, fy, fz, lx, ly, world_x, world_y
        );
    }

    let best = sorted.first();
    if let Some((neg_score, fx, fy, fz, lx, ly)) = best {
        let ssd = -neg_score;
        let world_x = fx + *lx as i32 + (tw / 2) as i32;
        let world_y = fy + *ly as i32 + (th / 2) as i32;
        println!();
        println!("═══ GROUND TRUTH ═══");
        println!("  Char coord real (centro minimap): ({}, {}, {})", world_x, world_y, fz);
        println!("  SSD score: {:.4} (menor = mejor)", ssd);
        if ssd < 0.05 {
            println!("  ✓ MATCH FUERTE — SSD<0.05, dHash está roto pero CCORR via SSD funciona");
        } else if ssd < 0.15 {
            println!("  ⚠ MATCH MODERADO — probablemente el lugar correcto con ruido");
        } else if ssd < 0.30 {
            println!("  ⚠ MATCH DÉBIL — posible scale mismatch o color shift severo");
        } else {
            println!("  ✗ SIN MATCH — problema estructural profundo");
        }
    }

    Ok(())
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_roi(s: &str) -> Result<(u32, u32, u32, u32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        bail!("roi debe ser x,y,w,h");
    }
    Ok((
        parts[0].trim().parse()?,
        parts[1].trim().parse()?,
        parts[2].trim().parse()?,
        parts[3].trim().parse()?,
    ))
}
