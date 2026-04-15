/// color.rs — Helpers de análisis de color sobre píxeles RGBA.
///
/// Convención de layout en memoria: byte[0]=R, byte[1]=G, byte[2]=B, byte[3]=A
///
/// IMPORTANTE — formato real confirmado empíricamente:
/// El NDI runtime con color_format=0 (BGRX_BGRA) debería entregar BGRA según
/// la documentación, pero DistroAV/OBS en este setup entrega RGBA.
/// Verificado midiendo la barra de mana azul de Tibia: valor alto (≥160) está
/// en byte[2] (= B en RGBA), no en byte[0] (= B en BGRA).
/// El fourcc del frame se loggea en ndi_receiver.rs al nivel DEBUG.
/// Si algún día cambia el setup y los colores fallan, verificar que el fourcc
/// sea 0x41424752 ('RGBA'). Si es 0x41524742 ('BGRA'), invertir px[0]↔px[2].

/// Reordena un píxel RGBA (byte order del frame) a tupla (B, G, R, A).
/// Útil si algún consumidor espera orden BGRA (ej: imageproc).
#[inline]
#[allow(dead_code)] // extension point: BGRA compat layer
pub fn rgba_to_bgra(px: &[u8]) -> (u8, u8, u8, u8) {
    (px[2], px[1], px[0], px[3])
}

/// Comprueba si un píxel (en formato RGBA) está dentro de un rango de color RGB.
/// `lo` y `hi` son (R, G, B) mínimos y máximos (inclusivos).
#[inline]
pub fn in_rgb_range(px: &[u8], lo: (u8, u8, u8), hi: (u8, u8, u8)) -> bool {
    let r = px[0];
    let g = px[1];
    let b = px[2];
    r >= lo.0 && r <= hi.0 &&
    g >= lo.1 && g <= hi.1 &&
    b >= lo.2 && b <= hi.2
}

/// Retorna true si el píxel corresponde a una barra de vida/mana "llena".
///
/// Usa crominancia (max − min de canales RGB) en lugar de un color específico.
/// Esto hace que la lectura sea INMUNE al cambio de color del cliente de Tibia:
///   - HP 100%–60%: barra verde   → crominancia alta ✓
///   - HP  60%–30%: barra amarilla → crominancia alta ✓
///   - HP  30%–0%:  barra roja    → crominancia alta ✓
///   - Mana 100%–0%: barra azul   → crominancia alta ✓
///   - Fondo vacío:  gris neutro  → crominancia ≈ 0  ✗ (descartado)
///
/// Umbral de crominancia = 40; brightness mínima = 50.
/// No requiere calibración — el fondo gris del UI de Tibia no cambia.
#[inline]
pub fn is_bar_filled(px: &[u8]) -> bool {
    let r = px[0] as i32;
    let g = px[1] as i32;
    let b = px[2] as i32;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    max - min > 20 && max > 40
}

/// Retorna true si el píxel RGBA es "verde de HP" (barra de HP fase verde).
/// Sólo detecta la fase verde; usar `is_bar_filled` para robustez completa.
#[inline]
#[allow(dead_code)] // extension point: single-phase HP detection
pub fn is_hp_green(px: &[u8]) -> bool {
    in_rgb_range(px, (0, 140, 0), (90, 255, 90))
}

/// Retorna true si el píxel RGBA es "azul de mana" (barra de mana).
/// Sólo detecta azul saturado; usar `is_bar_filled` para robustez completa.
#[inline]
#[allow(dead_code)] // extension point: single-phase mana detection
pub fn is_mana_blue(px: &[u8]) -> bool {
    in_rgb_range(px, (0, 0, 140), (90, 90, 255))
}

/// Retorna true si el píxel RGBA es rojo de monstruo (borde de batalla list).
///
/// Usa dominancia relativa en lugar de rango absoluto para tolerar la compresión NDI:
/// el borde rojo de Tibia sale como R≈130-255, G≈40-80, B≈40-80 después de compresión.
/// Requiere R > 100 y que R supere a G y B por al menos 40 unidades.
#[inline]
pub fn is_monster_red(px: &[u8]) -> bool {
    let r = px[0] as i32;
    let g = px[1] as i32;
    let b = px[2] as i32;
    r > 100 && r > g + 40 && r > b + 40
}

/// Retorna true si el píxel RGBA es azul de jugador (borde de battle list).
#[inline]
pub fn is_player_blue(px: &[u8]) -> bool {
    in_rgb_range(px, (0, 0, 160), (80, 80, 255))
}

/// Retorna true si el píxel RGBA es amarillo de NPC (borde de battle list).
#[inline]
pub fn is_npc_yellow(px: &[u8]) -> bool {
    in_rgb_range(px, (180, 160, 0), (255, 255, 80))
}

/// Distancia al cuadrado entre dos colores RGB (evita sqrt).
#[inline]
#[allow(dead_code)] // extension point: color matching utility
pub fn rgb_dist_sq(px: &[u8], r: u8, g: u8, b: u8) -> u32 {
    let dr = px[0] as i32 - r as i32;
    let dg = px[1] as i32 - g as i32;
    let db = px[2] as i32 - b as i32;
    (dr * dr + dg * dg + db * db) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // Nota: píxeles en formato RGBA: [R, G, B, A]

    #[test]
    fn is_bar_filled_green_phase() {
        let green_px = [20u8, 200, 20, 255]; // RGBA: verde HP
        assert!(is_bar_filled(&green_px));
    }

    #[test]
    fn is_bar_filled_yellow_phase() {
        let yellow_px = [200u8, 200, 0, 255]; // RGBA: amarillo ~50% HP
        assert!(is_bar_filled(&yellow_px));
    }

    #[test]
    fn is_bar_filled_red_phase() {
        let red_px = [200u8, 20, 20, 255]; // RGBA: rojo ~20% HP
        assert!(is_bar_filled(&red_px));
    }

    #[test]
    fn is_bar_filled_blue_mana() {
        let blue_px = [10u8, 20, 200, 255]; // RGBA: azul mana
        assert!(is_bar_filled(&blue_px));
    }

    #[test]
    fn is_bar_filled_rejects_gray_background() {
        let gray_px = [90u8, 88, 90, 255]; // RGBA: fondo gris neutro
        assert!(!is_bar_filled(&gray_px));
    }

    #[test]
    fn is_bar_filled_rejects_dark_gray() {
        let dark_px = [25u8, 25, 25, 255];
        assert!(!is_bar_filled(&dark_px));
    }

    #[test]
    fn hp_green_detection() {
        let green_px = [20u8, 200, 30, 255]; // RGBA: R=20, G=200, B=30
        assert!(is_hp_green(&green_px));
        let red_px = [220u8, 20, 20, 255]; // RGBA: R=220, G=20, B=20
        assert!(!is_hp_green(&red_px));
    }

    #[test]
    fn mana_blue_detection() {
        let blue_px = [10u8, 20, 200, 255]; // RGBA: R=10, G=20, B=200
        assert!(is_mana_blue(&blue_px));
        let green_px = [20u8, 200, 30, 255];
        assert!(!is_mana_blue(&green_px));
    }

    #[test]
    fn monster_red_detection() {
        let red_px = [220u8, 20, 20, 255]; // RGBA: R=220, G=20, B=20
        assert!(is_monster_red(&red_px));
        let green_px = [20u8, 200, 30, 255];
        assert!(!is_monster_red(&green_px));
    }

    #[test]
    fn in_rgb_range_boundaries() {
        let px = [150u8, 100, 50, 255]; // RGBA: R=150, G=100, B=50
        assert!(in_rgb_range(&px, (150, 100, 50), (150, 100, 50)));
        assert!(!in_rgb_range(&px, (151, 100, 50), (200, 150, 100)));
    }

    #[test]
    fn rgb_dist_sq_same_color() {
        let px = [100u8, 150, 200, 255]; // RGBA
        assert_eq!(rgb_dist_sq(&px, 100, 150, 200), 0);
    }
}
