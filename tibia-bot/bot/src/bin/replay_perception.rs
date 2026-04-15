//! replay_perception — Lee un archivo JSONL de sesión grabada (F1) y
//! produce reports offline.
//!
//! ## Modos
//!
//! - `--summary` (default): aggregate stats (ticks, state distribution,
//!   HP/mana p50/p95, items peak/min, unique coords, safety pauses)
//! - `--trace`: imprime línea-por-línea con tick, HP, mana, fsm, enemies
//! - `--filter hp_below:30`: solo muestra ticks donde HP < 30%
//! - `--filter in_combat`: solo ticks en combate
//!
//! ## Uso
//!
//! ```bash
//! cargo run --release --bin replay_perception -- --input session.jsonl
//! cargo run --release --bin replay_perception -- --input session.jsonl --trace
//! cargo run --release --bin replay_perception -- --input session.jsonl --filter hp_below:50
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use tibia_bot::sense::perception::PerceptionSnapshot;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let input = arg_value(&args, "--input")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("--input <session.jsonl> requerido"))?;

    let trace = args.iter().any(|a| a == "--trace");
    let filter = arg_value(&args, "--filter");

    println!("Reading {}...", input.display());
    let content = std::fs::read_to_string(&input)?;
    let snapshots: Vec<PerceptionSnapshot> = content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| match serde_json::from_str::<PerceptionSnapshot>(l) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("  skip invalid line: {}", e);
                None
            }
        })
        .collect();
    println!("Loaded {} snapshots\n", snapshots.len());

    // Filter si se especifica
    let filtered: Vec<&PerceptionSnapshot> = snapshots.iter()
        .filter(|s| apply_filter(&filter, s))
        .collect();

    if trace {
        print_trace(&filtered);
    } else {
        print_summary(&filtered);
    }

    Ok(())
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn apply_filter(filter: &Option<String>, snap: &PerceptionSnapshot) -> bool {
    let Some(f) = filter else { return true };
    if let Some(rest) = f.strip_prefix("hp_below:") {
        if let Ok(threshold) = rest.parse::<f32>() {
            let ratio = threshold / 100.0;
            return snap.hp_ratio.map(|hp| hp < ratio).unwrap_or(false);
        }
    }
    if f == "in_combat" {
        return snap.in_combat;
    }
    if f == "moving" {
        return snap.is_moving == Some(true);
    }
    if let Some(rest) = f.strip_prefix("has_item:") {
        return snap.inventory_counts.contains_key(rest);
    }
    eprintln!("Unknown filter: {}", f);
    true
}

fn print_trace(snapshots: &[&PerceptionSnapshot]) {
    println!("tick | hp    | mana  | enemies | combat | fsm_coords");
    println!("─────┼───────┼───────┼─────────┼────────┼───────────");
    for s in snapshots {
        let hp = s.hp_ratio.map(|h| format!("{:.0}%", h * 100.0)).unwrap_or_else(|| "?".into());
        let mana = s.mana_ratio.map(|m| format!("{:.0}%", m * 100.0)).unwrap_or_else(|| "?".into());
        let coords = s.game_coords.map(|(x,y,z)| format!("({},{},{})", x, y, z)).unwrap_or_else(|| "-".into());
        println!(
            "{:5} | {:5} | {:5} | {:7} | {:6} | {}",
            s.tick, hp, mana, s.enemy_count,
            if s.in_combat { "yes" } else { "no" },
            coords,
        );
    }
    println!("\n{} snapshots trazados", snapshots.len());
}

fn print_summary(snapshots: &[&PerceptionSnapshot]) {
    if snapshots.is_empty() {
        println!("No snapshots to summarize (filtro activo?)");
        return;
    }

    let n = snapshots.len();
    let tick_min = snapshots.iter().map(|s| s.tick).min().unwrap_or(0);
    let tick_max = snapshots.iter().map(|s| s.tick).max().unwrap_or(0);

    // HP/Mana percentiles
    let hp_values: Vec<f32> = snapshots.iter().filter_map(|s| s.hp_ratio).collect();
    let mana_values: Vec<f32> = snapshots.iter().filter_map(|s| s.mana_ratio).collect();

    // Combat stats
    let combat_count = snapshots.iter().filter(|s| s.in_combat).count();
    let max_enemies = snapshots.iter().map(|s| s.enemy_count).max().unwrap_or(0);

    // Inventory peak (por item)
    let mut inventory_peak: HashMap<String, u32> = HashMap::new();
    for s in snapshots {
        for (item, &count) in &s.inventory_counts {
            let peak = inventory_peak.entry(item.clone()).or_insert(0);
            if count > *peak { *peak = count; }
        }
    }

    // Unique coords (con game_coords activo)
    let mut unique_coords: std::collections::HashSet<(i32, i32, i32)> = std::collections::HashSet::new();
    for s in snapshots {
        if let Some(c) = s.game_coords {
            unique_coords.insert(c);
        }
    }

    // UI matches
    let mut ui_counts: HashMap<String, u32> = HashMap::new();
    for s in snapshots {
        for ui in &s.ui_matches {
            *ui_counts.entry(ui.clone()).or_insert(0) += 1;
        }
    }

    println!("═══════════════════════════════════════");
    println!("  Session Summary ({} snapshots)", n);
    println!("═══════════════════════════════════════");
    println!("  Tick range:   {} → {} (span: {})", tick_min, tick_max, tick_max - tick_min);
    if !hp_values.is_empty() {
        let (p50, p95) = percentiles(&hp_values);
        let min = hp_values.iter().cloned().fold(f32::INFINITY, f32::min);
        println!("  HP p50/p95:   {:.0}% / {:.0}% (min observed: {:.0}%)", p50 * 100.0, p95 * 100.0, min * 100.0);
    }
    if !mana_values.is_empty() {
        let (p50, p95) = percentiles(&mana_values);
        let min = mana_values.iter().cloned().fold(f32::INFINITY, f32::min);
        println!("  Mana p50/p95: {:.0}% / {:.0}% (min observed: {:.0}%)", p50 * 100.0, p95 * 100.0, min * 100.0);
    }
    println!("  In combat:    {}/{} ({:.1}%)", combat_count, n, combat_count as f64 / n as f64 * 100.0);
    println!("  Max enemies:  {}", max_enemies);
    println!("  Unique coords: {}", unique_coords.len());
    if !inventory_peak.is_empty() {
        println!("  Inventory peak (slots):");
        let mut items: Vec<_> = inventory_peak.iter().collect();
        items.sort_by(|a, b| b.1.cmp(a.1));
        for (item, peak) in items.iter().take(10) {
            println!("    {:25} {}", item, peak);
        }
    }
    if !ui_counts.is_empty() {
        println!("  UI matches:");
        let mut uis: Vec<_> = ui_counts.iter().collect();
        uis.sort_by(|a, b| b.1.cmp(a.1));
        for (ui, count) in uis.iter().take(5) {
            println!("    {:25} {} ticks", ui, count);
        }
    }
}

fn percentiles(values: &[f32]) -> (f32, f32) {
    let mut sorted: Vec<f32> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50_idx = sorted.len() / 2;
    let p95_idx = (sorted.len() as f64 * 0.95) as usize;
    (sorted[p50_idx], sorted[p95_idx.min(sorted.len() - 1)])
}
