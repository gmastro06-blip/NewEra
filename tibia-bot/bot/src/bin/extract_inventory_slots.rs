//! extract_inventory_slots — recorta los 16 slots del inventory_backpack_strip
//! y los guarda como PNGs numerados (slot_01.png ... slot_16.png) junto con
//! un overlay `_grid.png` que muestra la numeración sobre el frame completo.
//!
//! Uso típico: capturar un frame con todos los bags abiertos, correr este
//! bin, y ver qué item real tiene cada slot para luego crear los templates
//! correspondientes en `assets/templates/inventory/`.
//!
//! ```bash
//! cargo run --release --bin extract_inventory_slots -- \
//!     --frame /tmp/frame_base.png \
//!     --output /tmp/slots
//! ```

use clap::Parser;
use image::{GenericImageView, Rgba};
use imageproc::drawing::draw_hollow_rect_mut;
use imageproc::rect::Rect;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    frame: PathBuf,

    #[arg(long)]
    output: PathBuf,

    /// Top-left x del primer backpack (default desde calibration.toml actual).
    #[arg(long, default_value_t = 1567)]
    x: u32,
    #[arg(long, default_value_t = 22)]
    y: u32,
    #[arg(long, default_value_t = 174)]
    backpack_w: u32,
    #[arg(long, default_value_t = 68)]
    backpack_h: u32,
    #[arg(long, default_value_t = 4)]
    backpack_count: u32,
    #[arg(long, default_value_t = 18)]
    slot_x_offset: u32,
    #[arg(long, default_value_t = 0)]
    slot_y_offset: u32,
    #[arg(long, default_value_t = 32)]
    slot_size: u32,
    #[arg(long, default_value_t = 2)]
    slot_gap: u32,
    #[arg(long, default_value_t = 4)]
    slot_cols: u32,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.output)?;

    let img = image::open(&args.frame)?.to_rgba8();
    let mut overlay = img.clone();

    let mut slot_idx = 0u32;
    for bp in 0..args.backpack_count {
        let bp_y = args.y + bp * args.backpack_h;
        for col in 0..args.slot_cols {
            slot_idx += 1;
            let sx = args.x + args.slot_x_offset + col * (args.slot_size + args.slot_gap);
            let sy = bp_y + args.slot_y_offset;

            // Crop 32x32.
            let crop = img.view(sx, sy, args.slot_size, args.slot_size).to_image();
            let path = args.output.join(format!("slot_{:02}.png", slot_idx));
            crop.save(&path)?;
            println!("slot {:02} @ bp={} col={} coord=({},{}) -> {}",
                slot_idx, bp + 1, col + 1, sx, sy, path.display());

            // Dibujar rectángulo amarillo + número en el overlay.
            let rect = Rect::at(sx as i32, sy as i32)
                .of_size(args.slot_size, args.slot_size);
            draw_hollow_rect_mut(&mut overlay, rect, Rgba([255, 255, 0, 255]));
        }
    }

    let overlay_path = args.output.join("_grid.png");
    overlay.save(&overlay_path)?;
    println!("grid overlay -> {}", overlay_path.display());
    println!("total slots extracted: {}", slot_idx);
    Ok(())
}
