/// coords.rs — Conversión de coordenadas entre espacios:
///   viewport (píxeles del viewport de juego capturado por NDI)
///   → desktop  (píxeles del desktop virtual del PC gaming, 3840×1080)
///   → HID      (coordenadas absolutas del protocolo HID, 0..=32767)
///
/// Decisión de diseño: todas las funciones son puras (sin acceso a estado
/// global) para facilitar el testing unitario. La config se pasa por
/// referencia o se construye una instancia Coords una sola vez desde Config.

use crate::config::CoordsConfig;

/// Instancia construida desde CoordsConfig para evitar recalcular en cada tick.
#[derive(Debug, Clone)]
#[allow(dead_code)] // extension point: vp_w/vp_h + viewport_size()
pub struct Coords {
    desktop_w: u32,
    desktop_h: u32,
    win_x:     i32,
    win_y:     i32,
    vp_off_x:  i32,
    vp_off_y:  i32,
    vp_w:      u32,
    vp_h:      u32,
}

impl Coords {
    pub fn new(cfg: &CoordsConfig) -> Self {
        Self {
            desktop_w: cfg.desktop_total_w,
            desktop_h: cfg.desktop_total_h,
            win_x:     cfg.tibia_window_x,
            win_y:     cfg.tibia_window_y,
            vp_off_x:  cfg.game_viewport_offset_x,
            vp_off_y:  cfg.game_viewport_offset_y,
            vp_w:      cfg.game_viewport_w,
            vp_h:      cfg.game_viewport_h,
        }
    }

    /// Coordenadas HID absolutas (0..=32767) desde coordenadas del desktop virtual.
    /// Las coords HID abarcan el total del desktop, de modo que:
    ///   hid_x = desktop_x / desktop_w * 32767
    pub fn desktop_to_hid(&self, x: i32, y: i32) -> (u16, u16) {
        // Clampeamos al desktop para no enviar coords fuera de rango.
        let x = x.clamp(0, self.desktop_w as i32 - 1) as u32;
        let y = y.clamp(0, self.desktop_h as i32 - 1) as u32;
        let hid_x = (x as u64 * 32767 / self.desktop_w as u64) as u16;
        let hid_y = (y as u64 * 32767 / self.desktop_h as u64) as u16;
        (hid_x, hid_y)
    }

    /// Coords del viewport de juego → coords absolutas del desktop virtual.
    /// El viewport tiene su origen en win_x + vp_off_x, win_y + vp_off_y.
    pub fn viewport_to_desktop(&self, vx: i32, vy: i32) -> (i32, i32) {
        let dx = self.win_x + self.vp_off_x + vx;
        let dy = self.win_y + self.vp_off_y + vy;
        (dx, dy)
    }

    /// Atajo directo: viewport → HID (composición de las dos anteriores).
    pub fn viewport_to_hid(&self, vx: i32, vy: i32) -> (u16, u16) {
        let (dx, dy) = self.viewport_to_desktop(vx, vy);
        self.desktop_to_hid(dx, dy)
    }

    /// Dimensiones del viewport de captura NDI.
    #[allow(dead_code)]
    pub fn viewport_size(&self) -> (u32, u32) {
        (self.vp_w, self.vp_h)
    }
}

// ─── Tests unitarios ──────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordsConfig;

    /// Configuración de referencia: PC gaming con dos monitores 1920×1080,
    /// Tibia a pantalla completa en el monitor 1 (izquierdo, origen 0,0).
    fn test_cfg() -> CoordsConfig {
        CoordsConfig {
            desktop_total_w:        3840,
            desktop_total_h:        1080,
            tibia_window_x:         0,
            tibia_window_y:         0,
            tibia_window_w:         1920,
            tibia_window_h:         1080,
            game_viewport_offset_x: 0,
            game_viewport_offset_y: 0,
            game_viewport_w:        1920,
            game_viewport_h:        1080,
        }
    }

    #[test]
    fn desktop_to_hid_origin() {
        let c = Coords::new(&test_cfg());
        let (hx, hy) = c.desktop_to_hid(0, 0);
        assert_eq!(hx, 0);
        assert_eq!(hy, 0);
    }

    #[test]
    fn desktop_to_hid_top_right_corner_of_monitor1() {
        // El extremo derecho del monitor 1 en un desktop 3840 ancho
        // es x=1919. HID_x ≈ 1919/3840 * 32767 ≈ 16375.
        let c = Coords::new(&test_cfg());
        let (hx, _) = c.desktop_to_hid(1919, 0);
        let expected = (1919u64 * 32767 / 3840) as u16;
        assert_eq!(hx, expected);
    }

    #[test]
    fn desktop_to_hid_full_desktop_bottom_right() {
        // Esquina inferior derecha del desktop virtual → máximo HID.
        let c = Coords::new(&test_cfg());
        let (hx, hy) = c.desktop_to_hid(3839, 1079);
        // Debe ser el máximo posible sin llegar a 32767 exacto
        // (el pixel en 3839/3840*32767 ≈ 32758).
        let ex = (3839u64 * 32767 / 3840) as u16;
        let ey = (1079u64 * 32767 / 1080) as u16;
        assert_eq!(hx, ex);
        assert_eq!(hy, ey);
    }

    #[test]
    fn desktop_to_hid_clamp_negative() {
        let c = Coords::new(&test_cfg());
        let (hx, hy) = c.desktop_to_hid(-100, -50);
        assert_eq!(hx, 0);
        assert_eq!(hy, 0);
    }

    #[test]
    fn desktop_to_hid_clamp_over_max() {
        let c = Coords::new(&test_cfg());
        let (hx, hy) = c.desktop_to_hid(9999, 9999);
        // Debe clampear al borde del desktop.
        let ex = (3839u64 * 32767 / 3840) as u16;
        let ey = (1079u64 * 32767 / 1080) as u16;
        assert_eq!(hx, ex);
        assert_eq!(hy, ey);
    }

    #[test]
    fn viewport_to_desktop_identity_when_no_offset() {
        // Con ventana en (0,0) y offset de viewport en (0,0),
        // viewport coords == desktop coords.
        let c = Coords::new(&test_cfg());
        let (dx, dy) = c.viewport_to_desktop(100, 200);
        assert_eq!(dx, 100);
        assert_eq!(dy, 200);
    }

    #[test]
    fn viewport_to_desktop_with_window_offset() {
        let mut cfg = test_cfg();
        cfg.tibia_window_x         = 50;
        cfg.tibia_window_y         = 30;
        cfg.game_viewport_offset_x = 10;
        cfg.game_viewport_offset_y = 5;
        let c = Coords::new(&cfg);
        let (dx, dy) = c.viewport_to_desktop(100, 200);
        assert_eq!(dx, 50 + 10 + 100); // 160
        assert_eq!(dy, 30 + 5  + 200); // 235
    }

    #[test]
    fn viewport_to_hid_center_of_viewport() {
        let c = Coords::new(&test_cfg());
        // Centro del viewport en una pantalla 1920×1080 sobre desktop 3840×1080.
        let (vx, vy) = (960, 540);
        let (hx, hy) = c.viewport_to_hid(vx, vy);
        // desktop(960, 540) → HID
        let ex = (960u64 * 32767 / 3840) as u16;
        let ey = (540u64 * 32767 / 1080) as u16;
        assert_eq!(hx, ex);
        assert_eq!(hy, ey);
    }

    #[test]
    fn viewport_to_hid_composition_matches_manual() {
        let c = Coords::new(&test_cfg());
        let (vx, vy) = (300, 400);
        // Cálculo manual:
        let (dx, dy) = c.viewport_to_desktop(vx, vy);
        let (ex, ey) = c.desktop_to_hid(dx, dy);
        let (hx, hy) = c.viewport_to_hid(vx, vy);
        assert_eq!(hx, ex);
        assert_eq!(hy, ey);
    }
}
