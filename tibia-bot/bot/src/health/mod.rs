//! health/mod.rs — HealthSystem: detección + degradación adaptativa runtime.
//!
//! Capa que consume `TickMetrics` + `MetricsRegistry` (instrumentación) y
//! emite un `HealthStatus` por tick con:
//! - Severidad agregada (Ok / Warning / Critical)
//! - Issues activos (frame stale, anchor drift, vision slow, etc.)
//! - Score numérico
//! - DegradationLevel sticky con histéresis (Light / Heavy / SafeMode)
//!
//! El HealthSystem **solo decide**. La aplicación de la degradación (gate
//! del FSM, skip de slow readers, safety pause) la hace BotLoop tras leer
//! el HealthStatus.
//!
//! ## Threading
//!
//! - HealthSystem vive en el game loop thread (single writer).
//! - Output publicado vía `Arc<ArcSwap<HealthStatus>>` — HTTP read lock-free.
//! - HealthGate clonable a FSM/dispatch (snapshot temporal por tick).
//!
//! ## Costo
//!
//! Estimado: ~1 µs por evaluate_tick (10 threshold checks + 2 stddev sobre
//! windows + 1 ArcSwap store). Validar con bench. <0.003% del budget 33 ms.

pub mod system;
pub mod gate;

pub use system::HealthSystem;
pub use gate::HealthGate;

use serde::{Deserialize, Serialize};

use crate::sense::vision::anchors::DriftStatus;

// ── Severity ──────────────────────────────────────────────────────────────

/// Niveles ordenables — permite max() y comparación numérica entre issues.
/// Pesos para el scoring agregado: Ok=0, Warning=1, Critical=4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default,
         Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Severity {
    #[default]
    Ok       = 0,
    Warning  = 1,
    Critical = 2,
}

impl Severity {
    pub fn weight(self) -> u32 {
        match self {
            Severity::Ok       => 0,
            Severity::Warning  => 1,
            Severity::Critical => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Ok       => "ok",
            Severity::Warning  => "warning",
            Severity::Critical => "critical",
        }
    }
}

// ── DegradationLevel ──────────────────────────────────────────────────────

/// Modos de degradación. Sticky con histéresis — no flapping entre ticks.
/// Ordenados por restricción ascendente: ningún → Light → Heavy → SafeMode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord,
         Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum DegradationLevel {
    /// Skip diagnóstico (region_monitor), bajar cadencia de SLOW readers.
    /// FSM + acciones funcionan normalmente.
    Light    = 1,
    /// Pausa acciones normales. Solo emergency heal/escape pasan.
    /// SLOW readers off completamente.
    Heavy    = 2,
    /// Pausa total: equivalente a is_paused=true con
    /// safety_pause_reason="health:safe_mode".
    SafeMode = 3,
}

impl DegradationLevel {
    pub fn label(self) -> &'static str {
        match self {
            DegradationLevel::Light    => "light",
            DegradationLevel::Heavy    => "heavy",
            DegradationLevel::SafeMode => "safe_mode",
        }
    }

    /// Multiplicador de cadencia para SLOW readers. Light dobla el periodo
    /// (15→30 ticks), Heavy lo cuadruplica, SafeMode skipea siempre.
    pub fn slow_reader_multiplier(self) -> u32 {
        match self {
            DegradationLevel::Light    => 2,
            DegradationLevel::Heavy    => 4,
            DegradationLevel::SafeMode => u32::MAX,
        }
    }
}

// ── HealthIssue ───────────────────────────────────────────────────────────

/// Issues discretos que el HealthSystem puede emitir. Cada variant carga el
/// contexto numérico para que el HTTP endpoint y los logs muestren detalle,
/// no solo "WARNING".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HealthIssue {
    /// Frame age (now - captured_at) excede threshold.
    FrameStale {
        age_ms:       u32,
        threshold_ms: u32,
        severity:     Severity,
    },
    /// Tick processing time excedió budget.
    TickOverrun {
        tick_ms:      u32,
        budget_ms:    u32,
        severity:     Severity,
    },
    /// Vision::tick total cost demasiado alto (subset de TickOverrun
    /// que distingue causa específica).
    VisionSlow {
        vision_ms:    u32,
        threshold_ms: u32,
        severity:     Severity,
    },
    /// Anchor cluster diverging o all anchors lost.
    AnchorDrift {
        status:       DriftStatus,
        valid:        u8,
        total:        u8,
        severity:     Severity,
    },
    /// Detection confidence para vital field bajo threshold.
    LowDetectionConfidence {
        field:        String,         // "hp", "mana", "target"
        confidence:   f32,            // 0.0..1.0
        threshold:    f32,
        severity:     Severity,
    },
    /// Bridge RTT alto (round-trip > umbral).
    BridgeRttHigh {
        rtt_ms:       u32,
        threshold_ms: u32,
        severity:     Severity,
    },
    /// Tasa de éxito de acciones por action_kind bajo threshold.
    /// `action_kind` (no `kind`) para no colisionar con `#[serde(tag = "kind")]`.
    ActionFailureRate {
        action_kind:  String,
        success_rate: f32,
        threshold:    f32,
        severity:     Severity,
    },
    /// Tick-time stddev (jitter) elevado en rolling window.
    HighJitter {
        stddev_ms:    f32,
        threshold_ms: f32,
        severity:     Severity,
    },
    /// NDI frame seq gap detectado (frame loss).
    FrameSeqGap {
        gaps_total:   u64,
        severity:     Severity,
    },
    /// Issue sintético derivado de combinación de issues primarios.
    /// `name` discrimina el tipo de combinación (blind_mode,
    /// compute_saturation, io_unreliable). Renombrado de `kind` para no
    /// colisionar con el tag enum `#[serde(tag = "kind")]`.
    Composite {
        name:         String,
        causes:       Vec<String>,    // labels de los issues que lo triggearon
        severity:     Severity,
    },
}

impl HealthIssue {
    pub fn severity(&self) -> Severity {
        match self {
            HealthIssue::FrameStale       { severity, .. } => *severity,
            HealthIssue::TickOverrun      { severity, .. } => *severity,
            HealthIssue::VisionSlow       { severity, .. } => *severity,
            HealthIssue::AnchorDrift      { severity, .. } => *severity,
            HealthIssue::LowDetectionConfidence { severity, .. } => *severity,
            HealthIssue::BridgeRttHigh    { severity, .. } => *severity,
            HealthIssue::ActionFailureRate { severity, .. } => *severity,
            HealthIssue::HighJitter       { severity, .. } => *severity,
            HealthIssue::FrameSeqGap      { severity, .. } => *severity,
            HealthIssue::Composite        { severity, .. } => *severity,
        }
    }

    /// Tag corto identificando el tipo (para Prometheus labels, Composite causes).
    pub fn label(&self) -> &str {
        match self {
            HealthIssue::FrameStale       { .. } => "frame_stale",
            HealthIssue::TickOverrun      { .. } => "tick_overrun",
            HealthIssue::VisionSlow       { .. } => "vision_slow",
            HealthIssue::AnchorDrift      { .. } => "anchor_drift",
            HealthIssue::LowDetectionConfidence { .. } => "low_detection_confidence",
            HealthIssue::BridgeRttHigh    { .. } => "bridge_rtt_high",
            HealthIssue::ActionFailureRate { .. } => "action_failure_rate",
            HealthIssue::HighJitter       { .. } => "high_jitter",
            HealthIssue::FrameSeqGap      { .. } => "frame_seq_gap",
            HealthIssue::Composite        { name, .. } => name.as_str(),
        }
    }
}

// ── HealthStatus ──────────────────────────────────────────────────────────

/// Snapshot publicado cada tick. ArcSwap'd en el HealthSystem; HTTP read
/// lock-free vía `output_handle().load_full()`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HealthStatus {
    pub overall:         Severity,
    pub score:           u32,
    pub degraded:        Option<DegradationLevel>,
    pub issues:          Vec<HealthIssue>,
    pub summary:         String,
    pub tick:             u64,
    pub frame_seq:       u64,
    pub generated_at_ms: u64,
}

// ── HealthConfig ──────────────────────────────────────────────────────────

/// Thresholds + scoring + histéresis. Defaults razonables pero
/// **sin validación empírica live**. Tunear con data real.
#[derive(Debug, Clone)]
pub struct HealthConfig {
    // Per-issue thresholds (ms unless noted).
    pub frame_age_warning_ms:      u32,
    pub frame_age_critical_ms:     u32,
    pub tick_warning_ms:           u32,
    pub tick_critical_ms:          u32,
    pub vision_warning_ms:         u32,
    pub vision_critical_ms:        u32,
    pub jitter_warning_ms:         f32,
    pub jitter_critical_ms:        f32,
    pub bridge_rtt_warning_ms:     u32,
    pub bridge_rtt_critical_ms:    u32,
    pub vitals_conf_warning:       f32,
    pub vitals_conf_critical:      f32,
    pub action_rate_warning:       f32,
    pub action_rate_critical:      f32,
    pub frame_seq_gap_warning:     u64,
    pub frame_seq_gap_critical:    u64,

    // Scoring agregado: cuánto score exige cada DegradationLevel.
    pub light_score_threshold:     u32,
    pub heavy_score_threshold:     u32,
    pub safe_score_threshold:      u32,

    // Histéresis (ticks @ 30 Hz).
    pub promote_streak:            u32,
    pub recovery_streak:           u32,

    // Anchor drift streak gate (compartido con loop_.rs ANCHOR_DRIFT_PAUSE_TICKS).
    /// Anchor inconsistente solo cuenta como issue si el streak supera esto.
    /// Filtra transients del bg matching thread.
    pub anchor_drift_streak_min:   u32,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            // Frame age — NDI transit + game loop pickup.
            frame_age_warning_ms:    100,
            frame_age_critical_ms:   200,
            // Tick budget = 33 ms @ 30 Hz.
            tick_warning_ms:         33,
            tick_critical_ms:        50,
            // Vision: deja headroom para FSM + dispatch.
            vision_warning_ms:       20,
            vision_critical_ms:      28,
            // Jitter sobre rolling 1s.
            jitter_warning_ms:       3.0,
            jitter_critical_ms:      7.0,
            // Bridge: serial RTT ~5 ms en hardware, TCP +1 ms.
            bridge_rtt_warning_ms:   30,
            bridge_rtt_critical_ms:  80,
            // Vitals confidence: 0.5 = bad reads frecuentes, <0.2 = invisible.
            vitals_conf_warning:     0.50,
            vitals_conf_critical:    0.20,
            // Action success: 80% es preocupante, <50% indica bridge roto.
            action_rate_warning:     0.80,
            action_rate_critical:    0.50,
            // Frame seq gaps acumulados (totales desde boot).
            frame_seq_gap_warning:   3,
            frame_seq_gap_critical:  10,
            // Scoring: combinación de issues escala el target degradation.
            // Score 1 (1 warning) → Light. Score 4 (1 critical OR 4 warnings) → Heavy.
            // Score 8 (2 criticals) → SafeMode.
            light_score_threshold:   1,
            heavy_score_threshold:   4,
            safe_score_threshold:    8,
            // Hysteresis: 5 ticks (~165 ms) para promover, 60 (~2 s) para bajar.
            promote_streak:          5,
            recovery_streak:         60,
            // Anchor drift: requiere 5 ticks consecutivos antes de issue.
            anchor_drift_streak_min: 5,
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ordering_max_works() {
        assert!(Severity::Critical > Severity::Warning);
        assert!(Severity::Warning > Severity::Ok);
        assert_eq!(
            *[Severity::Ok, Severity::Warning, Severity::Critical].iter().max().unwrap(),
            Severity::Critical
        );
    }

    #[test]
    fn severity_weights_match_design() {
        assert_eq!(Severity::Ok.weight(), 0);
        assert_eq!(Severity::Warning.weight(), 1);
        assert_eq!(Severity::Critical.weight(), 4);
    }

    #[test]
    fn severity_serializes_lowercase() {
        let json = serde_json::to_string(&Severity::Critical).unwrap();
        assert_eq!(json, "\"critical\"");
        let parsed: Severity = serde_json::from_str("\"warning\"").unwrap();
        assert_eq!(parsed, Severity::Warning);
    }

    #[test]
    fn degradation_level_ordering() {
        assert!(DegradationLevel::SafeMode > DegradationLevel::Heavy);
        assert!(DegradationLevel::Heavy > DegradationLevel::Light);
    }

    #[test]
    fn degradation_slow_reader_multiplier() {
        assert_eq!(DegradationLevel::Light.slow_reader_multiplier(), 2);
        assert_eq!(DegradationLevel::Heavy.slow_reader_multiplier(), 4);
        assert_eq!(DegradationLevel::SafeMode.slow_reader_multiplier(), u32::MAX);
    }

    #[test]
    fn issue_severity_passthrough() {
        let i = HealthIssue::FrameStale {
            age_ms: 250, threshold_ms: 200, severity: Severity::Critical,
        };
        assert_eq!(i.severity(), Severity::Critical);
    }

    #[test]
    fn issue_label_distinguishes_kinds() {
        let frame = HealthIssue::FrameStale { age_ms: 0, threshold_ms: 0, severity: Severity::Ok };
        let vision = HealthIssue::VisionSlow { vision_ms: 0, threshold_ms: 0, severity: Severity::Ok };
        assert_eq!(frame.label(), "frame_stale");
        assert_eq!(vision.label(), "vision_slow");
    }

    #[test]
    fn composite_issue_uses_name_as_label() {
        let c = HealthIssue::Composite {
            name: "compute_saturation".to_string(),
            causes: vec!["vision_slow".into(), "high_jitter".into()],
            severity: Severity::Critical,
        };
        assert_eq!(c.label(), "compute_saturation");
    }

    #[test]
    fn issue_serializes_with_kind_tag() {
        let i = HealthIssue::TickOverrun {
            tick_ms: 50, budget_ms: 33, severity: Severity::Warning,
        };
        let json = serde_json::to_string(&i).unwrap();
        assert!(json.contains("\"kind\":\"tick_overrun\""));
        assert!(json.contains("\"tick_ms\":50"));
        assert!(json.contains("\"severity\":\"warning\""));
    }

    #[test]
    fn health_status_default_is_ok_no_issues() {
        let s = HealthStatus::default();
        assert_eq!(s.overall, Severity::Ok);
        assert_eq!(s.score, 0);
        assert!(s.degraded.is_none());
        assert!(s.issues.is_empty());
    }

    #[test]
    fn config_default_thresholds_are_sensible() {
        let c = HealthConfig::default();
        // Sanity: warning < critical para todos los thresholds que aplica.
        assert!(c.frame_age_warning_ms < c.frame_age_critical_ms);
        assert!(c.tick_warning_ms < c.tick_critical_ms);
        assert!(c.vision_warning_ms < c.vision_critical_ms);
        assert!(c.jitter_warning_ms < c.jitter_critical_ms);
        assert!(c.bridge_rtt_warning_ms < c.bridge_rtt_critical_ms);
        // Vitals/action: warning > critical (ratio mayor = mejor).
        assert!(c.vitals_conf_warning > c.vitals_conf_critical);
        assert!(c.action_rate_warning > c.action_rate_critical);
        // Scoring: light < heavy < safe.
        assert!(c.light_score_threshold < c.heavy_score_threshold);
        assert!(c.heavy_score_threshold < c.safe_score_threshold);
        // Hysteresis: recovery >> promote (no flapping).
        assert!(c.recovery_streak > c.promote_streak);
    }
}
