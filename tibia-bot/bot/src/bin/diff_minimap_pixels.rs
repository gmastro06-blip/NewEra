/// diff_minimap_pixels — Diagnóstico pixel-a-pixel entre el minimap capturado
/// por NDI y el archivo de referencia `Minimap_Color_*.png` correspondiente.
///
/// Este binario resuelve la pregunta "¿por qué el tile-hashing no matchea?"
/// comparando directamente los pixels del runtime contra los del reference.
///
/// **Causas posibles que este tool distingue**:
/// - **Color shift uniforme** (ej NDI/OBS aplicando BT.601→sRGB): los 3 canales
///   tienen un offset consistente (ej +5R -3G -8B). Fix: aplicar el offset
///   inverso antes de hashear.
/// - **Smoothing/anti-aliasing del cliente Tibia 12**: diffs ruidosas sin
///   patrón. Fix: switch a CCORR_NORMED template matching (ver B.2 del plan).
/// - **Scale incorrecto**: los patches tienen features reconocibles pero
///   desplazadas/escaladas. Fix: ajustar ndi_tile_scale.
/// - **ROI mal alineado**: los pixels del NDI son completamente distintos
///   (ej agua vs terreno). Fix: revisar minimap ROI en calibration.toml.
///
/// ## Uso
///
/// ```bash
/// cargo run --release --bin diff_minimap_pixels -- \
///     --frame debug_frame.png \
///     --minimap-roi 1736,54,107,110 \
///     --char-coord 32681,31686,6 \
///     --map-dir assets/minimap/minimap \
///     --scale 5 \
///     --output diff_report.png
/// ```
///
/// El char-coord es el centro del minimap (donde está el crosshair del
/// jugador). Se puede obtener mirando el minimap del frame y comparándolo
/// con la UI del cliente Tibia.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use image::{GenericImageView, RgbaImage};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let frame_path = get_arg(&args, "--frame")
        .context("uso: --frame <path.png>")?;
    let minimap_roi = get_arg(&args, "--minimap-roi")
        .context("uso: --minimap-roi x,y,w,h")?;
    let char_coord = get_arg(&args, "--char-coord")
        .context("uso: --char-coord x,y,z")?;
    let map_dir = get_arg(&args, "--map-dir")
        .context("uso: --map-dir <path>")?;
    let scale: u32 = get_arg(&args, "--scale")
        .unwrap_or_else(|| "5".to_string())
        .parse()
        .context("--scale debe ser entero")?;
    let output = get_arg(&args, "--output")
        .unwrap_or_else(|| "diff_report.png".to_string());

    let (rx, ry, rw, rh) = parse_4(&minimap_roi, "minimap-roi")?;
    let (cx, cy, cz) = parse_3(&char_coord, "char-coord")?;
    let map_dir = PathBuf::from(&map_dir);

    println!("Frame:       {}", frame_path);
    println!("Minimap ROI: ({}, {}) {}×{}", rx, ry, rw, rh);
    println!("Char coord:  ({}, {}, {})", cx, cy, cz);
    println!("Scale:       {} px/tile", scale);
    println!();

    // 1. Cargar el frame y extraer el minimap ROI.
    let frame = image::open(&frame_path)
        .with_context(|| format!("abrir frame '{}'", frame_path))?
        .to_rgba8();
    let (fw, fh) = frame.dimensions();
    if rx + rw > fw || ry + rh > fh {
        bail!("minimap-roi fuera del frame ({}x{})", fw, fh);
    }
    let minimap = frame.view(rx, ry, rw, rh).to_image();
    println!("NDI minimap extraído: {}×{}", rw, rh);

    // 2. Determinar qué archivo de referencia contiene el char coord.
    // Los reference PNGs son 256×256 tiles, nombrados por su (x,y,z) top-left.
    let file_x = (cx / 256) * 256;
    let file_y = (cy / 256) * 256;
    let ref_filename = format!("Minimap_Color_{}_{}_{}.png", file_x, file_y, cz);
    let ref_path = map_dir.join(&ref_filename);
    if !ref_path.exists() {
        bail!("reference no encontrado: {}", ref_path.display());
    }
    println!("Reference PNG: {}", ref_filename);

    let reference = image::open(&ref_path)
        .with_context(|| format!("abrir reference '{}'", ref_path.display()))?
        .to_rgba8();
    let (refw, refh) = reference.dimensions();
    println!("Reference dims: {}×{}", refw, refh);

    // 3. Calcular el offset del char dentro del reference (en tiles).
    let char_off_x_tiles = (cx - file_x) as u32;
    let char_off_y_tiles = (cy - file_y) as u32;
    println!("Char offset en reference (tiles): ({}, {})", char_off_x_tiles, char_off_y_tiles);

    // 4. El minimap NDI muestra (rw/scale) × (rh/scale) tiles centrados en el char.
    let half_tiles_x = (rw / scale / 2) as i32;
    let half_tiles_y = (rh / scale / 2) as i32;
    let ref_roi_x = (char_off_x_tiles as i32 - half_tiles_x).max(0) as u32;
    let ref_roi_y = (char_off_y_tiles as i32 - half_tiles_y).max(0) as u32;
    let ref_roi_w = (rw / scale).min(refw - ref_roi_x);
    let ref_roi_h = (rh / scale).min(refh - ref_roi_y);
    println!(
        "Reference ROI (tiles): ({}, {}) {}×{}",
        ref_roi_x, ref_roi_y, ref_roi_w, ref_roi_h
    );
    let ref_region = reference.view(ref_roi_x, ref_roi_y, ref_roi_w, ref_roi_h).to_image();

    // 5. Downsample el NDI minimap de scale × px/tile → 1 px/tile por averaging
    //    de bloques scale×scale. Resultado: (rw/scale) × (rh/scale) pixels,
    //    mismas dims que ref_region.
    let ndi_downsampled = downsample_avg(&minimap, scale);
    let (dw, dh) = ndi_downsampled.dimensions();
    println!("NDI downsampled: {}×{}", dw, dh);

    // 6. Comparar pixel a pixel.
    let (cmp_w, cmp_h) = (dw.min(ref_roi_w), dh.min(ref_roi_h));
    let (ndi_avg, ref_avg, max_diff, mean_diff, hist_ndi, hist_ref) =
        compare_pixels(&ndi_downsampled, &ref_region, cmp_w, cmp_h);

    println!();
    println!("═══ COMPARACIÓN PIXEL ═══");
    println!("Área comparada: {}×{} = {} pixels", cmp_w, cmp_h, cmp_w * cmp_h);
    println!();
    println!("NDI   avg RGB: ({:>3}, {:>3}, {:>3})", ndi_avg.0, ndi_avg.1, ndi_avg.2);
    println!("Ref   avg RGB: ({:>3}, {:>3}, {:>3})", ref_avg.0, ref_avg.1, ref_avg.2);
    println!(
        "Shift (NDI-Ref): ({:>+4}, {:>+4}, {:>+4})",
        ndi_avg.0 as i32 - ref_avg.0 as i32,
        ndi_avg.1 as i32 - ref_avg.1 as i32,
        ndi_avg.2 as i32 - ref_avg.2 as i32,
    );
    println!();
    println!("Max diff por canal: R={}  G={}  B={}", max_diff.0, max_diff.1, max_diff.2);
    println!("Mean diff por canal: R={:.1}  G={:.1}  B={:.1}", mean_diff.0, mean_diff.1, mean_diff.2);
    println!();

    // Histogramas luma 8-bin.
    println!("Histograma luma (8 bins, 0-255):");
    println!("         {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
        "0-31", "32-63", "64-95", "96-127", "128-159", "160-191", "192-223", "224-255");
    print!("  NDI:   ");
    for v in &hist_ndi { print!(" {:>5}", v); }
    println!();
    print!("  Ref:   ");
    for v in &hist_ref { print!(" {:>5}", v); }
    println!();

    // Diagnóstico automático.
    println!();
    println!("═══ DIAGNÓSTICO ═══");
    let shift_magnitude = ((ndi_avg.0 as i32 - ref_avg.0 as i32).abs()
        + (ndi_avg.1 as i32 - ref_avg.1 as i32).abs()
        + (ndi_avg.2 as i32 - ref_avg.2 as i32).abs()) as u32;
    let mean_total = mean_diff.0 + mean_diff.1 + mean_diff.2;

    if shift_magnitude > 30 && mean_total > 60.0 {
        println!("⚠  Grandes diferencias de color (shift>{}) Y variación alta (mean>{:.0}).", shift_magnitude, mean_total);
        println!("   HIPÓTESIS: ROI mal alineado o scale incorrecto — los pixels");
        println!("   comparados no están mirando la misma región del mapa.");
        println!("   ACCIÓN: verificar minimap_roi en calibration.toml y char-coord.");
    } else if shift_magnitude > 15 && mean_total < 40.0 {
        println!("⚠  Shift uniforme ({}) con variación baja ({:.0}).", shift_magnitude, mean_total);
        println!("   HIPÓTESIS: NDI/OBS está aplicando conversión de color space");
        println!("   (BT.601↔sRGB, full↔limited range). Los features coinciden");
        println!("   pero los valores absolutos difieren.");
        println!("   ACCIÓN: aplicar offset inverso al index o al runtime, O");
        println!("   switch a métrica robusta a color (ej dHash de luma, CCORR).");
    } else if shift_magnitude < 15 && mean_total > 40.0 {
        println!("⚠  Color promedio OK ({}) pero variación alta ({:.0}).", shift_magnitude, mean_total);
        println!("   HIPÓTESIS: smoothing/anti-aliasing del cliente Tibia 12");
        println!("   altera pixels individuales sin cambiar el color promedio.");
        println!("   ACCIÓN: switch a CCORR_NORMED template matching (B.2 del plan).");
    } else {
        println!("✓  Diferencias bajas (shift={}, mean={:.0}).", shift_magnitude, mean_total);
        println!("   Los pixels del NDI y el reference son muy similares.");
        println!("   Si el hashing falla, el problema está en el ALGORITMO de hash,");
        println!("   no en los pixels. Check dhash() con ambos patches directos.");
    }

    // 7. Generar imagen side-by-side para inspección visual.
    let report = make_side_by_side(&ndi_downsampled, &ref_region, 8);
    report.save(&output)
        .with_context(|| format!("guardar report a '{}'", output))?;
    println!();
    println!("Side-by-side guardado: {} ({}×{}, scale 8×)", output, report.width(), report.height());

    Ok(())
}

fn get_arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_3(s: &str, name: &str) -> Result<(i32, i32, i32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        bail!("{} debe ser a,b,c", name);
    }
    Ok((
        parts[0].trim().parse()?,
        parts[1].trim().parse()?,
        parts[2].trim().parse()?,
    ))
}

fn parse_4(s: &str, name: &str) -> Result<(u32, u32, u32, u32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        bail!("{} debe ser a,b,c,d", name);
    }
    Ok((
        parts[0].trim().parse()?,
        parts[1].trim().parse()?,
        parts[2].trim().parse()?,
        parts[3].trim().parse()?,
    ))
}

/// Downsample nearest/average: cada bloque `scale × scale` se promedia a 1 pixel.
fn downsample_avg(src: &RgbaImage, scale: u32) -> RgbaImage {
    let (sw, sh) = src.dimensions();
    let dw = sw / scale;
    let dh = sh / scale;
    let mut out = RgbaImage::new(dw, dh);
    for dy in 0..dh {
        for dx in 0..dw {
            let mut r = 0u32;
            let mut g = 0u32;
            let mut b = 0u32;
            let mut a = 0u32;
            let n = (scale * scale) as u32;
            for sy_off in 0..scale {
                for sx_off in 0..scale {
                    let sx = dx * scale + sx_off;
                    let sy = dy * scale + sy_off;
                    let p = src.get_pixel(sx, sy).0;
                    r += p[0] as u32;
                    g += p[1] as u32;
                    b += p[2] as u32;
                    a += p[3] as u32;
                }
            }
            out.put_pixel(dx, dy, image::Rgba([(r/n) as u8, (g/n) as u8, (b/n) as u8, (a/n) as u8]));
        }
    }
    out
}

/// Compara dos imágenes pixel-a-pixel. Retorna (avg_ndi, avg_ref, max_diff,
/// mean_diff, hist_ndi_8bin, hist_ref_8bin).
#[allow(clippy::type_complexity)]
fn compare_pixels(
    ndi: &RgbaImage,
    reference: &RgbaImage,
    w: u32,
    h: u32,
) -> ((u8, u8, u8), (u8, u8, u8), (u32, u32, u32), (f32, f32, f32), [u32; 8], [u32; 8]) {
    let mut sum_ndi = (0u64, 0u64, 0u64);
    let mut sum_ref = (0u64, 0u64, 0u64);
    let mut sum_diff = (0u64, 0u64, 0u64);
    let mut max_diff = (0u32, 0u32, 0u32);
    let mut hist_ndi = [0u32; 8];
    let mut hist_ref = [0u32; 8];
    let n = (w * h) as u64;

    for y in 0..h {
        for x in 0..w {
            let p1 = ndi.get_pixel(x, y).0;
            let p2 = reference.get_pixel(x, y).0;
            sum_ndi.0 += p1[0] as u64;
            sum_ndi.1 += p1[1] as u64;
            sum_ndi.2 += p1[2] as u64;
            sum_ref.0 += p2[0] as u64;
            sum_ref.1 += p2[1] as u64;
            sum_ref.2 += p2[2] as u64;
            let dr = (p1[0] as i32 - p2[0] as i32).unsigned_abs();
            let dg = (p1[1] as i32 - p2[1] as i32).unsigned_abs();
            let db = (p1[2] as i32 - p2[2] as i32).unsigned_abs();
            sum_diff.0 += dr as u64;
            sum_diff.1 += dg as u64;
            sum_diff.2 += db as u64;
            if dr > max_diff.0 { max_diff.0 = dr; }
            if dg > max_diff.1 { max_diff.1 = dg; }
            if db > max_diff.2 { max_diff.2 = db; }

            // Luma Y = 0.299R + 0.587G + 0.114B
            let luma_ndi = (0.299 * p1[0] as f32 + 0.587 * p1[1] as f32 + 0.114 * p1[2] as f32) as u8;
            let luma_ref = (0.299 * p2[0] as f32 + 0.587 * p2[1] as f32 + 0.114 * p2[2] as f32) as u8;
            hist_ndi[(luma_ndi / 32) as usize] += 1;
            hist_ref[(luma_ref / 32) as usize] += 1;
        }
    }
    let ndi_avg = ((sum_ndi.0 / n) as u8, (sum_ndi.1 / n) as u8, (sum_ndi.2 / n) as u8);
    let ref_avg = ((sum_ref.0 / n) as u8, (sum_ref.1 / n) as u8, (sum_ref.2 / n) as u8);
    let mean_diff = (
        sum_diff.0 as f32 / n as f32,
        sum_diff.1 as f32 / n as f32,
        sum_diff.2 as f32 / n as f32,
    );
    (ndi_avg, ref_avg, max_diff, mean_diff, hist_ndi, hist_ref)
}

/// Imagen side-by-side: NDI arriba, Reference abajo, ambas upscaled `zoom` veces.
fn make_side_by_side(ndi: &RgbaImage, reference: &RgbaImage, zoom: u32) -> RgbaImage {
    let w = ndi.width().max(reference.width()) * zoom;
    let h = (ndi.height() + reference.height() + 4) * zoom;
    let mut out = RgbaImage::from_pixel(w, h, image::Rgba([32, 32, 32, 255]));
    copy_upscaled(ndi, &mut out, 0, 0, zoom);
    copy_upscaled(reference, &mut out, 0, (ndi.height() + 4) * zoom, zoom);
    out
}

fn copy_upscaled(src: &RgbaImage, dst: &mut RgbaImage, dst_x: u32, dst_y: u32, zoom: u32) {
    for sy in 0..src.height() {
        for sx in 0..src.width() {
            let p = *src.get_pixel(sx, sy);
            for dy in 0..zoom {
                for dx in 0..zoom {
                    let x = dst_x + sx * zoom + dx;
                    let y = dst_y + sy * zoom + dy;
                    if x < dst.width() && y < dst.height() {
                        dst.put_pixel(x, y, p);
                    }
                }
            }
        }
    }
}
