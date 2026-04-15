/// spell_table.rs — Tabla de spells con prioridades y cooldowns individuales.
///
/// Reemplaza el sistema de 1-heal + 1-attack con N spells configurables.
/// Cada spell tiene condiciones (HP, mana, enemies) y cooldown propio.
/// El FSM llama `pick_heal()` o `pick_attack()` y recibe el mejor spell elegible.

use std::collections::HashMap;

use crate::config::{SpellConfig, SpellKind};
use crate::safety::timing::sample_gauss_ticks;

/// Entrada pre-parseada de un spell en la tabla.
#[derive(Debug, Clone)]
pub struct SpellEntry {
    pub hidcode:     u8,
    pub priority:    u32,
    pub min_hp:      f32,
    pub max_hp:      f32,
    pub min_mana:    f32,
    pub max_mana:    f32,
    pub min_enemies: u32,
    pub cooldown_ms: u64,
}

/// Contexto pasado al SpellTable para evaluar elegibilidad.
pub struct SpellContext {
    pub hp:       f32,
    pub mana:     f32,
    pub enemies:  u32,
    pub tick:     u64,
}

/// Tabla de spells con cooldowns individuales y jitter opcional.
pub struct SpellTable {
    heals:    Vec<SpellEntry>,
    attacks:  Vec<SpellEntry>,
    /// Cooldown por hidcode: tick mínimo para el próximo uso.
    cooldowns: HashMap<u8, u64>,
    /// Si Some, aplicar jitter gaussiano al cooldown (mean_factor, std_factor, fps).
    jitter:   Option<SpellJitter>,
}

struct SpellJitter {
    /// Factor sobre cooldown_ms para calcular stddev. Ej: 0.25 → std = cd * 0.25.
    std_factor: f64,
    fps:        u32,
}

impl SpellTable {
    /// Construye desde la lista de SpellConfig ya parseados.
    pub fn from_configs(configs: &[SpellConfig], fps: u32, jitter_std_factor: Option<f64>) -> Self {
        let mut heals = Vec::new();
        let mut attacks = Vec::new();

        for cfg in configs {
            let hidcode = match crate::act::keycode::parse(&cfg.key) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("SpellTable: key '{}' inválida: {} — ignorada", cfg.key, e);
                    continue;
                }
            };
            let entry = SpellEntry {
                hidcode,
                priority:    cfg.priority,
                min_hp:      cfg.min_hp,
                max_hp:      cfg.max_hp,
                min_mana:    cfg.min_mana,
                max_mana:    cfg.max_mana,
                min_enemies: cfg.min_enemies,
                cooldown_ms: cfg.cooldown_ms,
            };
            match cfg.kind {
                SpellKind::Heal    => heals.push(entry),
                SpellKind::Attack  => attacks.push(entry),
                SpellKind::Support => heals.push(entry), // support = heal priority
            }
        }

        heals.sort_by_key(|e| e.priority);
        attacks.sort_by_key(|e| e.priority);

        let jitter = jitter_std_factor.map(|f| SpellJitter { std_factor: f, fps });

        Self { heals, attacks, cooldowns: HashMap::new(), jitter }
    }

    /// Construye tabla legacy desde [actions] config (backward compat).
    pub fn from_legacy(hotkeys: &crate::config::Hotkeys, cd_heal_ms: u64, cd_attack_ms: u64) -> Self {
        let mut heals = vec![
            SpellEntry {
                hidcode: hotkeys.heal_spell, priority: 1,
                min_hp: 0.0, max_hp: 0.30, min_mana: 0.0, max_mana: 1.0,
                min_enemies: 0, cooldown_ms: cd_heal_ms,
            },
            SpellEntry {
                hidcode: hotkeys.mana_spell, priority: 2,
                min_hp: 0.0, max_hp: 1.0, min_mana: 0.0, max_mana: 0.20,
                min_enemies: 0, cooldown_ms: cd_heal_ms,
            },
        ];
        let attacks = vec![
            SpellEntry {
                hidcode: hotkeys.attack_default, priority: 1,
                min_hp: 0.0, max_hp: 1.0, min_mana: 0.0, max_mana: 1.0,
                min_enemies: 1, cooldown_ms: cd_attack_ms,
            },
        ];
        heals.sort_by_key(|e| e.priority);
        Self { heals, attacks, cooldowns: HashMap::new(), jitter: None }
    }

    /// Evalúa heals por prioridad. Retorna el primer spell elegible y con cooldown listo.
    pub fn pick_heal(&mut self, ctx: &SpellContext) -> Option<u8> {
        self.pick(&self.heals.clone(), ctx)
    }

    /// Evalúa attacks por prioridad.
    pub fn pick_attack(&mut self, ctx: &SpellContext) -> Option<u8> {
        self.pick(&self.attacks.clone(), ctx)
    }

    /// Retorna true si hay algún heal configurado.
    #[allow(dead_code)] // extension point: FSM can check before entering Emergency
    pub fn has_heals(&self) -> bool { !self.heals.is_empty() }

    /// Retorna true si hay algún attack configurado.
    #[allow(dead_code)] // extension point
    pub fn has_attacks(&self) -> bool { !self.attacks.is_empty() }

    fn pick(&mut self, entries: &[SpellEntry], ctx: &SpellContext) -> Option<u8> {
        for entry in entries {
            // Check conditions.
            if ctx.hp < entry.min_hp || ctx.hp > entry.max_hp {
                continue;
            }
            if ctx.mana < entry.min_mana || ctx.mana > entry.max_mana {
                continue;
            }
            if ctx.enemies < entry.min_enemies {
                continue;
            }
            // Check cooldown.
            if let Some(&next_tick) = self.cooldowns.get(&entry.hidcode) {
                if ctx.tick < next_tick {
                    continue;
                }
            }
            // Eligible — set cooldown and return.
            let cd_ticks = self.compute_cd_ticks(entry.cooldown_ms);
            self.cooldowns.insert(entry.hidcode, ctx.tick + cd_ticks);
            return Some(entry.hidcode);
        }
        None
    }

    fn compute_cd_ticks(&self, cooldown_ms: u64) -> u64 {
        match &self.jitter {
            Some(j) => {
                let mean = cooldown_ms as f64;
                let std = mean * j.std_factor;
                sample_gauss_ticks(mean, std, j.fps)
            }
            None => {
                // Fixed: ms → ticks at 30 fps (ceil).
                let fps = self.jitter.as_ref().map(|j| j.fps).unwrap_or(30);
                (cooldown_ms * fps as u64).div_ceil(1000)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(hp: f32, mana: f32, enemies: u32, tick: u64) -> SpellContext {
        SpellContext { hp, mana, enemies, tick }
    }

    #[test]
    fn pick_heal_by_priority() {
        let configs = vec![
            SpellConfig {
                key: "F1".into(), kind: SpellKind::Heal, priority: 1,
                min_hp: 0.0, max_hp: 0.30, min_mana: 0.0, max_mana: 1.0,
                min_enemies: 0, cooldown_ms: 300,
            },
            SpellConfig {
                key: "F2".into(), kind: SpellKind::Heal, priority: 2,
                min_hp: 0.0, max_hp: 0.60, min_mana: 0.0, max_mana: 1.0,
                min_enemies: 0, cooldown_ms: 300,
            },
        ];
        let mut table = SpellTable::from_configs(&configs, 30, None);

        // HP=25% → F1 (priority 1, max_hp=0.30 ✓)
        let f1 = crate::act::keycode::parse("F1").unwrap();
        let f2 = crate::act::keycode::parse("F2").unwrap();
        assert_eq!(table.pick_heal(&ctx(0.25, 1.0, 0, 0)), Some(f1));

        // HP=50% → F2 (priority 2, F1 max_hp=0.30 ✗)
        assert_eq!(table.pick_heal(&ctx(0.50, 1.0, 0, 100)), Some(f2));

        // HP=80% → None (both max_hp < 0.80)
        assert_eq!(table.pick_heal(&ctx(0.80, 1.0, 0, 200)), None);
    }

    #[test]
    fn cooldown_blocks_same_spell() {
        let configs = vec![
            SpellConfig {
                key: "F1".into(), kind: SpellKind::Heal, priority: 1,
                min_hp: 0.0, max_hp: 1.0, min_mana: 0.0, max_mana: 1.0,
                min_enemies: 0, cooldown_ms: 300, // 300ms = 9 ticks @ 30fps
            },
        ];
        let mut table = SpellTable::from_configs(&configs, 30, None);
        let f1 = crate::act::keycode::parse("F1").unwrap();

        // Tick 0: emit
        assert_eq!(table.pick_heal(&ctx(0.20, 1.0, 0, 0)), Some(f1));
        // Tick 5: still on cooldown (9 ticks)
        assert_eq!(table.pick_heal(&ctx(0.20, 1.0, 0, 5)), None);
        // Tick 9: cooldown expired
        assert_eq!(table.pick_heal(&ctx(0.20, 1.0, 0, 9)), Some(f1));
    }

    #[test]
    fn mana_condition_filters() {
        let configs = vec![
            SpellConfig {
                key: "F3".into(), kind: SpellKind::Heal, priority: 1,
                min_hp: 0.0, max_hp: 1.0, min_mana: 0.0, max_mana: 0.40,
                min_enemies: 0, cooldown_ms: 300,
            },
        ];
        let mut table = SpellTable::from_configs(&configs, 30, None);
        let f3 = crate::act::keycode::parse("F3").unwrap();

        // Mana=30% → ✓ (max_mana=0.40)
        assert_eq!(table.pick_heal(&ctx(0.50, 0.30, 0, 0)), Some(f3));
        // Mana=60% → ✗ (max_mana=0.40)
        assert_eq!(table.pick_heal(&ctx(0.50, 0.60, 0, 100)), None);
    }

    #[test]
    fn attack_requires_enemies() {
        let configs = vec![
            SpellConfig {
                key: "PageDown".into(), kind: SpellKind::Attack, priority: 1,
                min_hp: 0.0, max_hp: 1.0, min_mana: 0.0, max_mana: 1.0,
                min_enemies: 1, cooldown_ms: 1000,
            },
        ];
        let mut table = SpellTable::from_configs(&configs, 30, None);
        let pd = crate::act::keycode::parse("PageDown").unwrap();

        // 0 enemies → None
        assert_eq!(table.pick_attack(&ctx(1.0, 1.0, 0, 0)), None);
        // 1 enemy → emit
        assert_eq!(table.pick_attack(&ctx(1.0, 1.0, 1, 0)), Some(pd));
    }

    #[test]
    fn legacy_fallback_works() {
        let hotkeys = crate::config::Hotkeys {
            heal_spell: 0x3A, // F1
            heal_potion: 0x3B, // F2
            mana_spell: 0x3C, // F3
            attack_default: 0x4E, // PageDown
            loot_hotkey: None,
        };
        let mut table = SpellTable::from_legacy(&hotkeys, 333, 1000);

        // HP critical → F1 (heal_spell, priority 1, max_hp=0.30)
        assert_eq!(table.pick_heal(&ctx(0.20, 1.0, 0, 0)), Some(0x3A));
        // Mana critical → F3 (mana_spell, priority 2, max_mana=0.20)
        assert_eq!(table.pick_heal(&ctx(1.0, 0.15, 0, 100)), Some(0x3C));
        // Attack → PageDown
        assert_eq!(table.pick_attack(&ctx(1.0, 1.0, 1, 0)), Some(0x4E));
    }
}
