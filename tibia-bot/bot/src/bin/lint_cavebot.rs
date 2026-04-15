//! lint_cavebot — Linter estático para scripts de cavebot.
//!
//! Carga un .toml de cavebot via el parser real y reporta:
//!   - Orphan labels (definidos pero nunca usados como goto target)
//!   - Walks degenerados (duration_ms=0 o interval_ms > duration_ms)
//!   - Coords fuera de rango Tibia (X 31700-34000, Y 30900-33000, Z 0-15)
//!   - Cycles sin emisor (loops goto que no emiten ningún Action)
//!   - Refs a item templates inexistentes en `assets/templates/inventory/`
//!
//! Uso:
//!   cargo run --release --bin lint_cavebot -- assets/cavebot/abdendriel_wasps.toml
//!
//! Exit code: 0 = sin errores. 1 = errores encontrados.

use std::collections::HashSet;
use std::path::PathBuf;

use tibia_bot::cavebot::parser;
use tibia_bot::cavebot::step::{Condition, StepKind};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Uso: lint_cavebot <ruta.toml> [--templates <dir>]");
        std::process::exit(2);
    }

    let script_path = PathBuf::from(&args[1]);
    let templates_dir = arg_value(&args, "--templates")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/templates/inventory"));

    println!("Linting {}...", script_path.display());

    // Cargar templates conocidos para validar refs de has_item().
    let known_templates = collect_template_names(&templates_dir);
    if known_templates.is_empty() {
        println!("  ⓘ no se encontraron templates en '{}' — has_item() no será validado",
            templates_dir.display());
    }

    // Parser load: ya valida sintaxis, labels duplicados, refs no resueltos.
    let cb = match parser::load(&script_path, 30) {
        Ok(cb) => cb,
        Err(e) => {
            eprintln!("✗ Parser error: {}", e);
            std::process::exit(1);
        }
    };
    println!("  ✓ {} steps parsed OK", cb.steps.len());

    let mut errors = 0u32;
    let mut warnings = 0u32;

    // Indexar labels definidos y referenciados.
    let mut defined_labels: Vec<(String, usize)> = Vec::new();
    let mut referenced_labels: HashSet<String> = HashSet::new();
    for (idx, step) in cb.steps.iter().enumerate() {
        if let Some(name) = &step.label {
            if matches!(step.kind, StepKind::Label) {
                defined_labels.push((name.clone(), idx));
            }
        }
        match &step.kind {
            StepKind::Goto { target_label, .. } => {
                referenced_labels.insert(target_label.clone());
            }
            StepKind::GotoIf { target_label, .. } => {
                referenced_labels.insert(target_label.clone());
            }
            StepKind::CheckSupplies { on_fail_label, .. } => {
                referenced_labels.insert(on_fail_label.clone());
            }
            _ => {}
        }
    }

    // ── Check 1: Orphan labels ───────────────────────────────────────
    for (name, idx) in &defined_labels {
        // El primer label suele ser "start" — siempre alcanzable, no es orphan.
        if *idx == 0 {
            continue;
        }
        if !referenced_labels.contains(name) {
            println!("  ⚠ Orphan label '{}' (step #{}): defined but never used as goto target",
                name, idx);
            warnings += 1;
        }
    }

    // ── Check 2: Walks degenerados ───────────────────────────────────
    for (idx, step) in cb.steps.iter().enumerate() {
        if let StepKind::Walk { duration_ms, interval_ms, .. } = &step.kind {
            if *duration_ms == 0 {
                println!("  ⚠ Walk step #{}: duration_ms=0 (will not emit)", idx);
                warnings += 1;
            } else if *interval_ms > *duration_ms && *interval_ms > 0 {
                println!("  ⚠ Walk step #{}: interval_ms={} > duration_ms={} (only first emit will happen)",
                    idx, interval_ms, duration_ms);
                warnings += 1;
            }
        }
    }

    // ── Check 3: Coords Node fuera de rango Tibia ────────────────────
    for (idx, step) in cb.steps.iter().enumerate() {
        if let StepKind::Node { x, y, z, .. } = &step.kind {
            let x_ok = (31700..=34000).contains(x);
            let y_ok = (30900..=33000).contains(y);
            let z_ok = (0..=15).contains(z);
            if !x_ok || !y_ok || !z_ok {
                println!("  ⚠ Node step #{}: ({}, {}, {}) outside typical Tibia range \
                          (X 31700-34000, Y 30900-33000, Z 0-15)",
                    idx, x, y, z);
                warnings += 1;
            }
        }
    }

    // ── Check 4: has_item() refs a templates inexistentes ────────────
    if !known_templates.is_empty() {
        for (idx, step) in cb.steps.iter().enumerate() {
            // Buscar HasItem en GotoIf conditions.
            if let StepKind::GotoIf { condition, .. } = &step.kind {
                check_condition_has_item(condition, idx, &known_templates, &mut errors);
            }
            // CheckSupplies tiene requirements directos.
            if let StepKind::CheckSupplies { requirements, .. } = &step.kind {
                for (item, _) in requirements {
                    if !known_templates.contains(item) {
                        println!("  ✗ check_supplies step #{}: item '{}' not found in templates dir. \
                                 Did you mean one of: {}?",
                            idx, item, suggest_close_match(item, &known_templates));
                        errors += 1;
                    }
                }
            }
        }
    }

    // ── Check 5: Cycles sin emisor ───────────────────────────────────
    // Heurística: si un Goto apunta a un step previo que es Label, y entre el
    // Label y el Goto no hay ningún step que pueda emitir (Walk/Hotkey/Loot/...),
    // es un loop infinito. El runner ya tiene safety limit (64 iters) pero
    // detectarlo en lint es mejor.
    if let Err(msg) = detect_cycles_without_emitter(&cb.steps) {
        println!("  ✗ {}", msg);
        errors += 1;
    }

    // ── Resumen ──────────────────────────────────────────────────────
    println!();
    println!("{} errors, {} warnings", errors, warnings);

    if errors > 0 {
        std::process::exit(1);
    }
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn collect_template_names(dir: &std::path::Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return set;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("png") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                set.insert(stem.to_string());
            }
        }
    }
    set
}

fn check_condition_has_item(
    cond: &Condition,
    step_idx: usize,
    known: &HashSet<String>,
    errors: &mut u32,
) {
    match cond {
        Condition::HasItem { name, .. } => {
            if !known.contains(name) {
                println!("  ✗ goto_if step #{}: has_item('{}') — template not found. Did you mean: {}?",
                    step_idx, name, suggest_close_match(name, known));
                *errors += 1;
            }
        }
        Condition::Not(inner) => check_condition_has_item(inner, step_idx, known, errors),
        _ => {}
    }
}

/// Sugiere el template más parecido por distancia de Levenshtein simple.
fn suggest_close_match(name: &str, known: &HashSet<String>) -> String {
    let mut best = (usize::MAX, "(none)".to_string());
    for k in known {
        let d = levenshtein(name, k);
        if d < best.0 {
            best = (d, k.clone());
        }
    }
    best.1
}

#[allow(clippy::needless_range_loop)]
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (alen, blen) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; blen + 1]; alen + 1];
    for i in 0..=alen { dp[i][0] = i; }
    for j in 0..=blen { dp[0][j] = j; }
    for i in 1..=alen {
        for j in 1..=blen {
            let cost = if a[i-1] == b[j-1] { 0 } else { 1 };
            dp[i][j] = (dp[i-1][j] + 1)
                .min(dp[i][j-1] + 1)
                .min(dp[i-1][j-1] + cost);
        }
    }
    dp[alen][blen]
}

/// Detecta cycles donde un Goto loopea sin pasar por ningún step que emita.
/// Algoritmo: para cada Goto, simular forward desde su target hasta volver
/// al Goto. Si en el camino solo hay Label/Goto/GotoIf, es un cycle muerto.
fn detect_cycles_without_emitter(steps: &[tibia_bot::cavebot::step::Step]) -> Result<(), String> {
    use tibia_bot::cavebot::step::StepKind::*;

    fn is_emitter(kind: &StepKind) -> bool {
        // Cualquier kind QUE NO SEA puramente de control de flujo es "emitter".
        // Label, Goto, GotoIf no emiten — los demás sí (al menos potencialmente).
        !matches!(kind, Label | Goto { .. } | GotoIf { .. })
    }

    for (start_idx, step) in steps.iter().enumerate() {
        let target = match &step.kind {
            Goto { target_idx, .. } => *target_idx,
            _ => continue,
        };
        if target >= start_idx {
            // Solo nos importan backward gotos (potenciales loops).
            continue;
        }

        // Walk desde target hasta start_idx, viendo si hay algún emitter.
        let mut has_emitter = false;
        let mut visited = HashSet::new();
        let mut cursor = target;
        let mut max_iters = steps.len() + 1;
        while cursor <= start_idx && max_iters > 0 {
            if !visited.insert(cursor) {
                break;
            }
            if is_emitter(&steps[cursor].kind) {
                has_emitter = true;
                break;
            }
            cursor += 1;
            max_iters -= 1;
        }
        if !has_emitter {
            return Err(format!(
                "Cycle without emitter: goto step #{} → step #{} (no Walk/Hotkey/Wait between them)",
                start_idx, target
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("mama_potion", "mana_potion"), 1);
    }

    #[test]
    fn suggest_close_match_finds_closest() {
        let known: HashSet<String> = [
            "mana_potion",
            "great_mana_potion",
            "ultimate_healing_rune",
        ].iter().map(|s| s.to_string()).collect();

        // Typo de 1 char debe sugerir mana_potion.
        assert_eq!(suggest_close_match("mama_potion", &known), "mana_potion");
        // Substring debe sugerir el match más corto.
        assert!(["mana_potion", "great_mana_potion"].contains(&suggest_close_match("mana", &known).as_str()));
    }
}
