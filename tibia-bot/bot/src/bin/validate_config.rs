//! validate_config — Sanity check de `bot/config.toml` + `bridge/bridge_config.toml`
//! antes de arrancar una sesión live.
//!
//! Catch errores comunes de setup que si no, descubren en el primer live run
//! tras 10 min de debug:
//!
//! - Paths que no existen (map_index.bin, templates dir, minimap dir)
//! - `matcher_floors` inconsistente con floors cargados
//! - `starting_coord` sin map_index path configurado (→ warning)
//! - HTTP listen_addr con riesgo de LAN leak
//! - Bridge `mode = "sendinput"` (BE detection risk) vs "serial"
//! - Focus `poll_interval_ms` muy bajo (fingerprint)
//! - Safety timing values fuera de rango sensato
//! - Ruta COM del Arduino / serial sin asignar
//!
//! Exit code: 0 = OK, 1 = errors, 2 = uso inválido.
//!
//! Uso:
//!   cargo run --release --bin validate_config -- \
//!       --bot bot/config.toml --bridge bridge/bridge_config.toml
//!
//! Alternativas (ejecutable post-build):
//!   .\target\release\validate_config.exe --bot bot\config.toml

use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bot_path = arg_value(&args, "--bot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("bot/config.toml"));
    let bridge_path = arg_value(&args, "--bridge").map(PathBuf::from);

    let mut errors = 0u32;
    let mut warnings = 0u32;

    println!("=== validate_config ===");
    println!();

    // ── Bot config ────────────────────────────────────────────────────────
    println!("[bot] {}", bot_path.display());
    if !bot_path.exists() {
        println!("  ✗ archivo no existe");
        std::process::exit(1);
    }
    let bot_raw = match std::fs::read_to_string(&bot_path) {
        Ok(s) => s,
        Err(e) => {
            println!("  ✗ no se pudo leer: {}", e);
            std::process::exit(1);
        }
    };
    let bot_toml: toml::Value = match toml::from_str(&bot_raw) {
        Ok(v) => v,
        Err(e) => {
            println!("  ✗ TOML inválido: {}", e);
            std::process::exit(1);
        }
    };
    println!("  ✓ TOML parsed");

    // HTTP listen_addr
    if let Some(addr) = bot_toml.get("http")
        .and_then(|h| h.get("listen_addr"))
        .and_then(|v| v.as_str())
    {
        if addr.starts_with("0.0.0.0") {
            println!(
                "  ⚠ http.listen_addr = '{}' — expuesto a TODA la LAN. \
                 Cualquier host remoto puede scrapear /cavebot/status (hunt_profile, \
                 verifying, current_step) y identificar este host como bot server. \
                 Considerar '127.0.0.1:8080' (loopback only) o firewall rule.",
                addr
            );
            warnings += 1;
        }
    } else {
        println!("  ⚠ http.listen_addr no declarado (default 127.0.0.1:8080)");
        warnings += 1;
    }

    // Game coords config
    let gc = bot_toml.get("game_coords");
    if let Some(gc) = gc {
        let map_path = gc.get("map_index_path").and_then(|v| v.as_str()).unwrap_or("");
        if !map_path.is_empty() {
            let resolved = resolve_relative(&bot_path, map_path);
            if !resolved.exists() {
                println!(
                    "  ✗ game_coords.map_index_path = '{}' no existe (resolved: {})",
                    map_path, resolved.display()
                );
                errors += 1;
            } else {
                println!("  ✓ map_index_path OK ({})", map_path);
            }
        } else {
            println!("  ⓘ game_coords.map_index_path = '' (tile-hashing deshabilitado)");
            // Si hay starting_coord set pero no map_index, advertir.
            if gc.get("starting_coord").is_some() {
                println!(
                    "  ⚠ starting_coord declarado pero map_index_path vacío — el seed se ignora. \
                     Habilitar map_index_path para activar tile-hashing + seed."
                );
                warnings += 1;
            }
        }

        let minimap_dir = gc.get("minimap_dir").and_then(|v| v.as_str()).unwrap_or("");
        if !minimap_dir.is_empty() {
            let resolved = resolve_relative(&bot_path, minimap_dir);
            if !resolved.exists() {
                println!(
                    "  ✗ game_coords.minimap_dir = '{}' no existe (resolved: {})",
                    minimap_dir, resolved.display()
                );
                errors += 1;
            } else {
                // Verificar que tiene PNGs.
                let png_count = std::fs::read_dir(&resolved)
                    .map(|r| r.flatten()
                        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("png"))
                        .count())
                    .unwrap_or(0);
                if png_count == 0 {
                    println!("  ⚠ minimap_dir existe pero no tiene PNGs (esperado Minimap_Color_*.png)");
                    warnings += 1;
                } else {
                    println!("  ✓ minimap_dir OK ({} PNGs)", png_count);
                }
            }
        }

        // Validar starting_coord format si presente.
        if let Some(sc) = gc.get("starting_coord") {
            if let Some(arr) = sc.as_array() {
                if arr.len() != 3 {
                    println!("  ✗ starting_coord: se esperaban 3 elementos [x, y, z], got {}", arr.len());
                    errors += 1;
                } else {
                    let z_val = arr.get(2).and_then(|v| v.as_integer()).unwrap_or(-1);
                    if !(0..=15).contains(&z_val) {
                        println!("  ⚠ starting_coord z={} fuera del rango Tibia [0, 15]", z_val);
                        warnings += 1;
                    } else {
                        // Check que z está en matcher_floors si está set.
                        if let Some(floors_str) = gc.get("matcher_floors").and_then(|v| v.as_str()) {
                            let floors: Vec<i64> = floors_str.split(',')
                                .filter_map(|s| s.trim().parse().ok())
                                .collect();
                            if !floors.contains(&z_val) && !floors.is_empty() {
                                println!(
                                    "  ⚠ starting_coord.z = {} no está en matcher_floors = '{}'. \
                                     El seed apunta a un piso no cargado → primer detect() fallará.",
                                    z_val, floors_str
                                );
                                warnings += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    // Assets existence
    let assets_candidates = [
        ("assets/templates/inventory", "inventory templates dir"),
        ("assets/templates/prompts", "prompts templates dir"),
        ("assets/templates/ui", "ui templates dir"),
        ("assets/templates/anchors", "anchors dir"),
    ];
    for (rel, label) in assets_candidates {
        let resolved = resolve_relative(&bot_path, rel);
        if !resolved.exists() {
            println!("  ⚠ {} ('{}') no existe (optional pero recomendado)", label, rel);
            warnings += 1;
        }
    }

    // Safety config sanity
    if let Some(safety) = bot_toml.get("safety") {
        let humanize = safety.get("humanize_timing").and_then(|v| v.as_bool()).unwrap_or(true);
        if !humanize {
            println!("  ⚠ safety.humanize_timing = false — timing uniforme es detectable por BE");
            warnings += 1;
        }
        let max_h = safety.get("max_session_hours").and_then(|v| v.as_float()).unwrap_or(0.0);
        if max_h > 8.0 {
            println!(
                "  ⚠ safety.max_session_hours = {:.1}h — sesiones largas amplifican fatigue fingerprints",
                max_h
            );
            warnings += 1;
        }
        if max_h == 0.0 {
            println!("  ⓘ safety.max_session_hours = 0 (sesión ilimitada — riesgo detection)");
        }
    } else {
        println!("  ⚠ sección [safety] faltante — humanization deshabilitada");
        warnings += 1;
    }

    // ── Bridge config (opcional) ──────────────────────────────────────────
    if let Some(bp) = bridge_path {
        println!();
        println!("[bridge] {}", bp.display());
        if !bp.exists() {
            println!("  ⚠ archivo no existe — bridge corre con defaults (serial COM5)");
            warnings += 1;
        } else {
            let raw = std::fs::read_to_string(&bp).unwrap_or_default();
            let br: toml::Value = match toml::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    println!("  ✗ TOML inválido: {}", e);
                    std::process::exit(1);
                }
            };
            println!("  ✓ TOML parsed");

            // Input mode
            let mode = br.get("input")
                .and_then(|i| i.get("mode"))
                .and_then(|v| v.as_str())
                .unwrap_or("serial");
            if mode == "sendinput" {
                println!(
                    "  ⚠ input.mode = 'sendinput' — Windows SendInput API es DETECTABLE por \
                     BattleEye y anti-cheats kernel-level. Para live con cuenta real: \
                     usar 'serial' con Arduino Leonardo/Pico."
                );
                warnings += 1;
            } else if mode == "serial" {
                println!("  ✓ input.mode = 'serial' (Arduino HID — anti-detection OK)");
            } else {
                println!("  ✗ input.mode = '{}' — valor desconocido", mode);
                errors += 1;
            }

            // Serial port (solo si mode=serial)
            if mode == "serial" {
                let port = br.get("serial")
                    .and_then(|s| s.get("port"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if port.is_empty() {
                    println!("  ✗ serial.port vacío — configurar COM del Arduino");
                    errors += 1;
                } else if !port.starts_with("COM") && !port.starts_with("/dev/") {
                    println!("  ⚠ serial.port = '{}' formato inusual (esperado COMx o /dev/ttyACMn)", port);
                    warnings += 1;
                } else {
                    println!("  ✓ serial.port = '{}'", port);
                }
            }

            // Focus config
            if let Some(focus) = br.get("focus") {
                let enabled = focus.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                let poll = focus.get("poll_interval_ms").and_then(|v| v.as_integer()).unwrap_or(2000);
                if enabled && poll < 500 {
                    println!(
                        "  ⚠ focus.poll_interval_ms = {} < 500ms — GetForegroundWindow polling \
                         a alta frecuencia es fingerprint típico de bot. Recomendado ≥ 1000ms.",
                        poll
                    );
                    warnings += 1;
                } else if enabled {
                    println!("  ✓ focus.enabled + poll_interval_ms = {}", poll);
                }
            }
        }
    }

    println!();
    println!("Summary: {} errors, {} warnings", errors, warnings);
    std::process::exit(if errors > 0 { 1 } else { 0 });
}

/// Resuelve un path relativo al directorio del config file (no al CWD).
fn resolve_relative(config_path: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        config_path
            .parent()
            .and_then(Path::parent) // bot/config.toml → bot/ → <repo>
            .map(|repo| repo.join(rel))
            .unwrap_or_else(|| PathBuf::from(rel))
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == flag)?;
    args.get(pos + 1).cloned()
}
