//! variation.rs — Selección ponderada entre acciones equivalentes.
//!
//! Cuando el bot tiene varias formas de lograr el mismo objetivo (heal con
//! spell o con potion, caminar con flecha o con numpad) elegir siempre la
//! misma opción es una signature. `WeightedChoice` selecciona aleatoriamente
//! con pesos configurables.
//!
//! ## Ejemplo
//!
//! ```ignore
//! // 70% heal_spell, 30% heal_potion.
//! let mut choice = WeightedChoice::new(vec![
//!     (0x3A, 70),  // F1 = heal spell
//!     (0x3B, 30),  // F2 = heal potion
//! ]);
//! let hid = choice.pick(); // aleatorio ponderado
//! ```

use rand::Rng;

/// Selector ponderado de `u8` (keycodes HID).
/// Los pesos pueden ser cualquier valor positivo — se normalizan internamente.
#[derive(Debug, Clone)]
pub struct WeightedChoice {
    /// `(hidcode, cum_weight)` — pesos acumulados para búsqueda O(n) simple.
    entries: Vec<(u8, u32)>,
    total:   u32,
}

impl WeightedChoice {
    /// `choices` = lista de (hidcode, weight). Peso 0 excluye la opción.
    pub fn new(choices: Vec<(u8, u32)>) -> Self {
        let mut entries = Vec::with_capacity(choices.len());
        let mut total = 0u32;
        for (hid, weight) in choices {
            if weight == 0 { continue; }
            total += weight;
            entries.push((hid, total));
        }
        Self { entries, total }
    }

    /// Elige un hidcode aleatorio. Retorna `None` si no hay opciones.
    pub fn pick(&self) -> Option<u8> {
        if self.total == 0 || self.entries.is_empty() {
            return None;
        }
        let mut rng = rand::thread_rng();
        let roll = rng.gen_range(0..self.total);
        for (hid, cum) in &self.entries {
            if roll < *cum {
                return Some(*hid);
            }
        }
        // Unreachable por construcción, pero retornar el último por seguridad.
        self.entries.last().map(|(h, _)| *h)
    }

    /// Retorna `true` si no hay opciones con peso > 0.
    #[allow(dead_code)] // extension point
    pub fn is_empty(&self) -> bool { self.total == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn empty_choice_returns_none() {
        let c = WeightedChoice::new(vec![]);
        assert!(c.is_empty());
        assert_eq!(c.pick(), None);
    }

    #[test]
    fn zero_weights_are_excluded() {
        let c = WeightedChoice::new(vec![(0x3A, 0), (0x3B, 0)]);
        assert!(c.is_empty());
        assert_eq!(c.pick(), None);
    }

    #[test]
    fn single_option_always_picked() {
        let c = WeightedChoice::new(vec![(0x3A, 1)]);
        for _ in 0..100 {
            assert_eq!(c.pick(), Some(0x3A));
        }
    }

    #[test]
    fn distribution_approximates_weights() {
        // 70/30 → con 10_000 muestras debería ser ~7000/3000.
        let c = WeightedChoice::new(vec![(0x3A, 70), (0x3B, 30)]);
        let mut counts: HashMap<u8, u32> = HashMap::new();
        for _ in 0..10_000 {
            let pick = c.pick().unwrap();
            *counts.entry(pick).or_insert(0) += 1;
        }
        let a = counts[&0x3A] as f64;
        let b = counts[&0x3B] as f64;
        // Tolerancia 5% (±500).
        assert!((a - 7000.0).abs() < 500.0, "a={} esperaba ~7000", a);
        assert!((b - 3000.0).abs() < 500.0, "b={} esperaba ~3000", b);
    }

    #[test]
    fn three_options_all_reachable() {
        let c = WeightedChoice::new(vec![(0x01, 33), (0x02, 33), (0x03, 34)]);
        let mut seen: HashMap<u8, u32> = HashMap::new();
        for _ in 0..1000 {
            *seen.entry(c.pick().unwrap()).or_insert(0) += 1;
        }
        assert_eq!(seen.len(), 3);
        for (_, count) in seen {
            assert!(count > 100, "alguna opción se eligió menos de 100 veces");
        }
    }
}
