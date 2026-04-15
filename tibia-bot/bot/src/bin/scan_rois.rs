/// scan_rois — Auto-detecta ROIs de HP, mana, battle list y minimapa
/// escaneando frame_reference.png por colores y regiones conocidas.
///
/// Uso: scan_rois [frame.png]
///
/// Imprime las coordenadas detectadas listas para pegar en calibration.toml.
#[path = "../sense/vision/calibration.rs"]
mod calibration;

use std::path::PathBuf;
use calibration::RoiDef;

fn main() {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("frame_reference.png"));

    let img = match image::open(&path) {
        Ok(i)  => i.to_rgba8(),
        Err(e) => { eprintln!("ERROR: No se pudo abrir '{}': {}", path.display(), e); return; }
    };

    let (w, h) = img.dimensions();
    println!("Frame: {}x{}", w, h);

    // ── HP bar (verde) ────────────────────────────────────────────────────────
    if let Some(roi) = find_bar_roi(&img, is_hp_green, "HP (verde)") {
        println!("hp_bar   = {{ x = {}, y = {}, w = {}, h = {} }}", roi.x, roi.y, roi.w, roi.h);
    }

    // ── Mana bar (azul) ───────────────────────────────────────────────────────
    if let Some(roi) = find_bar_roi(&img, is_mana_blue, "Mana (azul)") {
        println!("mana_bar = {{ x = {}, y = {}, w = {}, h = {} }}", roi.x, roi.y, roi.w, roi.h);
    }

    // ── Battle list (borde rojo) ──────────────────────────────────────────────
    if let Some(roi) = find_battle_panel(&img) {
        println!("battle_list = {{ x = {}, y = {}, w = {}, h = {} }}", roi.x, roi.y, roi.w, roi.h);
    }

    // ── Minimap (región densa con variedad de colores en esquina superior derecha) ──
    if let Some(roi) = find_minimap(&img) {
        println!("minimap = {{ x = {}, y = {}, w = {}, h = {} }}", roi.x, roi.y, roi.w, roi.h);
    }

    // Guardar automáticamente si se detecta al menos HP y mana.
    println!();
    println!("# Para usar: descomentar estas líneas en assets/calibration.toml");
}

// ── Predicados de color (RGBA, no BGRA) ──────────────────────────────────────
// image::open devuelve RGBA, a diferencia de NDI que da BGRA.

fn is_hp_green(px: &image::Rgba<u8>) -> bool {
    let r = px[0]; let g = px[1]; let b = px[2];
    r < 80 && g > 140 && b < 80
}

fn is_mana_blue(px: &image::Rgba<u8>) -> bool {
    let r = px[0]; let g = px[1]; let b = px[2];
    r < 80 && g < 100 && b > 140
}

fn is_monster_red(px: &image::Rgba<u8>) -> bool {
    let r = px[0]; let g = px[1]; let b = px[2];
    r > 160 && g < 80 && b < 80
}

// ── Detección de barras horizontales ─────────────────────────────────────────

fn find_bar_roi<F>(
    img:    &image::RgbaImage,
    pred:   F,
    label:  &str,
) -> Option<RoiDef>
where
    F: Fn(&image::Rgba<u8>) -> bool,
{
    let (w, h) = img.dimensions();

    // Buscar el primer bloque de píxeles consecutivos del color.
    // Estrategia: para cada fila, contar píxeles del color.
    // La fila con más hits consecutivos es la barra.
    let mut best_row = 0u32;
    let mut best_count = 0u32;
    let mut best_start_x = 0u32;

    for y in 0..h {
        let mut run_start = 0u32;
        let mut run_len   = 0u32;

        for x in 0..w {
            if pred(img.get_pixel(x, y)) {
                if run_len == 0 { run_start = x; }
                run_len += 1;
            } else {
                if run_len > best_count {
                    best_count   = run_len;
                    best_row     = y;
                    best_start_x = run_start;
                }
                run_len = 0;
            }
        }
        if run_len > best_count {
            best_count   = run_len;
            best_row     = y;
            best_start_x = run_start;
        }
    }

    if best_count < 20 {
        eprintln!("  {label}: no encontrado (máx run = {} px)", best_count);
        return None;
    }

    // Expandir verticalmente: encontrar todas las filas adyacentes con píxeles del color.
    let min_hits_per_row = (best_count as f32 * 0.5) as u32;
    let mut top = best_row;
    let mut bot = best_row;

    while top > 0 {
        let count = count_row(img, top - 1, best_start_x, best_start_x + best_count, &pred);
        if count < min_hits_per_row { break; }
        top -= 1;
    }
    while bot + 1 < h {
        let count = count_row(img, bot + 1, best_start_x, best_start_x + best_count, &pred);
        if count < min_hits_per_row { break; }
        bot += 1;
    }

    let roi = RoiDef::new(best_start_x, top, best_count, bot - top + 1);
    eprintln!("  {label}: ({}, {}) {}×{} — {} px en fila {}", roi.x, roi.y, roi.w, roi.h, best_count, best_row);
    Some(roi)
}

fn count_row<F>(
    img:   &image::RgbaImage,
    y:     u32,
    x0:    u32,
    x1:    u32,
    pred:  &F,
) -> u32
where
    F: Fn(&image::Rgba<u8>) -> bool,
{
    (x0..x1).filter(|&x| pred(img.get_pixel(x, y))).count() as u32
}

// ── Detección del panel de batalla ───────────────────────────────────────────

/// Busca la columna más a la derecha que tenga runs verticales de rojo de monstruo.
/// El panel de batalla tiene entradas con borde rojo a la izquierda.
fn find_battle_panel(img: &image::RgbaImage) -> Option<RoiDef> {
    let (w, h) = img.dimensions();

    // Buscar en la mitad derecha de la imagen.
    let search_x_start = w * 3 / 4;

    let mut best_x = 0u32;
    let mut best_count = 0u32;

    for x in search_x_start..w.saturating_sub(10) {
        let count = (0..h).filter(|&y| is_monster_red(img.get_pixel(x, y))).count() as u32;
        if count > best_count {
            best_count = count;
            best_x = x;
        }
    }

    if best_count < 3 {
        eprintln!("  Battle list: no encontrado (máx rojo vertical = {})", best_count);
        return None;
    }

    // Encontrar bounding box del panel.
    // El panel va desde best_x hasta el borde derecho de la pantalla.
    let panel_w = w.saturating_sub(best_x);

    // Encontrar rango vertical de las entradas.
    let mut top = h;
    let mut bot = 0u32;
    for y in 0..h {
        if is_monster_red(img.get_pixel(best_x, y)) {
            if y < top { top = y; }
            if y > bot { bot = y; }
        }
    }

    if top >= bot {
        return None;
    }

    // Expandir el área verticalmente para incluir el panel completo.
    let top_padded = top.saturating_sub(30);
    let bot_padded = (bot + 100).min(h);

    let roi = RoiDef::new(best_x, top_padded, panel_w, bot_padded - top_padded);
    eprintln!("  Battle list: ({}, {}) {}×{}", roi.x, roi.y, roi.w, roi.h);
    Some(roi)
}

// ── Detección del minimapa ────────────────────────────────────────────────────

/// Busca una región cuadrada con alta variedad de colores en la esquina superior derecha.
/// El minimapa tiene muchos colores distintos (terreno variado).
fn find_minimap(img: &image::RgbaImage) -> Option<RoiDef> {
    let (w, h) = img.dimensions();

    // El minimapa está en la esquina superior derecha del sidebar.
    // Buscar en el cuadrante superior-derecho.
    let search_x = w * 3 / 4;
    let search_y_end = h / 4;

    let block_size = 80u32;
    let step = 10u32;

    let mut best_x = 0u32;
    let mut best_y = 0u32;
    let mut best_variety = 0usize;

    let mut x = search_x;
    while x + block_size < w {
        let mut y = 0u32;
        while y + block_size < search_y_end {
            let variety = color_variety(img, x, y, block_size);
            if variety > best_variety {
                best_variety = variety;
                best_x = x;
                best_y = y;
            }
            y += step;
        }
        x += step;
    }

    if best_variety < 20 {
        eprintln!("  Minimap: no encontrado (variedad = {})", best_variety);
        return None;
    }

    let roi = RoiDef::new(best_x, best_y, block_size, block_size);
    eprintln!("  Minimap: ({}, {}) {}×{} — variedad={}", roi.x, roi.y, roi.w, roi.h, best_variety);
    Some(roi)
}

/// Cuenta cuántos colores distintos (cuantizados a 4 bits por canal) hay en un bloque.
fn color_variety(img: &image::RgbaImage, x0: u32, y0: u32, size: u32) -> usize {
    use std::collections::HashSet;
    let mut colors = HashSet::new();
    for dy in 0..size {
        for dx in 0..size {
            let px = img.get_pixel(x0 + dx, y0 + dy);
            let key = ((px[0] >> 4) as u32) << 8 | ((px[1] >> 4) as u32) << 4 | (px[2] >> 4) as u32;
            colors.insert(key);
        }
    }
    colors.len()
}
