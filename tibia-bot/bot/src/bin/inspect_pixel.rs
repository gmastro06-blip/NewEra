/// inspect_pixel — Imprime los valores RGBA de una región del frame para debugging.
/// Uso: inspect_pixel [frame.png] x y w h
///
/// También muestra un "mapa ASCII" de qué pixels son HP-verde, mana-azul, etc.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        eprintln!("Uso: inspect_pixel frame.png x y w h");
        return;
    }

    let path = &args[1];
    let x: u32 = args[2].parse().unwrap();
    let y: u32 = args[3].parse().unwrap();
    let w: u32 = args[4].parse().unwrap();
    let h: u32 = args[5].parse().unwrap();

    let img = match image::open(path) {
        Ok(i)  => i.to_rgba8(),
        Err(e) => { eprintln!("ERROR: {}", e); return; }
    };

    println!("Región ({},{}) {}×{}", x, y, w, h);
    println!("G=HP verde  B=mana azul  R=monster rojo  .=otro");
    println!();

    for row in 0..h {
        print!("y={:4}: ", y + row);
        for col in 0..w {
            let px = img.get_pixel(x + col, y + row);
            let r = px[0]; let g = px[1]; let b = px[2];
            let c = if r < 80 && g > 140 && b < 80    { 'G' }
                    else if r < 80 && g < 100 && b > 140 { 'B' }
                    else if r > 160 && g < 80 && b < 80  { 'R' }
                    else { '.' };
            print!("{}", c);
        }
        // Mostrar conteo de píxeles de cada tipo
        let g_count = (0..w).filter(|&c| {
            let px = img.get_pixel(x + c, y + row);
            px[0] < 80 && px[1] > 140 && px[2] < 80
        }).count();
        let b_count = (0..w).filter(|&c| {
            let px = img.get_pixel(x + c, y + row);
            px[0] < 80 && px[1] < 100 && px[2] > 140
        }).count();
        println!("  G={:3} B={:3}", g_count, b_count);
    }

    // Resumen: para cada fila, qué barra de color tiene
    println!();
    println!("Resumen por fila (mín 5 píxeles del color):");
    for row in 0..h.min(80) {
        let g = (0..img.width()).filter(|&c| {
            let px = img.get_pixel(c, y + row);
            px[0] < 80 && px[1] > 140 && px[2] < 80
        }).count();
        let b = (0..img.width()).filter(|&c| {
            let px = img.get_pixel(c, y + row);
            px[0] < 80 && px[1] < 100 && px[2] > 140
        }).count();
        if g >= 5 || b >= 5 {
            println!("  y={}: G={} B={}", y + row, g, b);
        }
    }
}
