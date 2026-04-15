//! tune_inventory_strip — Dibuja las ROIs de un `InventoryBackpackStrip`
//! sobre un frame PNG para verificar el calibrado visualmente.
//!
//! ## Flujo
//!
//! 1. Captura un frame con `curl http://localhost:8080/test/grab -o frame.png`
//!    (o pásale cualquier PNG del juego tu layout, 1920×1080).
//! 2. Corre el tool con los parámetros que quieras probar:
//!    ```bash
//!    cargo run --release --bin tune_inventory_strip -- \
//!        --frame frame.png \
//!        --x 1567 --y 22 \
//!        --backpack-w 174 --backpack-h 67 \
//!        --count 8 \
//!        --slot-x-offset 6 --slot-y-offset 18 \
//!        --slot-size 32 --slot-gap 2 \
//!        --cols 4 --rows 1 \
//!        --output tuned.png
//!    ```
//! 3. Abre `tuned.png` y verifica que los rectángulos amarillos caen sobre
//!    los iconos de items.
//! 4. Si están desalineados, ajusta `--slot-x-offset` / `--slot-y-offset`
//!    hasta que matcheen.
//! 5. Cuando matcheen, copia esos valores a `assets/calibration.toml`
//!    en la sección `[inventory_backpack_strip]`.
//!
//! El tool también imprime el bloque TOML listo para pegar.

use std::path::PathBuf;

use image::{Rgba, RgbaImage};
use imageproc::drawing::draw_hollow_rect_mut;
use imageproc::rect::Rect;

use tibia_bot::sense::vision::calibration::InventoryBackpackStrip;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let frame_path = arg(&args, "--frame")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("--frame <path.png> requerido"))?;

    let output = arg(&args, "--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tuned_inventory.png"));

    // Defaults = valores actuales en assets/calibration.toml.
    let default = InventoryBackpackStrip::default();

    let strip = InventoryBackpackStrip {
        x:              arg_u32(&args, "--x").unwrap_or(default.x),
        y:              arg_u32(&args, "--y").unwrap_or(default.y),
        backpack_w:     arg_u32(&args, "--backpack-w").unwrap_or(default.backpack_w),
        backpack_h:     arg_u32(&args, "--backpack-h").unwrap_or(default.backpack_h),
        backpack_count: arg_u32(&args, "--count").unwrap_or(default.backpack_count),
        slot_x_offset:  arg_u32(&args, "--slot-x-offset").unwrap_or(default.slot_x_offset),
        slot_y_offset:  arg_u32(&args, "--slot-y-offset").unwrap_or(default.slot_y_offset),
        slot_size:      arg_u32(&args, "--slot-size").unwrap_or(default.slot_size),
        slot_gap:       arg_u32(&args, "--slot-gap").unwrap_or(default.slot_gap),
        slot_cols:      arg_u32(&args, "--cols").unwrap_or(default.slot_cols),
        slot_rows:      arg_u32(&args, "--rows").unwrap_or(default.slot_rows),
    };

    println!("Config:");
    println!("  strip at          ({}, {})", strip.x, strip.y);
    println!("  backpack size     {}×{}", strip.backpack_w, strip.backpack_h);
    println!("  backpack_count    {}", strip.backpack_count);
    println!("  slot offset       ({}, {})", strip.slot_x_offset, strip.slot_y_offset);
    println!("  slot size/gap     {}/{}", strip.slot_size, strip.slot_gap);
    println!("  grid per backpack {} cols × {} rows", strip.slot_cols, strip.slot_rows);
    println!();

    println!("Loading frame from {}...", frame_path.display());
    let img = image::open(&frame_path)?;
    let (w, h) = (img.width(), img.height());
    println!("  frame: {}×{}", w, h);
    println!();

    let mut rgba: RgbaImage = img.to_rgba8();

    // Dibujar el outer strip (rojo) para contexto.
    let strip_h = strip.backpack_h * strip.backpack_count;
    draw_rect_safe(
        &mut rgba,
        strip.x,
        strip.y,
        strip.backpack_w,
        strip_h,
        [255, 0, 0, 255],
    );

    // Dibujar cada backpack (azul claro) para mostrar las divisiones.
    for bp in 0..strip.backpack_count {
        draw_rect_safe(
            &mut rgba,
            strip.x,
            strip.y + bp * strip.backpack_h,
            strip.backpack_w,
            strip.backpack_h,
            [100, 150, 255, 255],
        );
    }

    // Dibujar cada slot (amarillo).
    let slots = strip.expand();
    for slot in &slots {
        draw_rect_safe(
            &mut rgba,
            slot.x,
            slot.y,
            slot.w,
            slot.h,
            [255, 255, 0, 255],
        );
    }

    println!("Drew {} slots ({} backpacks × {} cols × {} rows)", slots.len(), strip.backpack_count, strip.slot_cols, strip.slot_rows);
    rgba.save(&output)?;
    println!("Saved: {}", output.display());
    println!();

    // Imprimir el bloque TOML listo para pegar.
    println!("─── Paste into calibration.toml ───");
    println!("[inventory_backpack_strip]");
    println!("x              = {}", strip.x);
    println!("y              = {}", strip.y);
    println!("backpack_w     = {}", strip.backpack_w);
    println!("backpack_h     = {}", strip.backpack_h);
    println!("backpack_count = {}", strip.backpack_count);
    println!("slot_x_offset  = {}", strip.slot_x_offset);
    println!("slot_y_offset  = {}", strip.slot_y_offset);
    println!("slot_size      = {}", strip.slot_size);
    println!("slot_gap       = {}", strip.slot_gap);
    println!("slot_cols      = {}", strip.slot_cols);
    println!("slot_rows      = {}", strip.slot_rows);

    Ok(())
}

fn draw_rect_safe(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
    let (iw, ih) = (img.width(), img.height());
    if x >= iw || y >= ih || w == 0 || h == 0 {
        return;
    }
    let clip_w = (x + w).min(iw) - x;
    let clip_h = (y + h).min(ih) - y;
    let rect = Rect::at(x as i32, y as i32).of_size(clip_w, clip_h);
    draw_hollow_rect_mut(img, rect, Rgba(color));
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn arg_u32(args: &[String], name: &str) -> Option<u32> {
    arg(args, name).and_then(|s| s.parse::<u32>().ok())
}
