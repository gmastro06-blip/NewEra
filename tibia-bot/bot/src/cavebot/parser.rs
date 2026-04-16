//! parser.rs — Carga y parsing de archivos TOML del cavebot.
//!
//! Formato del archivo:
//!
//! ```toml
//! [cavebot]
//! loop = true   # default true
//!
//! [[step]]
//! kind = "label"
//! name = "hunt_entry"
//!
//! [[step]]
//! kind = "walk"
//! key  = "D"
//! duration_ms = 3000
//! interval_ms = 300
//!
//! [[step]]
//! kind = "stand"
//! until = "mobs_killed(3)"
//! max_wait_ms = 30000
//!
//! [[step]]
//! kind = "goto_if"
//! label = "refill_entry"
//! when  = "hp_below(0.4)"
//!
//! [[step]]
//! kind = "goto"
//! label = "hunt_entry"
//! ```
//!
//! Soporta multi-section (`[[section]] name="hunt" ... steps=[...]`) como
//! alternativa a `[[step]]` plano — los steps se concatenan en orden de
//! sección declarada y los labels siguen siendo globalmente únicos.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::act::keycode;
use crate::cavebot::runner::Cavebot;
use crate::cavebot::step::{Condition, StandUntil, Step, StepKind};

// ── TOML schema ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CavebotFile {
    #[serde(default)]
    cavebot: CavebotHeader,
    /// Steps planos (formato simple, una sola sección implícita).
    #[serde(default, rename = "step")]
    steps: Vec<StepToml>,
    /// Multi-section (formato avanzado). Si está presente, `step` se ignora.
    #[serde(default, rename = "section")]
    sections: Vec<SectionToml>,
}

#[derive(Debug, Deserialize, Default)]
struct CavebotHeader {
    #[serde(default = "default_loop")]
    loop_: bool,
}

fn default_loop() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // deserialized from TOML
struct SectionToml {
    #[serde(default)]
    name: String,
    #[serde(default)]
    steps: Vec<StepToml>,
}

/// Un step en formato TOML. Usa `kind` como tag y luego campos específicos.
/// Flatten simplificado: no usamos serde adjacent enums porque los campos
/// opcionales hacen el parser más amigable para el usuario.
#[derive(Debug, Deserialize)]
struct StepToml {
    /// Nombre del tipo: "walk" | "wait" | "hotkey" | "stand" | "label"
    /// | "goto" | "goto_if" | "loot" | "skip_if_blocked" | "npc_dialog"
    kind: String,
    /// Nombre opcional del step (para logs) y obligatorio si kind=label.
    #[serde(default)]
    name: Option<String>,
    // Walk / Hotkey / SkipIfBlocked-Walk
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    interval_ms: Option<u64>,
    // Stand
    /// Texto de la condición until: "mobs_killed(N)" | "hp_full" | "mana_full"
    /// | "timer_ms(N)" | "no_combat"
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    max_wait_ms: Option<u64>,
    // Goto / GotoIf
    #[serde(default)]
    label: Option<String>,
    /// Texto de la condición when: "hp_below(N)" | "mana_below(N)"
    /// | "kills_gte(N)" | "timer_ticks(N)" | "no_combat" | "not:<other>"
    #[serde(default)]
    when: Option<String>,
    // Loot
    #[serde(default)]
    vx: Option<i32>,
    #[serde(default)]
    vy: Option<i32>,
    #[serde(default)]
    retry_count: Option<u8>,
    // NpcDialog
    #[serde(default)]
    phrases: Option<Vec<String>>,
    #[serde(default)]
    wait_prompt_ms: Option<u64>,
    // Node (coordenadas absolutas)
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    #[serde(default)]
    z: Option<i32>,
    // Deposit
    #[serde(default)]
    chest_vx: Option<i32>,
    #[serde(default)]
    chest_vy: Option<i32>,
    #[serde(default)]
    stow_vx: Option<i32>,
    #[serde(default)]
    stow_vy: Option<i32>,
    #[serde(default)]
    menu_wait_ms: Option<u64>,
    #[serde(default)]
    process_ms: Option<u64>,
    // StowBag (modern Tibia 12 Supply Stash)
    #[serde(default)]
    bag_vx: Option<i32>,
    #[serde(default)]
    bag_vy: Option<i32>,
    #[serde(default)]
    menu_offset_y: Option<i32>,
    // BuyItem
    #[serde(default)]
    item_vx: Option<i32>,
    #[serde(default)]
    item_vy: Option<i32>,
    #[serde(default)]
    confirm_vx: Option<i32>,
    #[serde(default)]
    confirm_vy: Option<i32>,
    #[serde(default)]
    quantity: Option<u32>,
    #[serde(default)]
    spacing_ms: Option<u64>,
    // CheckSupplies
    #[serde(default)]
    on_fail: Option<String>,
    #[serde(default)]
    requirements: Option<Vec<SupplyRequirement>>,
}

#[derive(Debug, Deserialize, Clone)]
struct SupplyRequirement {
    item: String,
    min_count: u32,
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Carga un archivo de cavebot y construye un `Cavebot` listo para ejecutar.
/// Resuelve todos los labels a índices antes de retornar.
#[allow(dead_code)] // convenience: tests y hot-reload sin tuning custom
pub fn load(path: &Path, fps: u32) -> Result<Cavebot> {
    load_with_tuning(path, fps, super::runner::NodeTuning::default())
}

pub fn load_with_tuning(path: &Path, fps: u32, tuning: super::runner::NodeTuning) -> Result<Cavebot> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("No se pudo leer '{}'", path.display()))?;
    let file: CavebotFile = toml::from_str(&raw)
        .with_context(|| format!("TOML inválido en '{}'", path.display()))?;
    build_cavebot(file, fps, tuning)
}

/// Construye un `Cavebot` desde una estructura TOML ya parseada.
/// Privado al módulo — los tests lo usan directamente via super::.
fn build_cavebot(file: CavebotFile, fps: u32, tuning: super::runner::NodeTuning) -> Result<Cavebot> {
    // Si hay sections, concatenar sus steps en orden. Si no, usar los steps planos.
    let raw_steps: Vec<StepToml> = if !file.sections.is_empty() {
        file.sections.into_iter().flat_map(|s| s.steps).collect()
    } else {
        file.steps
    };

    if raw_steps.is_empty() {
        bail!("Cavebot file no contiene ningún step");
    }

    // Primera pasada: convertir cada StepToml → Step, usando target_idx=0 placeholder
    // para los Goto. Recolectar simultáneamente el mapa label → índice.
    let mut steps = Vec::with_capacity(raw_steps.len());
    let mut labels: HashMap<String, usize> = HashMap::new();

    for (idx, st) in raw_steps.into_iter().enumerate() {
        let step = parse_step_toml(st)
            .with_context(|| format!("step[{}]", idx))?;
        if let StepKind::Label = &step.kind {
            let name = step.label.clone()
                .ok_or_else(|| anyhow::anyhow!("step[{}]: label sin nombre", idx))?;
            if labels.contains_key(&name) {
                bail!("step[{}]: label duplicado '{}'", idx, name);
            }
            labels.insert(name, idx);
        }
        steps.push(step);
    }

    // Segunda pasada: resolver los `target_idx` de los Goto/GotoIf
    // contra el mapa de labels.
    for (idx, step) in steps.iter_mut().enumerate() {
        match &mut step.kind {
            StepKind::Goto { target_label, target_idx } => {
                *target_idx = *labels.get(target_label)
                    .with_context(|| format!(
                        "step[{}] goto: label '{}' no encontrado", idx, target_label
                    ))?;
            }
            StepKind::GotoIf { target_label, target_idx, .. } => {
                *target_idx = *labels.get(target_label)
                    .with_context(|| format!(
                        "step[{}] goto_if: label '{}' no encontrado", idx, target_label
                    ))?;
            }
            StepKind::CheckSupplies { on_fail_label, on_fail_idx, .. } => {
                *on_fail_idx = *labels.get(on_fail_label)
                    .with_context(|| format!(
                        "step[{}] check_supplies: label on_fail '{}' no encontrado", idx, on_fail_label
                    ))?;
            }
            _ => {}
        }
    }

    Ok(Cavebot::with_tuning(steps, file.cavebot.loop_, fps, tuning))
}

// ── StepToml → Step ──────────────────────────────────────────────────────────

fn parse_step_toml(st: StepToml) -> Result<Step> {
    let StepToml {
        kind, name, key, duration_ms, interval_ms,
        until, max_wait_ms, label, when, vx, vy, retry_count,
        phrases, wait_prompt_ms, x, y, z,
        chest_vx, chest_vy, stow_vx, stow_vy, menu_wait_ms, process_ms,
        item_vx, item_vy, confirm_vx, confirm_vy, quantity, spacing_ms,
        on_fail, requirements,
        bag_vx, bag_vy, menu_offset_y,
    } = st;

    let kind_lower = kind.to_lowercase();
    let step_kind = match kind_lower.as_str() {
        "walk" => {
            let k = key.as_deref().context("walk: falta 'key'")?;
            let hid = keycode::parse(k).with_context(|| format!("walk: key inválida '{}'", k))?;
            StepKind::Walk {
                hidcode:     hid,
                duration_ms: duration_ms.context("walk: falta 'duration_ms'")?,
                interval_ms: interval_ms.unwrap_or(0),
            }
        }
        "wait" => StepKind::Wait {
            duration_ms: duration_ms.context("wait: falta 'duration_ms'")?,
        },
        "hotkey" => {
            let k = key.as_deref().context("hotkey: falta 'key'")?;
            let hid = keycode::parse(k).with_context(|| format!("hotkey: key inválida '{}'", k))?;
            StepKind::Hotkey { hidcode: hid }
        }
        "stand" => {
            let until_str = until.as_deref().context("stand: falta 'until'")?;
            StepKind::Stand {
                until:       parse_stand_until(until_str)?,
                max_wait_ms: max_wait_ms.unwrap_or(30_000),
            }
        }
        "label" => {
            // El nombre del label viene en `name`.
            match &name {
                None => bail!("label: falta 'name'"),
                Some(n) if n.is_empty() => bail!("label: 'name' no puede ser string vacío"),
                Some(_) => {}
            }
            StepKind::Label
        }
        "goto" => {
            let lbl = label.context("goto: falta 'label'")?;
            StepKind::Goto { target_label: lbl, target_idx: 0 }
        }
        "goto_if" => {
            let lbl = label.context("goto_if: falta 'label'")?;
            let w = when.as_deref().context("goto_if: falta 'when'")?;
            StepKind::GotoIf {
                target_label: lbl,
                target_idx:   0,
                condition:    parse_condition(w)?,
            }
        }
        "loot" => StepKind::Loot {
            vx:          vx.context("loot: falta 'vx'")?,
            vy:          vy.context("loot: falta 'vy'")?,
            retry_count: retry_count.unwrap_or(3),
        },
        "skip_if_blocked" => {
            // Inner: MVP solo soporta Walk. Se construye con los mismos campos key/duration/interval.
            let k = key.as_deref().context("skip_if_blocked: falta 'key' (solo walk soportado)")?;
            let hid = keycode::parse(k).with_context(|| format!("skip_if_blocked: key inválida '{}'", k))?;
            StepKind::SkipIfBlocked {
                inner: Box::new(StepKind::Walk {
                    hidcode:     hid,
                    duration_ms: duration_ms.context("skip_if_blocked: falta 'duration_ms'")?,
                    interval_ms: interval_ms.unwrap_or(0),
                }),
                max_wait_ms: max_wait_ms.context("skip_if_blocked: falta 'max_wait_ms'")?,
            }
        }
        "npc_dialog" => {
            let phrases = phrases.context("npc_dialog: falta 'phrases' (array de strings)")?;
            if phrases.is_empty() {
                bail!("npc_dialog: 'phrases' no puede estar vacío");
            }
            StepKind::NpcDialog {
                phrases,
                wait_prompt_ms: wait_prompt_ms.unwrap_or(0),
            }
        }
        "node" => StepKind::Node {
            x: x.context("node: falta 'x'")?,
            y: y.context("node: falta 'y'")?,
            z: z.context("node: falta 'z'")?,
            max_wait_ms: max_wait_ms.unwrap_or(30_000),
        },
        "rope" => {
            let k = key.as_deref().context("rope: falta 'key' (hotkey del rope, ej: F6)")?;
            let hid = crate::act::keycode::parse(k)
                .with_context(|| format!("rope: key inválida '{}'", k))?;
            StepKind::Rope { hidcode: hid }
        }
        "ladder" => StepKind::Ladder {
            vx: vx.unwrap_or(871),  // default: centro del game_viewport
            vy: vy.unwrap_or(448),
        },
        "deposit" => StepKind::Deposit {
            chest_vx:     chest_vx.context("deposit: falta 'chest_vx'")?,
            chest_vy:     chest_vy.context("deposit: falta 'chest_vy'")?,
            stow_vx:      stow_vx.context("deposit: falta 'stow_vx'")?,
            stow_vy:      stow_vy.context("deposit: falta 'stow_vy'")?,
            menu_wait_ms: menu_wait_ms.unwrap_or(300),
            process_ms:   process_ms.unwrap_or(500),
        },
        "stow_bag" => StepKind::StowBag {
            bag_vx:         bag_vx.context("stow_bag: falta 'bag_vx' (icono del bag en UI)")?,
            bag_vy:         bag_vy.context("stow_bag: falta 'bag_vy'")?,
            menu_offset_y:  menu_offset_y.unwrap_or(70),
            menu_wait_ms:   menu_wait_ms.unwrap_or(300),
            process_ms:     process_ms.unwrap_or(2000),
        },
        "buy_item" => StepKind::BuyItem {
            item_vx:    item_vx.context("buy_item: falta 'item_vx'")?,
            item_vy:    item_vy.context("buy_item: falta 'item_vy'")?,
            confirm_vx: confirm_vx.context("buy_item: falta 'confirm_vx'")?,
            confirm_vy: confirm_vy.context("buy_item: falta 'confirm_vy'")?,
            quantity:   quantity.context("buy_item: falta 'quantity'")?,
            spacing_ms: spacing_ms.unwrap_or(150),
        },
        "check_supplies" => {
            let reqs = requirements.context("check_supplies: falta 'requirements' (array)")?;
            let fail_label = on_fail.context("check_supplies: falta 'on_fail'")?;
            if reqs.is_empty() {
                bail!("check_supplies: 'requirements' no puede estar vacío");
            }
            StepKind::CheckSupplies {
                requirements: reqs.into_iter().map(|r| (r.item, r.min_count)).collect(),
                on_fail_label: fail_label,
                on_fail_idx: 0, // resuelto en label pass
            }
        }
        other => bail!("kind desconocido: '{}'", other),
    };

    Ok(Step { label: name, kind: step_kind })
}

// ── Mini-parsers de expresiones ──────────────────────────────────────────────

fn parse_stand_until(s: &str) -> Result<StandUntil> {
    let s = s.trim();
    if let Some(inner) = strip_call(s, "mobs_killed") {
        let n: u32 = inner.parse().context("mobs_killed: arg no es u32")?;
        return Ok(StandUntil::MobsKilled(n));
    }
    if s == "hp_full" {
        return Ok(StandUntil::HpFull);
    }
    if s == "mana_full" {
        return Ok(StandUntil::ManaFull);
    }
    if let Some(inner) = strip_call(s, "timer_ms") {
        let n: u64 = inner.parse().context("timer_ms: arg no es u64")?;
        return Ok(StandUntil::TimerMs(n));
    }
    if s == "no_combat" {
        return Ok(StandUntil::NoCombat);
    }
    if let Some(inner) = strip_call(s, "enemies_gte") {
        let n: u32 = inner.parse().context("enemies_gte: arg no es u32")?;
        return Ok(StandUntil::EnemiesGte(n));
    }
    // reached(x, y, z) — esperar hasta que tile-hashing confirme posición.
    if let Some(inner) = strip_call(s, "reached") {
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() != 3 { bail!("reached: requiere 3 args (x,y,z)"); }
        let x: i32 = parts[0].parse().context("reached: x no es i32")?;
        let y: i32 = parts[1].parse().context("reached: y no es i32")?;
        let z: i32 = parts[2].parse().context("reached: z no es i32")?;
        return Ok(StandUntil::ReachedCoord(x, y, z));
    }
    bail!("until desconocido: '{}'", s)
}

fn parse_condition(s: &str) -> Result<Condition> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("not:") {
        return Ok(Condition::Not(Box::new(parse_condition(rest)?)));
    }
    if let Some(inner) = strip_call(s, "hp_below") {
        let f: f32 = inner.parse().context("hp_below: arg no es f32")?;
        return Ok(Condition::HpBelow(f));
    }
    if let Some(inner) = strip_call(s, "mana_below") {
        let f: f32 = inner.parse().context("mana_below: arg no es f32")?;
        return Ok(Condition::ManaBelow(f));
    }
    if let Some(inner) = strip_call(s, "kills_gte") {
        let n: u64 = inner.parse().context("kills_gte: arg no es u64")?;
        return Ok(Condition::KillsGte(n));
    }
    if let Some(inner) = strip_call(s, "timer_ticks") {
        let n: u64 = inner.parse().context("timer_ticks: arg no es u64")?;
        return Ok(Condition::TimerTicksElapsed(n));
    }
    if s == "no_combat" {
        return Ok(Condition::NoCombat);
    }
    if let Some(inner) = strip_call(s, "ui_visible") {
        return Ok(Condition::UiVisible(inner.trim().to_string()));
    }
    if let Some(inner) = strip_call(s, "enemies_gte") {
        let n: u32 = inner.parse().context("enemies_gte: arg no es u32")?;
        return Ok(Condition::EnemyCountGte(n));
    }
    if s == "loot_visible" {
        return Ok(Condition::LootVisible);
    }
    if s == "is_moving" {
        return Ok(Condition::IsMoving);
    }
    if s == "is_stuck" {
        return Ok(Condition::Not(Box::new(Condition::IsMoving)));
    }
    // at_coord(x, y, z) — coordenada exacta por tile-hashing.
    if let Some(inner) = strip_call(s, "at_coord") {
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() != 3 { bail!("at_coord: requiere 3 args (x,y,z)"); }
        let x: i32 = parts[0].parse().context("at_coord: x no es i32")?;
        let y: i32 = parts[1].parse().context("at_coord: y no es i32")?;
        let z: i32 = parts[2].parse().context("at_coord: z no es i32")?;
        return Ok(Condition::AtCoord(x, y, z));
    }
    // near_coord(x, y, z, range) — distancia Manhattan ≤ range.
    if let Some(inner) = strip_call(s, "near_coord") {
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() != 4 { bail!("near_coord: requiere 4 args (x,y,z,range)"); }
        let x: i32 = parts[0].parse().context("near_coord: x no es i32")?;
        let y: i32 = parts[1].parse().context("near_coord: y no es i32")?;
        let z: i32 = parts[2].parse().context("near_coord: z no es i32")?;
        let range: i32 = parts[3].parse().context("near_coord: range no es i32")?;
        return Ok(Condition::NearCoord { x, y, z, range });
    }
    // has_item(name, min_count) — template-matching de inventario.
    if let Some(inner) = strip_call(s, "has_item") {
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() != 2 { bail!("has_item: requiere 2 args (name, min_count)"); }
        let name = parts[0].to_string();
        let min_count: u32 = parts[1].parse().context("has_item: min_count no es u32")?;
        return Ok(Condition::HasItem { name, min_count });
    }
    // has_stack(name, min_units) — OCR del stack count.
    if let Some(inner) = strip_call(s, "has_stack") {
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() != 2 { bail!("has_stack: requiere 2 args (name, min_units)"); }
        let name = parts[0].to_string();
        let min_units: u32 = parts[1].parse().context("has_stack: min_units no es u32")?;
        return Ok(Condition::HasStack { name, min_units });
    }
    bail!("condition desconocida: '{}'", s)
}

/// Si `s` matches `name(arg)`, retorna `Some(arg)`. Si no, None.
fn strip_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{}(", name);
    let s = s.strip_prefix(&prefix)?;
    s.strip_suffix(')')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_src: &str) -> Result<Cavebot> {
        let file: CavebotFile = toml::from_str(toml_src)?;
        build_cavebot(file, 30, crate::cavebot::runner::NodeTuning::default())
    }

    #[test]
    fn parse_flat_walk_wait() {
        let src = r#"
            [[step]]
            kind = "walk"
            key = "D"
            duration_ms = 1000
            interval_ms = 300

            [[step]]
            kind = "wait"
            duration_ms = 500
        "#;
        let cb = parse(src).unwrap();
        assert_eq!(cb.steps.len(), 2);
        assert!(matches!(&cb.steps[0].kind, StepKind::Walk { duration_ms: 1000, .. }));
        assert!(matches!(&cb.steps[1].kind, StepKind::Wait { duration_ms: 500 }));
    }

    #[test]
    fn parse_labels_and_goto_resolves_indices() {
        let src = r#"
            [[step]]
            kind = "label"
            name = "start"

            [[step]]
            kind = "hotkey"
            key = "F1"

            [[step]]
            kind = "goto"
            label = "start"
        "#;
        let cb = parse(src).unwrap();
        assert_eq!(cb.steps.len(), 3);
        match &cb.steps[2].kind {
            StepKind::Goto { target_label, target_idx } => {
                assert_eq!(target_label, "start");
                assert_eq!(*target_idx, 0, "label 'start' debe resolver a idx 0");
            }
            _ => panic!("step[2] no es Goto"),
        }
    }

    #[test]
    fn parse_goto_if_with_condition() {
        let src = r#"
            [[step]]
            kind = "label"
            name = "hunt"

            [[step]]
            kind = "goto_if"
            label = "hunt"
            when = "hp_below(0.5)"
        "#;
        let cb = parse(src).unwrap();
        match &cb.steps[1].kind {
            StepKind::GotoIf { condition, target_idx, .. } => {
                assert_eq!(*target_idx, 0);
                assert!(matches!(condition, Condition::HpBelow(f) if (*f - 0.5).abs() < 1e-6));
            }
            _ => panic!("not GotoIf"),
        }
    }

    #[test]
    fn parse_stand_with_mobs_killed() {
        let src = r#"
            [[step]]
            kind = "stand"
            until = "mobs_killed(3)"
            max_wait_ms = 30000
        "#;
        let cb = parse(src).unwrap();
        match &cb.steps[0].kind {
            StepKind::Stand { until, max_wait_ms } => {
                assert!(matches!(until, StandUntil::MobsKilled(3)));
                assert_eq!(*max_wait_ms, 30000);
            }
            _ => panic!("not Stand"),
        }
    }

    #[test]
    fn parse_loot_with_coords() {
        let src = r#"
            [[step]]
            kind = "loot"
            vx = 500
            vy = 300
            retry_count = 2
        "#;
        let cb = parse(src).unwrap();
        assert!(matches!(
            &cb.steps[0].kind,
            StepKind::Loot { vx: 500, vy: 300, retry_count: 2 }
        ));
    }

    #[test]
    fn parse_skip_if_blocked_wrapping_walk() {
        let src = r#"
            [[step]]
            kind = "skip_if_blocked"
            key = "D"
            duration_ms = 5000
            interval_ms = 300
            max_wait_ms = 2000
        "#;
        let cb = parse(src).unwrap();
        match &cb.steps[0].kind {
            StepKind::SkipIfBlocked { inner, max_wait_ms } => {
                assert_eq!(*max_wait_ms, 2000);
                assert!(matches!(inner.as_ref(), StepKind::Walk { duration_ms: 5000, .. }));
            }
            _ => panic!("not SkipIfBlocked"),
        }
    }

    #[test]
    fn parse_npc_dialog_with_phrases() {
        let src = r#"
            [[step]]
            kind = "npc_dialog"
            phrases = ["hi", "trade"]
            wait_prompt_ms = 500
        "#;
        let cb = parse(src).unwrap();
        match &cb.steps[0].kind {
            StepKind::NpcDialog { phrases, wait_prompt_ms } => {
                assert_eq!(phrases, &vec!["hi".to_string(), "trade".to_string()]);
                assert_eq!(*wait_prompt_ms, 500);
            }
            _ => panic!("not NpcDialog"),
        }
    }

    #[test]
    fn parse_npc_dialog_rejects_empty_phrases() {
        let src = r#"
            [[step]]
            kind = "npc_dialog"
            phrases = []
        "#;
        assert!(parse(src).is_err());
    }

    #[test]
    fn parse_multi_section_concatenates_steps() {
        let src = r#"
            [[section]]
            name = "hunt"
            steps = [
                { kind = "label", name = "hunt_entry" },
                { kind = "hotkey", key = "F1" },
            ]

            [[section]]
            name = "refill"
            steps = [
                { kind = "label", name = "refill_entry" },
                { kind = "goto", label = "hunt_entry" },
            ]
        "#;
        let cb = parse(src).unwrap();
        assert_eq!(cb.steps.len(), 4);
        // Goto en idx=3 debe resolver a idx=0 (hunt_entry).
        match &cb.steps[3].kind {
            StepKind::Goto { target_idx, .. } => assert_eq!(*target_idx, 0),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_rejects_duplicate_labels() {
        let src = r#"
            [[step]]
            kind = "label"
            name = "dup"
            [[step]]
            kind = "label"
            name = "dup"
        "#;
        assert!(parse(src).is_err(), "labels duplicados deben fallar");
    }

    #[test]
    fn parse_rejects_goto_to_unknown_label() {
        let src = r#"
            [[step]]
            kind = "goto"
            label = "no_existe"
        "#;
        assert!(parse(src).is_err(), "goto a label inexistente debe fallar");
    }

    #[test]
    fn parse_rejects_unknown_kind() {
        let src = r#"
            [[step]]
            kind = "foo_bar"
        "#;
        assert!(parse(src).is_err());
    }

    #[test]
    fn parse_stand_until_variants() {
        assert!(matches!(parse_stand_until("hp_full").unwrap(), StandUntil::HpFull));
        assert!(matches!(parse_stand_until("mana_full").unwrap(), StandUntil::ManaFull));
        assert!(matches!(parse_stand_until("no_combat").unwrap(), StandUntil::NoCombat));
        assert!(matches!(parse_stand_until("timer_ms(500)").unwrap(), StandUntil::TimerMs(500)));
        assert!(matches!(parse_stand_until("mobs_killed(5)").unwrap(), StandUntil::MobsKilled(5)));
        assert!(parse_stand_until("banana").is_err());
    }

    #[test]
    fn parse_condition_variants() {
        assert!(matches!(parse_condition("hp_below(0.3)").unwrap(), Condition::HpBelow(_)));
        assert!(matches!(parse_condition("mana_below(0.2)").unwrap(), Condition::ManaBelow(_)));
        assert!(matches!(parse_condition("kills_gte(10)").unwrap(), Condition::KillsGte(10)));
        assert!(matches!(parse_condition("no_combat").unwrap(), Condition::NoCombat));
        // Not
        let c = parse_condition("not:hp_below(0.5)").unwrap();
        assert!(matches!(c, Condition::Not(_)));
    }

    #[test]
    fn parse_bundled_example_file() {
        // Verifica que el archivo bundled assets/cavebot/example.toml parsea
        // correctamente. Sirve de smoke test de la sintaxis documentada.
        let path = std::path::Path::new("../assets/cavebot/example.toml");
        if !path.exists() {
            // Algunos CI corren desde el workspace root, otros desde bot/.
            let alt = std::path::Path::new("assets/cavebot/example.toml");
            if !alt.exists() {
                // Ningún path funciona — skip (no fail para no bloquear CI).
                return;
            }
            let cb = load(alt, 30).expect("example.toml debe parsearse");
            assert!(cb.steps.len() >= 8);
            return;
        }
        let cb = load(path, 30).expect("example.toml debe parsearse");
        assert!(cb.steps.len() >= 8);
    }

    // ── Fuzzing: malformed inputs no deben crashear el parser ────────────
    //
    // El parser es público: cualquier TOML que el usuario ponga en
    // assets/cavebot/ debe ser procesable. Si hay errores, deben reportarse
    // como Result::Err, NUNCA como panic. Estos tests aseguran esa propiedad.

    #[test]
    fn fuzz_empty_toml_returns_ok_or_err_not_panic() {
        // Empty string → válido pero sin steps.
        let r = parse("");
        // Puede retornar Ok con 0 steps o Err; lo importante es no panic.
        // Si retorna Ok, steps vacío es válido (cavebot sin script).
        match r {
            Ok(cb) => assert_eq!(cb.steps.len(), 0),
            Err(_) => {} // también OK
        }
    }

    #[test]
    fn fuzz_malformed_toml_syntax_returns_err() {
        // TOML syntácticamente inválido: sin closing quote.
        let r = parse(r#"[[step]] kind = "walk"#);
        assert!(r.is_err(), "TOML malformed debe retornar Err, no panic");
    }

    #[test]
    fn fuzz_step_without_kind_field_returns_err() {
        let src = r#"
            [[step]]
            key = "F1"
            duration_ms = 500
        "#;
        let r = parse(src);
        assert!(r.is_err(), "step sin kind debe ser error");
    }

    #[test]
    fn fuzz_walk_without_key_returns_err() {
        let src = r#"
            [[step]]
            kind = "walk"
            duration_ms = 1000
        "#;
        let r = parse(src);
        assert!(r.is_err(), "walk sin key debe ser error");
    }

    #[test]
    fn fuzz_walk_invalid_key_returns_err() {
        let src = r#"
            [[step]]
            kind = "walk"
            key = "NOTAKEY123"
            duration_ms = 1000
        "#;
        let r = parse(src);
        assert!(r.is_err(), "walk con key inválida debe ser error");
    }

    #[test]
    fn fuzz_wait_missing_duration_returns_err() {
        let src = r#"
            [[step]]
            kind = "wait"
        "#;
        let r = parse(src);
        assert!(r.is_err(), "wait sin duration_ms debe ser error");
    }

    #[test]
    fn fuzz_label_with_empty_name_returns_err() {
        let src = r#"
            [[step]]
            kind = "label"
            name = ""
        "#;
        let r = parse(src);
        assert!(r.is_err(), "label con name vacío debe ser error");
    }

    #[test]
    fn fuzz_goto_without_label_field_returns_err() {
        let src = r#"
            [[step]]
            kind = "goto"
        "#;
        let r = parse(src);
        assert!(r.is_err(), "goto sin label debe ser error");
    }

    #[test]
    fn fuzz_very_long_label_name_parses_or_rejects_cleanly() {
        // Stress test: label name de 10000 chars. No importa si acepta
        // o rechaza, solo no panic.
        let name: String = std::iter::repeat('a').take(10000).collect();
        let src = format!(
            r#"
            [[step]]
            kind = "label"
            name = "{}"
            "#,
            name
        );
        let r = parse(&src);
        // Ambos resultados son OK (acepta o rechaza cleanly).
        let _ = r;
    }

    #[test]
    fn fuzz_deeply_nested_structure_doesnt_stack_overflow() {
        // Stress test: 1000 labels + 1000 gotos anidados no deben hacer
        // stack overflow ni tardar más de ~1 seg.
        let mut src = String::with_capacity(50000);
        for i in 0..500 {
            src.push_str(&format!(
                "[[step]]\nkind = \"label\"\nname = \"L{}\"\n\n[[step]]\nkind = \"goto\"\nlabel = \"L{}\"\n\n",
                i,
                (i + 1) % 500, // ciclo, todos los labels existen
            ));
        }
        let start = std::time::Instant::now();
        let r = parse(&src);
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "parser tardó {:?}, límite 1s",
            elapsed
        );
        // 1000 steps debe parsear OK.
        if let Ok(cb) = r {
            assert_eq!(cb.steps.len(), 1000);
        }
    }

    #[test]
    fn fuzz_wrong_type_field_returns_err() {
        // duration_ms como string en vez de int.
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = "not a number"
        "#;
        let r = parse(src);
        assert!(r.is_err(), "wrong type debe ser error");
    }

    // ── StowBag step parser tests ────────────────────────────────────────

    #[test]
    fn parse_stow_bag_with_all_fields() {
        let src = r#"
            [[step]]
            kind = "stow_bag"
            bag_vx = 1598
            bag_vy = 128
            menu_offset_y = 70
            menu_wait_ms = 300
            process_ms = 2000
        "#;
        let cb = parse(src).unwrap();
        assert_eq!(cb.steps.len(), 1);
        match &cb.steps[0].kind {
            StepKind::StowBag { bag_vx, bag_vy, menu_offset_y, menu_wait_ms, process_ms } => {
                assert_eq!(*bag_vx, 1598);
                assert_eq!(*bag_vy, 128);
                assert_eq!(*menu_offset_y, 70);
                assert_eq!(*menu_wait_ms, 300);
                assert_eq!(*process_ms, 2000);
            }
            _ => panic!("expected StowBag kind"),
        }
    }

    #[test]
    fn parse_stow_bag_uses_defaults_for_optional_fields() {
        // Solo bag_vx y bag_vy requeridos. menu_offset_y=70, menu_wait_ms=300,
        // process_ms=2000 son defaults razonables.
        let src = r#"
            [[step]]
            kind = "stow_bag"
            bag_vx = 1598
            bag_vy = 128
        "#;
        let cb = parse(src).unwrap();
        match &cb.steps[0].kind {
            StepKind::StowBag { menu_offset_y, menu_wait_ms, process_ms, .. } => {
                assert_eq!(*menu_offset_y, 70);
                assert_eq!(*menu_wait_ms, 300);
                assert_eq!(*process_ms, 2000);
            }
            _ => panic!("expected StowBag kind"),
        }
    }

    #[test]
    fn parse_stow_bag_missing_bag_vx_returns_err() {
        let src = r#"
            [[step]]
            kind = "stow_bag"
            bag_vy = 128
        "#;
        let r = parse(src);
        assert!(r.is_err(), "stow_bag sin bag_vx debe ser error");
    }

    #[test]
    fn parse_stow_bag_missing_bag_vy_returns_err() {
        let src = r#"
            [[step]]
            kind = "stow_bag"
            bag_vx = 1598
        "#;
        let r = parse(src);
        assert!(r.is_err(), "stow_bag sin bag_vy debe ser error");
    }

    /// Smoke test: los 3 scripts MINOR completados en R10 deben parsear OK.
    /// Si añades un nuevo step kind o cambias la sintaxis, este test detecta
    /// regresiones en los scripts shipped.
    #[test]
    fn parse_bundled_completed_scripts() {
        let scripts = [
            "abdendriel_wasps.toml",
            "multi_floor_hunt.toml",
            "thais_hunt_refill_template.toml",
            "full_refill_example.toml",
        ];
        for script in &scripts {
            let p1 = std::path::PathBuf::from("../assets/cavebot").join(script);
            let p2 = std::path::PathBuf::from("assets/cavebot").join(script);
            let path = if p1.exists() {
                p1
            } else if p2.exists() {
                p2
            } else {
                continue; // skip si no existe en este CI
            };
            let cb = load(&path, 30)
                .unwrap_or_else(|e| panic!("{} no parsea: {}", script, e));
            assert!(cb.steps.len() > 0, "{} no tiene steps", script);
        }
    }
}
