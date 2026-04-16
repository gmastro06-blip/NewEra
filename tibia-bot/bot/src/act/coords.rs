/// coords.rs — Conversión de coordenadas entre espacios:
///   viewport (píxeles del viewport de juego capturado por NDI)
///   → desktop  (píxeles del desktop virtual del PC gaming, 3840×1080)
///   → HID      (coordenadas absolutas del protocolo HID, 0..=32767)
///
/// Decisión de diseño: todas las funciones son puras (sin acceso a estado
/// global) para facilitar el testing unitario. La config se pasa por
/// referencia o se construye una instancia Coords una sola vez desde Config.

use crate::config::CoordsConfig;

/// Instancia construida desde CoordsConfig (defaults) y opcionalmente
/// actualizada con geometría real reportada por el bridge via WinAPI.
///
/// Fields renombrados 2026-04-16:
/// - `vscreen_x/y/w/h`: bounding box del virtual desktop (origen puede ser
///   negativo si hay monitores a la izquierda/arriba del primario).
/// - `win_x/y`: posición ABSOLUTA (screen coords) de la ventana Tibia.
/// - `vp_off_x/y`: offset del game viewport dentro de la ventana Tibia.
///
/// Cuando auto-detect via WinAPI está activo (método `override_from_geometry`),
/// estos valores se actualizan dinámicamente al boot del bot, eliminando la
/// necesidad de calibración manual en setups multi-monitor.
#[derive(Debug, Clone)]
#[allow(dead_code)] // extension point: vp_w/vp_h + viewport_size()
pub struct Coords {
    vscreen_x: i32,
    vscreen_y: i32,
    vscreen_w: u32,
    vscreen_h: u32,
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
            vscreen_x: cfg.vscreen_origin_x,
            vscreen_y: cfg.vscreen_origin_y,
            vscreen_w: cfg.desktop_total_w,
            vscreen_h: cfg.desktop_total_h,
            win_x:     cfg.tibia_window_x,
            win_y:     cfg.tibia_window_y,
            vp_off_x:  cfg.game_viewport_offset_x,
            vp_off_y:  cfg.game_viewport_offset_y,
            vp_w:      cfg.game_viewport_w,
            vp_h:      cfg.game_viewport_h,
        }
    }

    /// Override los valores de virtual screen + Tibia window con los
    /// reportados por el bridge WinAPI. Elimina dependencia de calibración
    /// manual en multi-monitor.
    ///
    /// `vp_off_x/y` y `vp_w/h` no se alteran (requieren análisis del frame
    /// para detectar viewport offset dentro de la ventana).
    #[allow(dead_code)]
    pub fn override_from_geometry(
        &mut self,
        vscreen_x: i32, vscreen_y: i32, vscreen_w: i32, vscreen_h: i32,
        tibia_x: i32, tibia_y: i32, tibia_w: i32, tibia_h: i32,
    ) {
        self.vscreen_x = vscreen_x;
        self.vscreen_y = vscreen_y;
        self.vscreen_w = vscreen_w.max(1) as u32;
        self.vscreen_h = vscreen_h.max(1) as u32;
        self.win_x = tibia_x;
        self.win_y = tibia_y;
        // Solo override vp_w/h si el viewport cubre toda la ventana (caso
        // fullscreen). Para windowed con UI elements, el offset se queda
        // del config manual.
        if self.vp_off_x == 0 && self.vp_off_y == 0 {
            self.vp_w = tibia_w.max(1) as u32;
            self.vp_h = tibia_h.max(1) as u32;
        }
    }

    /// Coordenadas HID absolutas (0..=32767) desde coordenadas del desktop virtual.
    ///
    /// El HID absoluto mapea al VIRTUAL SCREEN completo (bbox de todos los
    /// monitores). Si el virtual screen tiene origen negativo (ej monitor
    /// secundario a la izquierda del primario), ajustamos:
    ///   hid_x = (desktop_x - vscreen_x) / vscreen_w * 32767
    pub fn desktop_to_hid(&self, x: i32, y: i32) -> (u16, u16) {
        // Clampear al virtual screen bbox.
        let x_min = self.vscreen_x;
        let x_max = self.vscreen_x + self.vscreen_w as i32 - 1;
        let y_min = self.vscreen_y;
        let y_max = self.vscreen_y + self.vscreen_h as i32 - 1;
        let x = x.clamp(x_min, x_max);
        let y = y.clamp(y_min, y_max);
        // Desplazar al origen del virtual screen y normalizar.
        let rel_x = (x - self.vscreen_x) as u64;
        let rel_y = (y - self.vscreen_y) as u64;
        let hid_x = (rel_x * 32767 / self.vscreen_w as u64) as u16;
        let hid_y = (rel_y * 32767 / self.vscreen_h as u64) as u16;
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

    /// Accessor para logging / diagnostic.
    #[allow(dead_code)]
    pub fn snapshot(&self) -> CoordsSnapshot {
        CoordsSnapshot {
            vscreen_x: self.vscreen_x,
            vscreen_y: self.vscreen_y,
            vscreen_w: self.vscreen_w,
            vscreen_h: self.vscreen_h,
            win_x:     self.win_x,
            win_y:     self.win_y,
        }
    }
}

/// Snapshot de la configuración actual de Coords, para logging.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct CoordsSnapshot {
    pub vscreen_x: i32,
    pub vscreen_y: i32,
    pub vscreen_w: u32,
    pub vscreen_h: u32,
    pub win_x:     i32,
    pub win_y:     i32,
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
            vscreen_origin_x:       0,
            vscreen_origin_y:       0,
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

    // ── Tests con virtual screen origen NEGATIVO (multi-monitor real) ────
    //
    // Setup del usuario session 2026-04-16:
    //   Monitor 2 (secundario): X = -1920..0, Y = 0..1080
    //   Monitor 1 (primary, Tibia): X = 0..1920, Y = 0..1080
    //   Virtual screen: origen (-1920, 0), size 3840×1080
    //
    // Auto-detect via bridge retorna esos valores. Coord (960, 540) del
    // viewport Tibia debería mapear a HID que el HID driver traduce a
    // X=960 absoluto, dentro de monitor 1 (donde está Tibia).

    fn multimonitor_cfg() -> CoordsConfig {
        CoordsConfig {
            vscreen_origin_x:       -1920,
            vscreen_origin_y:       0,
            desktop_total_w:        3840,
            desktop_total_h:        1080,
            tibia_window_x:         0,      // Tibia en monitor primary absoluto X=0
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
    fn desktop_to_hid_handles_negative_vscreen_origin() {
        // Click at center of Tibia (viewport 960, 540) should map to HID
        // that translates back to absolute desktop X=960.
        let c = Coords::new(&multimonitor_cfg());
        // desktop absolute X=960 (in monitor 1, virtual offset from -1920)
        let (hx, _) = c.desktop_to_hid(960, 540);
        // Offset relativo al vscreen origin: 960 - (-1920) = 2880
        // hid_x = 2880 / 3840 * 32767 = 24575
        assert_eq!(hx, 24575);
    }

    #[test]
    fn desktop_to_hid_leftmost_pixel_of_secondary_monitor() {
        // Leftmost pixel of monitor 2 = absolute X=-1920 (= vscreen origin).
        // HID must be 0.
        let c = Coords::new(&multimonitor_cfg());
        let (hx, _) = c.desktop_to_hid(-1920, 0);
        assert_eq!(hx, 0);
    }

    #[test]
    fn desktop_to_hid_border_between_monitors() {
        // Border between monitor 2 (ends X=-1, mon2 is X=-1920..-1) and
        // monitor 1 (starts X=0). HID at X=0 absolute should be 16383-ish.
        let c = Coords::new(&multimonitor_cfg());
        let (hx, _) = c.desktop_to_hid(0, 0);
        // Offset: 0 - (-1920) = 1920. hid = 1920/3840 * 32767 = 16383
        assert_eq!(hx, 16383);
    }

    #[test]
    fn desktop_to_hid_clamp_handles_negative_origin() {
        // Click at X=-9999 should clamp to leftmost pixel of virtual screen.
        let c = Coords::new(&multimonitor_cfg());
        let (hx, _) = c.desktop_to_hid(-9999, 0);
        assert_eq!(hx, 0);
    }

    #[test]
    fn viewport_to_hid_center_of_tibia_in_primary_monitor() {
        // User session 2026-04-16 setup: Tibia en primary monitor, primary
        // a la DERECHA físicamente pero virtual X=0..1920. Monitor 2 a la
        // izquierda virtual X=-1920..0.
        //
        // Click al centro del viewport (960, 540) debería mapear a HID que
        // Windows traduce a desktop X=960 (centro de Tibia en monitor 1).
        let c = Coords::new(&multimonitor_cfg());
        let (hx, hy) = c.viewport_to_hid(960, 540);
        // desktop = (0 + 0 + 960, 0 + 0 + 540) = (960, 540)
        // offset from vscreen origin (-1920, 0): (2880, 540)
        // hid = (2880/3840*32767, 540/1080*32767) = (24575, 16383)
        assert_eq!(hx, 24575);
        assert_eq!(hy, 16383);
    }

    #[test]
    fn viewport_to_hid_origin_of_tibia_when_in_primary() {
        // Click al (0, 0) del viewport (top-left de Tibia) con Tibia en
        // primary monitor X=0. Desktop abs = (0, 0). HID = (16383, 0).
        let c = Coords::new(&multimonitor_cfg());
        let (hx, hy) = c.viewport_to_hid(0, 0);
        assert_eq!(hx, 16383);
        assert_eq!(hy, 0);
    }
}
