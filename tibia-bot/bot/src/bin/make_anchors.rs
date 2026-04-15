/// make_anchors — Recorta regiones de referencia del frame de referencia
/// y las guarda como imágenes en escala de grises en assets/anchors/.
///
/// Uso: make_anchors [frame.png] [assets_dir]
///
/// Genera:
///   assets/anchors/sidebar_top.png  — borde superior del panel lateral (x=1683,y=0,w=90,h=70)
///
/// Para agregar más anclas: duplicar el bloque de cada Anchor abajo.
/// Los tamaños mínimos recomendados son 20×20 px.
fn main() {
    let frame_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "frame_reference.png".to_string());
    let assets_dir = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "assets".to_string());

    let img = match image::open(&frame_path) {
        Ok(i)  => i.to_rgba8(),
        Err(e) => { eprintln!("ERROR abriendo '{}': {}", frame_path, e); return; }
    };
    let (fw, fh) = img.dimensions();
    println!("Frame: {}×{}", fw, fh);

    let anchors_dir = std::path::PathBuf::from(&assets_dir).join("anchors");
    if let Err(e) = std::fs::create_dir_all(&anchors_dir) {
        eprintln!("ERROR creando directorio '{}': {}", anchors_dir.display(), e);
        return;
    }

    // ── Definición de anclas ──────────────────────────────────────────────────
    // Cada ancla es: (nombre, x, y, w, h)
    // Elegir regiones de la UI que sean: estables, distintivas, ≥20×20 px.
    let anchors: &[(&str, u32, u32, u32, u32)] = &[
        // Top del panel lateral derecho — completamente dentro del sidebar (x > 1690).
        // x=1683 era problemático: incluía ~7px del game world edge.
        // x=1700 garantiza que todo el patch es sidebar UI estable.
        ("sidebar_top", 1700, 0, 80, 70),
    ];

    for &(name, ax, ay, aw, ah) in anchors {
        if ax + aw > fw || ay + ah > fh {
            eprintln!("SKIP '{}': ROI ({},{},{},{}) fuera del frame {}×{}",
                name, ax, ay, aw, ah, fw, fh);
            continue;
        }

        // Recortar patch del frame.
        let patch = image::imageops::crop_imm(&img, ax, ay, aw, ah).to_image();

        // Convertir a escala de grises (Rec.601: 0.299R + 0.587G + 0.114B).
        // El AnchorTracker usa la misma fórmula en extract_gray().
        let gray = image::imageops::grayscale(&patch);

        let out_path = anchors_dir.join(format!("{}.png", name));
        match gray.save(&out_path) {
            Ok(_)  => println!("Ancla '{}' guardada: {} ({}×{} px)", name, out_path.display(), aw, ah),
            Err(e) => eprintln!("ERROR guardando '{}': {}", out_path.display(), e),
        }
    }

    println!("\nHecho. Registrar anclas en calibration.toml:");
    println!("  [[anchors]]");
    println!("  name         = \"sidebar_top\"");
    println!("  template_path = \"sidebar_top.png\"");
    println!("  expected_roi  = {{ x = 1700, y = 0, w = 80, h = 70 }}");
}
