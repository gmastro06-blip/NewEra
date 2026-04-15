//! synth_frames — Generador de frames sintéticos para tests de integración.
//!
//! Genera PNGs 1920x1080 RGBA que simulan escenarios del juego (HP bajo,
//! combate, nominal, etc). Los ROIs se pintan leyendo `assets/calibration.toml`
//! para que se alineen con la vision activa del bot.
//!
//! Se usa como parte de `test_smoke/integration_tests.py` — no es un bot binary,
//! es una herramienta de desarrollo.
//!
//! ## Uso
//!
//! ```bash
//! # Frame nominal: HP full, mana full, sin enemigos, sin prompts.
//! cargo run --release --bin synth_frames -- \
//!     --out test_smoke/frames/nominal.png
//!
//! # Frame con HP crítico (20%).
//! cargo run --release --bin synth_frames -- \
//!     --hp-ratio 0.20 --out test_smoke/frames/low_hp.png
//!
//! # Frame con enemigo en battle list.
//! cargo run --release --bin synth_frames -- \
//!     --enemies 1 --out test_smoke/frames/combat.png
//!
//! # Frame con login prompt (pinta un rectángulo de color uniforme en la ROI).
//! cargo run --release --bin synth_frames -- \
//!     --prompt login --out test_smoke/frames/login.png
//!
//! # Prompts soportados: login, char_select, npc_trade
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use image::{ImageBuffer, Rgba, RgbaImage};

// Reutilizamos la misma estructura de calibración que lee el bot.
#[path = "../sense/vision/calibration.rs"]
mod calibration;

use calibration::{Calibration, RoiDef};

const FRAME_W: u32 = 1920;
const FRAME_H: u32 = 1080;

// Colores clave que los detectores del bot reconocen (ver color.rs).
// HP bar verde fuerte.
const HP_GREEN:   Rgba<u8> = Rgba([0x20, 0xD8, 0x20, 0xFF]);
const HP_EMPTY:   Rgba<u8> = Rgba([0x30, 0x30, 0x30, 0xFF]);
// Mana bar azul.
const MANA_BLUE:  Rgba<u8> = Rgba([0x30, 0x30, 0xE0, 0xFF]);
const MANA_EMPTY: Rgba<u8> = Rgba([0x30, 0x30, 0x30, 0xFF]);
// Monster border rojo.
const MONSTER_RED: Rgba<u8> = Rgba([0xE0, 0x20, 0x20, 0xFF]);
// Luma clave para prompts (detector hace template matching en gris).
const PROMPT_FILL: Rgba<u8> = Rgba([0xC8, 0xC8, 0xC8, 0xFF]); // luma ~200

// Entrada: height per battle list row (de battle_list.rs).
const BATTLE_ENTRY_H: u32 = 22;
const BATTLE_BORDER_W: u32 = 3;

#[derive(Debug)]
struct Args {
    hp_ratio:    f32,
    mana_ratio:  f32,
    enemies:     u32,
    prompt:      Option<String>,
    out:         PathBuf,
    calibration: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut hp_ratio: f32 = 1.0;
    let mut mana_ratio: f32 = 1.0;
    let mut enemies: u32 = 0;
    let mut prompt: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut calibration = PathBuf::from("assets/calibration.toml");

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--hp-ratio"   => { hp_ratio   = raw[i + 1].parse()?; i += 2; }
            "--mana-ratio" => { mana_ratio = raw[i + 1].parse()?; i += 2; }
            "--enemies"    => { enemies    = raw[i + 1].parse()?; i += 2; }
            "--prompt"     => { prompt = Some(raw[i + 1].clone()); i += 2; }
            "--out"        => { out = Some(PathBuf::from(&raw[i + 1])); i += 2; }
            "--calibration" => { calibration = PathBuf::from(&raw[i + 1]); i += 2; }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => anyhow::bail!("flag desconocido: {}", other),
        }
    }

    Ok(Args {
        hp_ratio, mana_ratio, enemies, prompt,
        out: out.context("--out es requerido")?,
        calibration,
    })
}

fn print_help() {
    println!("synth_frames — Generador de frames sintéticos");
    println!();
    println!("Flags:");
    println!("  --hp-ratio FLOAT       Fill ratio de la HP bar (0.0-1.0, default 1.0)");
    println!("  --mana-ratio FLOAT     Fill ratio de la mana bar (0.0-1.0, default 1.0)");
    println!("  --enemies N            Número de monsters en battle list (default 0)");
    println!("  --prompt KIND          Pintar prompt sintético: login|char_select|npc_trade");
    println!("  --calibration PATH     Ruta a calibration.toml (default assets/calibration.toml)");
    println!("  --out PATH             Archivo PNG de salida (REQUERIDO)");
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let cal = Calibration::load(&args.calibration)
        .with_context(|| format!("Cargando calibration: {}", args.calibration.display()))?;

    let mut img: RgbaImage = ImageBuffer::from_pixel(
        FRAME_W, FRAME_H,
        Rgba([0x20, 0x20, 0x20, 0xFF]), // fondo gris oscuro
    );

    // ── HP bar ────────────────────────────────────────────────────────────
    if let Some(roi) = cal.hp_bar {
        paint_horizontal_bar(&mut img, roi, args.hp_ratio, HP_GREEN, HP_EMPTY);
    }

    // ── Mana bar ──────────────────────────────────────────────────────────
    if let Some(roi) = cal.mana_bar {
        paint_horizontal_bar(&mut img, roi, args.mana_ratio, MANA_BLUE, MANA_EMPTY);
    }

    // ── Battle list: N entradas con borde rojo ────────────────────────────
    if let Some(roi) = cal.battle_list {
        paint_battle_list(&mut img, roi, args.enemies);
    }

    // ── Prompts (opcional): pinta un rectángulo uniforme en el ROI ─────────
    if let Some(kind) = &args.prompt {
        let prompt_roi = match kind.as_str() {
            "npc_trade" => cal.prompt_npc_trade,
            other       => anyhow::bail!(
                "prompt desconocido: '{}' — válidos: npc_trade", other
            ),
        };
        if let Some(roi) = prompt_roi {
            fill_rect(&mut img, roi, PROMPT_FILL);
        } else {
            eprintln!("warning: calibration.toml no tiene ROI para prompt '{}'", kind);
        }
    }

    // Asegurar el directorio existe.
    if let Some(parent) = args.out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    img.save(&args.out)
        .with_context(|| format!("Guardando {}", args.out.display()))?;
    println!("✓ frame sintético: {} ({}×{})", args.out.display(), FRAME_W, FRAME_H);
    println!("  hp={:.2} mana={:.2} enemies={} prompt={:?}",
             args.hp_ratio, args.mana_ratio, args.enemies, args.prompt);
    Ok(())
}

/// Pinta una barra horizontal con relleno proporcional.
/// Los últimos píxeles se dejan vacíos para simular la parte "no rellena".
fn paint_horizontal_bar(
    img: &mut RgbaImage,
    roi: RoiDef,
    ratio: f32,
    filled: Rgba<u8>,
    empty:  Rgba<u8>,
) {
    let ratio = ratio.clamp(0.0, 1.0);
    let fill_cols = (roi.w as f32 * ratio).round() as u32;
    for dy in 0..roi.h {
        for dx in 0..roi.w {
            let x = roi.x + dx;
            let y = roi.y + dy;
            if x >= FRAME_W || y >= FRAME_H { continue; }
            let color = if dx < fill_cols { filled } else { empty };
            img.put_pixel(x, y, color);
        }
    }
}

/// Pinta N entradas de battle list con borde rojo (monster).
/// Las entradas se pintan desde la parte superior del ROI hacia abajo.
fn paint_battle_list(img: &mut RgbaImage, roi: RoiDef, n: u32) {
    for row in 0..n {
        let entry_y = roi.y + row * BATTLE_ENTRY_H;
        if entry_y + BATTLE_ENTRY_H > roi.y + roi.h { break; }
        // Borde izquierdo de BATTLE_BORDER_W píxeles en rojo.
        for dy in 0..BATTLE_ENTRY_H {
            for dx in 0..BATTLE_BORDER_W {
                let x = roi.x + dx;
                let y = entry_y + dy;
                if x >= FRAME_W || y >= FRAME_H { continue; }
                img.put_pixel(x, y, MONSTER_RED);
            }
        }
    }
}

/// Rellena un rectángulo con un color uniforme (para prompts sintéticos).
fn fill_rect(img: &mut RgbaImage, roi: RoiDef, color: Rgba<u8>) {
    for dy in 0..roi.h {
        for dx in 0..roi.w {
            let x = roi.x + dx;
            let y = roi.y + dy;
            if x >= FRAME_W || y >= FRAME_H { continue; }
            img.put_pixel(x, y, color);
        }
    }
}
