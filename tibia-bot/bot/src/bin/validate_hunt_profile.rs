//! validate_hunt_profile — Validador estático de archivos `assets/hunts/<name>.toml`.
//!
//! Carga el profile via el parser real y chequea:
//!   - Schema OK (parsea sin errors)
//!   - `[supplies]` items tienen template matching en `assets/templates/inventory/`
//!     (si no, `check_supplies from_profile=true` siempre fallará porque
//!     `inventory_counts[item] == 0`).
//!   - `[loot].stackables` items tienen template matching (usado por
//!     `stow_all_items from_profile=true` como whitelist del pre-check).
//!   - `level_range` coherente (min <= max, dentro de rangos Tibia 8-999).
//!   - `vocation` es uno de los valores esperados (druid/knight/paladin/sorcerer).
//!   - `[metrics]` baseline values razonables (xp > 0 si presente, kills
//!     proporcional, etc).
//!   - `[calibration_hints].stow_bags` índices consistentes con backpack_count.
//!
//! Uso:
//!   cargo run --release --bin validate_hunt_profile -- assets/hunts/abdendriel_wasps.toml
//!   cargo run --release --bin validate_hunt_profile -- assets/hunts/abdendriel_wasps.toml --templates assets/templates/inventory
//!
//! Exit code: 0 = sin errores. 1 = errores. 2 = uso inválido.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tibia_bot::cavebot::hunt_profile::HuntProfile;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Uso: validate_hunt_profile <profile.toml> [--templates <dir>]");
        std::process::exit(2);
    }

    let profile_path = PathBuf::from(&args[1]);
    let templates_dir = arg_value(&args, "--templates")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/templates/inventory"));

    println!("Validating {}...", profile_path.display());

    let profile = match HuntProfile::load(&profile_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✗ Parser error: {:#}", e);
            std::process::exit(1);
        }
    };
    println!("  ✓ Profile '{}' parsed OK", profile.name);

    // Templates conocidos (para validar refs a inventory templates).
    let known_templates = collect_template_names(&templates_dir);
    if known_templates.is_empty() {
        println!(
            "  ⓘ no se encontraron templates en '{}' — validación de items skipeada",
            templates_dir.display()
        );
    } else {
        println!(
            "  ⓘ {} templates conocidos en '{}'",
            known_templates.len(),
            templates_dir.display()
        );
    }

    let mut errors = 0u32;
    let mut warnings = 0u32;

    // ── Check 1: name matches filename ────────────────────────────────
    if let Some(stem) = profile_path.file_stem().and_then(|s| s.to_str()) {
        if stem != profile.name {
            println!(
                "  ⚠ `name = \"{}\"` no matches filename '{}' — load_by_name('{}') no encontrará este profile",
                profile.name, stem, profile.name
            );
            warnings += 1;
        }
    }

    // ── Check 2: vocation ─────────────────────────────────────────────
    if let Some(voc) = &profile.vocation {
        const VALID: &[&str] = &["druid", "knight", "paladin", "sorcerer"];
        if !VALID.contains(&voc.to_lowercase().as_str()) {
            println!(
                "  ⚠ vocation '{}' no es uno de los valores esperados ({:?})",
                voc, VALID
            );
            warnings += 1;
        }
    }

    // ── Check 3: level_range ──────────────────────────────────────────
    if let Some([min, max]) = profile.level_range {
        if min > max {
            println!("  ✗ level_range [{}, {}]: min > max", min, max);
            errors += 1;
        }
        if min < 1 || max > 999 {
            println!(
                "  ⚠ level_range [{}, {}]: valores fuera del rango típico Tibia (1-999)",
                min, max
            );
            warnings += 1;
        }
    }

    // ── Check 4: [supplies] items tienen templates ────────────────────
    if !profile.supplies.is_empty() {
        for (item, cfg) in &profile.supplies {
            if !known_templates.is_empty() && !known_templates.contains(item) {
                println!(
                    "  ✗ [supplies].{}: item '{}' sin template en '{}'. \
                     `check_supplies from_profile=true` fallará: inventory_counts[{}] == 0.",
                    item, item, templates_dir.display(), item
                );
                errors += 1;
            }
            if cfg.min > cfg.target {
                println!(
                    "  ⚠ [supplies].{}: min={} > target={}. El refill compraría menos de lo que necesita para cruzar el umbral.",
                    item, cfg.min, cfg.target
                );
                warnings += 1;
            }
            if cfg.min == 0 {
                println!(
                    "  ⚠ [supplies].{}: min=0 — el check_supplies siempre pasa. ¿Es intencional?",
                    item
                );
                warnings += 1;
            }
        }
    }

    // ── Check 5: [loot].stackables items tienen templates ─────────────
    if !profile.loot.stackables.is_empty() {
        for item in &profile.loot.stackables {
            if !known_templates.is_empty() && !known_templates.contains(item) {
                println!(
                    "  ⚠ [loot].stackables: '{}' sin template en '{}'. \
                     `stow_all_items from_profile=true` no podrá detectar este item en el bag (pre-check lo verá como 0 unidades).",
                    item, templates_dir.display()
                );
                warnings += 1;
            }
        }
    }

    // ── Check 6: [monsters] no vacío ──────────────────────────────────
    if profile.monsters.expected.is_empty() && profile.monsters.avoid.is_empty() {
        println!("  ⓘ [monsters] vacío — battle list validator sin información");
    }
    // Detectar overlap expected ∩ avoid (incoherente).
    let expected_set: HashSet<String> = profile.monsters.expected
        .iter().map(|s| s.to_lowercase()).collect();
    for avoid in &profile.monsters.avoid {
        if expected_set.contains(&avoid.to_lowercase()) {
            println!(
                "  ✗ [monsters]: '{}' está tanto en expected como en avoid. Elegir uno.",
                avoid
            );
            errors += 1;
        }
    }

    // ── Check 7: [metrics] valores razonables ─────────────────────────
    if let (Some(main), Some(min_)) = (
        profile.metrics.expected_xp_per_hour,
        profile.metrics.expected_xp_min_per_hour,
    ) {
        if min_ >= main {
            println!(
                "  ⚠ [metrics]: expected_xp_min_per_hour={} >= expected_xp_per_hour={}. min_ debería ser un umbral bajo (típicamente ~30-50% del main).",
                min_, main
            );
            warnings += 1;
        }
    }
    if let Some(deaths) = profile.metrics.expected_deaths_per_session {
        if deaths > 3 {
            println!(
                "  ⚠ [metrics].expected_deaths_per_session={} > 3: el hunt parece peligroso. ¿Es intencional?",
                deaths
            );
            warnings += 1;
        }
    }

    // ── Check 8: [calibration_hints].stow_bags ────────────────────────
    if !profile.calibration_hints.stow_bags.is_empty() {
        let backpack_count = profile.calibration_hints.backpack_count;
        let mut seen_indices: HashSet<u32> = HashSet::new();
        for bag in &profile.calibration_hints.stow_bags {
            if !seen_indices.insert(bag.bag_index) {
                println!(
                    "  ✗ [calibration_hints].stow_bags: bag_index={} duplicado",
                    bag.bag_index
                );
                errors += 1;
            }
            if let Some(count) = backpack_count {
                if bag.bag_index == 0 || bag.bag_index > count {
                    println!(
                        "  ⚠ [calibration_hints].stow_bags[{}]: bag_index fuera del rango [1, {}] declarado por backpack_count",
                        bag.bag_index, count
                    );
                    warnings += 1;
                }
            }
            for item in &bag.expected_loot {
                if !known_templates.is_empty()
                    && !known_templates.contains(item)
                    && !profile.loot.stackables.contains(item)
                {
                    println!(
                        "  ⓘ [calibration_hints].stow_bags[{}].expected_loot: '{}' sin template y no declarado en [loot].stackables (documentación only)",
                        bag.bag_index, item
                    );
                }
            }
        }
    }

    println!();
    println!("Summary: {} errors, {} warnings", errors, warnings);
    std::process::exit(if errors > 0 { 1 } else { 0 });
}

fn collect_template_names(dir: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return set,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            set.insert(stem.to_string());
        }
    }
    set
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == flag)?;
    args.get(pos + 1).cloned()
}
