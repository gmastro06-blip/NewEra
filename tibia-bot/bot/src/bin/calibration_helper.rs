//! calibration_helper — Extrae digit templates de un frame real de Tibia.
//!
//! Esta herramienta asiste al proceso de calibración de los digit templates
//! (0-9) necesarios para `has_stack()` / inventory_ocr.
//!
//! ## Flujo de uso
//!
//! 1. Captura un frame con `curl /test/grab -o frame.png` cuando tienes un
//!    slot con un stack visible (ej. 50 mana potions = dígitos "5" y "0").
//! 2. Mide con GIMP la esquina inferior-derecha del slot (área donde está
//!    el número), ej. `x=1820, y=445, w=16, h=8`.
//! 3. Ejecuta:
//!    ```
//!    cargo run --release --bin calibration_helper -- \
//!        --frame frame.png \
//!        --area 1820,445,16,8 \
//!        --digits 50 \
//!        --output assets/templates/digits
//!    ```
//! 4. La herramienta:
//!    - Extrae el área y la muestra en ASCII
//!    - Divide el número "50" en 2 dígitos (5 y 0)
//!    - Intenta auto-segmentar horizontalmente por gaps vacíos
//!    - Guarda cada segmento como `{digit}.png`
//!    - Reporta coords relativas para que repitas manualmente si hace falta
//!
//! La auto-segmentación es heurística; para dígitos que no existen aún,
//! repetir con más capturas (ej. un slot con "123" para cubrir 1, 2, 3).

use std::path::PathBuf;

use image::GrayImage;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let frame_path = arg_value(&args, "--frame")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("--frame <path.png> requerido"))?;
    let area_str = arg_value(&args, "--area")
        .ok_or_else(|| anyhow::anyhow!("--area x,y,w,h requerido"))?;
    let digits_str = arg_value(&args, "--digits")
        .ok_or_else(|| anyhow::anyhow!("--digits <numero visible> requerido (ej: 50)"))?;
    let output_dir = arg_value(&args, "--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/templates/digits"));

    let (x, y, w, h) = parse_area(&area_str)?;
    let expected_digits: Vec<u8> = digits_str.chars()
        .filter_map(|c| c.to_digit(10).map(|d| d as u8))
        .collect();

    if expected_digits.is_empty() {
        anyhow::bail!("--digits debe contener al menos un dígito (0-9)");
    }

    println!("Frame:     {}", frame_path.display());
    println!("Area:      ({}, {}, {}, {})", x, y, w, h);
    println!("Expected:  {:?} ({} digit{})",
        expected_digits,
        expected_digits.len(),
        if expected_digits.len() == 1 { "" } else { "s" });
    println!("Output:    {}", output_dir.display());
    println!();

    // Cargar y convertir a luma.
    let img = image::open(&frame_path)?.to_luma8();
    let (fw, fh) = (img.width(), img.height());
    if x + w > fw || y + h > fh {
        anyhow::bail!("área ({},{},{},{}) fuera del frame {}×{}", x, y, w, h, fw, fh);
    }

    // Extraer el área.
    let mut area = GrayImage::new(w, h);
    for ry in 0..h {
        for rx in 0..w {
            area.put_pixel(rx, ry, *img.get_pixel(x + rx, y + ry));
        }
    }

    // Dibujar ASCII art del área para inspección visual.
    println!("Extracted area ({}×{}):", w, h);
    print_ascii(&area);
    println!();

    // Binarizar: pixels >= 128 son "tinta" (blanco en Tibia).
    let threshold = compute_otsu(&area);
    println!("Otsu threshold: {}", threshold);
    println!();

    // Segmentación horizontal por columnas vacías.
    let segments = segment_horizontally(&area, threshold);
    println!("Auto-segmentation found {} column segments:", segments.len());
    for (i, (sx, sw)) in segments.iter().enumerate() {
        println!("  {}: x={}, w={}", i, sx, sw);
    }
    println!();

    if segments.len() != expected_digits.len() {
        println!("⚠ Warning: expected {} digits but found {} segments.",
            expected_digits.len(), segments.len());
        println!("  Puede necesitar ajustar el --area o segmentar manualmente.");
        if segments.len() < expected_digits.len() {
            println!("  Posible causa: dígitos adyacentes sin gap (font serif), o área muy estrecha.");
        } else {
            println!("  Posible causa: área demasiado ancha e incluye pixels fuera del número.");
        }
    }

    // Crear dir de salida.
    std::fs::create_dir_all(&output_dir)?;

    // Guardar cada segmento con el dígito esperado correspondiente.
    let n = segments.len().min(expected_digits.len());
    for i in 0..n {
        let (sx, sw) = segments[i];
        let digit = expected_digits[i];
        let mut seg_img = GrayImage::new(sw, h);
        for ry in 0..h {
            for rx in 0..sw {
                seg_img.put_pixel(rx, ry, *area.get_pixel(sx + rx, ry));
            }
        }
        let out_path = output_dir.join(format!("{}.png", digit));
        seg_img.save(&out_path)?;
        println!("✓ Saved digit {} → {} ({}×{})", digit, out_path.display(), sw, h);
    }

    if n < expected_digits.len() {
        println!();
        println!("⚠ {} dígito(s) no guardado(s). Ajusta el área e intenta de nuevo.",
            expected_digits.len() - n);
        std::process::exit(1);
    }

    println!();
    println!("Calibration complete. Test con:");
    println!("  cargo test --release inventory_ocr");

    Ok(())
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_area(s: &str) -> anyhow::Result<(u32, u32, u32, u32)> {
    let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
    if parts.len() != 4 {
        anyhow::bail!("--area requiere formato x,y,w,h (got: {})", s);
    }
    Ok((
        parts[0].parse()?,
        parts[1].parse()?,
        parts[2].parse()?,
        parts[3].parse()?,
    ))
}

/// Imprime un GrayImage como ASCII art (█ = pixel bright, · = dark).
fn print_ascii(img: &GrayImage) {
    for y in 0..img.height() {
        for x in 0..img.width() {
            let v = img.get_pixel(x, y)[0];
            let ch = match v {
                0..=63 => '·',
                64..=127 => '░',
                128..=191 => '▒',
                192..=255 => '█',
            };
            print!("{}", ch);
        }
        println!();
    }
}

/// Threshold de Otsu (separación automática foreground/background).
#[allow(clippy::needless_range_loop)]
fn compute_otsu(img: &GrayImage) -> u8 {
    let mut hist = [0u32; 256];
    for px in img.pixels() {
        hist[px[0] as usize] += 1;
    }
    let total = (img.width() * img.height()) as f64;
    let mut sum = 0f64;
    for i in 0..256 {
        sum += i as f64 * hist[i] as f64;
    }
    let mut sum_b = 0f64;
    let mut w_b = 0f64;
    let mut max_var = 0f64;
    let mut threshold = 128u8;
    for t in 0..256 {
        w_b += hist[t] as f64;
        if w_b == 0.0 { continue; }
        let w_f = total - w_b;
        if w_f == 0.0 { break; }
        sum_b += t as f64 * hist[t] as f64;
        let mean_b = sum_b / w_b;
        let mean_f = (sum - sum_b) / w_f;
        let var = w_b * w_f * (mean_b - mean_f).powi(2);
        if var > max_var {
            max_var = var;
            threshold = t as u8;
        }
    }
    threshold
}

/// Segmenta horizontalmente la imagen en N bloques por columnas vacías.
/// Una columna está "vacía" si ningún pixel supera el threshold.
/// Retorna `Vec<(x_start, width)>` para cada segmento contiguo.
fn segment_horizontally(img: &GrayImage, threshold: u8) -> Vec<(u32, u32)> {
    let w = img.width();
    let h = img.height();
    let mut col_has_pixel = vec![false; w as usize];
    for x in 0..w {
        for y in 0..h {
            if img.get_pixel(x, y)[0] >= threshold {
                col_has_pixel[x as usize] = true;
                break;
            }
        }
    }
    let mut segments = Vec::new();
    let mut start: Option<u32> = None;
    for x in 0..w {
        let has = col_has_pixel[x as usize];
        match (has, start) {
            (true, None) => start = Some(x),
            (false, Some(s)) => {
                segments.push((s, x - s));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        segments.push((s, w - s));
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_area_rejects_bad_format() {
        assert!(parse_area("1,2,3").is_err());
        assert!(parse_area("x,y,z,w").is_err());
        assert!(parse_area("1,2,3,4").is_ok());
        assert_eq!(parse_area("10,20,30,40").unwrap(), (10, 20, 30, 40));
    }

    #[test]
    fn otsu_finds_reasonable_threshold() {
        // Imagen 8x8 con distribución bimodal realista: foreground=220, background=40.
        // Otsu debe encontrar un threshold entre ambas modas.
        let mut img = GrayImage::new(8, 8);
        for y in 0..8 { for x in 0..8 {
            let v = if x < 4 { 40 } else { 220 };
            img.put_pixel(x, y, image::Luma([v]));
        }}
        let t = compute_otsu(&img);
        // Threshold debe caer entre los dos valores (40, 220) — típicamente
        // cerca del medio (~130) pero aceptamos cualquier valor entre ellos.
        assert!(t >= 40 && t <= 220, "threshold {} out of [40, 220]", t);
    }

    #[test]
    fn segment_horizontally_finds_two_digits() {
        // 7x5 imagen con 2 "digits" separados por columna vacía:
        //   column 0-1: bloque blanco
        //   column 2: vacía (negro)
        //   column 3-5: bloque blanco
        //   column 6: vacía
        let mut img = GrayImage::new(7, 5);
        for y in 0..5 { for x in 0..7 {
            let v = if x == 2 || x == 6 { 0 } else { 255 };
            img.put_pixel(x, y, image::Luma([v]));
        }}
        let segs = segment_horizontally(&img, 128);
        assert_eq!(segs.len(), 2, "esperaba 2 segmentos, got {:?}", segs);
        assert_eq!(segs[0], (0, 2));
        assert_eq!(segs[1], (3, 3));
    }

    #[test]
    fn segment_horizontally_empty_image_returns_empty() {
        let img = GrayImage::new(10, 5); // todo negro
        let segs = segment_horizontally(&img, 128);
        assert!(segs.is_empty());
    }

    #[test]
    fn segment_horizontally_single_digit() {
        let mut img = GrayImage::new(5, 3);
        for y in 0..3 { for x in 0..5 {
            img.put_pixel(x, y, image::Luma([200]));
        }}
        let segs = segment_horizontally(&img, 128);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0], (0, 5));
    }
}
