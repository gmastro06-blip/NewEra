pub mod coords;
pub mod keycode;
pub mod pico_link;

use std::time::Duration;

use anyhow::Result;

use crate::act::coords::Coords;
use crate::act::pico_link::{PicoHandle, PicoReply};
use crate::config::CoordsConfig;
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
}

impl Actuator {
    pub fn new(pico: PicoHandle, coords_cfg: &CoordsConfig) -> Self {
        Self {
            pico,
            coords: Coords::new(coords_cfg),
            jitter: PresendJitter::disabled(),
        }
    }

    /// Crea un Actuator con jitter pre-send (Fase 5 safety).
    pub fn with_jitter(pico: PicoHandle, coords_cfg: &CoordsConfig, jitter: PresendJitter) -> Self {
        Self {
            pico,
            coords: Coords::new(coords_cfg),
            jitter,
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
        self.pico.send(format!("MOUSE_MOVE {} {}", hx, hy)).await?;
        self.pico.send(format!("MOUSE_CLICK {}", button)).await
    }

    /// Teclado: tap de una tecla por HID keycode.
    pub async fn key_tap(&self, hidcode: u8) -> Result<PicoReply> {
        self.apply_presend_jitter().await;
        self.pico.send(format!("KEY_TAP 0x{:02X}", hidcode)).await
    }

    /// Libera todas las teclas y botones. Útil en shutdown o emergencia.
    pub async fn reset(&self) -> Result<PicoReply> {
        self.pico.send("RESET").await
    }

    /// Ping a la Pico para verificar latencia del pipeline.
    pub async fn ping(&self) -> Result<PicoReply> {
        self.pico.send("PING").await
    }

    /// Retorna `true` si el bridge reportó que Tibia no tiene el foco.
    pub fn is_focus_lost(&self) -> bool {
        self.pico.is_focus_lost()
    }
}
