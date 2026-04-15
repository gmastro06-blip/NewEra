/// metrics.rs — Historial rodante de métricas de visión.
///
/// Mantiene los últimos 1800 samples (~60s a 30 Hz) de:
///   - hp_ratio    (0.0..=1.0)
///   - mana_ratio  (0.0..=1.0)
///   - vision_cost (ms de procesamiento de visión por tick)
///
/// Expone estadísticas derivadas (avg, stddev, percentiles) sin retener
/// el lock de GameState durante el cómputo — calcular con `snapshot()`.

use std::collections::VecDeque;

const HISTORY_CAPACITY: usize = 1800; // 60 s × 30 Hz

/// Historial rodante de métricas de visión.
#[derive(Debug, Clone)]
pub struct VisionMetrics {
    pub hp_history:   VecDeque<f32>,
    pub mana_history: VecDeque<f32>,
    /// Costo de visión en ms por tick (tiempo de `vision.tick()`).
    pub cost_history: VecDeque<f32>,
    /// Número de ticks en que el AnchorTracker perdió el ancla.
    pub anchors_lost_count: u64,
}

impl Default for VisionMetrics {
    fn default() -> Self {
        Self {
            hp_history:         VecDeque::with_capacity(HISTORY_CAPACITY),
            mana_history:       VecDeque::with_capacity(HISTORY_CAPACITY),
            cost_history:       VecDeque::with_capacity(HISTORY_CAPACITY),
            anchors_lost_count: 0,
        }
    }
}

impl VisionMetrics {
    pub fn push_hp(&mut self, ratio: f32) {
        push_capped(&mut self.hp_history, ratio);
    }

    pub fn push_mana(&mut self, ratio: f32) {
        push_capped(&mut self.mana_history, ratio);
    }

    pub fn push_cost(&mut self, ms: f32) {
        push_capped(&mut self.cost_history, ms);
    }

    #[allow(dead_code)] // extension point
    pub fn mark_anchor_lost(&mut self) {
        self.anchors_lost_count += 1;
    }

    /// Calcula un snapshot de estadísticas sin mutar el estado.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            hp_avg:             avg(&self.hp_history),
            hp_stddev:          stddev(&self.hp_history),
            mana_avg:           avg(&self.mana_history),
            mana_stddev:        stddev(&self.mana_history),
            vision_cost_p50:    percentile(&self.cost_history, 50),
            vision_cost_p95:    percentile(&self.cost_history, 95),
            vision_cost_p99:    percentile(&self.cost_history, 99),
            anchors_lost_count: self.anchors_lost_count,
            sample_count:       self.hp_history.len() as u32,
        }
    }
}

fn push_capped(q: &mut VecDeque<f32>, val: f32) {
    if q.len() == HISTORY_CAPACITY {
        q.pop_front();
    }
    q.push_back(val);
}

fn avg(q: &VecDeque<f32>) -> Option<f32> {
    if q.is_empty() { return None; }
    Some(q.iter().sum::<f32>() / q.len() as f32)
}

fn stddev(q: &VecDeque<f32>) -> Option<f32> {
    let n = q.len();
    if n < 2 { return None; }
    let mean = q.iter().sum::<f32>() / n as f32;
    let variance = q.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n as f32;
    Some(variance.sqrt())
}

/// Percentil k (0–100) sobre una coleción desordenada. Copia para ordenar.
fn percentile(q: &VecDeque<f32>, k: u8) -> Option<f32> {
    if q.is_empty() { return None; }
    let mut v: Vec<f32> = q.iter().copied().collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((k as f32 / 100.0) * (v.len() - 1) as f32).round() as usize;
    Some(v[idx.min(v.len() - 1)])
}

/// Snapshot calculado de las métricas de visión (sin historial bruto).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricsSnapshot {
    /// Promedio de ratio de HP en los últimos ~60s (0.0–1.0). None si sin datos.
    pub hp_avg:    Option<f32>,
    pub hp_stddev: Option<f32>,
    /// Promedio de ratio de mana en los últimos ~60s (0.0–1.0). None si sin datos.
    pub mana_avg:    Option<f32>,
    pub mana_stddev: Option<f32>,
    /// Percentiles del costo de visión por tick (ms).
    pub vision_cost_p50: Option<f32>,
    pub vision_cost_p95: Option<f32>,
    pub vision_cost_p99: Option<f32>,
    /// Ticks totales desde el inicio en que el ancla se perdió.
    pub anchors_lost_count: u64,
    /// Número de samples en el historial (máx. 1800).
    pub sample_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simula el patrón real del loop: `if let Some(r) = hp_ratio { push_hp(r) }`.
    /// Verifica que Nones NO contaminan la rolling average.
    #[test]
    fn hp_avg_excludes_missing_readings() {
        let mut m = VisionMetrics::default();

        // 3 reads reales + 5 Nones + 3 reads reales más.
        let inputs: [Option<f32>; 11] = [
            Some(0.4),  // real
            Some(0.5),  // real
            Some(0.6),  // real
            None,       // skipped
            None,       // skipped
            None,       // skipped
            None,       // skipped
            None,       // skipped
            Some(0.4),  // real
            Some(0.5),  // real
            Some(0.6),  // real
        ];
        for r in inputs.iter() {
            if let Some(v) = r {
                m.push_hp(*v);
            }
        }

        let snap = m.snapshot();
        assert_eq!(snap.sample_count, 6, "solo 6 valores reales deben contar");
        // Avg = (0.4+0.5+0.6+0.4+0.5+0.6) / 6 = 0.5.
        let avg = snap.hp_avg.expect("hp_avg debe estar presente con 6 samples");
        assert!((avg - 0.5).abs() < 1e-6, "hp_avg esperado 0.5, got {}", avg);
    }

    /// Verifica que un historial vacío retorna None (no 0.0, no 1.0).
    #[test]
    fn empty_metrics_return_none() {
        let m = VisionMetrics::default();
        let snap = m.snapshot();
        assert_eq!(snap.hp_avg, None);
        assert_eq!(snap.hp_stddev, None);
        assert_eq!(snap.mana_avg, None);
        assert_eq!(snap.sample_count, 0);
    }

    /// Verifica que el historial rodante se capa en HISTORY_CAPACITY.
    #[test]
    fn history_caps_at_capacity() {
        let mut m = VisionMetrics::default();
        for _ in 0..(HISTORY_CAPACITY + 100) {
            m.push_hp(0.5);
        }
        let snap = m.snapshot();
        assert_eq!(snap.sample_count, HISTORY_CAPACITY as u32);
    }
}
