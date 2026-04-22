//! health/system.rs — HealthSystem core (evaluate + hysteresis + publish).
//!
//! Diseño:
//! - Single writer: el game loop llama `evaluate_tick(metrics, registry)`
//!   al final de cada tick.
//! - Output via `Arc<ArcSwap<HealthStatus>>` — HTTP + FSM read lock-free.
//! - Histéresis sticky: promote requiere N ticks consecutivos, recovery N ticks.
//!
//! Costo medido empírico (bench `health_evaluate_tick_steady_state_*`):
//! - Estado Ok (sin issues): **655 ns/call**
//! - Estado degraded (multiple critical + composite): **1.10 µs/call**
//! 0.002-0.003% del budget 33 ms/tick. Negligible.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use arc_swap::ArcSwap;

use crate::instrumentation::{
    ActionKindTag, MetricsRegistry, TickFlags, TickMetrics,
};
use crate::sense::vision::anchors::DriftStatus;

use super::{
    DegradationLevel, HealthConfig, HealthIssue, HealthStatus, Severity,
};

pub struct HealthSystem {
    config:               HealthConfig,
    output:               Arc<ArcSwap<HealthStatus>>,

    // Hysteresis state — solo accedido desde game loop.
    current_degradation:  Option<DegradationLevel>,
    /// Nivel hacia el que estamos acumulando racha de promoción.
    promote_target:       Option<DegradationLevel>,
    promote_streak:       u32,
    recovery_streak:      u32,

    /// Counter externo: anchor_drift_streak vive en BotLoop. Lo recibimos
    /// como input cada tick. Lo guardamos para serializar en HealthIssue.
    last_anchor_drift_streak: u32,
}

/// Inputs adicionales que NO viven en TickMetrics y deben pasar explícitos.
/// Mantiene `evaluate_tick` con firma compacta sin perder señales.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtraInputs {
    /// Streak consecutivo de DriftStatus != Ok. Vive en BotLoop.
    pub anchor_drift_streak:      u32,
    /// RTT actual del bridge (último ack medido). None si nunca medido.
    pub bridge_rtt_ms:            Option<u32>,
    /// Millis desde el último PONG exitoso del bridge. `u32::MAX` si nunca.
    /// Usado para emitir `HealthIssue::BridgeUnreachable`.
    pub bridge_last_pong_ms_ago:  u32,
}

impl HealthSystem {
    pub fn new(config: HealthConfig) -> Self {
        Self {
            config,
            output: Arc::new(ArcSwap::from_pointee(HealthStatus::default())),
            current_degradation: None,
            promote_target:      None,
            promote_streak:      0,
            recovery_streak:     0,
            last_anchor_drift_streak: 0,
        }
    }

    pub fn output_handle(&self) -> Arc<ArcSwap<HealthStatus>> {
        Arc::clone(&self.output)
    }

    pub fn last_status(&self) -> Arc<HealthStatus> {
        self.output.load_full()
    }

    /// Evalúa el tick actual, actualiza histéresis, publica HealthStatus.
    /// Llamado por BotLoop tras `metrics.record_tick(tick_metrics)`.
    pub fn evaluate_tick(
        &mut self,
        m: &TickMetrics,
        registry: &MetricsRegistry,
        extras: ExtraInputs,
    ) -> Arc<HealthStatus> {
        self.last_anchor_drift_streak = extras.anchor_drift_streak;

        // 1. Detectar issues primarios (per-threshold).
        let mut issues = self.detect_primary_issues(m, registry, extras);

        // 2. Aplicar reglas cross-signal → potencialmente añadir Composites.
        self.apply_combination_rules(&mut issues);

        // 3. Calcular score + overall.
        let score   = issues.iter().map(|i| i.severity().weight()).sum::<u32>();
        let overall = issues.iter().map(|i| i.severity()).max().unwrap_or(Severity::Ok);

        // 4. Determinar target degradation desde score.
        let target = self.target_degradation(score);

        // 5. Aplicar hysteresis → degradation efectivo.
        let degradation = self.apply_hysteresis(target);

        // Log de transiciones (solo cuando cambia).
        if degradation != self.current_degradation {
            tracing::warn!(
                "health: degradation {:?} → {:?} (score={} issues={})",
                self.current_degradation, degradation, score,
                issues.iter().map(|i| i.label()).collect::<Vec<_>>().join(",")
            );
            self.current_degradation = degradation;
        }

        // 6. Build + publish.
        let summary = build_summary(overall, score, &issues, degradation);
        let status = HealthStatus {
            overall, score, degraded: degradation,
            issues, summary,
            tick: m.tick,
            frame_seq: m.frame_seq,
            generated_at_ms: now_unix_ms(),
        };
        let arc = Arc::new(status);
        self.output.store(Arc::clone(&arc));
        arc
    }

    // ── Detección per-issue ────────────────────────────────────────────

    fn detect_primary_issues(
        &self,
        m: &TickMetrics,
        registry: &MetricsRegistry,
        extras: ExtraInputs,
    ) -> Vec<HealthIssue> {
        let mut out = Vec::with_capacity(8);
        let cfg = &self.config;

        // Frame stale.
        let frame_age_ms = m.frame_age_us / 1000;
        if frame_age_ms >= cfg.frame_age_critical_ms {
            out.push(HealthIssue::FrameStale {
                age_ms: frame_age_ms, threshold_ms: cfg.frame_age_critical_ms,
                severity: Severity::Critical,
            });
        } else if frame_age_ms >= cfg.frame_age_warning_ms {
            out.push(HealthIssue::FrameStale {
                age_ms: frame_age_ms, threshold_ms: cfg.frame_age_warning_ms,
                severity: Severity::Warning,
            });
        }

        // Tick overrun.
        let tick_ms = m.tick_total_us / 1000;
        if tick_ms >= cfg.tick_critical_ms {
            out.push(HealthIssue::TickOverrun {
                tick_ms, budget_ms: cfg.tick_critical_ms,
                severity: Severity::Critical,
            });
        } else if tick_ms >= cfg.tick_warning_ms {
            out.push(HealthIssue::TickOverrun {
                tick_ms, budget_ms: cfg.tick_warning_ms,
                severity: Severity::Warning,
            });
        }

        // Vision slow (subset de TickOverrun, distingue causa).
        let vision_ms = m.vision_total_us / 1000;
        if vision_ms >= cfg.vision_critical_ms {
            out.push(HealthIssue::VisionSlow {
                vision_ms, threshold_ms: cfg.vision_critical_ms,
                severity: Severity::Critical,
            });
        } else if vision_ms >= cfg.vision_warning_ms {
            out.push(HealthIssue::VisionSlow {
                vision_ms, threshold_ms: cfg.vision_warning_ms,
                severity: Severity::Warning,
            });
        }

        // Anchor drift (con gate de streak para evitar transients).
        if extras.anchor_drift_streak >= cfg.anchor_drift_streak_min {
            let status = drift_status_from_flags(m.flags);
            let severity = match status {
                DriftStatus::AllLost      => Some(Severity::Critical),
                DriftStatus::Inconsistent => Some(Severity::Warning),
                DriftStatus::Ok           => None,
            };
            if let Some(severity) = severity {
                out.push(HealthIssue::AnchorDrift {
                    status,
                    valid: m.valid_anchors,
                    total: m.total_anchors,
                    severity,
                });
            }
        }

        // Vitals confidence.
        let vit = m.vitals_confidence_bp as f32 / 10_000.0;
        if vit < cfg.vitals_conf_critical {
            out.push(HealthIssue::LowDetectionConfidence {
                field: "vitals".into(),
                confidence: vit,
                threshold: cfg.vitals_conf_critical,
                severity: Severity::Critical,
            });
        } else if vit < cfg.vitals_conf_warning {
            out.push(HealthIssue::LowDetectionConfidence {
                field: "vitals".into(),
                confidence: vit,
                threshold: cfg.vitals_conf_warning,
                severity: Severity::Warning,
            });
        }

        // Bridge RTT high (solo si tenemos RTT medido).
        if let Some(rtt) = extras.bridge_rtt_ms {
            if rtt >= cfg.bridge_rtt_critical_ms {
                out.push(HealthIssue::BridgeRttHigh {
                    rtt_ms: rtt, threshold_ms: cfg.bridge_rtt_critical_ms,
                    severity: Severity::Critical,
                });
            } else if rtt >= cfg.bridge_rtt_warning_ms {
                out.push(HealthIssue::BridgeRttHigh {
                    rtt_ms: rtt, threshold_ms: cfg.bridge_rtt_warning_ms,
                    severity: Severity::Warning,
                });
            }
        }

        // Bridge unreachable: last_pong demasiado viejo. u32::MAX antes del
        // primer ping (cold boot) — NO emitir issue durante boot inicial;
        // el periodic ping task se asegura de que a los 2s tengamos data.
        // Si last_pong > critical (5s default) con bot ya corriendo (>10s),
        // el bridge está roto de verdad.
        if extras.bridge_last_pong_ms_ago != u32::MAX
           && m.tick > 300  // ~10 s @ 30 Hz — evita warns en cold boot
        {
            let ago = extras.bridge_last_pong_ms_ago;
            if ago >= cfg.bridge_pong_critical_ms {
                out.push(HealthIssue::BridgeUnreachable {
                    last_pong_ms_ago: ago, severity: Severity::Critical,
                });
            } else if ago >= cfg.bridge_pong_warning_ms {
                out.push(HealthIssue::BridgeUnreachable {
                    last_pong_ms_ago: ago, severity: Severity::Warning,
                });
            }
        }

        // Action failure rate (cumulativo lifetime — proxy hasta que tengamos
        // ventana sliding). Solo evalúa si hubo emisiones del kind.
        if let Some(rate) = registry.action_success_rate(ActionKindTag::Heal) {
            let issue = self.action_failure_issue("heal", rate);
            if let Some(i) = issue { out.push(i); }
        }

        // Jitter.
        let win = registry.windows_snapshot();
        let jitter_ms = win.jitter_us as f32 / 1000.0;
        if jitter_ms >= cfg.jitter_critical_ms {
            out.push(HealthIssue::HighJitter {
                stddev_ms: jitter_ms, threshold_ms: cfg.jitter_critical_ms,
                severity: Severity::Critical,
            });
        } else if jitter_ms >= cfg.jitter_warning_ms {
            out.push(HealthIssue::HighJitter {
                stddev_ms: jitter_ms, threshold_ms: cfg.jitter_warning_ms,
                severity: Severity::Warning,
            });
        }

        // Frame seq gaps.
        let gaps = registry.frame_seq_gaps.load(std::sync::atomic::Ordering::Relaxed);
        if gaps >= cfg.frame_seq_gap_critical {
            out.push(HealthIssue::FrameSeqGap {
                gaps_total: gaps, severity: Severity::Critical,
            });
        } else if gaps >= cfg.frame_seq_gap_warning {
            out.push(HealthIssue::FrameSeqGap {
                gaps_total: gaps, severity: Severity::Warning,
            });
        }

        out
    }

    fn action_failure_issue(&self, kind: &str, rate: f32) -> Option<HealthIssue> {
        let cfg = &self.config;
        if rate < cfg.action_rate_critical {
            Some(HealthIssue::ActionFailureRate {
                action_kind: kind.into(), success_rate: rate,
                threshold: cfg.action_rate_critical, severity: Severity::Critical,
            })
        } else if rate < cfg.action_rate_warning {
            Some(HealthIssue::ActionFailureRate {
                action_kind: kind.into(), success_rate: rate,
                threshold: cfg.action_rate_warning, severity: Severity::Warning,
            })
        } else {
            None
        }
    }

    // ── Reglas cross-signal ────────────────────────────────────────────

    fn apply_combination_rules(&self, issues: &mut Vec<HealthIssue>) {
        let has_critical_anchor = issues.iter().any(|i| matches!(
            i, HealthIssue::AnchorDrift { severity: Severity::Critical, .. }
        ));
        let has_frame_stale = issues.iter().any(|i| matches!(i, HealthIssue::FrameStale { .. }));
        let has_warn_vision_slow = issues.iter().any(|i| matches!(i, HealthIssue::VisionSlow { .. }));
        let has_warn_jitter = issues.iter().any(|i| matches!(i, HealthIssue::HighJitter { .. }));
        let has_warn_bridge = issues.iter().any(|i| matches!(i, HealthIssue::BridgeRttHigh { .. }));
        let has_warn_action = issues.iter().any(|i| matches!(i, HealthIssue::ActionFailureRate { .. }));

        // 1. blind_mode: ningún anchor + frame viejo → no podemos confiar en NADA visual.
        if has_critical_anchor && has_frame_stale {
            issues.push(HealthIssue::Composite {
                name: "blind_mode".into(),
                causes: vec!["anchor_drift".into(), "frame_stale".into()],
                severity: Severity::Critical,
            });
        }

        // 2. compute_saturation: vision lento + jitter alto → CPU saturado.
        if has_warn_vision_slow && has_warn_jitter {
            issues.push(HealthIssue::Composite {
                name: "compute_saturation".into(),
                causes: vec!["vision_slow".into(), "high_jitter".into()],
                severity: Severity::Critical,
            });
        }

        // 3. io_unreliable: bridge RTT + actions fallando → red rota.
        if has_warn_bridge && has_warn_action {
            issues.push(HealthIssue::Composite {
                name: "io_unreliable".into(),
                causes: vec!["bridge_rtt_high".into(), "action_failure_rate".into()],
                severity: Severity::Critical,
            });
        }
    }

    // ── Scoring → target degradation ───────────────────────────────────

    fn target_degradation(&self, score: u32) -> Option<DegradationLevel> {
        let c = &self.config;
        if score >= c.safe_score_threshold      { Some(DegradationLevel::SafeMode) }
        else if score >= c.heavy_score_threshold { Some(DegradationLevel::Heavy) }
        else if score >= c.light_score_threshold { Some(DegradationLevel::Light) }
        else                                     { None }
    }

    // ── Histéresis (anti-flapping) ─────────────────────────────────────
    //
    // Contrato:
    // - target == current → reset ambos streaks.
    // - target > current → promote_streak++; al llenarse, sube UN nivel.
    // - target < current → recovery_streak++; al llenarse, baja UN nivel.
    //
    // Solo se cambia un nivel por iteración (Light → Heavy → SafeMode, no
    // jump directo) — da feedback visible al operador antes de bloqueo total.

    fn apply_hysteresis(&mut self, target: Option<DegradationLevel>) -> Option<DegradationLevel> {
        match (self.current_degradation, target) {
            // Estable.
            (curr, t) if curr == t => {
                self.promote_streak = 0;
                self.recovery_streak = 0;
                self.promote_target = None;
                curr
            }
            // Promotion (incluye None → Some).
            (curr, t) if cmp_level(t) > cmp_level(curr) => {
                if self.promote_target != t {
                    self.promote_target = t;
                    self.promote_streak = 0;
                }
                self.promote_streak += 1;
                self.recovery_streak = 0;
                if self.promote_streak >= self.config.promote_streak {
                    let next = step_up(curr);
                    self.promote_streak = 0;
                    self.promote_target = None;
                    next
                } else {
                    curr
                }
            }
            // Recovery (incluye Some → None).
            (curr, _t) => {
                self.recovery_streak += 1;
                self.promote_streak = 0;
                self.promote_target = None;
                if self.recovery_streak >= self.config.recovery_streak {
                    let next = step_down(curr);
                    self.recovery_streak = 0;
                    next
                } else {
                    curr
                }
            }
        }
    }

    /// Streak actual de promoción. Diagnóstico para HTTP `/health/detailed`.
    pub fn promote_streak(&self) -> u32 { self.promote_streak }
    pub fn recovery_streak(&self) -> u32 { self.recovery_streak }
    pub fn current_degradation(&self) -> Option<DegradationLevel> { self.current_degradation }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn cmp_level(d: Option<DegradationLevel>) -> u8 {
    match d {
        None                              => 0,
        Some(DegradationLevel::Light)     => 1,
        Some(DegradationLevel::Heavy)     => 2,
        Some(DegradationLevel::SafeMode)  => 3,
    }
}

fn step_up(curr: Option<DegradationLevel>) -> Option<DegradationLevel> {
    match curr {
        None                              => Some(DegradationLevel::Light),
        Some(DegradationLevel::Light)     => Some(DegradationLevel::Heavy),
        Some(DegradationLevel::Heavy)     => Some(DegradationLevel::SafeMode),
        Some(DegradationLevel::SafeMode)  => Some(DegradationLevel::SafeMode),
    }
}

fn step_down(curr: Option<DegradationLevel>) -> Option<DegradationLevel> {
    match curr {
        Some(DegradationLevel::SafeMode)  => Some(DegradationLevel::Heavy),
        Some(DegradationLevel::Heavy)     => Some(DegradationLevel::Light),
        Some(DegradationLevel::Light)     => None,
        None                              => None,
    }
}

fn drift_status_from_flags(flags: TickFlags) -> DriftStatus {
    if flags.contains(TickFlags::ANCHOR_LOST)       { DriftStatus::AllLost }
    else if flags.contains(TickFlags::ANCHOR_DRIFT_WARN) { DriftStatus::Inconsistent }
    else                                            { DriftStatus::Ok }
}

fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn build_summary(
    overall: Severity,
    score: u32,
    issues: &[HealthIssue],
    degradation: Option<DegradationLevel>,
) -> String {
    if issues.is_empty() {
        return format!("ok (score=0)");
    }
    let labels: Vec<&str> = issues.iter().map(|i| i.label()).collect();
    let deg_label = degradation.map(|d| d.label()).unwrap_or("none");
    format!(
        "{} (score={}, degraded={}, issues=[{}])",
        overall.label(), score, deg_label, labels.join(",")
    )
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_short_hysteresis() -> HealthConfig {
        HealthConfig {
            promote_streak: 2,
            recovery_streak: 3,
            ..Default::default()
        }
    }

    fn empty_tick() -> TickMetrics { TickMetrics::default() }

    fn dummy_registry() -> MetricsRegistry { MetricsRegistry::new() }

    fn no_extras() -> ExtraInputs { ExtraInputs::default() }

    // ── target_degradation ────────────────────────────────────────────

    #[test]
    fn target_none_when_score_zero() {
        let s = HealthSystem::new(HealthConfig::default());
        assert!(s.target_degradation(0).is_none());
    }

    #[test]
    fn target_light_at_warning_score() {
        let s = HealthSystem::new(HealthConfig::default());
        assert_eq!(s.target_degradation(1), Some(DegradationLevel::Light));
        assert_eq!(s.target_degradation(3), Some(DegradationLevel::Light));
    }

    #[test]
    fn target_heavy_at_critical_score() {
        let s = HealthSystem::new(HealthConfig::default());
        assert_eq!(s.target_degradation(4), Some(DegradationLevel::Heavy));
        assert_eq!(s.target_degradation(7), Some(DegradationLevel::Heavy));
    }

    #[test]
    fn target_safe_at_high_score() {
        let s = HealthSystem::new(HealthConfig::default());
        assert_eq!(s.target_degradation(8), Some(DegradationLevel::SafeMode));
        assert_eq!(s.target_degradation(20), Some(DegradationLevel::SafeMode));
    }

    // ── apply_hysteresis ──────────────────────────────────────────────

    #[test]
    fn hysteresis_promote_requires_streak() {
        let mut s = HealthSystem::new(cfg_with_short_hysteresis()); // promote=2
        // 1 tick con target Light → no promueve aún.
        let r1 = s.apply_hysteresis(Some(DegradationLevel::Light));
        assert!(r1.is_none());
        assert_eq!(s.promote_streak, 1);
        // 2do tick consecutivo → promueve.
        let r2 = s.apply_hysteresis(Some(DegradationLevel::Light));
        assert_eq!(r2, Some(DegradationLevel::Light));
    }

    #[test]
    fn hysteresis_promote_steps_one_level_at_a_time() {
        let mut s = HealthSystem::new(cfg_with_short_hysteresis());
        // Target SafeMode directo. Histéresis sube solo 1 nivel.
        let r = s.apply_hysteresis(Some(DegradationLevel::SafeMode));
        assert!(r.is_none());
        let r = s.apply_hysteresis(Some(DegradationLevel::SafeMode));
        assert_eq!(r, Some(DegradationLevel::Light)); // step 1
        s.current_degradation = Some(DegradationLevel::Light);

        let r = s.apply_hysteresis(Some(DegradationLevel::SafeMode));
        assert!(r.is_some());
        assert_eq!(r, Some(DegradationLevel::Light)); // streak 1 aún
        let r = s.apply_hysteresis(Some(DegradationLevel::SafeMode));
        assert_eq!(r, Some(DegradationLevel::Heavy)); // step 2
    }

    #[test]
    fn hysteresis_recovery_requires_streak() {
        let mut s = HealthSystem::new(cfg_with_short_hysteresis()); // recovery=3
        s.current_degradation = Some(DegradationLevel::Light);
        // 2 ticks Ok → no baja aún.
        for _ in 0..2 {
            let r = s.apply_hysteresis(None);
            assert_eq!(r, Some(DegradationLevel::Light));
        }
        // 3er tick → baja a None.
        let r = s.apply_hysteresis(None);
        assert!(r.is_none());
    }

    #[test]
    fn hysteresis_promote_streak_resets_when_target_changes() {
        let mut s = HealthSystem::new(cfg_with_short_hysteresis());
        s.apply_hysteresis(Some(DegradationLevel::Light));
        assert_eq!(s.promote_streak, 1);
        // Target cambia a Heavy → streak resetea.
        s.apply_hysteresis(Some(DegradationLevel::Heavy));
        assert_eq!(s.promote_streak, 1);
    }

    #[test]
    fn hysteresis_stable_state_resets_streaks() {
        let mut s = HealthSystem::new(cfg_with_short_hysteresis());
        s.current_degradation = Some(DegradationLevel::Light);
        s.promote_streak = 1;
        s.recovery_streak = 1;
        // Target == current → ambos streaks resetean.
        s.apply_hysteresis(Some(DegradationLevel::Light));
        assert_eq!(s.promote_streak, 0);
        assert_eq!(s.recovery_streak, 0);
    }

    // ── detect_primary_issues ─────────────────────────────────────────

    #[test]
    fn detects_frame_stale_at_warning() {
        let s = HealthSystem::new(HealthConfig::default());
        let mut m = empty_tick();
        m.frame_age_us = 150_000; // 150 ms → warning (100..200)
        m.vitals_confidence_bp = 10_000;
        let issues = s.detect_primary_issues(&m, &dummy_registry(), no_extras());
        let frame_stale = issues.iter().find(|i| matches!(i, HealthIssue::FrameStale { .. }));
        assert!(frame_stale.is_some());
        assert_eq!(frame_stale.unwrap().severity(), Severity::Warning);
    }

    #[test]
    fn detects_frame_stale_at_critical() {
        let s = HealthSystem::new(HealthConfig::default());
        let mut m = empty_tick();
        m.frame_age_us = 250_000; // 250 ms → critical
        m.vitals_confidence_bp = 10_000;
        let issues = s.detect_primary_issues(&m, &dummy_registry(), no_extras());
        let frame_stale = issues.iter().find(|i| matches!(i, HealthIssue::FrameStale { .. }));
        assert_eq!(frame_stale.unwrap().severity(), Severity::Critical);
    }

    #[test]
    fn no_frame_stale_below_threshold() {
        let s = HealthSystem::new(HealthConfig::default());
        let mut m = empty_tick();
        m.frame_age_us = 50_000; // 50 ms → ok
        m.vitals_confidence_bp = 10_000;
        let issues = s.detect_primary_issues(&m, &dummy_registry(), no_extras());
        assert!(!issues.iter().any(|i| matches!(i, HealthIssue::FrameStale { .. })));
    }

    #[test]
    fn anchor_drift_requires_streak_min() {
        let s = HealthSystem::new(HealthConfig::default()); // streak_min=5
        let mut m = empty_tick();
        m.flags = TickFlags::ANCHOR_DRIFT_WARN;
        m.vitals_confidence_bp = 10_000;
        // Streak 3 < 5 → no issue.
        let issues = s.detect_primary_issues(&m, &dummy_registry(),
            ExtraInputs { anchor_drift_streak: 3, ..Default::default() });
        assert!(!issues.iter().any(|i| matches!(i, HealthIssue::AnchorDrift { .. })));
        // Streak 5 → warning.
        let issues = s.detect_primary_issues(&m, &dummy_registry(),
            ExtraInputs { anchor_drift_streak: 5, ..Default::default() });
        let drift = issues.iter().find(|i| matches!(i, HealthIssue::AnchorDrift { .. }));
        assert_eq!(drift.unwrap().severity(), Severity::Warning);
    }

    #[test]
    fn anchor_lost_is_critical() {
        let s = HealthSystem::new(HealthConfig::default());
        let mut m = empty_tick();
        m.flags = TickFlags::ANCHOR_LOST;
        m.vitals_confidence_bp = 10_000;
        let issues = s.detect_primary_issues(&m, &dummy_registry(),
            ExtraInputs { anchor_drift_streak: 10, ..Default::default() });
        let drift = issues.iter().find(|i| matches!(i, HealthIssue::AnchorDrift { .. }));
        assert_eq!(drift.unwrap().severity(), Severity::Critical);
    }

    #[test]
    fn vitals_low_confidence_emits_issue() {
        let s = HealthSystem::new(HealthConfig::default());
        let mut m = empty_tick();
        m.vitals_confidence_bp = 1500; // 0.15 < 0.20 critical
        let issues = s.detect_primary_issues(&m, &dummy_registry(), no_extras());
        let vit = issues.iter().find(|i| matches!(i, HealthIssue::LowDetectionConfidence { .. }));
        assert_eq!(vit.unwrap().severity(), Severity::Critical);
    }

    // ── Combination rules ─────────────────────────────────────────────

    #[test]
    fn blind_mode_when_anchor_critical_and_frame_stale() {
        let s = HealthSystem::new(HealthConfig::default());
        let mut issues = vec![
            HealthIssue::AnchorDrift {
                status: DriftStatus::AllLost, valid: 0, total: 2,
                severity: Severity::Critical,
            },
            HealthIssue::FrameStale {
                age_ms: 250, threshold_ms: 200, severity: Severity::Critical,
            },
        ];
        s.apply_combination_rules(&mut issues);
        let comp = issues.iter().find(|i| matches!(i, HealthIssue::Composite { name, .. } if name == "blind_mode"));
        assert!(comp.is_some());
    }

    #[test]
    fn compute_saturation_when_vision_slow_plus_jitter() {
        let s = HealthSystem::new(HealthConfig::default());
        let mut issues = vec![
            HealthIssue::VisionSlow {
                vision_ms: 25, threshold_ms: 20, severity: Severity::Warning,
            },
            HealthIssue::HighJitter {
                stddev_ms: 4.0, threshold_ms: 3.0, severity: Severity::Warning,
            },
        ];
        s.apply_combination_rules(&mut issues);
        let comp = issues.iter().find(|i| matches!(i, HealthIssue::Composite { name, .. } if name == "compute_saturation"));
        assert!(comp.is_some());
    }

    #[test]
    fn no_composite_when_only_one_cause() {
        let s = HealthSystem::new(HealthConfig::default());
        let mut issues = vec![HealthIssue::VisionSlow {
            vision_ms: 25, threshold_ms: 20, severity: Severity::Warning,
        }];
        s.apply_combination_rules(&mut issues);
        assert!(!issues.iter().any(|i| matches!(i, HealthIssue::Composite { .. })));
    }

    // ── End-to-end evaluate_tick ──────────────────────────────────────

    #[test]
    fn evaluate_tick_publishes_status() {
        let mut s = HealthSystem::new(HealthConfig::default());
        let mut m = empty_tick();
        m.tick = 42;
        m.frame_seq = 42;
        m.vitals_confidence_bp = 10_000;
        let registry = dummy_registry();
        let status = s.evaluate_tick(&m, &registry, no_extras());
        assert_eq!(status.tick, 42);
        // Y la output_handle también lo refleja.
        let snap = s.last_status();
        assert_eq!(snap.tick, 42);
    }

    #[test]
    fn evaluate_tick_ok_status_when_clean() {
        let mut s = HealthSystem::new(HealthConfig::default());
        let mut m = empty_tick();
        m.vitals_confidence_bp = 10_000;
        m.frame_age_us = 50_000;
        m.tick_total_us = 15_000;
        m.vision_total_us = 10_000;
        let status = s.evaluate_tick(&m, &dummy_registry(), no_extras());
        assert_eq!(status.overall, Severity::Ok);
        assert_eq!(status.score, 0);
        assert!(status.degraded.is_none());
    }

    #[test]
    fn evaluate_tick_promotes_after_streak() {
        let mut s = HealthSystem::new(cfg_with_short_hysteresis()); // promote=2
        let mut m = empty_tick();
        m.vitals_confidence_bp = 10_000;
        m.frame_age_us = 150_000; // warning → score 1 → target Light
        // Tick 1: no degradation aún.
        let st = s.evaluate_tick(&m, &dummy_registry(), no_extras());
        assert!(st.degraded.is_none());
        // Tick 2: promote streak completo → Light.
        let st = s.evaluate_tick(&m, &dummy_registry(), no_extras());
        assert_eq!(st.degraded, Some(DegradationLevel::Light));
    }

    #[test]
    fn build_summary_lists_active_issues() {
        let issues = vec![
            HealthIssue::TickOverrun { tick_ms: 50, budget_ms: 33, severity: Severity::Warning },
            HealthIssue::FrameStale { age_ms: 150, threshold_ms: 100, severity: Severity::Warning },
        ];
        let s = build_summary(Severity::Warning, 2, &issues, Some(DegradationLevel::Light));
        assert!(s.contains("warning"));
        assert!(s.contains("score=2"));
        assert!(s.contains("light"));
        assert!(s.contains("tick_overrun"));
        assert!(s.contains("frame_stale"));
    }

    #[test]
    fn build_summary_ok_when_no_issues() {
        let s = build_summary(Severity::Ok, 0, &[], None);
        assert!(s.contains("ok"));
    }
}
