/// sendinput.rs — Inyección de input via Windows SendInput API.
///
/// Reemplaza la cadena serial→Pico2→USB HID por llamadas directas a
/// SendInput(). El bot envía los mismos comandos TCP (KEY_TAP, MOUSE_MOVE,
/// MOUSE_CLICK) y este módulo los ejecuta localmente.
///
/// Scan codes: SendInput usa Windows scan codes, no HID keycodes.
/// La tabla hid_to_scan() convierte entre ambos.

#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_TYPE,
    KEYBDINPUT, MOUSEINPUT,
    KEYEVENTF_SCANCODE, KEYEVENTF_KEYUP, KEYEVENTF_EXTENDEDKEY,
    MOUSE_EVENT_FLAGS, MOUSEEVENTF_MOVE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
};

use std::sync::Mutex;
use tracing::{debug, warn};

/// Última posición absoluta (0-65535, rango SendInput) del cursor enviada por
/// `MOUSE_MOVE`. Se usa para que `MOUSE_CLICK` posterior empaquete en el mismo
/// evento SendInput los flags `MOVE|ABSOLUTE|VIRTUALDESK|BUTTON_DOWN` con esta
/// posición, garantizando que el botón se dispara EXACTAMENTE donde queríamos.
///
/// Bug fix 2026-04-16 (V7 blocker): sin esto, MOUSE_MOVE y MOUSE_CLICK viajan
/// como 2 comandos TCP separados. Entre ambos el cursor puede derivar (mouse
/// físico en otro monitor, input ajeno en la cola de Windows) o Tibia puede
/// no haber actualizado su hit-test interno. En el viewport el margen tolera
/// el drift pero los slots del sidebar (32×32 px) NO — el right-click cae en
/// el borde y no produce context menu. Atomic MOVE+BUTTON en un solo SendInput
/// batch elimina ambos problemas.
static LAST_MOVE_POS: Mutex<Option<(i32, i32)>> = Mutex::new(None);

/// Resultado de un comando ejecutado.
pub struct SendInputResult {
    pub ok: bool,
}

/// Ejecuta un comando del protocolo del bridge localmente via SendInput.
/// Retorna "OK" si se ejecutó, "ERR" si no se reconoce.
pub fn execute_command(line: &str) -> SendInputResult {
    let line = line.trim();

    if line.starts_with("KEY_TAP ") {
        let code_str = line.strip_prefix("KEY_TAP ").unwrap().trim();
        let hidcode = parse_hex(code_str);
        if let Some(hid) = hidcode {
            key_tap(hid);
            return SendInputResult { ok: true };
        }
        warn!("SendInput: KEY_TAP código inválido: {}", code_str);
        return SendInputResult { ok: false };
    }

    if line.starts_with("MOUSE_MOVE ") {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() == 3 {
            if let (Ok(hx), Ok(hy)) = (parts[1].parse::<i32>(), parts[2].parse::<i32>()) {
                // HID usa 0-32767, SendInput usa 0-65535
                let sx = (hx as i64 * 65535 / 32767).min(65535) as i32;
                let sy = (hy as i64 * 65535 / 32767).min(65535) as i32;
                mouse_move_abs(sx, sy);
                return SendInputResult { ok: true };
            }
        }
        warn!("SendInput: MOUSE_MOVE formato inválido: {}", line);
        return SendInputResult { ok: false };
    }

    if line.starts_with("MOUSE_CLICK") {
        let button = line.strip_prefix("MOUSE_CLICK").unwrap().trim();
        mouse_click(button);
        return SendInputResult { ok: true };
    }

    if line == "RESET" {
        // Noop — no hay teclas "held" con tap-based input
        debug!("SendInput: RESET (noop)");
        return SendInputResult { ok: true };
    }

    warn!("SendInput: comando no reconocido: {}", line);
    SendInputResult { ok: false }
}

fn parse_hex(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u8>().ok()
    }
}

/// Tap de una tecla: key down + key up.
#[cfg(windows)]
fn key_tap(hidcode: u8) {
    let (scan, extended) = hid_to_scan(hidcode);
    if scan == 0 {
        warn!("SendInput: HID 0x{:02X} sin scan code conocido", hidcode);
        return;
    }

    let mut flags_down = KEYEVENTF_SCANCODE;
    let mut flags_up = KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP;
    if extended {
        flags_down |= KEYEVENTF_EXTENDEDKEY;
        flags_up |= KEYEVENTF_EXTENDEDKEY;
    }

    let inputs = [
        make_key_input(scan, flags_down),
        make_key_input(scan, flags_up),
    ];

    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32); }
    debug!("SendInput: KEY_TAP hid=0x{:02X} scan=0x{:02X} ext={}", hidcode, scan, extended);
}

#[cfg(windows)]
fn make_key_input(scan: u16, flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_TYPE(1), // INPUT_KEYBOARD
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Mover el mouse a posición absoluta (0-65535) en el **virtual desktop**.
///
/// CRÍTICO: usa MOUSEEVENTF_VIRTUALDESK para que las coords 0..65535
/// mapeen al virtual desktop COMPLETO (todos los monitores), NO solo al
/// primario. Sin este flag, Windows interpreta las coords en el monitor
/// primary, causando que clicks caigan al monitor equivocado en setups
/// multi-monitor.
///
/// Bug fix 2026-04-16: la sesión live descubrió que sin VIRTUALDESK los
/// clicks con coords "correctas" (virtual X=2872) caían en monitor Claude
/// porque Windows los interpretaba como relativos al primary solamente.
///
/// Además cachea la posición en `LAST_MOVE_POS` para que `MOUSE_CLICK`
/// posterior pueda hacer atomic MOVE+BUTTON en el mismo SendInput batch.
#[cfg(windows)]
fn mouse_move_abs(x: i32, y: i32) {
    let input = INPUT {
        r#type: INPUT_TYPE(0), // INPUT_MOUSE
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32); }
    // Cachear para que el próximo MOUSE_CLICK use atomic MOVE+BUTTON.
    if let Ok(mut pos) = LAST_MOVE_POS.lock() {
        *pos = Some((x, y));
    }
    debug!("SendInput: MOUSE_MOVE ({}, {})", x, y);
}

/// Click de un botón del mouse.
///
/// Si hay una posición cacheada por un `MOUSE_MOVE` previo, empaqueta en cada
/// evento SendInput los flags `MOVE|ABSOLUTE|VIRTUALDESK|BUTTON_*` con esa
/// posición. Esto garantiza que el botón se dispara EXACTAMENTE donde
/// queríamos, sin importar si otro input ajeno movió el cursor entre el
/// MOUSE_MOVE y el MOUSE_CLICK.
///
/// Si no hay posición cacheada (ej. cliente mandó MOUSE_CLICK sin MOVE
/// previo), cae al comportamiento legacy: down/up sin posición en el
/// evento, usando cursor position actual.
///
/// Timing:
///   - Settling pre-DOWN: 25ms (permite a Tibia procesar el WM_MOUSEMOVE
///     que dispara MOVE|... antes del WM_*BUTTONDOWN, actualizando hit-test).
///   - Hold DOWN→UP: 80ms (antes 45ms; era corto para que Tibia registre
///     el right-click como context-menu en widgets pequeños del sidebar).
///
/// Fix 2026-04-16 (V7 blocker): antes MOVE y CLICK viajaban en eventos
/// separados y drift del cursor entre ambos rompía clicks en slots de 32×32.
#[cfg(windows)]
fn mouse_click(button: &str) {
    let (down_flag, up_flag) = match button.to_uppercase().as_str() {
        "L" | "LEFT"   => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        "R" | "RIGHT"  => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        "M" | "MIDDLE" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        _ => {
            warn!("SendInput: botón de mouse desconocido: {}", button);
            return;
        }
    };

    let pos = LAST_MOVE_POS.lock().ok().and_then(|p| *p);
    let pos_flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;

    // Pre-settle: SOLO en flujo legacy (sin atomic). En legacy, el MOUSE_MOVE
    // previo fue un evento separado y el renderer del cliente puede aún no
    // haber procesado el WM_MOUSEMOVE cuando llega el WM_*BUTTONDOWN. 25ms da
    // 1-2 frames al client para actualizar hit-test.
    //
    // En flujo atómico la posición va empaquetada en el mismo evento que el
    // BUTTON_DOWN, así que Tibia procesa WM_MOUSEMOVE → hit-test update →
    // WM_*BUTTONDOWN en orden en un solo frame. Pre-settle innecesario y, más
    // crítico, rompía el timeout de 100ms del bot: 25 + 80 = 105ms > timeout.
    let pre_settle_ms = if pos.is_some() { 0 } else { 25 };
    if pre_settle_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(pre_settle_ms));
    }

    let (down, up) = match pos {
        Some((x, y)) => (
            [make_mouse_input_pos(x, y, pos_flags | down_flag)],
            [make_mouse_input_pos(x, y, pos_flags | up_flag)],
        ),
        None => (
            [make_mouse_input(down_flag)],
            [make_mouse_input(up_flag)],
        ),
    };

    unsafe { SendInput(&down, std::mem::size_of::<INPUT>() as i32); }
    std::thread::sleep(std::time::Duration::from_millis(80));
    unsafe { SendInput(&up, std::mem::size_of::<INPUT>() as i32); }
    match pos {
        Some((x, y)) => debug!(
            "SendInput: MOUSE_CLICK {} atomic @ ({},{}) (0ms pre, 80ms hold, total ~80ms)",
            button, x, y
        ),
        None => debug!(
            "SendInput: MOUSE_CLICK {} legacy (sin MOVE previo, cursor actual)",
            button
        ),
    }
}

#[cfg(windows)]
fn make_mouse_input(flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_TYPE(0), // INPUT_MOUSE
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Crea un evento MOUSEINPUT con posición absoluta + flags (útil para atomic
/// MOVE+BUTTON donde queremos que la posición se reafirme en el mismo evento
/// que el BUTTON_DOWN/UP).
#[cfg(windows)]
fn make_mouse_input_pos(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_TYPE(0), // INPUT_MOUSE
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Convierte HID keycode a (Windows scan code, is_extended).
/// Extended keys usan el prefijo E0 en el protocolo de teclado.
fn hid_to_scan(hid: u8) -> (u16, bool) {
    match hid {
        // Letters A-Z
        0x04 => (0x1E, false), // A
        0x05 => (0x30, false), // B
        0x06 => (0x2E, false), // C
        0x07 => (0x20, false), // D
        0x08 => (0x12, false), // E
        0x09 => (0x21, false), // F
        0x0A => (0x22, false), // G
        0x0B => (0x23, false), // H
        0x0C => (0x17, false), // I
        0x0D => (0x24, false), // J
        0x0E => (0x25, false), // K
        0x0F => (0x26, false), // L
        0x10 => (0x32, false), // M
        0x11 => (0x31, false), // N
        0x12 => (0x18, false), // O
        0x13 => (0x19, false), // P
        0x14 => (0x10, false), // Q
        0x15 => (0x13, false), // R
        0x16 => (0x1F, false), // S
        0x17 => (0x14, false), // T
        0x18 => (0x16, false), // U
        0x19 => (0x2F, false), // V
        0x1A => (0x11, false), // W
        0x1B => (0x2D, false), // X
        0x1C => (0x15, false), // Y
        0x1D => (0x2C, false), // Z
        // Digits 1-9, 0
        0x1E => (0x02, false), // 1
        0x1F => (0x03, false), // 2
        0x20 => (0x04, false), // 3
        0x21 => (0x05, false), // 4
        0x22 => (0x06, false), // 5
        0x23 => (0x07, false), // 6
        0x24 => (0x08, false), // 7
        0x25 => (0x09, false), // 8
        0x26 => (0x0A, false), // 9
        0x27 => (0x0B, false), // 0
        // Control keys
        0x28 => (0x1C, false), // Enter
        0x29 => (0x01, false), // Escape
        0x2A => (0x0E, false), // Backspace
        0x2B => (0x0F, false), // Tab
        0x2C => (0x39, false), // Space
        // Punctuation
        0x2D => (0x0C, false), // - _
        0x2E => (0x0D, false), // = +
        0x2F => (0x1A, false), // [ {
        0x30 => (0x1B, false), // ] }
        0x33 => (0x27, false), // ; :
        0x34 => (0x28, false), // ' "
        0x35 => (0x29, false), // ` ~
        0x36 => (0x33, false), // , <
        0x37 => (0x34, false), // . >
        0x38 => (0x35, false), // / ?
        // Function keys F1-F12
        0x3A => (0x3B, false), // F1
        0x3B => (0x3C, false), // F2
        0x3C => (0x3D, false), // F3
        0x3D => (0x3E, false), // F4
        0x3E => (0x3F, false), // F5
        0x3F => (0x40, false), // F6
        0x40 => (0x41, false), // F7
        0x41 => (0x42, false), // F8
        0x42 => (0x43, false), // F9
        0x43 => (0x44, false), // F10
        0x44 => (0x57, false), // F11
        0x45 => (0x58, false), // F12
        // Navigation (extended keys)
        0x49 => (0x52, true),  // Insert
        0x4A => (0x47, true),  // Home
        0x4B => (0x49, true),  // Page Up
        0x4C => (0x53, true),  // Delete
        0x4D => (0x4F, true),  // End
        0x4E => (0x51, true),  // Page Down
        // Arrow keys (extended)
        0x4F => (0x4D, true),  // Right
        0x50 => (0x4B, true),  // Left
        0x51 => (0x50, true),  // Down
        0x52 => (0x48, true),  // Up
        // Default: unknown
        _ => (0, false),
    }
}

// ── Stubs para non-windows (compilación cruzada) ────────────────────────────

#[cfg(not(windows))]
fn key_tap(_hidcode: u8) {
    warn!("SendInput: no disponible fuera de Windows");
}

#[cfg(not(windows))]
fn mouse_move_abs(_x: i32, _y: i32) {
    warn!("SendInput: no disponible fuera de Windows");
}

#[cfg(not(windows))]
fn mouse_click(_button: &str) {
    warn!("SendInput: no disponible fuera de Windows");
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_0x_prefix() {
        assert_eq!(parse_hex("0x3A"), Some(0x3A));
        assert_eq!(parse_hex("0X3a"), Some(0x3A));
    }

    #[test]
    fn parse_hex_accepts_decimal() {
        assert_eq!(parse_hex("58"), Some(58));
        assert_eq!(parse_hex("255"), Some(255));
    }

    #[test]
    fn parse_hex_rejects_garbage() {
        assert_eq!(parse_hex("xyz"), None);
        assert_eq!(parse_hex(""), None);
    }

    /// Regresión V7: verifica que un MOUSE_MOVE seguido de MOUSE_CLICK
    /// deja la posición cacheada en LAST_MOVE_POS para el click atómico.
    /// Sin esto, el right-click en sidebar slots (32×32) no producía
    /// context menu por drift del cursor entre MOVE y CLICK.
    #[cfg(windows)]
    #[test]
    #[ignore = "mueve el cursor real del usuario — correr manualmente con --ignored"]
    fn mouse_move_abs_updates_last_pos_cache() {
        // Reset
        *LAST_MOVE_POS.lock().unwrap() = None;
        // Call
        mouse_move_abs(12345, 6789);
        // Verify
        let cached = LAST_MOVE_POS.lock().unwrap();
        assert_eq!(*cached, Some((12345, 6789)));
    }

    /// Verifica que execute_command rechaza comandos malformados sin panic.
    #[test]
    fn execute_command_rejects_malformed() {
        assert!(!execute_command("MOUSE_MOVE bad").ok);
        assert!(!execute_command("KEY_TAP xyz").ok);
        assert!(!execute_command("UNKNOWN_CMD").ok);
    }

    /// RESET es noop (no hay teclas held en SendInput mode).
    #[test]
    fn execute_command_reset_is_ok() {
        assert!(execute_command("RESET").ok);
    }
}
