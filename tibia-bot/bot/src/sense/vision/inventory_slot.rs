//! inventory_slot.rs — `SlotReading` per-slot + confidence mapping.
//!
//! Paso #2 del plan inventory robustez 2026-04-22. Reemplaza el output
//! agregado (`HashMap<item, count>`) por una estructura per-slot con
//! confidence numérico, stage tag y stack count.
//!
//! `InventoryReading` (externa) sigue teniendo los HashMaps para
//! retro-compat con FSM/cavebot/HTTP consumers, pero ahora **además** carga
//! `slots: Vec<SlotReading>` para:
//! - Debugging (endpoint `/vision/inventory/slots`)
//! - Grabación JSONL en `PerceptionSnapshot` (análisis offline)
//! - Futuro: temporal filtering per-slot (item #4 del plan)
//!
//! ## Confidence scoring
//!
//! El codebase usa `MatchTemplateMethod::CrossCorrelationNormalized`
//! (CCORR_NORMED). Rango `[-1, 1]` en teoría, `[0, 1]` en práctica para
//! sprites. **Higher = better**. Threshold default `MATCH_THRESHOLD = 0.80`.
//!
//! Mapeo confidence:
//! - `score < threshold` → 0.0 (rechazado, no cuenta como match)
//! - `score = threshold` → 0.0 (borderline, treat as rechazado)
//! - `score = 1.0` (perfect) → 1.0
//! - Lineal en el rango `[threshold, 1.0]`.
//!
//! Permite al consumer distinguir un match "borderline 0.81" de un match
//! "casi perfecto 0.98" — útil para temporal voting weighted.

use serde::{Deserialize, Serialize};

/// Etapa del pipeline donde se originó la clasificación del slot.
/// Tag interno para debugging + métricas; no es parte de la decisión.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SlotStage {
    /// Placeholder antes de que cualquier stage haya corrido.
    #[default]
    Unknown      = 0,
    /// Stage A: detectado como empty por luma stddev.
    Empty        = 1,
    /// Stage B: match con template cacheado (última detección válida).
    /// Futuro — commit 8dd741d solo implementó Stage A; Stage B pendiente.
    CachedHit    = 2,
    /// Stage C: full sweep contra todos los templates SSE.
    FullSweep    = 3,
    /// Classifier ML (feature `ml-runtime`) resolvió la slot.
    MlClassified = 4,
}

impl SlotStage {
    pub fn label(self) -> &'static str {
        match self {
            SlotStage::Unknown      => "unknown",
            SlotStage::Empty        => "empty",
            SlotStage::CachedHit    => "cached_hit",
            SlotStage::FullSweep    => "full_sweep",
            SlotStage::MlClassified => "ml_classified",
        }
    }
}

/// Resultado per-slot del inventory reader.
///
/// **Clone + Serialize** para propagar al JSONL del recorder + HTTP JSON.
/// No Copy porque `item` es `Option<String>`.
///
/// Campos mutuamente exclusivos según `stage`:
/// - `Empty`: `item=None`, `confidence=1.0` (alta confianza en que está vacío),
///   `stack_count=None`, `raw_score=None`.
/// - `CachedHit` / `FullSweep` / `MlClassified`: `item=Some(name)`,
///   `confidence=[0..1]`, `stack_count` opcional (requiere OCR + digit templates),
///   `raw_score` = el score CCORR original (None si es ML).
/// - `Unknown`: estado transitorio, nunca emitido al final.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotReading {
    pub slot_idx:    u32,
    #[serde(default)]
    pub item:        Option<String>,
    pub confidence:  f32,
    #[serde(default)]
    pub stack_count: Option<u32>,
    #[serde(default)]
    pub stage:       SlotStage,
    #[serde(default)]
    pub raw_score:   Option<f32>,
}

impl SlotReading {
    /// Constructor para un slot clasificado como empty (Stage A).
    pub fn empty(slot_idx: u32) -> Self {
        Self {
            slot_idx,
            item: None,
            confidence: 1.0,
            stack_count: None,
            stage: SlotStage::Empty,
            raw_score: None,
        }
    }

    /// Constructor para un slot clasificado con template matching (SSE).
    pub fn matched(
        slot_idx: u32,
        item: String,
        raw_score: f32,
        threshold: f32,
        stack_count: Option<u32>,
        stage: SlotStage,
    ) -> Self {
        Self {
            slot_idx,
            item: Some(item),
            confidence: ccorr_to_confidence(raw_score, threshold),
            stack_count,
            stage,
            raw_score: Some(raw_score),
        }
    }

    /// Constructor para un slot clasificado por ML classifier.
    /// `ml_confidence` es la probabilidad softmax (ya en [0, 1]), usada
    /// directamente como confidence sin mapeo.
    pub fn ml_classified(
        slot_idx: u32,
        item: String,
        ml_confidence: f32,
        stack_count: Option<u32>,
    ) -> Self {
        Self {
            slot_idx,
            item: Some(item),
            confidence: ml_confidence.clamp(0.0, 1.0),
            stack_count,
            stage: SlotStage::MlClassified,
            raw_score: None,
        }
    }

    /// Constructor para un slot con contenido pero NINGÚN template matcheó.
    /// Stage A pasó (stddev > threshold) pero Stage C no encontró match.
    /// Típicamente: item nuevo sin template cargado O template de otro item
    /// similar que falló el threshold per-template.
    pub fn unmatched(slot_idx: u32) -> Self {
        Self {
            slot_idx,
            item: None,
            confidence: 0.0,
            stack_count: None,
            stage: SlotStage::FullSweep,
            raw_score: None,
        }
    }
}

/// Convierte un score CCORR_NORMED a confidence [0..1].
///
/// - `score < threshold` → 0.0 (below accept threshold, no match).
/// - `score = threshold` → 0.0 (borderline).
/// - `score = 1.0` → 1.0 (perfect match).
/// - Lineal en el rango `[threshold, 1.0]`.
///
/// Ejemplos con threshold=0.80:
/// - 0.80 → 0.0
/// - 0.85 → 0.25
/// - 0.90 → 0.50
/// - 0.95 → 0.75
/// - 1.00 → 1.00
///
/// Rationale del mapeo lineal (no sigmoid): permite comparación directa
/// entre slots de forma intuitiva. Un consumer que pida `confidence > 0.5`
/// equivale a `score > 0.90` con threshold default — razonable.
///
/// Threshold <= 0 retorna 0.0 (guard contra config inválida; no debería
/// ocurrir porque `load_thresholds` valida el rango).
pub fn ccorr_to_confidence(score: f32, threshold: f32) -> f32 {
    if !(0.0..=1.0).contains(&threshold) || threshold >= 1.0 {
        return 0.0;
    }
    if score < threshold {
        return 0.0;
    }
    let range = 1.0 - threshold;
    ((score - threshold) / range).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ccorr_to_confidence ─────────────────────────────────────────────

    #[test]
    fn confidence_zero_below_threshold() {
        assert_eq!(ccorr_to_confidence(0.50, 0.80), 0.0);
        assert_eq!(ccorr_to_confidence(0.79, 0.80), 0.0);
    }

    #[test]
    fn confidence_zero_at_threshold() {
        assert_eq!(ccorr_to_confidence(0.80, 0.80), 0.0);
    }

    #[test]
    fn confidence_one_at_perfect_match() {
        let c = ccorr_to_confidence(1.0, 0.80);
        assert!((c - 1.0).abs() < 0.001, "got {}", c);
    }

    #[test]
    fn confidence_linear_in_range() {
        // score = threshold + half of range → confidence = 0.5.
        let c = ccorr_to_confidence(0.90, 0.80); // 0.80 + (1-0.80)/2 = 0.90
        assert!((c - 0.50).abs() < 0.001, "got {}", c);
    }

    #[test]
    fn confidence_quarter_of_range() {
        // score = 0.85, threshold = 0.80 → (0.85 - 0.80) / 0.20 = 0.25
        let c = ccorr_to_confidence(0.85, 0.80);
        assert!((c - 0.25).abs() < 0.001, "got {}", c);
    }

    #[test]
    fn confidence_handles_high_threshold() {
        // threshold = 0.95 (dragon_ham override)
        let c = ccorr_to_confidence(0.975, 0.95); // (0.975 - 0.95) / 0.05 = 0.5
        assert!((c - 0.50).abs() < 0.01, "got {}", c);
    }

    #[test]
    fn confidence_invalid_threshold_returns_zero() {
        // threshold >= 1.0 es degenerate
        assert_eq!(ccorr_to_confidence(0.99, 1.0), 0.0);
        assert_eq!(ccorr_to_confidence(0.99, 1.5), 0.0);
        // threshold negativo
        assert_eq!(ccorr_to_confidence(0.5, -0.1), 0.0);
    }

    #[test]
    fn confidence_score_above_one_clamps() {
        // CCORR puede dar >1 en edge cases numéricos; debe clamp a 1.0
        let c = ccorr_to_confidence(1.05, 0.80);
        assert!((c - 1.0).abs() < 0.001, "got {}", c);
    }

    // ── SlotReading constructors ───────────────────────────────────────

    #[test]
    fn slot_empty_constructor() {
        let r = SlotReading::empty(3);
        assert_eq!(r.slot_idx, 3);
        assert!(r.item.is_none());
        assert_eq!(r.confidence, 1.0); // alta confianza en que está vacío
        assert!(r.stack_count.is_none());
        assert_eq!(r.stage, SlotStage::Empty);
        assert!(r.raw_score.is_none());
    }

    #[test]
    fn slot_matched_maps_score_to_confidence() {
        let r = SlotReading::matched(
            5,
            "mana_potion".into(),
            0.90,  // score
            0.80,  // threshold
            Some(47),
            SlotStage::FullSweep,
        );
        assert_eq!(r.slot_idx, 5);
        assert_eq!(r.item, Some("mana_potion".to_string()));
        assert!((r.confidence - 0.50).abs() < 0.001);
        assert_eq!(r.stack_count, Some(47));
        assert_eq!(r.stage, SlotStage::FullSweep);
        assert_eq!(r.raw_score, Some(0.90));
    }

    #[test]
    fn slot_ml_classified_uses_softmax_directly() {
        let r = SlotReading::ml_classified(
            7,
            "vial".into(),
            0.87,
            None,
        );
        assert_eq!(r.confidence, 0.87); // no mapeo, directo
        assert_eq!(r.stage, SlotStage::MlClassified);
        assert!(r.raw_score.is_none()); // ML no tiene raw CCORR
    }

    #[test]
    fn slot_ml_confidence_clamps() {
        // Softmax debería estar en [0,1] pero por seguridad clamp.
        let r = SlotReading::ml_classified(0, "x".into(), 1.5, None);
        assert_eq!(r.confidence, 1.0);
        let r = SlotReading::ml_classified(0, "x".into(), -0.1, None);
        assert_eq!(r.confidence, 0.0);
    }

    #[test]
    fn slot_unmatched_content_not_identified() {
        let r = SlotReading::unmatched(2);
        assert_eq!(r.slot_idx, 2);
        assert!(r.item.is_none());
        assert_eq!(r.confidence, 0.0);
        assert_eq!(r.stage, SlotStage::FullSweep);
    }

    // ── Serialization ───────────────────────────────────────────────────

    #[test]
    fn slot_reading_serializes_stable_json() {
        let r = SlotReading::matched(
            1, "mana_potion".into(), 0.92, 0.80, Some(30), SlotStage::FullSweep,
        );
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("\"slot_idx\":1"));
        assert!(json.contains("\"item\":\"mana_potion\""));
        assert!(json.contains("\"stage\":\"full_sweep\""));
        assert!(json.contains("\"stack_count\":30"));
        // Confidence: score 0.92 con threshold 0.80 = (0.92-0.80)/0.20 = 0.6
        assert!(json.contains("\"confidence\":0.6"));
    }

    #[test]
    fn slot_empty_serializes_compactly() {
        let r = SlotReading::empty(0);
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("\"stage\":\"empty\""));
        assert!(json.contains("\"item\":null"));
    }

    #[test]
    fn slot_reading_deserializes_with_defaults() {
        // JSONL viejo sin el campo `stage` debe parsear con default Unknown.
        let json = r#"{"slot_idx":3,"confidence":0.5}"#;
        let r: SlotReading = serde_json::from_str(json).expect("deserialize");
        assert_eq!(r.slot_idx, 3);
        assert_eq!(r.confidence, 0.5);
        assert!(r.item.is_none());
        assert_eq!(r.stage, SlotStage::Unknown);
        assert!(r.raw_score.is_none());
    }

    #[test]
    fn slot_stage_label_matches_variants() {
        assert_eq!(SlotStage::Unknown.label(),       "unknown");
        assert_eq!(SlotStage::Empty.label(),         "empty");
        assert_eq!(SlotStage::CachedHit.label(),     "cached_hit");
        assert_eq!(SlotStage::FullSweep.label(),     "full_sweep");
        assert_eq!(SlotStage::MlClassified.label(),  "ml_classified");
    }
}
