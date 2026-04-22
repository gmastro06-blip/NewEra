pub mod coords;
pub mod keycode;
pub mod pico_link;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::act::coords::Coords;
use crate::act::pico_link::{PicoHandle, PicoReply};
use crate::config::CoordsConfig;
use crate::instrumentation::{ActionKindTag, MetricsRegistry};
use crate::safety::timing::sample_gauss_ms;

/// Parámetros de jitter pre-send. Ver `safety::timing::sample_gauss_ms`.
/// `mean = 0.0` y `std = 0.0` desactivan el jitter completamente.
#[derive(Debug, Clone, Copy, Default)]
pub struct PresendJitter {
    pub mean_ms: f64,
    pub std_ms:  f64,
}

impl PresendJitter {
    pub fn disabled() -> Self { Self { mean_ms: 0.0, std_ms: 0.0 } }
    pub fn is_disabled(&self) -> bool { self.mean_ms <= 0.0 && self.std_ms <= 0.0 }
}

/// Actuator de alto nivel.
/// Convierte coordenadas del viewport a HID y envía comandos a la Pico.
pub struct Actuator {
    pico:    PicoHandle,
    coords:  Coords,
    jitter:  PresendJitter,
    /// Optional handle al MetricsRegistry para registrar action_rtt cada
    /// vez que el bridge ack-ea un comando. None = no se registra (modo
    /// legacy / tests sin registry).
    metrics: Option<Arc<MetricsRegistry>>,
}

impl Actuator {
    pub fn new(pico: PicoHandle, coords_cfg: &CoordsConfig) -> Self {
        Self {
            pico,
            coords: Coords::new(coords_cfg),
            jitter: PresendJitter::disabled(),
            metrics: None,
        }
    }

    /// Crea un Actuator con jitter pre-send (Fase 5 safety).
    pub fn with_jitter(pico: PicoHandle, coords_cfg: &CoordsConfig, jitter: PresendJitter) -> Self {
        Self {
            pico,
            coords: Coords::new(coords_cfg),
            jitter,
            metrics: None,
        }
    }

    /// Constructor builder-style: añade el handle al MetricsRegistry para
    /// que cada ACK de la Pico se registre vía record_action_ack.
    pub fn with_metrics(mut self, metrics: Arc<MetricsRegistry>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Registra un PicoReply en el MetricsRegistry si está configurado.
    /// `latency_ms * 1000.0` → µs (saturado a u32::MAX).
    fn record_ack(&self, reply: &PicoReply, kind: ActionKindTag) {
        if let Some(reg) = &self.metrics {
            let rtt_us = (reply.latency_ms * 1000.0).min(u32::MAX as f64) as u32;
            reg.record_action_ack(kind, rtt_us, reply.ok);
        }
    }

    /// Espera un delay gaussiano antes de enviar el siguiente comando.
    /// No-op si `jitter` está deshabilitado (mean=0, std=0).
    async fn apply_presend_jitter(&self) {
        if self.jitter.is_disabled() {
            return;
        }
        let ms = sample_gauss_ms(self.jitter.mean_ms, self.jitter.std_ms);
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    /// Mueve el mouse a coordenadas del viewport de juego.
    pub async fn mouse_move(&self, vx: i32, vy: i32) {
        self.apply_presend_jitter().await;
        let (hx, hy) = self.coords.viewport_to_hid(vx, vy);
        self.pico
            .send_fire_forget(format!("MOUSE_MOVE {} {}", hx, hy))
            .await;
    }

    /// Click en coordenadas del viewport de juego.
    pub async fn click(&self, vx: i32, vy: i32, button: &str) -> Result<PicoReply> {
        self.apply_presend_jitter().await;
        let (hx, hy) = self.coords.viewport_to_hid(vx, vy);
        // MOUSE_MOVE sincrónico: debemos leer su respuesta antes de enviar
        // MOUSE_CLICK, o la respuesta del move sería leída como respuesta del click.
        // Registramos el RTT del MOUSE_MOVE (es parte del click round-trip).
        let move_reply = self.pico.send(format!("MOUSE_MOVE {} {}", hx, hy)).await?;
        self.record_ack(&move_reply, ActionKindTag::MouseMove);
        let click_reply = self.pico.send(format!("MOUSE_CLICK {}", button)).await?;
        self.record_ack(&click_reply, ActionKindTag::Click);
        Ok(click_reply)
    }

    /// Teclado: tap de una tecla por HID keycode.
    pub async fn key_tap(&self, hidcode: u8) -> Result<PicoReply> {
        self.apply_presend_jitter().await;
        let reply = self.pico.send(format!("KEY_TAP 0x{:02X}", hidcode)).await?;
        self.record_ack(&reply, ActionKindTag::Key);
        Ok(reply)
    }

    /// Libera todas las teclas y botones. Útil en shutdown o emergencia.
    pub async fn reset(&self) -> Result<PicoReply> {
        // No registramos RESET — es diagnóstico/cleanup, no acción de gameplay.
        self.pico.send("RESET").await
    }

    /// Ping a la Pico para verificar latencia del pipeline.
    pub async fn ping(&self) -> Result<PicoReply> {
        // No registramos PING — es heartbeat, no acción.
        self.pico.send("PING").await
    }

    /// Retorna `true` si el bridge reportó que Tibia no tiene el foco.
    pub fn is_focus_lost(&self) -> bool {
        self.pico.is_focus_lost()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Construye un Actuator de test sin TCP real (PicoHandle dummy).
    /// Solo usable para invocar `record_ack()` directamente.
    fn test_actuator_with_metrics() -> (Actuator, Arc<MetricsRegistry>) {
        let cfg = crate::config::PicoConfig {
            bridge_addr: "127.0.0.1:0".into(),
            connect_timeout_ms: 1000,
            command_timeout_ms: 100,
            max_backoff_secs: 5,
            auth_token: None,
        };
        let pico = crate::act::pico_link::spawn(cfg);
        let coords_cfg = crate::config::CoordsConfig::default();
        let metrics = Arc::new(MetricsRegistry::new());
        let act = Actuator::new(pico, &coords_cfg)
            .with_metrics(Arc::clone(&metrics));
        (act, metrics)
    }

    #[tokio::test]
    async fn record_ack_increments_counter_and_histogram_on_ok() {
        let (act, metrics) = test_actuator_with_metrics();
        let reply = PicoReply { ok: true, body: "OK".into(), latency_ms: 12.5 };
        act.record_ack(&reply, ActionKindTag::Click);

        let acked = metrics.actions_acked[ActionKindTag::Click as usize].load(Ordering::Relaxed);
        assert_eq!(acked, 1);
        assert_eq!(metrics.action_rtt.count(), 1);
        // 12.5 ms = 12500 µs; mean del histograma debe estar en ese ballpark.
        let mean = metrics.action_rtt.mean_us();
        assert!((10_000..=15_000).contains(&mean), "mean={}", mean);
    }

    #[tokio::test]
    async fn record_ack_increments_failed_counter_on_err() {
        let (act, metrics) = test_actuator_with_metrics();
        let reply = PicoReply { ok: false, body: "ERR".into(), latency_ms: 100.0 };
        act.record_ack(&reply, ActionKindTag::Heal);

        let failed = metrics.actions_failed[ActionKindTag::Heal as usize].load(Ordering::Relaxed);
        let acked  = metrics.actions_acked[ActionKindTag::Heal as usize].load(Ordering::Relaxed);
        assert_eq!(failed, 1);
        assert_eq!(acked, 0);
        // RTT histogram NO debe tener sample para failures.
        assert_eq!(metrics.action_rtt.count(), 0);
    }

    #[tokio::test]
    async fn record_ack_no_op_when_metrics_not_set() {
        // Actuator sin .with_metrics() → record_ack es no-op silencioso.
        let cfg = crate::config::PicoConfig {
            bridge_addr: "127.0.0.1:0".into(),
            connect_timeout_ms: 1000,
            command_timeout_ms: 100,
            max_backoff_secs: 5,
            auth_token: None,
        };
        let pico = crate::act::pico_link::spawn(cfg);
        let act = Actuator::new(pico, &crate::config::CoordsConfig::default());
        let reply = PicoReply { ok: true, body: "OK".into(), latency_ms: 5.0 };
        // No panic, no efecto.
        act.record_ack(&reply, ActionKindTag::Click);
    }
}
