//! hunt_profile.rs — Parser de `assets/hunts/<name>.toml`.
//!
//! Un hunt profile centraliza datos de un hunt específico (loot esperado,
//! supplies, mobs, métricas baseline). Los cavebot TOML referencian un profile
//! via `hunt_profile = "<name>"` en top-level, y los steps (CheckSupplies,
//! StowAllItems) pueden consultar estos datos sin repetir listas inline.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Profile completo de un hunt. Todos los sub-structs llevan `#[serde(default)]`
/// para que secciones ausentes se deserialicen como vacías/None (forward-compat
/// con futuros profiles que no rellenen todo).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HuntProfile {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub level_range: Option<[u32; 2]>,
    #[serde(default)]
    pub vocation: Option<String>,

    #[serde(default)]
    pub loot: LootConfig,
    #[serde(default)]
    pub supplies: HashMap<String, SupplyConfig>,
    #[serde(default)]
    pub monsters: MonsterConfig,
    #[serde(default)]
    pub metrics: MetricsBaseline,
    #[serde(default)]
    pub calibration_hints: CalibrationHints,
}

/// Configuración de loot del hunt. `stackables` es la whitelist de items que
/// el StowAllItems va a iterar; `non_stackables` se dejan en bag/depot; `drop`
/// son trash que el bot descarta si hace falta espacio.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LootConfig {
    #[serde(default)]
    pub stackables: Vec<String>,
    #[serde(default)]
    pub non_stackables: Vec<String>,
    #[serde(default)]
    pub drop: Vec<String>,
}

/// Umbrales de un supply individual. `min` dispara refill; `target` es la
/// cantidad a comprar al refillear.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SupplyConfig {
    pub min: u32,
    pub target: u32,
}

/// Listado de monstruos relevantes: `expected` (se esperan ver en battle list),
/// `avoid` (lure protection — si aparecen, retreat/pause), `priority` (mobs
/// raros a priorizar, placeholder).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MonsterConfig {
    #[serde(default)]
    pub expected: Vec<String>,
    #[serde(default)]
    pub avoid: Vec<String>,
    #[serde(default)]
    pub priority: Vec<String>,
}

/// Valores baseline para comparar métricas actuales vs esperadas. Todos
/// opcionales — profiles nuevos pueden omitirlos hasta tener datos reales.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsBaseline {
    #[serde(default)]
    pub expected_xp_per_hour: Option<u64>,
    #[serde(default)]
    pub expected_xp_min_per_hour: Option<u64>,
    #[serde(default)]
    pub expected_loot_gp_per_hour: Option<u64>,
    #[serde(default)]
    pub expected_kills_per_hour: Option<u64>,
    #[serde(default)]
    pub expected_deaths_per_session: Option<u32>,
    #[serde(default)]
    pub expected_cycle_min: Option<u32>,
}

/// Hints para calibrar scanner de inventory y stow logic al layout del
/// usuario. `backpack_count` viaja al inventory scanner; `stow_bags` le dice
/// al StowAllItems qué backpack contiene qué loot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalibrationHints {
    #[serde(default)]
    pub backpack_count: Option<u32>,
    #[serde(default)]
    pub stow_bags: Vec<StowBagHint>,
}

/// Un hint sobre un backpack específico: su índice en el sidebar, nombre
/// humano (solo para diagnóstico) y qué items esperamos stashear ahí.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StowBagHint {
    pub bag_index: u32,
    pub name: String,
    #[serde(default)]
    pub expected_loot: Vec<String>,
}

impl HuntProfile {
    /// Carga un profile desde un archivo TOML.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading hunt profile `{}`", path.display()))?;
        let profile: HuntProfile = toml::from_str(&raw)
            .with_context(|| format!("parsing hunt profile `{}`", path.display()))?;
        Ok(profile)
    }

    /// Carga un profile por nombre desde el directorio `hunts_dir`.
    /// Ejemplo: `HuntProfile::load_by_name(Path::new("assets/hunts"), "abdendriel_wasps")`.
    pub fn load_by_name(hunts_dir: &Path, name: &str) -> Result<Self> {
        let path = hunts_dir.join(format!("{name}.toml"));
        Self::load(&path)
    }

    /// ¿El item `name` está en la whitelist de stackables de este hunt?
    /// Usado por StowAllItems para saber si vale la pena iterar. Comparación
    /// case-insensitive para tolerar variaciones entre templates y TOML.
    ///
    /// **Extension point**: aún no consumido por el runner (StowAllItems
    /// usa directamente `stackables_whitelist: Option<Vec<String>>` populado
    /// desde `profile.loot.stackables` al parse). Reservado para futuras
    /// integraciones (ej: battle list validator, loot filter, reporting).
    #[allow(dead_code)]
    pub fn is_stackable_loot(&self, item_name: &str) -> bool {
        self.loot
            .stackables
            .iter()
            .any(|s| s.eq_ignore_ascii_case(item_name))
    }

    /// ¿El monstruo `name` está en la lista de avoid (lure protection)?
    /// Case-insensitive porque la battle list puede capitalizar distinto
    /// ("Stalker" vs "stalker") que el TOML del profile.
    ///
    /// **Extension point**: aún no consumido — la battle list actual no
    /// tiene OCR de nombres (`BattleEntry.name` es `Option<String>` pero
    /// siempre `None`). Cuando se agregue OCR, este helper se wireará para
    /// implementar lure protection (SafetyPause si entra un mob del avoid).
    #[allow(dead_code)]
    pub fn is_monster_to_avoid(&self, monster_name: &str) -> bool {
        self.monsters
            .avoid
            .iter()
            .any(|m| m.eq_ignore_ascii_case(monster_name))
    }

    /// Lista de supplies como `(item_name, SupplyConfig)` para CheckSupplies.
    /// Orden no determinístico (HashMap), pero CheckSupplies no depende del
    /// orden — evalúa cada entrada independientemente.
    pub fn supplies_list(&self) -> Vec<(String, SupplyConfig)> {
        self.supplies
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile_path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("assets/hunts/abdendriel_wasps.toml")
    }

    fn sample_hunts_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("assets/hunts")
    }

    #[test]
    fn loads_abdendriel_wasps_profile_ok() {
        let profile = HuntProfile::load(&sample_profile_path()).expect("load profile");
        assert_eq!(profile.name, "abdendriel_wasps");
        assert_eq!(profile.vocation.as_deref(), Some("druid"));
        assert!(
            profile.loot.stackables.len() >= 2,
            "expected at least 2 stackables, got {}",
            profile.loot.stackables.len()
        );
        assert!(
            profile
                .monsters
                .expected
                .iter()
                .any(|m| m == "Wasp"),
            "expected `Wasp` in monsters.expected, got {:?}",
            profile.monsters.expected
        );
        let mana = profile
            .supplies
            .get("mana_potion")
            .expect("mana_potion supply");
        assert_eq!(mana.min, 20);
    }

    #[test]
    fn is_stackable_loot_works() {
        let profile = HuntProfile::load(&sample_profile_path()).expect("load profile");
        assert!(profile.is_stackable_loot("honeycomb"));
        assert!(!profile.is_stackable_loot("magic_ring"));
    }

    #[test]
    fn is_monster_to_avoid_case_insensitive() {
        let profile = HuntProfile::load(&sample_profile_path()).expect("load profile");
        assert!(profile.is_monster_to_avoid("stalker"));
        assert!(profile.is_monster_to_avoid("Stalker"));
        assert!(profile.is_monster_to_avoid("STALKER"));
        assert!(!profile.is_monster_to_avoid("Rat"));
    }

    #[test]
    fn supplies_list_roundtrips() {
        let profile = HuntProfile::load(&sample_profile_path()).expect("load profile");
        let list = profile.supplies_list();
        // 2 items actualmente (mana_potion + health_potion). rope está
        // documentada en comentario pero no en [supplies] porque no tiene
        // template de inventory (uncheckable). Si se agrega rope.png, el
        // profile debería re-incluirla y este assert sube a 3.
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|(name, _)| name == "mana_potion"));
        assert!(list.iter().any(|(name, _)| name == "health_potion"));
    }

    #[test]
    fn load_by_name_ok() {
        let profile =
            HuntProfile::load_by_name(&sample_hunts_dir(), "abdendriel_wasps").expect("load_by_name");
        assert_eq!(profile.name, "abdendriel_wasps");
    }

    #[test]
    fn load_by_name_missing_errors() {
        let result = HuntProfile::load_by_name(&sample_hunts_dir(), "nonexistent_profile_xyz");
        assert!(result.is_err(), "expected error for missing profile");
    }

    #[test]
    fn metrics_baseline_optional_fields() {
        let synthetic = r#"
name = "minimal"
"#;
        let profile: HuntProfile = toml::from_str(synthetic).expect("parse minimal");
        assert_eq!(profile.name, "minimal");
        assert!(profile.metrics.expected_xp_per_hour.is_none());
        assert!(profile.metrics.expected_xp_min_per_hour.is_none());
        assert!(profile.metrics.expected_loot_gp_per_hour.is_none());
        assert!(profile.metrics.expected_kills_per_hour.is_none());
        assert!(profile.metrics.expected_deaths_per_session.is_none());
        assert!(profile.metrics.expected_cycle_min.is_none());
        assert!(profile.loot.stackables.is_empty());
        assert!(profile.supplies.is_empty());
        assert!(profile.monsters.expected.is_empty());
        assert!(profile.calibration_hints.backpack_count.is_none());
    }
}
