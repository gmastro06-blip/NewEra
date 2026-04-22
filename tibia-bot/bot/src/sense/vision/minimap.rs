/// minimap.rs — Captura del minimapa de Tibia.
///
/// El minimapa es un recorte del frame NDI. Por ahora solo capturamos los datos
/// crudos BGRA. El análisis de navegación (colores de tiles, posición del jugador)
/// se implementará en milestone 3.

use crate::sense::frame_buffer::Frame;
use crate::sense::perception::MinimapSnapshot;
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::crop::crop_bgra;

/// Captura el minimapa del frame y retorna un MinimapSnapshot.
/// Retorna None si el ROI no está definido o no cabe en el frame.
pub fn capture_minimap(frame: &Frame, roi: RoiDef) -> Option<MinimapSnapshot> {
    let data = crop_bgra(frame, crate::sense::vision::crop::Roi::new(roi.x, roi.y, roi.w, roi.h))?;
    Some(MinimapSnapshot {
        width:  roi.w,
        height: roi.h,
        data,
    })
}

/// Compara dos snapshots del minimapa y retorna la diferencia L1 normalizada [0.0..1.0].
///
/// Solo compara canales B, G, R — ignora el canal alfa (byte[3] de cada pixel BGRA)
/// para evitar ruido de codificación NDI en el alfa.
///
/// Un valor > 0 indica que el char se movió (el minimapa cambió de posición).
/// Datos empíricos con Tibia + NDI/DistroAV:
///   - Idle (animaciones de minimap): ~0.006-0.010
///   - Movimiento real (1+ tiles):    ~0.05-0.30
/// Retorna 0.0 si las dimensiones no coinciden o alguno está vacío.
pub fn diff_l1(prev: &MinimapSnapshot, curr: &MinimapSnapshot) -> f32 {
    if prev.width != curr.width
        || prev.height != curr.height
        || prev.data.is_empty()
        || curr.data.is_empty()
        || prev.data.len() != curr.data.len()
    {
        return 0.0;
    }
    // Iterar por pixels BGRA de 4 bytes, sumar solo B+G+R (skip alfa).
    let sum: u64 = prev.data.chunks_exact(4)
        .zip(curr.data.chunks_exact(4))
        .map(|(a, b)| {
            a[0].abs_diff(b[0]) as u64  // B
            + a[1].abs_diff(b[1]) as u64 // G
            + a[2].abs_diff(b[2]) as u64 // R
        })
        .sum();
    // max_possible = num_pixels * 3 canales * 255
    let num_pixels = prev.data.len() as u64 / 4;
    let max_possible = num_pixels * 3 * 255;
    if max_possible == 0 { return 0.0; }
    sum as f32 / max_possible as f32
}

/// Retorna el color promedio del minimapa (para diagnóstico rápido).
/// Retorna (R, G, B) promedio.
#[allow(dead_code)] // extension point: diagnostics
pub fn average_color(snapshot: &MinimapSnapshot) -> (u8, u8, u8) {
    if snapshot.data.is_empty() {
        return (0, 0, 0);
    }
    let n = (snapshot.width * snapshot.height) as u64;
    if n == 0 { return (0, 0, 0); }

    let (mut sum_r, mut sum_g, mut sum_b) = (0u64, 0u64, 0u64);
    for chunk in snapshot.data.chunks_exact(4) {
        sum_b += chunk[0] as u64;
        sum_g += chunk[1] as u64;
        sum_r += chunk[2] as u64;
    }
    ((sum_r / n) as u8, (sum_g / n) as u8, (sum_b / n) as u8)
}

/// Máximo desplazamiento en píxeles a buscar por eje.
/// ±7 cubre 1 tile de movimiento por frame a 30Hz.
const MAX_SHIFT: i32 = 7;

/// Umbral de confianza por pixel (SAD promedio por canal BGR).
/// Si el mejor match supera este valor, el desplazamiento no es fiable
/// (cambio de piso, teleport, etc.) y se retorna (0, 0).
const DISPLACEMENT_CONFIDENCE_THRESHOLD: f32 = 20.0;

/// Calcula el desplazamiento en píxeles entre dos snapshots consecutivos
/// del minimap mediante búsqueda SAD (Sum of Absolute Differences).
///
/// Retorna `(dx, dy)` donde +dx = derecha, +dy = abajo.
/// Retorna `(0, 0)` si no hay desplazamiento fiable.
///
/// Rendimiento: ~1ms para 107×110 con ventana ±7.
pub fn displacement(prev: &MinimapSnapshot, curr: &MinimapSnapshot) -> Option<(i32, i32)> {
    if prev.width != curr.width
        || prev.height != curr.height
        || prev.data.is_empty()
        || curr.data.is_empty()
        || prev.data.len() != curr.data.len()
    {
        return None;
    }

    let w = prev.width as i32;
    let h = prev.height as i32;

    // Región interior: excluir borde de MAX_SHIFT para que el overlap
    // nunca salga de bounds. También evita artefactos del borde del minimap.
    let x0 = MAX_SHIFT;
    let x1 = w - MAX_SHIFT;
    let y0 = MAX_SHIFT;
    let y1 = h - MAX_SHIFT;

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    let stride = w as usize * 4;
    let mut best_sad: u64 = u64::MAX;
    let mut best_sx: i32 = 0;
    let mut best_sy: i32 = 0;

    for sy in -MAX_SHIFT..=MAX_SHIFT {
        for sx in -MAX_SHIFT..=MAX_SHIFT {
            let mut sad: u64 = 0;
            let mut broke = false;
            for row in y0..y1 {
                let prev_start = row as usize * stride + x0 as usize * 4;
                let curr_start = (row + sy) as usize * stride + (x0 + sx) as usize * 4;
                let cols = (x1 - x0) as usize;
                for col in 0..cols {
                    let pi = prev_start + col * 4;
                    let ci = curr_start + col * 4;
                    sad += prev.data[pi].abs_diff(curr.data[ci]) as u64
                         + prev.data[pi + 1].abs_diff(curr.data[ci + 1]) as u64
                         + prev.data[pi + 2].abs_diff(curr.data[ci + 2]) as u64;
                }
                if sad >= best_sad {
                    broke = true;
                    break;
                }
            }
            if !broke && sad < best_sad {
                best_sad = sad;
                best_sx = sx;
                best_sy = sy;
            }
        }
    }

    // Confianza: ¿el SAD por pixel es aceptable?
    let overlap_pixels = ((x1 - x0) * (y1 - y0)) as f32;
    let sad_per_pixel = best_sad as f32 / (overlap_pixels * 3.0);
    if sad_per_pixel > DISPLACEMENT_CONFIDENCE_THRESHOLD {
        return None;
    }

    Some((best_sx, best_sy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn make_frame(w: u32, h: u32, fill: [u8; 4]) -> Frame {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            data.extend_from_slice(&fill);
        }
        Frame { width: w, height: h, data, captured_at: Instant::now() }
    }

    #[test]
    fn capture_returns_correct_size() {
        let frame = make_frame(200, 200, [10, 20, 30, 255]);
        let roi = RoiDef::new(50, 50, 40, 40);
        let snap = capture_minimap(&frame, roi).unwrap();
        assert_eq!(snap.width, 40);
        assert_eq!(snap.height, 40);
        assert_eq!(snap.data.len(), 40 * 40 * 4);
    }

    #[test]
    fn capture_out_of_bounds_returns_none() {
        let frame = make_frame(100, 100, [0, 0, 0, 255]);
        let roi = RoiDef::new(90, 90, 20, 20); // 90+20 > 100
        assert!(capture_minimap(&frame, roi).is_none());
    }

    #[test]
    fn average_color_uniform() {
        let frame = make_frame(50, 50, [50u8, 100, 150, 255]); // BGRA: B=50, G=100, R=150
        let roi = RoiDef::new(0, 0, 50, 50);
        let snap = capture_minimap(&frame, roi).unwrap();
        let (r, g, b) = average_color(&snap);
        assert_eq!(r, 150);
        assert_eq!(g, 100);
        assert_eq!(b, 50);
    }

    // ── Displacement ────────────────────────────────────────────────

    fn make_snap(w: u32, h: u32, f: impl Fn(i32, i32) -> [u8; 4]) -> MinimapSnapshot {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                data.extend_from_slice(&f(x, y));
            }
        }
        MinimapSnapshot { width: w, height: h, data }
    }

    #[test]
    fn displacement_identical_returns_zero() {
        let snap = make_snap(30, 30, |x, y| {
            let v = ((x * 7 + y * 13) % 256) as u8;
            [v, v, v, 255]
        });
        assert_eq!(displacement(&snap, &snap), Some((0, 0)));
    }

    #[test]
    fn displacement_detects_horizontal_shift() {
        let prev = make_snap(30, 30, |x, y| {
            let v = ((x * 7 + y * 13) % 256) as u8;
            [v, v, v, 255]
        });
        // curr = prev shifted 3px to the right
        let curr = make_snap(30, 30, |x, y| {
            let v = (((x - 3) * 7 + y * 13).rem_euclid(256)) as u8;
            [v, v, v, 255]
        });
        assert_eq!(displacement(&prev, &curr), Some((3, 0)));
    }

    #[test]
    fn displacement_detects_vertical_shift() {
        let prev = make_snap(30, 30, |x, y| {
            let v = ((x * 7 + y * 13) % 256) as u8;
            [v, v, v, 255]
        });
        // curr = prev shifted 2px up (content moves up → dy = -2)
        let curr = make_snap(30, 30, |x, y| {
            let v = ((x * 7 + (y + 2) * 13).rem_euclid(256)) as u8;
            [v, v, v, 255]
        });
        assert_eq!(displacement(&prev, &curr), Some((0, -2)));
    }

    #[test]
    fn displacement_mismatched_dimensions_returns_none() {
        let a = make_snap(30, 30, |_, _| [128, 128, 128, 255]);
        let b = make_snap(20, 20, |_, _| [128, 128, 128, 255]);
        assert_eq!(displacement(&a, &b), None);
    }

    #[test]
    fn displacement_confidence_fail_returns_none() {
        // Snapshots sin correlación → SAD alto → None.
        let prev = make_snap(30, 30, |x, y| {
            let v = ((x * 7 + y * 13) % 256) as u8;
            [v, v.wrapping_add(80), v.wrapping_add(160), 255]
        });
        // Completamente aleatorio — no hay shift posible que matchee.
        let curr = make_snap(30, 30, |x, y| {
            let v = ((x * 31 + y * 97 + 73) % 256) as u8;
            [v.wrapping_add(100), v, v.wrapping_add(50), 255]
        });
        assert_eq!(displacement(&prev, &curr), None);
    }

    /// Bench-style guard: el budget de 5 ms aplica al binario release. En
    /// debug, `match_template` puede tardar 10-30 ms (se ve en CI / dev box).
    /// Marcado `#[ignore]` para no fallar el test default — correr con
    /// `cargo test --release -- --ignored` cuando se quiera validar el budget.
    /// Datos empíricos previos a este ignore: debug ~7-28 ms (varía por máquina);
    /// release < 1 ms en hardware razonable.
    #[test]
    #[ignore = "perf-only: usar `cargo test --release -- --ignored` para verificar budget"]
    fn displacement_107x110_under_5ms() {
        let prev = make_snap(107, 110, |x, y| {
            let v = ((x * 17 + y * 31) % 256) as u8;
            [v, v.wrapping_add(50), v.wrapping_add(100), 255]
        });
        let curr = make_snap(107, 110, |x, y| {
            let v = (((x - 2) * 17 + (y + 1) * 31).rem_euclid(256)) as u8;
            [v, v.wrapping_add(50), v.wrapping_add(100), 255]
        });
        let start = Instant::now();
        let result = displacement(&prev, &curr);
        let elapsed = start.elapsed();
        assert_eq!(result, Some((2, -1)));
        assert!(elapsed.as_millis() < 5, "took {}ms", elapsed.as_millis());
    }
}
