/// battle_list.rs — Análisis del panel de batalla.
///
/// Estrategia dual:
///   1. Primario: borde izquierdo de color (rojo/azul/amarillo) — clientes clásicos.
///   2. Fallback: barra de HP verde en la parte inferior de cada entrada.
///      Este cliente muestra un ícono con borde teal en vez de franja de color sólido,
///      por lo que el método primario no detecta nada. El fallback detecta cualquier
///      entrada con una barra de HP visible y la clasifica como Monster.
///
/// **Histéresis sticky-until-empty (fix in-game)**: el fallback HP-bar usa
/// dos thresholds asimétricos:
/// - `MIN_HP_BAR_PX` (alto): requerido para ENTRAR en detección. Filtra
///   falsos positivos de iconos o noise al aparecer un slot nuevo.
/// - `EMPTY_SLOT_PX` (muy bajo): un slot ya detectado solo se libera cuando
///   el slot está **realmente vacío** (conteo <= este valor). Esto resuelve
///   el problema de "mob con HP bajo se pierde": la HP bar de un mob al 10%
///   tiene solo ~8 px coloreados, pero el slot sigue conteniendo el icono
///   del mob. Solo cuando el mob muere y Tibia retira la entry del panel
///   el conteo cae por debajo de EMPTY_SLOT_PX.
///
/// Antes de este fix usábamos `STICKY_HP_BAR_PX=25` como threshold sticky
/// proporcional, pero eso perdía mobs debajo de ~30% HP (<25 px coloreados)
/// aunque el slot estuviera activo. El bot entraba en Idle dejando al mob
/// vivo porque ya no detectaba el flanco de retarget.
/// El estado por slot se mantiene en `BattleListDetector`.

use crate::sense::frame_buffer::Frame;
use crate::sense::perception::{BattleEntry, BattleList, EntryKind, SlotDebug};
use crate::sense::vision::calibration::RoiDef;
use crate::sense::vision::color::{is_attack_highlight, is_bar_filled, is_monster_red, is_npc_yellow, is_player_blue};
use crate::sense::vision::crop::{count_pixels, longest_horizontal_run, Roi};

/// Altura en píxeles de cada entrada en la lista de batalla de Tibia (1920x1080).
const ENTRY_HEIGHT_PX: u32 = 22;
/// Ancho en píxeles del borde de color a analizar (columnas más a la izquierda).
const BORDER_WIDTH_PX: u32 = 3;
/// Mínimo de píxeles del color para confirmar la clasificación.
/// Hits mínimos del color dominante en el borde (3×22=66 px máx) para
/// clasificar. Anti-false-positive 2026-04-18: valor anterior era 2, lo
/// que disparaba con texto rojo del NPC chat que deja 3-5 red pixels en
/// los primeros 3 cols. Un border REAL de Tibia tiene 40-60 red pixels
/// en esos 66. Bumpeado a 15 para rechazar texto + JPEG noise sin romper
/// detection de borders reales.
const MIN_BORDER_HITS:  u32 = 15;

/// Threshold ALTO: píxeles "HP bar" requeridos para **empezar** a detectar
/// un slot como Monster. Calibrado para HP bars reales (≥ ~40 px verdes).
const MIN_HP_BAR_PX: u32 = 40;

/// 2026-04-18: upper bound anti-false-positive. Un HP bar real de Tibia
/// (171 px ancho × ~4 px alto) tiene máximo ~500 pixeles coloreados. Chat
/// NPC / portraits / dialog overlays pueden contribuir 1500-1800 pixeles
/// matching el color filter. Si hits supera este umbral, NO es HP bar.
///
/// Típico:
///   - HP bar real (mob con HP full): 300-500 hits
///   - HP bar real (mob low HP): 30-100 hits
///   - Chat NPC + portrait en el ROI: 1000-2000 hits
const MAX_HP_BAR_PX: u32 = 700;
/// Threshold BAJO: conteo máximo de píxeles para considerar un slot
/// realmente vacío (mob muerto + entry retirada del panel). Un slot con
/// mob vivo mantiene el icono visible (~30-60 px cromáticos del portrait)
/// y supera ampliamente este valor. Un slot truly empty solo tiene noise
/// de compresión JPEG.
///
/// **Semántica sticky**: una vez detectado, el slot SOLO se libera cuando
/// `hits <= EMPTY_SLOT_PX`. No importa si el HP bar es corto — mientras
/// haya icono (mob vivo) hay hits>>20.
///
/// **Nota sobre el valor**: con el panel ROI nuevo (171x22 = 3762 px),
/// el noise JPEG del fondo gris puede dar 5-12 hits cromáticos espurios.
/// Subimos el threshold a 20 para dejar margen. Un slot vacío real tiene
/// <20 hits incluso con noise fuerte. Un mob vivo (icono 30+ px) supera
/// el threshold sin problema.
const EMPTY_SLOT_PX: u32 = 20;

/// Threshold de run horizontal CONTIGUO mínimo para clasificar un slot
/// como Monster. Una HP bar real tiene 30+ pixels consecutivos del mismo
/// color (verde/amarillo/rojo/azul según fase). Texto tiene gaps (<10
/// pixels consecutivos máximo por carácter).
///
/// Anti-false-positive 2026-04-18: el detector anterior usaba solo
/// `count_pixels` (total) que cuenta todos los pixeles colored en el
/// ROI, incluso discontinuos. Texto rojo del NPC chat sumaba hits
/// suficientes para pasar MIN_HP_BAR_PX=40 → false Monster detection
/// cada vez que había diálogo activo.
const MIN_HP_BAR_RUN: u32 = 30;
/// Sticky threshold para mantener slot como Monster: run ≥ este valor.
/// Más tolerante que MIN para absorber mobs con HP bar bajo (<30% HP
/// puede tener bar rendered short).
const STICKY_HP_BAR_RUN: u32 = 12;

/// Detector stateful del battle list.
///
/// Mantiene, por cada slot del panel, si el último frame fue clasificado
/// como Monster. Esto permite aplicar histéresis en `detect_by_hp_bar`:
/// una vez detectado, el threshold de confirmación baja para absorber
/// noise JPEG del NDI stream.
pub struct BattleListDetector {
    /// `slot_was_monster[row]` = `true` si el frame anterior clasificó ese
    /// row como Monster. Se inicializa todo `false` y se actualiza tras cada
    /// `read()`. Resize dinámico si el panel cambia de tamaño.
    slot_was_monster: Vec<bool>,
}

impl BattleListDetector {
    pub fn new() -> Self {
        Self { slot_was_monster: Vec::new() }
    }

    /// Analiza el ROI completo del panel de batalla y retorna la lista de
    /// entradas. Las entradas vacías (sin borde de color ni HP bar sticky)
    /// se descartan. `slot_debug` se rellena siempre para diagnóstico.
    ///
    /// Usa histéresis: si un slot fue Monster en el frame previo, el
    /// threshold para mantenerlo baja de `MIN_HP_BAR_PX` a `STICKY_HP_BAR_PX`.
    pub fn read(&mut self, frame: &Frame, panel_roi: RoiDef) -> BattleList {
        let mut entries    = Vec::new();
        let mut slot_debug = Vec::new();

        let n_rows = (panel_roi.h / ENTRY_HEIGHT_PX) as usize;
        // Asegurar tamaño del state per-slot.
        if self.slot_was_monster.len() != n_rows {
            self.slot_was_monster.resize(n_rows, false);
        }

        for row in 0..n_rows {
            let entry_y = panel_roi.y + row as u32 * ENTRY_HEIGHT_PX;

            // ROI del borde izquierdo de esta fila.
            let border_roi = Roi::new(panel_roi.x, entry_y, BORDER_WIDTH_PX, ENTRY_HEIGHT_PX);
            if !border_roi.fits_in(frame.width, frame.height) {
                break;
            }

            let (red_hits, blue_hits, yellow_hits) = count_border_hits(frame, border_roi);
            let was_monster = self.slot_was_monster[row];
            // Primero probamos el detector de borders clásicos. Si no mata,
            // calculamos el HP-bar hits (lo cacheamos para debug aunque
            // classify_hits ya haya dado resultado).
            let border_kind = classify_hits(red_hits, blue_hits, yellow_hits);
            let (hp_bar_hits, hp_bar_kind) = count_and_detect_hp_bar(
                frame, panel_roi, entry_y, was_monster,
            );
            let kind = border_kind.or(hp_bar_kind);

            // Detectar highlight de "atacando este slot".
            // Estrategia dual:
            //  (1) Clientes clásicos: borde rojo en el left edge → red_hits ≥ 2.
            //  (2) Tibia 12: cuadrado cyan/purple alrededor del icono del slot
            //      (medido live 2026-04-20 en Abdendriel). Escaneamos el área
            //      del icono (first ~25 cols) buscando pixels is_attack_highlight.
            let is_being_attacked = detect_is_being_attacked(
                frame, panel_roi, entry_y, red_hits,
            );

            // Actualizar state para el próximo frame.
            self.slot_was_monster[row] = matches!(kind, Some(EntryKind::Monster));

            slot_debug.push(SlotDebug {
                row: row as u8,
                frame_y: entry_y,
                red_hits,
                blue_hits,
                yellow_hits,
                hp_bar_hits,
                is_being_attacked,
                kind: kind.clone(),
            });

            if let Some(k) = kind {
                entries.push(BattleEntry {
                    kind: k,
                    row: row as u8,
                    hp_ratio: None, // `read_entry_hp` era dead code — eliminado
                    name: None,
                    is_being_attacked,
                });
            }
        }

        // enemy_count_filtered = None aquí (raw vision). Lo popula
        // PerceptionFilter aguas abajo en el game loop.
        BattleList { entries, slot_debug, enemy_count_filtered: None }
    }

    /// Reset del estado interno. Llamar si el panel se reconfigura o si
    /// hay un cambio brusco de escena (login, death).
    #[allow(dead_code)] // extension point
    pub fn reset(&mut self) {
        self.slot_was_monster.fill(false);
    }
}

impl Default for BattleListDetector {
    fn default() -> Self { Self::new() }
}

/// **DEPRECATED** — usar `BattleListDetector::read()` en su lugar.
/// Mantenido temporalmente por compatibilidad con tests antiguos.
/// Crea un detector stateless (sin histéresis), equivalente al comportamiento
/// pre-Fase 5.
#[cfg(test)]
pub fn read_battle_list(frame: &Frame, panel_roi: RoiDef) -> BattleList {
    let mut det = BattleListDetector::new();
    det.read(frame, panel_roi)
}

/// Cuenta hits de cada color en el borde. Retorna (red, blue, yellow).
fn count_border_hits(frame: &Frame, border: Roi) -> (u32, u32, u32) {
    let stride = frame.width as usize * 4;
    let mut red_count    = 0u32;
    let mut blue_count   = 0u32;
    let mut yellow_count = 0u32;

    for row in 0..border.h {
        for col in 0..border.w {
            let off = (border.y + row) as usize * stride + (border.x + col) as usize * 4;
            if off + 3 >= frame.data.len() { continue; }
            let px = &frame.data[off..off + 4];
            if is_monster_red(px)  { red_count    += 1; }
            if is_player_blue(px)  { blue_count   += 1; }
            if is_npc_yellow(px)   { yellow_count += 1; }
        }
    }

    (red_count, blue_count, yellow_count)
}

/// Clasifica el tipo de entrada según los conteos de color.
fn classify_hits(red: u32, blue: u32, yellow: u32) -> Option<EntryKind> {
    let max = red.max(blue).max(yellow);
    if max < MIN_BORDER_HITS {
        return None; // Entrada vacía o fondo.
    }
    if max == red {
        Some(EntryKind::Monster)
    } else if max == blue {
        Some(EntryKind::Player)
    } else {
        Some(EntryKind::Npc)
    }
}

/// Hits mínimos de "rojo dominante" en el borde del slot para considerarlo
/// atacado. Calibrado conservador: dos píxeles rojos en una columna vertical
/// de 22px es un signal muy específico porque el fondo del panel es gris.
const ATTACK_BORDER_MIN_HITS: u32 = 2;

/// Ancho del área a escanear para el highlight cyan/purple de Tibia 12.
/// El icono del mob ocupa ~20-25 px horizontales al inicio del slot.
const ICON_AREA_WIDTH_PX: u32 = 25;

/// Hits mínimos de is_attack_highlight en el área del icono para considerar
/// el slot como atacado (fuente Tibia 12). Calibrado live 2026-04-20:
/// un slot atacado produce 30-100+ pixels cyan/purple en el borde del icono.
/// Threshold 10 es conservador — rechaza icons-con-trazas-cyan-intrínsecas
/// (algunos mobs tienen sprite azul que no alcanza los 10 px).
const ATTACK_HIGHLIGHT_MIN_HITS: u32 = 10;

/// Detecta si un slot del battle list está siendo atacado activamente.
///
/// **Estrategia dual** (cliente-agnóstica):
/// 1. **Clientes clásicos**: borde rojo estático en left edge → `red_hits ≥ 2`.
/// 2. **Tibia 12**: cuadrado cyan/purple alrededor del icono cuando el target
///    es seleccionado via click izquierdo. Detectamos pixels
///    `is_attack_highlight` en el area del icono (first ICON_AREA_WIDTH_PX=25
///    columns del slot).
///
/// Cualquier de las dos fuentes que supere su threshold activa `true`.
///
/// **Evidencia live 2026-04-20** (sesión en Abdendriel wasps): el detector
/// legacy de `red_hits` devolvía 0 en todos los slots (0/2505 frames con
/// target_active en recording). Tibia 12 NO pinta borde rojo. El highlight
/// es un cuadrado de color (146,146,209)..(91,203,245) alrededor del icono.
fn detect_is_being_attacked(
    frame:      &Frame,
    panel_roi:  RoiDef,
    entry_y:    u32,
    red_hits:   u32,
) -> bool {
    // Fuente 1: cliente clásico (red border).
    if red_hits >= ATTACK_BORDER_MIN_HITS {
        return true;
    }
    // Fuente 2: Tibia 12 (cyan/purple highlight alrededor del icon).
    let icon_roi = Roi::new(
        panel_roi.x,
        entry_y,
        ICON_AREA_WIDTH_PX.min(panel_roi.w),
        ENTRY_HEIGHT_PX,
    );
    if !icon_roi.fits_in(frame.width, frame.height) {
        return false;
    }
    let hits = count_pixels(frame, icon_roi, is_attack_highlight);
    hits >= ATTACK_HIGHLIGHT_MIN_HITS
}

/// Detecta una entrada de battle list por presencia de píxeles "HP bar" verdes.
///
/// Clientes modernos de Tibia no muestran borders de color en el panel —
/// cada entry tiene icono + nombre + una barra HP verde. Escaneamos el
/// slot COMPLETO y contamos píxeles `is_bar_filled` (cualquier cromático
/// saturado: verde, amarillo, rojo, azul).
///
/// **Histéresis sticky-until-empty**:
/// - `was_monster_prev=false`: requiere `>= MIN_HP_BAR_PX` (40) hits.
///   Esto exige que el mob entre con HP bar clara (no solo icono + noise).
/// - `was_monster_prev=true`:  requiere solo `> EMPTY_SLOT_PX` (5) hits.
///   Mantiene el mob mientras haya CUALQUIER actividad en el slot. Solo
///   lo libera cuando el slot queda realmente vacío (mob muerto + entry
///   retirada del panel por Tibia).
///
/// **Por qué sticky-until-empty**: la HP bar de un mob a 10% HP tiene solo
/// ~8 px coloreados (roja). Un threshold proporcional perdería la detección
/// y el FSM entraría en Idle dejando al mob vivo. Como el slot sigue
/// mostrando el icono del mob mientras esté en la lista, contamos cualquier
/// actividad cromática como "slot ocupado".
/// Variante que retorna `(hits, kind)` para alimentar `SlotDebug.hp_bar_hits`.
/// El cálculo del count ocurre una sola vez aunque se descarte el resultado.
fn count_and_detect_hp_bar(
    frame:            &Frame,
    panel_roi:        RoiDef,
    entry_y:          u32,
    was_monster_prev: bool,
) -> (u32, Option<EntryKind>) {
    let scan_roi = Roi::new(panel_roi.x, entry_y, panel_roi.w, ENTRY_HEIGHT_PX);
    if !scan_roi.fits_in(frame.width, frame.height) {
        return (0, None);
    }
    let hits = count_pixels(frame, scan_roi, is_bar_filled);
    // 2026-04-18: requerir run horizontal contiguo además de total count.
    // Texto del NPC chat tiene MUCHOS pixeles colored pero DISCONTINUOS
    // (gaps entre letras). HP bar real es strip continuo ≥30 pixeles.
    let longest_run = longest_horizontal_run(frame, scan_roi, is_bar_filled);
    let detected = if was_monster_prev {
        // Sticky: mantener slot si pasa EITHER threshold (tolera mobs de
        // bajo HP con bar short, pero también exige evidencia de run).
        // MAX bound: si hits > MAX_HP_BAR_PX, es NPC dialog/portrait, NO mob.
        hits > EMPTY_SLOT_PX && hits <= MAX_HP_BAR_PX && longest_run >= STICKY_HP_BAR_RUN
    } else {
        // Inicial: hits + run AMBOS deben pasar. Texto falla en run.
        // MAX bound: slots con 1000+ hits son false positives (NPC chat / portrait).
        hits >= MIN_HP_BAR_PX && hits <= MAX_HP_BAR_PX && longest_run >= MIN_HP_BAR_RUN
    };
    let kind = if detected { Some(EntryKind::Monster) } else { None };
    (hits, kind)
}

// NOTA: la antigua función `read_entry_hp` fue eliminada (Fase 5 audit).
// Buscaba la HP bar en una zona fija (`y+17, bar_w=40, bar_h=4`) que no
// coincide con el layout del cliente moderno — siempre retornaba ratio=0.
// El `BattleEntry.hp_ratio` ahora se deja como `None`. Si en el futuro
// necesitamos el ratio real, se reimplementará detectando dinámicamente
// la posición de la HP bar dentro del slot.

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn make_empty_frame(w: u32, h: u32) -> Frame {
        Frame {
            width:       w,
            height:      h,
            data:        vec![50u8; (w * h * 4) as usize], // gris neutro
            captured_at: Instant::now(),
        }
    }

    fn paint_border(frame: &mut Frame, x: u32, y: u32, h: u32, color: (u8, u8, u8)) {
        let stride = frame.width as usize * 4;
        for row in 0..h {
            for col in 0..BORDER_WIDTH_PX {
                let off = (y + row) as usize * stride + (x + col) as usize * 4;
                // RGBA: color tuple is (R, G, B)
                frame.data[off]     = color.0; // R
                frame.data[off + 1] = color.1; // G
                frame.data[off + 2] = color.2; // B
                frame.data[off + 3] = 255;
            }
        }
    }

    #[test]
    fn empty_panel_no_entries() {
        let frame = make_empty_frame(400, 300);
        let roi   = RoiDef::new(300, 100, 80, 88);
        let bl    = read_battle_list(&frame, roi);
        assert!(bl.is_empty());
    }

    #[test]
    fn monster_red_border_detected() {
        let mut frame = make_empty_frame(500, 200);
        let panel_x = 400u32;
        let panel_y = 10u32;
        // Pintar borde rojo en la primera fila
        // R=220, G=20, B=20 → rojo de monstruo
        paint_border(&mut frame, panel_x, panel_y, ENTRY_HEIGHT_PX, (220, 20, 20));

        let roi = RoiDef::new(panel_x, panel_y, 80, ENTRY_HEIGHT_PX * 2);
        let bl  = read_battle_list(&frame, roi);

        assert_eq!(bl.entries.len(), 1);
        assert_eq!(bl.entries[0].kind, EntryKind::Monster);
        assert_eq!(bl.entries[0].row, 0);
    }

    #[test]
    fn player_blue_border_detected() {
        let mut frame = make_empty_frame(500, 200);
        let panel_x = 400u32;
        let panel_y = 10u32;
        // Azul de jugador: R=20, G=20, B=200
        paint_border(&mut frame, panel_x, panel_y, ENTRY_HEIGHT_PX, (20, 20, 200));

        let roi = RoiDef::new(panel_x, panel_y, 80, ENTRY_HEIGHT_PX * 2);
        let bl  = read_battle_list(&frame, roi);

        assert_eq!(bl.entries.len(), 1);
        assert_eq!(bl.entries[0].kind, EntryKind::Player);
        assert!(bl.has_player());
        assert!(!bl.has_enemies());
    }

    /// Pinta una barra horizontal verde saturada en el slot dado.
    /// `pixel_count` controla cuántos píxeles verdes se dibujan — simula
    /// una HP bar al X% de su ancho.
    fn paint_hp_bar_in_slot(frame: &mut Frame, panel_x: u32, entry_y: u32, pixel_count: u32) {
        let stride = frame.width as usize * 4;
        let bar_y = entry_y + 10; // mitad del slot
        for i in 0..pixel_count {
            let x = panel_x + 5 + i;
            let off = bar_y as usize * stride + x as usize * 4;
            if off + 3 >= frame.data.len() { break; }
            // Verde saturado que is_bar_filled reconoce.
            frame.data[off]     = 0x20; // R
            frame.data[off + 1] = 0xD8; // G
            frame.data[off + 2] = 0x20; // B
            frame.data[off + 3] = 0xFF;
        }
    }

    #[test]
    fn hysteresis_sticky_detection_absorbs_noise() {
        // Un slot con 45 píxeles verdes (por encima del threshold 40) se
        // detecta como Monster. Frames posteriores con menos píxeles (mob
        // perdiendo HP pero icon aún visible) deben mantener la detección
        // hasta que el slot quede realmente vacío.
        //
        // NOTA: en realidad el ICONO del mob contribuye ~30-50 px cromáticos
        // siempre, no solo la HP bar. Así que incluso un mob al 1% HP tiene
        // icono + 1 px de bar rojo = 30+ hits. Los tests usan "paint_hp_bar"
        // que solo pinta la HP bar (sin simular icono), por eso los números
        // son diferentes al caso real.
        let mut det = BattleListDetector::new();
        let panel_x = 300u32;
        let panel_y = 50u32;
        let roi = RoiDef::new(panel_x, panel_y, 157, ENTRY_HEIGHT_PX);

        // Frame 1: HP bar saludable (45 píxeles, entra al detector).
        let mut frame_hi = make_empty_frame(500, 200);
        paint_hp_bar_in_slot(&mut frame_hi, panel_x, panel_y, 45);
        let bl1 = det.read(&frame_hi, roi);
        assert_eq!(bl1.entries.len(), 1, "frame alto: debe detectar 1 Monster");

        // Frame 2: mob con HP bar de 30 px (simula HP intermedio + icono).
        // Sticky-until-empty → sigue detectado porque 30 > EMPTY_SLOT_PX (20).
        let mut frame_mid = make_empty_frame(500, 200);
        paint_hp_bar_in_slot(&mut frame_mid, panel_x, panel_y, 30);
        let bl2 = det.read(&frame_mid, roi);
        assert_eq!(bl2.entries.len(), 1,
            "frame medio: sticky debe mantener Monster detectado");

        // Frame 3: simulación de slot con activity residual (22 px).
        // Debe mantenerse porque 22 > 20.
        let mut frame_lo = make_empty_frame(500, 200);
        paint_hp_bar_in_slot(&mut frame_lo, panel_x, panel_y, 22);
        let bl3 = det.read(&frame_lo, roi);
        assert_eq!(bl3.entries.len(), 1,
            "frame bajo: 22 px > 20 debe seguir detectado");

        // Frame 4: slot realmente vacío (noise JPEG residual de 10 px).
        // Ahora sí se libera porque 10 < 20.
        let mut frame_empty = make_empty_frame(500, 200);
        paint_hp_bar_in_slot(&mut frame_empty, panel_x, panel_y, 10);
        let bl4 = det.read(&frame_empty, roi);
        assert_eq!(bl4.entries.len(), 0,
            "frame vacío (10 px ≤ EMPTY_SLOT_PX=20): debe liberar la detección");
    }

    #[test]
    fn sticky_releases_only_when_slot_truly_empty() {
        // Verifica el límite exacto entre "mob vivo con HP muy bajo" y
        // "slot vacío". EMPTY_SLOT_PX=20 es la frontera (margen para
        // noise JPEG en ROIs grandes).
        let mut det = BattleListDetector::new();
        let panel_x = 300u32;
        let panel_y = 50u32;
        let roi = RoiDef::new(panel_x, panel_y, 157, ENTRY_HEIGHT_PX);

        // Entrar en detección con HP bar clara.
        let mut frame_entry = make_empty_frame(500, 200);
        paint_hp_bar_in_slot(&mut frame_entry, panel_x, panel_y, 50);
        let _ = det.read(&frame_entry, roi);

        // 21 px > EMPTY_SLOT_PX (20) → sigue detectado (mob con HP bajo).
        let mut frame_21 = make_empty_frame(500, 200);
        paint_hp_bar_in_slot(&mut frame_21, panel_x, panel_y, 21);
        let bl = det.read(&frame_21, roi);
        assert_eq!(bl.entries.len(), 1, "21 px > 20 debe mantener");

        // 20 px <= EMPTY_SLOT_PX → libera.
        let mut frame_20 = make_empty_frame(500, 200);
        paint_hp_bar_in_slot(&mut frame_20, panel_x, panel_y, 20);
        let bl = det.read(&frame_20, roi);
        assert_eq!(bl.entries.len(), 0, "20 px <= 20 debe liberar");
    }

    #[test]
    fn hysteresis_new_detection_requires_high_threshold() {
        // Sin state previo (was_monster_prev=false), el detector requiere
        // MIN_HP_BAR_PX=40 hits para activar. Un slot con 30 píxeles NO
        // debe detectarse como Monster si nunca fue detectado antes.
        let mut det = BattleListDetector::new();
        let panel_x = 300u32;
        let panel_y = 50u32;
        let roi = RoiDef::new(panel_x, panel_y, 157, ENTRY_HEIGHT_PX);

        let mut frame = make_empty_frame(500, 200);
        paint_hp_bar_in_slot(&mut frame, panel_x, panel_y, 30); // por debajo de 40
        let bl = det.read(&frame, roi);
        assert_eq!(bl.entries.len(), 0,
            "30 px < MIN_HP_BAR_PX (40) debe rechazar si no había state previo");
    }

    #[test]
    fn battle_list_helpers() {
        let entries = vec![
            BattleEntry { kind: EntryKind::Monster, row: 0, hp_ratio: Some(0.8), name: None, is_being_attacked: false },
            BattleEntry { kind: EntryKind::Monster, row: 1, hp_ratio: Some(0.3), name: None, is_being_attacked: false },
            BattleEntry { kind: EntryKind::Player,  row: 2, hp_ratio: None,      name: None, is_being_attacked: false },
        ];
        let bl = BattleList { entries, slot_debug: vec![], enemy_count_filtered: None };
        assert_eq!(bl.enemy_count(), 2);
        assert!(bl.has_player());
        assert!(bl.has_enemies());
        assert!(!bl.is_empty());
    }

    // ── Attack detection (post-TibiaPilotNG audit) ────────────────────

    /// Helper: pinta un cuadrado de highlight cyan/purple en el icon area del slot.
    /// Simula el highlight de Tibia 12 para "target seleccionado".
    fn paint_cyan_highlight(frame: &mut Frame, panel_x: u32, entry_y: u32, pixel_count: u32) {
        let stride = frame.width as usize * 4;
        // Pintar pixels (91, 203, 245) — muestra empírica sesión 2026-04-20.
        // Distribuir dentro del icon area (primeros 20 cols del slot, toda la altura).
        let mut painted = 0u32;
        'outer: for row in 0..ENTRY_HEIGHT_PX {
            for col in 0..20u32 {
                if painted >= pixel_count { break 'outer; }
                let off = (entry_y + row) as usize * stride + (panel_x + col) as usize * 4;
                if off + 3 >= frame.data.len() { continue; }
                frame.data[off]     = 91;
                frame.data[off + 1] = 203;
                frame.data[off + 2] = 245;
                frame.data[off + 3] = 255;
                painted += 1;
            }
        }
    }

    #[test]
    fn detect_is_being_attacked_red_border_classic() {
        // Fuente 1: borde rojo clásico (red_hits ≥ 2).
        let frame = make_empty_frame(500, 200);
        let roi   = RoiDef::new(400, 10, 80, ENTRY_HEIGHT_PX);
        // Con red_hits=0 pero sin cyan, debe ser false.
        assert!(!detect_is_being_attacked(&frame, roi, 10, 0));
        assert!(!detect_is_being_attacked(&frame, roi, 10, 1));
        // Con red_hits ≥ 2 (threshold clásico), pasa sin necesitar cyan.
        assert!(detect_is_being_attacked(&frame, roi, 10, 2));
        assert!(detect_is_being_attacked(&frame, roi, 10, 50));
    }

    #[test]
    fn detect_is_being_attacked_cyan_highlight_tibia12() {
        // Fuente 2: highlight cyan/purple alrededor del icono (Tibia 12).
        let mut frame = make_empty_frame(500, 200);
        let panel_x   = 400u32;
        let entry_y   = 10u32;
        // Pintar 15 pixels cyan en el icon area (> threshold 10).
        paint_cyan_highlight(&mut frame, panel_x, entry_y, 15);
        let roi = RoiDef::new(panel_x, entry_y, 80, ENTRY_HEIGHT_PX);
        // Sin red_hits, la fuente 2 debe activar.
        assert!(detect_is_being_attacked(&frame, roi, entry_y, 0));
    }

    #[test]
    fn detect_is_being_attacked_cyan_below_threshold() {
        // Muy pocos pixels cyan (< threshold 10) → no dispara.
        let mut frame = make_empty_frame(500, 200);
        paint_cyan_highlight(&mut frame, 400, 10, 5);
        let roi = RoiDef::new(400, 10, 80, ENTRY_HEIGHT_PX);
        assert!(!detect_is_being_attacked(&frame, roi, 10, 0));
    }

    #[test]
    fn slot_with_red_border_marked_attacked() {
        // Frame con un mob con borde rojo (slot 0, estilo "attack highlight").
        // Comparte el MISMO path que detect_by_border_color pero con el flag
        // is_being_attacked propagado a la entry y al slot_debug.
        let mut det = BattleListDetector::new();
        let panel_x = 300u32;
        let panel_y = 50u32;
        let roi = RoiDef::new(panel_x, panel_y, 80, ENTRY_HEIGHT_PX * 2);
        let mut frame = make_empty_frame(500, 200);
        // Pintar borde rojo dominante en slot 0 (row=0).
        paint_border(&mut frame, panel_x, panel_y, ENTRY_HEIGHT_PX, (220, 20, 20));

        let bl = det.read(&frame, roi);
        assert_eq!(bl.entries.len(), 1);
        assert!(bl.entries[0].is_being_attacked,
            "slot con borde rojo debe estar marcado is_being_attacked=true");
        assert!(bl.has_attacked_entry());
    }

    #[test]
    fn slot_without_red_border_not_attacked() {
        // Frame con un slot detectado solo por HP bar (sin borde rojo).
        // is_being_attacked debe ser false porque red_hits=0.
        let mut det = BattleListDetector::new();
        let panel_x = 300u32;
        let panel_y = 50u32;
        let roi = RoiDef::new(panel_x, panel_y, 157, ENTRY_HEIGHT_PX);
        let mut frame = make_empty_frame(500, 200);
        // Pintar HP bar (sin bordes colored) — detección por fallback.
        paint_hp_bar_in_slot(&mut frame, panel_x, panel_y, 50);

        let bl = det.read(&frame, roi);
        assert_eq!(bl.entries.len(), 1);
        assert!(!bl.entries[0].is_being_attacked,
            "slot sin borde rojo NO debe estar marcado is_being_attacked");
        assert!(!bl.has_attacked_entry());
    }

    #[test]
    fn has_attacked_entry_false_when_all_slots_idle() {
        let entries = vec![
            BattleEntry { kind: EntryKind::Monster, row: 0, hp_ratio: None, name: None, is_being_attacked: false },
            BattleEntry { kind: EntryKind::Monster, row: 1, hp_ratio: None, name: None, is_being_attacked: false },
        ];
        let bl = BattleList { entries, slot_debug: vec![], enemy_count_filtered: None };
        assert!(!bl.has_attacked_entry());
    }

    #[test]
    fn has_attacked_entry_true_when_any_slot_attacked() {
        let entries = vec![
            BattleEntry { kind: EntryKind::Monster, row: 0, hp_ratio: None, name: None, is_being_attacked: false },
            BattleEntry { kind: EntryKind::Monster, row: 1, hp_ratio: None, name: None, is_being_attacked: true  },
            BattleEntry { kind: EntryKind::Monster, row: 2, hp_ratio: None, name: None, is_being_attacked: false },
        ];
        let bl = BattleList { entries, slot_debug: vec![], enemy_count_filtered: None };
        assert!(bl.has_attacked_entry());
    }

    /// Pinta un slot con simulación de TEXTO ROJO del NPC chat
    /// (caracteres de rojo con gaps entre ellos, ~60 pixeles total
    /// pero ningún run > 6 pixeles contiguos). Este era el false-positive
    /// case del 2026-04-18 live run.
    fn paint_red_text_in_slot(
        frame: &mut Frame, panel_x: u32, panel_y: u32,
    ) {
        let stride = frame.width as usize * 4;
        // Simular 7 "caracteres" de 5 px cada uno con 3 px gap entre letras.
        // 7 × (5 + 3) = 56 pixels en una fila, total ~80 px de ancho.
        // Cada char son ~5px contiguos → longest_run = 5 (bien bajo de 30 threshold).
        for y_off in 8..=10u32 { // 3 filas de altura (como fuente Tibia)
            let y = panel_y + y_off;
            for char_idx in 0..7u32 {
                let char_start = panel_x + 2 + char_idx * 8;
                for pix in 0..5u32 {
                    let x = char_start + pix;
                    if x >= frame.width || y >= frame.height { continue; }
                    let off = y as usize * stride + x as usize * 4;
                    if off + 3 < frame.data.len() {
                        frame.data[off]     = 200; // R — rojo oscuro (NPC red)
                        frame.data[off + 1] = 40;  // G
                        frame.data[off + 2] = 40;  // B
                        frame.data[off + 3] = 255;
                    }
                }
            }
        }
    }

    #[test]
    fn hp_bar_detector_rejects_npc_red_text_as_false_positive() {
        // Regression test del bug live 2026-04-18: NPC chat "Aelzerand
        // Neeymas: hi" era detectado como 13 Monster entries porque
        // cada línea de texto rojo tenía 60+ pixeles total colored.
        // Con el fix (longest_horizontal_run ≥ 30), texto no pasa.
        let mut frame = make_empty_frame(200, 100);
        let roi = RoiDef { x: 0, y: 0, w: 180, h: 88 };
        paint_red_text_in_slot(&mut frame, 0, 0);

        let mut det = BattleListDetector::new();
        let bl = det.read(&frame, roi);

        // Con texto rojo scattered: cada línea tiene longest_run=5 < 30
        // threshold → NO debe detectarse como Monster.
        let monsters: Vec<_> = bl.entries.iter()
            .filter(|e| matches!(e.kind, EntryKind::Monster))
            .collect();
        assert_eq!(
            monsters.len(), 0,
            "texto rojo del NPC chat NO debe ser detectado como Monster. \
             debug: {:?}",
            bl.slot_debug.iter()
                .filter(|d| d.kind.is_some())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn hp_bar_detector_accepts_contiguous_bar() {
        // Sanity check: barra contigua de 50+ pixeles SÍ debe detectarse.
        let mut frame = make_empty_frame(200, 100);
        let roi = RoiDef { x: 0, y: 0, w: 180, h: 44 };
        // paint_hp_bar_in_slot ya pinta una barra contigua (50 px).
        paint_hp_bar_in_slot(&mut frame, 0, 0, 50);

        let mut det = BattleListDetector::new();
        let bl = det.read(&frame, roi);
        let monsters: Vec<_> = bl.entries.iter()
            .filter(|e| matches!(e.kind, EntryKind::Monster))
            .collect();
        assert_eq!(monsters.len(), 1, "HP bar contigua 50px debe detectarse como Monster");
    }
}
