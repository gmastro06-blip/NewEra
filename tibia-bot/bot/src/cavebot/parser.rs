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
use crate::cavebot::step::{Condition, StandUntil, Step, StepKind, StepVerify, VerifyCheck, VerifyFailAction};

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
    /// Opcional. Si se setea, el parser carga `assets/hunts/<name>.toml`
    /// y los steps pueden consumir datos del profile con `from_profile = true`
    /// (ej: check_supplies lee las thresholds de `[supplies]` en vez de repetir
    /// la lista inline). Ver `assets/hunts/abdendriel_wasps.toml` para un ejemplo.
    #[serde(default)]
    hunt_profile: Option<String>,
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
    /// | "open_npc_trade" | "type_in_field"
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
    // OpenNpcTrade (greeting + click en botón bag del greeting window)
    #[serde(default)]
    greeting_phrases: Option<Vec<String>>,
    #[serde(default)]
    bag_button_vx: Option<i32>,
    #[serde(default)]
    bag_button_vy: Option<i32>,
    #[serde(default)]
    wait_button_ms: Option<u64>,
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
    // StowAllItems (Tibia 12 Supply Stash — iterativo per-item)
    #[serde(default)]
    slot_vx: Option<i32>,
    #[serde(default)]
    slot_vy: Option<i32>,
    #[serde(default)]
    menu_offset_x: Option<i32>,
    #[serde(default)]
    menu_offset_y: Option<i32>,
    #[serde(default)]
    stow_process_ms: Option<u64>,
    #[serde(default)]
    max_iterations: Option<u8>,
    // BuyItem
    #[serde(default)]
    item_vx: Option<i32>,
    #[serde(default)]
    item_vy: Option<i32>,
    /// Coords del input field "Amount" (Tibia 12). Opcionales: si ambos están
    /// presentes, el runner usa el flujo moderno (tipear dígitos + 1 click);
    /// si faltan, usa el legacy (N clicks de confirm).
    #[serde(default)]
    amount_vx: Option<i32>,
    #[serde(default)]
    amount_vy: Option<i32>,
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
    /// Si true, CheckSupplies toma las thresholds del hunt profile declarado
    /// en `[cavebot].hunt_profile`. Mutuamente exclusivo con `requirements`.
    #[serde(default)]
    from_profile: bool,
    // TypeInField
    #[serde(default)]
    field_vx: Option<i32>,
    #[serde(default)]
    field_vy: Option<i32>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    wait_after_click_ms: Option<u64>,
    #[serde(default)]
    wait_after_type_ms: Option<u64>,
    #[serde(default)]
    char_spacing_ms: Option<u64>,
    // Verify (postcondition)
    #[serde(default)]
    verify: Option<StepVerifyToml>,
}

#[derive(Debug, Deserialize, Clone)]
struct SupplyRequirement {
    item: String,
    min_count: u32,
}

/// Parsed TOML representation of `[step.verify]` sub-table.
///
/// Schema:
/// ```toml
/// [step.verify]
/// # Exactly ONE of the following must be set:
/// template        = "npc_trade"             # TemplateVisible
/// absent_template = "npc_trade"             # TemplateAbsent
/// condition       = "has_item(mana_potion, 3)"   # ConditionMet (same grammar as goto_if.when)
/// inventory_delta = { item = "mana_potion", min_abs_delta = 50, require_positive = true }
///
/// # Optional for template checks: ROI override
/// roi = { x = 100, y = 200, w = 400, h = 300 }
///
/// # Common optional fields:
/// timeout_ms = 3000              # default
/// on_fail    = "safety_pause"    # | "advance" | "goto:<label>"
/// ```
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct StepVerifyToml {
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    absent_template: Option<String>,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    inventory_delta: Option<InventoryDeltaToml>,
    #[serde(default)]
    roi: Option<RoiDefToml>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    on_fail: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct InventoryDeltaToml {
    item: String,
    min_abs_delta: u32,
    #[serde(default)]
    require_positive: bool,
}

#[derive(Debug, Deserialize, Clone, Copy)]
struct RoiDefToml {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl From<RoiDefToml> for crate::sense::vision::calibration::RoiDef {
    fn from(r: RoiDefToml) -> Self {
        Self { x: r.x, y: r.y, w: r.w, h: r.h }
    }
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

    // Resolver `hunts_dir` convencional: el cavebot vive típicamente en
    // `<assets>/cavebot/<name>.toml` y los hunt profiles en `<assets>/hunts/`.
    // Si la estructura de directorios es distinta, el profile no se carga y
    // los steps con `from_profile = true` fallarán con un error explícito.
    let hunts_dir = path.parent().and_then(Path::parent).map(|p| p.join("hunts"));

    build_cavebot(file, fps, tuning, hunts_dir.as_deref())
}

/// Construye un `Cavebot` desde una estructura TOML ya parseada.
/// Privado al módulo — los tests lo usan directamente via super::.
fn build_cavebot(
    file:      CavebotFile,
    fps:       u32,
    tuning:    super::runner::NodeTuning,
    hunts_dir: Option<&Path>,
) -> Result<Cavebot> {
    // Si hay sections, concatenar sus steps en orden. Si no, usar los steps planos.
    let raw_steps: Vec<StepToml> = if !file.sections.is_empty() {
        file.sections.into_iter().flat_map(|s| s.steps).collect()
    } else {
        file.steps
    };

    if raw_steps.is_empty() {
        bail!("Cavebot file no contiene ningún step");
    }

    // Cargar hunt profile si el cavebot lo referencia.
    let hunt_profile: Option<super::hunt_profile::HuntProfile> = match (
        file.cavebot.hunt_profile.as_deref(),
        hunts_dir,
    ) {
        (Some(name), Some(dir)) => {
            let profile = super::hunt_profile::HuntProfile::load_by_name(dir, name)
                .with_context(|| format!(
                    "no se pudo cargar hunt profile '{}' desde '{}'",
                    name, dir.display()
                ))?;
            tracing::info!(
                "Cavebot hunt_profile='{}' cargado: {} stackables, {} supplies",
                profile.name,
                profile.loot.stackables.len(),
                profile.supplies.len()
            );
            Some(profile)
        }
        (Some(name), None) => {
            bail!(
                "cavebot TOML declara hunt_profile='{}' pero no se pudo derivar \
                 hunts_dir del path. Los cavebots deben vivir en <assets>/cavebot/*.toml \
                 con hunt profiles en <assets>/hunts/*.toml.",
                name
            );
        }
        (None, _) => None,
    };

    // Primera pasada: convertir cada StepToml → Step, usando target_idx=0 placeholder
    // para los Goto. Recolectar simultáneamente el mapa label → índice.
    let mut steps = Vec::with_capacity(raw_steps.len());
    let mut labels: HashMap<String, usize> = HashMap::new();

    for (idx, st) in raw_steps.into_iter().enumerate() {
        let step = parse_step_toml(st, hunt_profile.as_ref())
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
        // Resolve label for verify.on_fail = GotoLabel (same label map).
        if let Some(v) = &mut step.verify {
            if let VerifyFailAction::GotoLabel { target_label, target_idx } = &mut v.on_fail {
                *target_idx = *labels.get(target_label)
                    .with_context(|| format!(
                        "step[{}] verify.on_fail goto: label '{}' no encontrado", idx, target_label
                    ))?;
            }
        }
    }

    Ok(Cavebot::with_tuning_and_profile(
        steps,
        file.cavebot.loop_,
        fps,
        tuning,
        hunt_profile.map(|p| p.name),
    ))
}

// ── StepToml → Step ──────────────────────────────────────────────────────────

fn parse_step_toml(
    st:            StepToml,
    hunt_profile:  Option<&super::hunt_profile::HuntProfile>,
) -> Result<Step> {
    let StepToml {
        kind, name, key, duration_ms, interval_ms,
        until, max_wait_ms, label, when, vx, vy, retry_count,
        phrases, wait_prompt_ms,
        greeting_phrases, bag_button_vx, bag_button_vy, wait_button_ms,
        x, y, z,
        chest_vx, chest_vy, stow_vx, stow_vy, menu_wait_ms, process_ms,
        item_vx, item_vy, amount_vx, amount_vy, confirm_vx, confirm_vy, quantity, spacing_ms,
        on_fail, requirements, from_profile,
        slot_vx, slot_vy, menu_offset_x, menu_offset_y, stow_process_ms, max_iterations,
        field_vx, field_vy, text, wait_after_click_ms, wait_after_type_ms, char_spacing_ms,
        verify,
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
        "open_npc_trade" => {
            let greeting_phrases = greeting_phrases
                .context("open_npc_trade: falta 'greeting_phrases' (array de strings)")?;
            if greeting_phrases.is_empty() {
                bail!("open_npc_trade: 'greeting_phrases' no puede estar vacío");
            }
            StepKind::OpenNpcTrade {
                greeting_phrases,
                bag_button_vx: bag_button_vx
                    .context("open_npc_trade: falta 'bag_button_vx'")?,
                bag_button_vy: bag_button_vy
                    .context("open_npc_trade: falta 'bag_button_vy'")?,
                wait_button_ms: wait_button_ms.unwrap_or(800),
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
        "stow_all_items" => StepKind::StowAllItems {
            slot_vx:         slot_vx.context("stow_all_items: falta 'slot_vx' (primer slot del bag)")?,
            slot_vy:         slot_vy.context("stow_all_items: falta 'slot_vy'")?,
            menu_offset_x:   menu_offset_x.unwrap_or(90),
            menu_offset_y:   menu_offset_y.unwrap_or(197),
            menu_wait_ms:    menu_wait_ms.unwrap_or(300),
            stow_process_ms: stow_process_ms.unwrap_or(800),
            max_iterations:  max_iterations.unwrap_or(8),
        },
        "buy_item" => {
            // amount_vx y amount_vy son opcionales. Si sólo uno está presente
            // es una config ambigua → error explícito para que el usuario
            // complete o remueva el campo.
            match (amount_vx, amount_vy) {
                (Some(_), None) => bail!(
                    "buy_item: 'amount_vx' presente pero falta 'amount_vy'. \
                     Ambos deben estar o ninguno."
                ),
                (None, Some(_)) => bail!(
                    "buy_item: 'amount_vy' presente pero falta 'amount_vx'. \
                     Ambos deben estar o ninguno."
                ),
                _ => {}
            }
            StepKind::BuyItem {
                item_vx:    item_vx.context("buy_item: falta 'item_vx'")?,
                item_vy:    item_vy.context("buy_item: falta 'item_vy'")?,
                amount_vx,
                amount_vy,
                confirm_vx: confirm_vx.context("buy_item: falta 'confirm_vx'")?,
                confirm_vy: confirm_vy.context("buy_item: falta 'confirm_vy'")?,
                quantity:   quantity.context("buy_item: falta 'quantity'")?,
                spacing_ms: spacing_ms.unwrap_or(150),
            }
        }
        "check_supplies" => {
            let fail_label = on_fail.context("check_supplies: falta 'on_fail'")?;

            // Tres casos:
            // 1. from_profile=true + requirements=None → lee del hunt profile
            // 2. from_profile=false + requirements=Some(...) → inline legacy
            // 3. ambos presentes → error (ambiguous)
            // 4. ninguno → error
            let reqs: Vec<(String, u32)> = match (from_profile, requirements) {
                (true, Some(_)) => bail!(
                    "check_supplies: `from_profile = true` es mutuamente exclusivo con \
                     `requirements = [...]`. Elegir uno."
                ),
                (true, None) => {
                    let profile = hunt_profile.context(
                        "check_supplies: `from_profile = true` requiere que el cavebot \
                         TOML declare `[cavebot] hunt_profile = \"<name>\"` al top-level."
                    )?;
                    let list = profile.supplies_list();
                    if list.is_empty() {
                        bail!(
                            "check_supplies: hunt_profile '{}' tiene [supplies] vacío.",
                            profile.name
                        );
                    }
                    // Convert (String, SupplyConfig) → (String, u32) usando el umbral `min`.
                    list.into_iter().map(|(name, cfg)| (name, cfg.min)).collect()
                }
                (false, Some(rs)) => {
                    if rs.is_empty() {
                        bail!("check_supplies: 'requirements' no puede estar vacío");
                    }
                    rs.into_iter().map(|r| (r.item, r.min_count)).collect()
                }
                (false, None) => bail!(
                    "check_supplies: falta `requirements` inline o `from_profile = true`"
                ),
            };

            StepKind::CheckSupplies {
                requirements: reqs,
                on_fail_label: fail_label,
                on_fail_idx: 0, // resuelto en label pass
            }
        }
        "type_in_field" => {
            let text = text.context("type_in_field: falta 'text'")?;
            if text.is_empty() {
                bail!("type_in_field: 'text' no puede estar vacío");
            }
            StepKind::TypeInField {
                field_vx:            field_vx.context("type_in_field: falta 'field_vx'")?,
                field_vy:            field_vy.context("type_in_field: falta 'field_vy'")?,
                text,
                wait_after_click_ms: wait_after_click_ms.unwrap_or(150),
                wait_after_type_ms:  wait_after_type_ms.unwrap_or(200),
                char_spacing_ms:     char_spacing_ms.unwrap_or(80),
            }
        }
        other => bail!("kind desconocido: '{}'", other),
    };

    let verify = match verify {
        Some(v) => Some(parse_verify_toml(v).context("verify")?),
        None => None,
    };

    Ok(Step { label: name, kind: step_kind, verify })
}

fn parse_verify_toml(toml_v: StepVerifyToml) -> Result<StepVerify> {
    // Count how many check fields are set — must be exactly 1.
    let count = [
        toml_v.template.is_some(),
        toml_v.absent_template.is_some(),
        toml_v.condition.is_some(),
        toml_v.inventory_delta.is_some(),
    ].iter().filter(|&&b| b).count();

    if count == 0 {
        bail!("verify: debe especificar exactamente uno de: template, absent_template, condition, inventory_delta");
    }
    if count > 1 {
        bail!("verify: solo uno de template/absent_template/condition/inventory_delta puede estar presente, no múltiples");
    }

    let check = if let Some(name) = toml_v.template {
        VerifyCheck::TemplateVisible { name, roi: toml_v.roi.map(Into::into) }
    } else if let Some(name) = toml_v.absent_template {
        VerifyCheck::TemplateAbsent { name, roi: toml_v.roi.map(Into::into) }
    } else if let Some(cond_str) = toml_v.condition {
        VerifyCheck::ConditionMet(parse_condition(&cond_str)?)
    } else if let Some(delta) = toml_v.inventory_delta {
        if delta.min_abs_delta == 0 {
            bail!("verify.inventory_delta.min_abs_delta debe ser > 0");
        }
        VerifyCheck::InventoryDelta {
            item: delta.item,
            min_abs_delta: delta.min_abs_delta,
            require_positive: delta.require_positive,
        }
    } else {
        unreachable!();
    };

    let on_fail = match toml_v.on_fail.as_deref() {
        None | Some("safety_pause") => VerifyFailAction::SafetyPause,
        Some("advance") => VerifyFailAction::Advance,
        Some(s) if s.starts_with("goto:") => {
            let label = s.trim_start_matches("goto:").to_string();
            if label.is_empty() {
                bail!("verify.on_fail: 'goto:' requiere nombre de label. Ej: 'goto:refill'");
            }
            VerifyFailAction::GotoLabel { target_label: label, target_idx: 0 }
        }
        Some(other) => bail!(
            "verify.on_fail: valor inválido '{}'. Esperado: 'safety_pause' | 'advance' | 'goto:<label>'",
            other
        ),
    };

    Ok(StepVerify {
        check,
        timeout_ms: toml_v.timeout_ms.unwrap_or(3_000),
        on_fail,
    })
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
        build_cavebot(file, 30, crate::cavebot::runner::NodeTuning::default(), None)
    }

    /// Variant que carga hunt profiles desde el directorio real `assets/hunts/`.
    /// Usado por tests que validan `[cavebot] hunt_profile = "..."` en vivo.
    #[allow(dead_code)]
    fn parse_with_hunts_dir(toml_src: &str) -> Result<Cavebot> {
        let file: CavebotFile = toml::from_str(toml_src)?;
        let hunts_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("assets/hunts");
        build_cavebot(
            file,
            30,
            crate::cavebot::runner::NodeTuning::default(),
            Some(&hunts_dir),
        )
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

    // ── OpenNpcTrade parser tests ────────────────────────────────────

    #[test]
    fn parse_open_npc_trade_with_all_fields() {
        let src = r#"
            [[step]]
            kind = "open_npc_trade"
            greeting_phrases = ["hi"]
            bag_button_vx    = 350
            bag_button_vy    = 400
            wait_button_ms   = 800
        "#;
        let cb = parse(src).unwrap();
        assert_eq!(cb.steps.len(), 1);
        match &cb.steps[0].kind {
            StepKind::OpenNpcTrade {
                greeting_phrases, bag_button_vx, bag_button_vy, wait_button_ms,
            } => {
                assert_eq!(greeting_phrases, &vec!["hi".to_string()]);
                assert_eq!(*bag_button_vx, 350);
                assert_eq!(*bag_button_vy, 400);
                assert_eq!(*wait_button_ms, 800);
            }
            _ => panic!("not OpenNpcTrade"),
        }
    }

    #[test]
    fn parse_open_npc_trade_uses_default_wait_button_ms() {
        // Sin wait_button_ms explícito → default 800.
        let src = r#"
            [[step]]
            kind = "open_npc_trade"
            greeting_phrases = ["hi", "yes"]
            bag_button_vx    = 100
            bag_button_vy    = 200
        "#;
        let cb = parse(src).unwrap();
        match &cb.steps[0].kind {
            StepKind::OpenNpcTrade { wait_button_ms, greeting_phrases, .. } => {
                assert_eq!(*wait_button_ms, 800);
                assert_eq!(greeting_phrases.len(), 2);
            }
            _ => panic!("not OpenNpcTrade"),
        }
    }

    #[test]
    fn parse_open_npc_trade_rejects_missing_greeting_phrases() {
        let src = r#"
            [[step]]
            kind = "open_npc_trade"
            bag_button_vx = 100
            bag_button_vy = 200
        "#;
        assert!(
            parse(src).is_err(),
            "open_npc_trade sin greeting_phrases debe ser error"
        );
    }

    #[test]
    fn parse_open_npc_trade_rejects_empty_greeting_phrases() {
        let src = r#"
            [[step]]
            kind = "open_npc_trade"
            greeting_phrases = []
            bag_button_vx = 100
            bag_button_vy = 200
        "#;
        assert!(
            parse(src).is_err(),
            "open_npc_trade con greeting_phrases vacío debe ser error"
        );
    }

    #[test]
    fn parse_open_npc_trade_rejects_missing_bag_button_vx() {
        let src = r#"
            [[step]]
            kind = "open_npc_trade"
            greeting_phrases = ["hi"]
            bag_button_vy = 200
        "#;
        assert!(
            parse(src).is_err(),
            "open_npc_trade sin bag_button_vx debe ser error"
        );
    }

    #[test]
    fn parse_open_npc_trade_rejects_missing_bag_button_vy() {
        let src = r#"
            [[step]]
            kind = "open_npc_trade"
            greeting_phrases = ["hi"]
            bag_button_vx = 100
        "#;
        assert!(
            parse(src).is_err(),
            "open_npc_trade sin bag_button_vy debe ser error"
        );
    }

    #[test]
    fn parse_npc_dialog_still_works_after_open_npc_trade_added() {
        // Backwards-compat: el viejo NpcDialog sigue parseando OK.
        let src = r#"
            [[step]]
            kind = "npc_dialog"
            phrases = ["hi", "trade"]
            wait_prompt_ms = 500
        "#;
        let cb = parse(src).unwrap();
        assert!(matches!(&cb.steps[0].kind, StepKind::NpcDialog { .. }));
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

    // ── StowAllItems step parser tests ───────────────────────────────────

    #[test]
    fn parse_stow_all_items_with_all_fields() {
        let src = r#"
            [[step]]
            kind = "stow_all_items"
            slot_vx = 1595
            slot_vy = 140
            menu_offset_x = 90
            menu_offset_y = 197
            menu_wait_ms = 300
            stow_process_ms = 800
            max_iterations = 8
        "#;
        let cb = parse(src).unwrap();
        assert_eq!(cb.steps.len(), 1);
        match &cb.steps[0].kind {
            StepKind::StowAllItems {
                slot_vx, slot_vy, menu_offset_x, menu_offset_y,
                menu_wait_ms, stow_process_ms, max_iterations,
            } => {
                assert_eq!(*slot_vx, 1595);
                assert_eq!(*slot_vy, 140);
                assert_eq!(*menu_offset_x, 90);
                assert_eq!(*menu_offset_y, 197);
                assert_eq!(*menu_wait_ms, 300);
                assert_eq!(*stow_process_ms, 800);
                assert_eq!(*max_iterations, 8);
            }
            _ => panic!("expected StowAllItems kind"),
        }
    }

    #[test]
    fn parse_stow_all_items_uses_defaults() {
        // Solo slot_vx y slot_vy requeridos. Resto usa defaults.
        let src = r#"
            [[step]]
            kind = "stow_all_items"
            slot_vx = 1595
            slot_vy = 140
        "#;
        let cb = parse(src).unwrap();
        match &cb.steps[0].kind {
            StepKind::StowAllItems {
                menu_offset_x, menu_offset_y, menu_wait_ms,
                stow_process_ms, max_iterations, ..
            } => {
                assert_eq!(*menu_offset_x, 90);
                assert_eq!(*menu_offset_y, 197);
                assert_eq!(*menu_wait_ms, 300);
                assert_eq!(*stow_process_ms, 800);
                assert_eq!(*max_iterations, 8);
            }
            _ => panic!("expected StowAllItems kind"),
        }
    }

    #[test]
    fn parse_stow_all_items_missing_slot_vx_returns_err() {
        let src = r#"
            [[step]]
            kind = "stow_all_items"
            slot_vy = 140
        "#;
        let r = parse(src);
        assert!(r.is_err(), "stow_all_items sin slot_vx debe ser error");
    }

    #[test]
    fn parse_stow_all_items_missing_slot_vy_returns_err() {
        let src = r#"
            [[step]]
            kind = "stow_all_items"
            slot_vx = 1595
        "#;
        let r = parse(src);
        assert!(r.is_err(), "stow_all_items sin slot_vy debe ser error");
    }

    // ── TypeInField step parser tests ─────────────────────────────────

    #[test]
    fn parse_type_in_field_with_all_fields() {
        let src = r#"
            [[step]]
            kind = "type_in_field"
            field_vx            = 426
            field_vy            = 261
            text                = "mana potion"
            wait_after_click_ms = 120
            wait_after_type_ms  = 250
            char_spacing_ms     = 60
        "#;
        let cb = parse(src).unwrap();
        assert_eq!(cb.steps.len(), 1);
        match &cb.steps[0].kind {
            StepKind::TypeInField {
                field_vx, field_vy, text,
                wait_after_click_ms, wait_after_type_ms, char_spacing_ms,
            } => {
                assert_eq!(*field_vx, 426);
                assert_eq!(*field_vy, 261);
                assert_eq!(text, "mana potion");
                assert_eq!(*wait_after_click_ms, 120);
                assert_eq!(*wait_after_type_ms, 250);
                assert_eq!(*char_spacing_ms, 60);
            }
            _ => panic!("expected TypeInField kind"),
        }
    }

    #[test]
    fn parse_type_in_field_rejects_empty_text() {
        let src = r#"
            [[step]]
            kind = "type_in_field"
            field_vx = 100
            field_vy = 200
            text     = ""
        "#;
        assert!(
            parse(src).is_err(),
            "type_in_field con text vacío debe ser error"
        );
    }

    #[test]
    fn parse_type_in_field_applies_defaults() {
        // Sin wait/spacing explícitos → defaults 150 / 200 / 80 ms.
        let src = r#"
            [[step]]
            kind = "type_in_field"
            field_vx = 426
            field_vy = 261
            text     = "ab"
        "#;
        let cb = parse(src).unwrap();
        match &cb.steps[0].kind {
            StepKind::TypeInField {
                wait_after_click_ms, wait_after_type_ms, char_spacing_ms, ..
            } => {
                assert_eq!(*wait_after_click_ms, 150);
                assert_eq!(*wait_after_type_ms, 200);
                assert_eq!(*char_spacing_ms, 80);
            }
            _ => panic!("expected TypeInField kind"),
        }
    }

    #[test]
    fn parse_type_in_field_rejects_missing_coords() {
        // field_vx ausente.
        let src_no_vx = r#"
            [[step]]
            kind = "type_in_field"
            field_vy = 200
            text     = "ab"
        "#;
        assert!(parse(src_no_vx).is_err(), "type_in_field sin field_vx debe ser error");
        // field_vy ausente.
        let src_no_vy = r#"
            [[step]]
            kind = "type_in_field"
            field_vx = 100
            text     = "ab"
        "#;
        assert!(parse(src_no_vy).is_err(), "type_in_field sin field_vy debe ser error");
        // text ausente.
        let src_no_text = r#"
            [[step]]
            kind = "type_in_field"
            field_vx = 100
            field_vy = 200
        "#;
        assert!(parse(src_no_text).is_err(), "type_in_field sin text debe ser error");
    }

    // ── StepVerify parser tests (Fase 2C) ────────────────────────────────

    #[test]
    fn verify_template_visible_parses() {
        let src = r#"
            [[step]]
            kind             = "open_npc_trade"
            greeting_phrases = ["hi"]
            bag_button_vx    = 100
            bag_button_vy    = 200

            [step.verify]
            template = "npc_trade"
        "#;
        let cb = parse(src).unwrap();
        let v = cb.steps[0].verify.as_ref().expect("verify present");
        match &v.check {
            VerifyCheck::TemplateVisible { name, roi } => {
                assert_eq!(name, "npc_trade");
                assert!(roi.is_none());
            }
            _ => panic!("expected TemplateVisible"),
        }
    }

    #[test]
    fn verify_with_roi_override() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            template = "npc_trade"
            roi      = { x = 100, y = 200, w = 400, h = 300 }
        "#;
        let cb = parse(src).unwrap();
        let v = cb.steps[0].verify.as_ref().unwrap();
        match &v.check {
            VerifyCheck::TemplateVisible { roi: Some(r), .. } => {
                assert_eq!(r.x, 100);
                assert_eq!(r.y, 200);
                assert_eq!(r.w, 400);
                assert_eq!(r.h, 300);
            }
            _ => panic!("expected TemplateVisible with Some(roi)"),
        }
    }

    #[test]
    fn verify_condition_parses() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            condition = "has_item(mana_potion, 3)"
        "#;
        let cb = parse(src).unwrap();
        let v = cb.steps[0].verify.as_ref().unwrap();
        match &v.check {
            VerifyCheck::ConditionMet(Condition::HasItem { name, min_count }) => {
                assert_eq!(name, "mana_potion");
                assert_eq!(*min_count, 3);
            }
            _ => panic!("expected ConditionMet(HasItem)"),
        }
    }

    #[test]
    fn verify_inventory_delta_parses() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            inventory_delta = { item = "mana_potion", min_abs_delta = 50, require_positive = true }
        "#;
        let cb = parse(src).unwrap();
        let v = cb.steps[0].verify.as_ref().unwrap();
        match &v.check {
            VerifyCheck::InventoryDelta { item, min_abs_delta, require_positive } => {
                assert_eq!(item, "mana_potion");
                assert_eq!(*min_abs_delta, 50);
                assert!(*require_positive);
            }
            _ => panic!("expected InventoryDelta"),
        }
    }

    #[test]
    fn verify_multiple_checks_fails() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            template  = "npc_trade"
            condition = "has_item(mana_potion, 3)"
        "#;
        assert!(parse(src).is_err(), "verify con múltiples checks debe fallar");
    }

    #[test]
    fn verify_no_checks_fails() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            timeout_ms = 3000
        "#;
        assert!(parse(src).is_err(), "verify sin ningún check debe fallar");
    }

    #[test]
    fn verify_on_fail_advance() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            template = "npc_trade"
            on_fail  = "advance"
        "#;
        let cb = parse(src).unwrap();
        let v = cb.steps[0].verify.as_ref().unwrap();
        assert!(matches!(v.on_fail, VerifyFailAction::Advance));
    }

    #[test]
    fn verify_on_fail_goto_resolves_idx() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            template = "npc_trade"
            on_fail  = "goto:refill_entry"

            [[step]]
            kind = "label"
            name = "refill_entry"

            [[step]]
            kind = "hotkey"
            key  = "F1"
        "#;
        let cb = parse(src).unwrap();
        let v = cb.steps[0].verify.as_ref().unwrap();
        match &v.on_fail {
            VerifyFailAction::GotoLabel { target_label, target_idx } => {
                assert_eq!(target_label, "refill_entry");
                assert_eq!(*target_idx, 1, "label 'refill_entry' debe resolver a idx 1");
            }
            _ => panic!("expected GotoLabel"),
        }
    }

    #[test]
    fn verify_on_fail_goto_missing_label_errors() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            template = "npc_trade"
            on_fail  = "goto:nonexistent"
        "#;
        assert!(parse(src).is_err(), "verify goto a label inexistente debe fallar");
    }

    #[test]
    fn verify_on_fail_goto_empty_label_errors() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            template = "npc_trade"
            on_fail  = "goto:"
        "#;
        assert!(parse(src).is_err(), "verify on_fail 'goto:' sin label debe fallar");
    }

    #[test]
    fn verify_inventory_delta_zero_fails() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            inventory_delta = { item = "gp", min_abs_delta = 0 }
        "#;
        assert!(parse(src).is_err(), "inventory_delta con min_abs_delta=0 debe fallar");
    }

    #[test]
    fn verify_on_fail_invalid_errors() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            template = "npc_trade"
            on_fail  = "wat"
        "#;
        assert!(parse(src).is_err(), "on_fail con valor inválido debe fallar");
    }

    #[test]
    fn step_without_verify_has_none() {
        let src = r#"
            [[step]]
            kind        = "walk"
            key         = "D"
            duration_ms = 1000
        "#;
        let cb = parse(src).unwrap();
        assert!(cb.steps[0].verify.is_none());
    }

    #[test]
    fn verify_timeout_default() {
        let src = r#"
            [[step]]
            kind = "wait"
            duration_ms = 100

            [step.verify]
            template = "npc_trade"
        "#;
        let cb = parse(src).unwrap();
        let v = cb.steps[0].verify.as_ref().unwrap();
        assert_eq!(v.timeout_ms, 3000);
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

    // ── hunt_profile integration tests ────────────────────────────────────

    #[test]
    fn check_supplies_from_profile_loads_supplies_list() {
        // Usa el hunt profile real abdendriel_wasps.toml y verifica que
        // check_supplies con from_profile=true toma las thresholds del [supplies].
        let toml_src = r#"
[cavebot]
loop = true
hunt_profile = "abdendriel_wasps"

[[step]]
kind = "label"
name = "refill"

[[step]]
kind = "check_supplies"
on_fail = "refill"
from_profile = true
"#;
        let cb = parse_with_hunts_dir(toml_src)
            .expect("parse debería resolver hunt_profile correctamente");
        // El step 1 (idx=1) debe ser CheckSupplies con reqs del profile.
        match &cb.steps[1].kind {
            StepKind::CheckSupplies { requirements, .. } => {
                // abdendriel_wasps tiene 2 supplies checkables: mana_potion + health_potion.
                // (rope está documentada pero excluida porque no hay template de inventory.)
                assert_eq!(requirements.len(), 2, "expected 2 checkable supplies from profile");
                let has_mana = requirements.iter().any(|(n, v)| n == "mana_potion" && *v == 20);
                let has_health = requirements.iter().any(|(n, v)| n == "health_potion" && *v == 5);
                assert!(has_mana, "mana_potion con min=20 esperado");
                assert!(has_health, "health_potion con min=5 esperado");
            }
            other => panic!("esperado CheckSupplies, got {:?}", other),
        }
    }

    #[test]
    fn check_supplies_from_profile_without_hunt_profile_declared_fails() {
        // from_profile = true pero [cavebot].hunt_profile no declarado → error.
        let toml_src = r#"
[cavebot]
loop = true

[[step]]
kind = "label"
name = "refill"

[[step]]
kind = "check_supplies"
on_fail = "refill"
from_profile = true
"#;
        let err = parse(toml_src).expect_err("debería fallar sin hunt_profile");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("hunt_profile"),
            "error debería mencionar hunt_profile: {}", msg
        );
    }

    #[test]
    fn check_supplies_inline_and_from_profile_mutually_exclusive() {
        // Ambos set → ambiguous error.
        let toml_src = r#"
[cavebot]
loop = true
hunt_profile = "abdendriel_wasps"

[[step]]
kind = "label"
name = "refill"

[[step]]
kind = "check_supplies"
on_fail = "refill"
from_profile = true
requirements = [{ item = "mana_potion", min_count = 10 }]
"#;
        let err = parse_with_hunts_dir(toml_src)
            .expect_err("debería fallar con ambos presentes");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("mutuamente exclusivo"),
            "error debería mencionar mutual exclusion: {}", msg
        );
    }

    #[test]
    fn check_supplies_inline_legacy_still_works() {
        // Backwards compat: requirements = [...] sin hunt_profile.
        let toml_src = r#"
[cavebot]
loop = true

[[step]]
kind = "label"
name = "refill"

[[step]]
kind = "check_supplies"
on_fail = "refill"
requirements = [
    { item = "mana_potion", min_count = 3 },
    { item = "health_potion", min_count = 2 },
]
"#;
        let cb = parse(toml_src).expect("inline legacy debería seguir parseando");
        match &cb.steps[1].kind {
            StepKind::CheckSupplies { requirements, .. } => {
                assert_eq!(requirements.len(), 2);
                assert!(requirements.iter().any(|(n, _)| n == "mana_potion"));
            }
            _ => panic!("expected CheckSupplies"),
        }
    }

    #[test]
    fn check_supplies_without_reqs_or_profile_fails() {
        let toml_src = r#"
[cavebot]
loop = true

[[step]]
kind = "label"
name = "refill"

[[step]]
kind = "check_supplies"
on_fail = "refill"
"#;
        let err = parse(toml_src).expect_err("sin reqs ni from_profile debería fallar");
        let msg = format!("{:#}", err);
        assert!(msg.contains("requirements") || msg.contains("from_profile"));
    }

    #[test]
    fn hunt_profile_unknown_name_fails_with_helpful_error() {
        let toml_src = r#"
[cavebot]
loop = true
hunt_profile = "definitely_not_a_real_hunt_xyz"

[[step]]
kind = "label"
name = "refill"

[[step]]
kind = "wait"
duration_ms = 100
"#;
        let err = parse_with_hunts_dir(toml_src)
            .expect_err("profile inexistente debería fallar");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("definitely_not_a_real_hunt_xyz") || msg.contains("no se pudo cargar"),
            "error debería mencionar el nombre del profile: {}", msg
        );
    }
}
