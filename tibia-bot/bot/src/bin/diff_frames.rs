/// diff_frames — Compara dos frames y reporta las regiones que cambiaron.
///
/// Uso: diff_frames frame_a.png frame_b.png
///
/// Imprime las filas/columnas con mayor diferencia de color — esas son las
/// zonas dinámicas donde están HP/mana bars, battle list, etc.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Uso: diff_frames frame_a.png frame_b.png [x] [y] [w] [h]");
        return;
    }

    let a = image::open(&args[1]).expect("no se pudo abrir frame_a").to_rgba8();
    let b = image::open(&args[2]).expect("no se pudo abrir frame_b").to_rgba8();
    assert_eq!(a.dimensions(), b.dimensions(), "Frames de distinto tamaño");

    let (fw, fh) = a.dimensions();

    // Región de búsqueda (default: todo el frame).
    let x0: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let y0: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
    let w:  u32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(fw - x0);
    let h:  u32 = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(fh - y0);

    println!("Comparando región ({},{}) {}×{}", x0, y0, w, h);
    println!();

    // Calcular diferencia por fila (suma de |a-b| para cada píxel).
    let mut row_diff: Vec<(u32, u64)> = Vec::new();
    for row in 0..h {
        let y = y0 + row;
        let mut sum = 0u64;
        for col in 0..w {
            let x = x0 + col;
            let pa = a.get_pixel(x, y);
            let pb = b.get_pixel(x, y);
            for c in 0..3 {
                sum += (pa[c] as i32 - pb[c] as i32).unsigned_abs() as u64;
            }
        }
        row_diff.push((y, sum));
    }

    // Calcular diferencia por columna.
    let mut col_diff: Vec<(u32, u64)> = Vec::new();
    for col in 0..w {
        let x = x0 + col;
        let mut sum = 0u64;
        for row in 0..h {
            let y = y0 + row;
            let pa = a.get_pixel(x, y);
            let pb = b.get_pixel(x, y);
            for c in 0..3 {
                sum += (pa[c] as i32 - pb[c] as i32).unsigned_abs() as u64;
            }
        }
        col_diff.push((x, sum));
    }

    // Top 20 filas con más diferencia.
    row_diff.sort_by(|a, b| b.1.cmp(&a.1));
    println!("Top 20 filas con más cambio (y → diff):");
    for (y, d) in row_diff.iter().take(20) {
        println!("  y={:4}: diff={}", y, d);
    }

    println!();

    // Top 20 columnas con más diferencia.
    col_diff.sort_by(|a, b| b.1.cmp(&a.1));
    println!("Top 20 columnas con más cambio (x → diff):");
    for (x, d) in col_diff.iter().take(20) {
        println!("  x={:4}: diff={}", x, d);
    }

    println!();

    // Buscar "barras horizontales de cambio": rangos de filas adyacentes con diff alta.
    // Ordenar de nuevo por y.
    let mut row_diff2 = row_diff.clone();
    row_diff2.sort_by_key(|(y, _)| *y);
    let threshold = row_diff2.iter().map(|(_,d)| *d).sum::<u64>() / row_diff2.len() as u64 * 3;
    println!("Zonas dinámicas (threshold={}): filas con diff > threshold:", threshold);
    let mut in_zone = false;
    let mut zone_start = 0u32;
    let mut zone_max = 0u64;
    for (y, d) in &row_diff2 {
        if *d > threshold {
            if !in_zone { zone_start = *y; in_zone = true; zone_max = 0; }
            if *d > zone_max { zone_max = *d; }
        } else if in_zone {
            println!("  y={} – y={} ({}px), max_diff={}", zone_start, y-1, y - zone_start, zone_max);
            in_zone = false;
        }
    }
    if in_zone {
        println!("  y={} – end, max_diff={}", zone_start, zone_max);
    }
}
