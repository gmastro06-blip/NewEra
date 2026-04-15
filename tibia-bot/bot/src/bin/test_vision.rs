/// test_vision — Verifica la visión leyendo frame_reference.png con calibration.toml real.
///
/// Uso: test_vision [frame.png] [assets_dir]
///
/// Imprime los valores detectados de HP, mana, etc.
/// Útil para verificar la calibración antes de usar el bot en vivo.
use std::path::PathBuf;
use serde::Deserialize;

#[derive(Deserialize)]
struct Calibration {
    hp_bar:     Option<Roi>,
    mana_bar:   Option<Roi>,
    battle_list: Option<Roi>,
    minimap:    Option<Roi>,
}

#[derive(Deserialize, Clone, Copy, Debug)]
struct Roi {
    x: u32, y: u32, w: u32, h: u32,
}

fn main() {
    let frame_path = std::env::args().nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("frame_reference.png"));
    let assets_dir = std::env::args().nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets"));

    // Cargar imagen como BGRA.
    let img = match image::open(&frame_path) {
        Ok(i)  => i,
        Err(e) => { eprintln!("ERROR cargando frame: {}", e); return; }
    };
    let rgba = img.to_rgba8();
    let (fw, fh) = rgba.dimensions();

    // Convertir RGBA → BGRA.
    let mut bgra = rgba.into_raw();
    for c in bgra.chunks_exact_mut(4) { c.swap(0, 2); }

    println!("Frame: {}×{}", fw, fh);

    // Cargar calibración.
    let cal_path = assets_dir.join("calibration.toml");
    let raw = match std::fs::read_to_string(&cal_path) {
        Ok(r)  => r,
        Err(e) => { eprintln!("ERROR leyendo calibration.toml: {}", e); return; }
    };
    let cal: Calibration = match toml::from_str(&raw) {
        Ok(c)  => c,
        Err(e) => { eprintln!("ERROR parseando calibration.toml: {}", e); return; }
    };

    println!("Calibración: {}", cal_path.display());
    println!();

    // Leer HP.
    if let Some(roi) = cal.hp_bar {
        let (filled, total) = count_color(&bgra, fw, roi, is_hp_green);
        let ratio = if total > 0 { filled as f32 / total as f32 } else { 0.0 };
        println!("HP   ({},{}) {}×{}: {}/{} px verdes → {:.1}%",
            roi.x, roi.y, roi.w, roi.h, filled, total, ratio * 100.0);
        if ratio > 0.95 { println!("  → HP: LLENO"); }
        else if ratio > 0.5 { println!("  → HP: alto (>{:.0}%)", ratio * 100.0); }
        else if ratio > 0.3 { println!("  → HP: medio ({:.0}%)", ratio * 100.0); }
        else { println!("  → HP: CRÍTICO ({:.0}%)", ratio * 100.0); }
    } else {
        println!("HP: sin calibrar");
    }

    // Leer mana.
    if let Some(roi) = cal.mana_bar {
        let (filled, total) = count_color(&bgra, fw, roi, is_mana_blue);
        let ratio = if total > 0 { filled as f32 / total as f32 } else { 0.0 };
        println!("Mana ({},{}) {}×{}: {}/{} px azules → {:.1}%",
            roi.x, roi.y, roi.w, roi.h, filled, total, ratio * 100.0);
        if ratio > 0.95 { println!("  → Mana: LLENO"); }
        else if ratio > 0.3 { println!("  → Mana: {:.0}%", ratio * 100.0); }
        else { println!("  → Mana: CRÍTICO ({:.0}%)", ratio * 100.0); }
    } else {
        println!("Mana: sin calibrar");
    }

    // Battle list: contar entradas con borde rojo.
    if let Some(roi) = cal.battle_list {
        let entry_h = 22u32;
        let n_rows  = roi.h / entry_h;
        let mut monster_count = 0u32;
        for row in 0..n_rows {
            let border_roi = Roi { x: roi.x, y: roi.y + row * entry_h, w: 3, h: entry_h };
            let (red, _) = count_color(&bgra, fw, border_roi, is_monster_red);
            if red >= 2 { monster_count += 1; }
        }
        println!("Battle list ({},{}) {}×{}: {} monstruos detectados",
            roi.x, roi.y, roi.w, roi.h, monster_count);
    }

    // Minimap: promedio de color.
    if let Some(roi) = cal.minimap {
        let (sum_r, sum_g, sum_b, n) = avg_color(&bgra, fw, roi);
        if n > 0 {
            println!("Minimap ({},{}) {}×{}: avg RGB=({},{},{})",
                roi.x, roi.y, roi.w, roi.h,
                sum_r / n, sum_g / n, sum_b / n);
        }
    }

    println!();
    println!("test_vision: OK");
}

fn count_color<F>(bgra: &[u8], fw: u32, roi: Roi, pred: F) -> (u32, u32)
where
    F: Fn(u8, u8, u8) -> bool,
{
    let stride = fw as usize * 4;
    let (mut filled, mut total) = (0u32, 0u32);
    for row in 0..roi.h {
        let ay = (roi.y + row) as usize;
        for col in 0..roi.w {
            let ax = (roi.x + col) as usize;
            let off = ay * stride + ax * 4;
            if off + 2 < bgra.len() {
                total += 1;
                if pred(bgra[off], bgra[off+1], bgra[off+2]) { filled += 1; }
            }
        }
    }
    (filled, total)
}

fn avg_color(bgra: &[u8], fw: u32, roi: Roi) -> (u32, u32, u32, u32) {
    let stride = fw as usize * 4;
    let (mut sr, mut sg, mut sb, mut n) = (0u32, 0u32, 0u32, 0u32);
    for row in 0..roi.h {
        let ay = (roi.y + row) as usize;
        for col in 0..roi.w {
            let ax = (roi.x + col) as usize;
            let off = ay * stride + ax * 4;
            if off + 2 < bgra.len() {
                sb += bgra[off]   as u32;
                sg += bgra[off+1] as u32;
                sr += bgra[off+2] as u32;
                n  += 1;
            }
        }
    }
    (sr, sg, sb, n)
}

// Predicados BGRA.
fn is_hp_green(b: u8, g: u8, r: u8) -> bool { r < 80 && g > 140 && b < 80 }
fn is_mana_blue(b: u8, g: u8, r: u8) -> bool { r < 80 && g < 100 && b > 140 }
fn is_monster_red(b: u8, g: u8, r: u8) -> bool { r > 180 && g < 60 && b < 60 }
