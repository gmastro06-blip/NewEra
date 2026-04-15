/// rgb_dump — Imprime valores RGB exactos de una región pequeña del frame.
/// Uso: rgb_dump frame.png x y w h
/// Útil para encontrar el color exacto de las barras de HP/mana.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        eprintln!("Uso: rgb_dump frame.png x y w h");
        return;
    }

    let img = image::open(&args[1]).expect("no frame").to_rgba8();
    let x0: u32 = args[2].parse().unwrap();
    let y0: u32 = args[3].parse().unwrap();
    let w:  u32 = args[4].parse().unwrap();
    let h:  u32 = args[5].parse().unwrap();

    println!("RGB dump ({},{}) {}×{}", x0, y0, w, h);
    println!();
    println!("     x: {}", (x0..x0+w).map(|x| format!("{:>9}", x)).collect::<Vec<_>>().join(""));
    for row in 0..h {
        let y = y0 + row;
        print!("y={:4}: ", y);
        for col in 0..w {
            let x = x0 + col;
            let px = img.get_pixel(x, y);
            print!("{:3},{:3},{:3} ", px[0], px[1], px[2]);
        }
        println!();
    }
    println!();

    // Resumen: color más frecuente por fila
    println!("Color dominante por fila (muestra 3 píxeles del centro):");
    for row in 0..h {
        let y = y0 + row;
        let mid = x0 + w / 2;
        let px = img.get_pixel(mid, y);
        let px1 = img.get_pixel(x0 + w / 4, y);
        let px2 = img.get_pixel(x0 + w * 3 / 4, y);
        println!("  y={}: [{:3},{:3},{:3}] [{:3},{:3},{:3}] [{:3},{:3},{:3}]",
            y, px1[0], px1[1], px1[2],
               px[0], px[1], px[2],
               px2[0], px2[1], px2[2]);
    }
}
