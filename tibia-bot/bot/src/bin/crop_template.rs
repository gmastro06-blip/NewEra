//! crop_template — recorta un PNG template a un sub-rectángulo.
//!
//! Uso típico: regenerar un template más chico para acelerar el template
//! matching sin recalibrar desde un frame NDI. Preserva exactamente los
//! píxeles del original en el rango indicado.
//!
//! ```bash
//! cargo run --release --bin crop_template -- \
//!     --input  assets/templates/ui/stow_menu.png \
//!     --output assets/templates/ui/stow_menu.png \
//!     --x 15 --y 197 --w 195 --h 20
//! ```
//!
//! Guardar backup antes de sobrescribir: `cp stow_menu.png stow_menu.full.png`.

use clap::Parser;
use image::GenericImageView;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(about = "Crop a PNG template to a sub-rectangle")]
struct Args {
    /// Input PNG path.
    #[arg(long)]
    input: PathBuf,

    /// Output PNG path (may be the same as input).
    #[arg(long)]
    output: PathBuf,

    /// Top-left x of the crop.
    #[arg(long)]
    x: u32,

    /// Top-left y of the crop.
    #[arg(long)]
    y: u32,

    /// Width of the crop.
    #[arg(long)]
    w: u32,

    /// Height of the crop.
    #[arg(long)]
    h: u32,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let img = image::open(&args.input)?;
    let (iw, ih) = img.dimensions();

    if args.x + args.w > iw || args.y + args.h > ih {
        anyhow::bail!(
            "crop ({},{}, {}x{}) fuera de la imagen ({}x{})",
            args.x, args.y, args.w, args.h, iw, ih
        );
    }

    let cropped = img.crop_imm(args.x, args.y, args.w, args.h);
    cropped.save(&args.output)?;

    println!(
        "crop_template: {} ({}x{}) -> {} ({}x{}) [bbox x={}, y={}, w={}, h={}]",
        args.input.display(), iw, ih,
        args.output.display(), args.w, args.h,
        args.x, args.y, args.w, args.h
    );
    Ok(())
}
