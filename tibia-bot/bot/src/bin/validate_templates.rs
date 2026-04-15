/// validate_templates — Herramienta CLI para validar thresholds de template matching.
///
/// Toma un directorio de frames PNG (capturas NDI reales) y un directorio de
/// templates (p.ej. `assets/templates/inventory/*.png`). Para cada frame +
/// template, computa el mejor score de match en una grilla de slots dada y
/// reporta cuántos "hits" produce cada umbral. Permite tunear los thresholds
/// sin recompilar.
///
/// Uso:
///   cargo run --release --bin validate_templates -- \
///       --frames path/to/frames_dir \
///       --templates assets/templates/inventory \
///       --grid 1760,420,4,5,32,2 \
///       --thresholds 0.05,0.10,0.15,0.20,0.25,0.30
///
/// Grid: "x,y,cols,rows,slot_size,gap"
/// Thresholds: lista CSV de umbrales a probar (default: 0.05..0.30 cada 0.05).

use std::path::{Path, PathBuf};

use image::GrayImage;
use imageproc::template_matching::{match_template, MatchTemplateMethod};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let frames_dir = arg_value(&args, "--frames")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("uso: --frames <dir>"))?;
    let templates_dir = arg_value(&args, "--templates")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("uso: --templates <dir>"))?;
    let grid_str = arg_value(&args, "--grid")
        .unwrap_or_else(|| "1760,420,4,5,32,2".to_string());
    let thresholds_str = arg_value(&args, "--thresholds")
        .unwrap_or_else(|| "0.05,0.10,0.15,0.20,0.25,0.30".to_string());

    let grid = parse_grid(&grid_str)?;
    let thresholds: Vec<f32> = thresholds_str
        .split(',')
        .map(|s| s.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("thresholds invalidos: {}", e))?;

    println!("Frames dir:   {}", frames_dir.display());
    println!("Templates:    {}", templates_dir.display());
    println!("Grid:         {:?}", grid);
    println!("Thresholds:   {:?}", thresholds);
    println!();

    // Cargar templates.
    let templates = load_templates_dir(&templates_dir)?;
    if templates.is_empty() {
        anyhow::bail!("no se cargaron templates en '{}'", templates_dir.display());
    }
    println!("Cargados {} templates", templates.len());

    // Cargar frames.
    let frame_paths = collect_png_files(&frames_dir)?;
    if frame_paths.is_empty() {
        anyhow::bail!("no hay PNG en '{}'", frames_dir.display());
    }
    println!("Cargados {} frames\n", frame_paths.len());

    // Para cada template, para cada threshold, contar slot-hits across todos los frames.
    let slots = grid.expand();

    for (tpl_name, tpl) in &templates {
        println!("Template: {} ({}×{})", tpl_name, tpl.width(), tpl.height());
        // Matriz [threshold][hits en ese threshold].
        let mut hits_per_threshold: Vec<u32> = vec![0; thresholds.len()];
        let mut total_slots = 0u32;
        let mut best_score_overall = f32::MAX;

        for frame_path in &frame_paths {
            let frame = match image::open(frame_path) {
                Ok(img) => img.to_luma8(),
                Err(e) => {
                    eprintln!("  skip {}: {}", frame_path.display(), e);
                    continue;
                }
            };
            for slot in &slots {
                total_slots += 1;
                let Some(patch) = extract_slot_gray(&frame, slot) else { continue };
                if patch.width() < tpl.width() || patch.height() < tpl.height() {
                    continue;
                }
                let scores = match_template(&patch, tpl, MatchTemplateMethod::SumOfSquaredErrorsNormalized);
                let best = scores.iter().cloned().fold(f32::MAX, f32::min);
                if best < best_score_overall {
                    best_score_overall = best;
                }
                for (i, &th) in thresholds.iter().enumerate() {
                    if best <= th {
                        hits_per_threshold[i] += 1;
                    }
                }
            }
        }

        // Reporte por template.
        let best_display = if best_score_overall == f32::MAX {
            "N/A".to_string()
        } else {
            format!("{:.4}", best_score_overall)
        };
        println!("  Best score observed: {}", best_display);
        println!("  Hits por threshold (de {} slot-samples):", total_slots);
        for (i, &th) in thresholds.iter().enumerate() {
            let hits = hits_per_threshold[i];
            let pct = if total_slots > 0 {
                hits as f64 / total_slots as f64 * 100.0
            } else {
                0.0
            };
            println!("    {:.3}: {:>5} hits ({:.1}%)", th, hits, pct);
        }
        // Recomendar el threshold más bajo que produce ≥1 hit.
        if let Some(idx) = hits_per_threshold.iter().position(|&h| h > 0) {
            println!("  → Threshold mínimo con match: {:.3}", thresholds[idx]);
        } else {
            println!("  → Ningún threshold produjo match");
        }
        println!();
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[derive(Debug, Clone, Copy)]
struct Grid {
    x: u32,
    y: u32,
    cols: u32,
    rows: u32,
    slot_size: u32,
    gap: u32,
}

#[derive(Debug, Clone, Copy)]
struct SlotRoi { x: u32, y: u32, w: u32, h: u32 }

impl Grid {
    fn expand(&self) -> Vec<SlotRoi> {
        let stride = self.slot_size + self.gap;
        let mut out = Vec::new();
        for r in 0..self.rows {
            for c in 0..self.cols {
                out.push(SlotRoi {
                    x: self.x + c * stride,
                    y: self.y + r * stride,
                    w: self.slot_size,
                    h: self.slot_size,
                });
            }
        }
        out
    }
}

fn parse_grid(s: &str) -> anyhow::Result<Grid> {
    let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
    if parts.len() != 6 {
        anyhow::bail!("grid requiere 6 valores (x,y,cols,rows,slot_size,gap)");
    }
    Ok(Grid {
        x: parts[0].parse()?,
        y: parts[1].parse()?,
        cols: parts[2].parse()?,
        rows: parts[3].parse()?,
        slot_size: parts[4].parse()?,
        gap: parts[5].parse()?,
    })
}

fn load_templates_dir(dir: &Path) -> anyhow::Result<Vec<(String, GrayImage)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        let img = image::open(&path)?.to_luma8();
        out.push((name, img));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn collect_png_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("png") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn extract_slot_gray(frame: &GrayImage, slot: &SlotRoi) -> Option<GrayImage> {
    if slot.x + slot.w > frame.width() || slot.y + slot.h > frame.height() {
        return None;
    }
    let mut out = GrayImage::new(slot.w, slot.h);
    for row in 0..slot.h {
        for col in 0..slot.w {
            out.put_pixel(col, row, *frame.get_pixel(slot.x + col, slot.y + row));
        }
    }
    Some(out)
}
