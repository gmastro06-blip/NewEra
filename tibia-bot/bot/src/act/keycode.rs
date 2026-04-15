//! keycode.rs — Parseo de nombres de teclas a códigos HID.
//!
//! Los config files usan strings humanos ("F1", "Space", "a") y el protocolo
//! de la Pico usa códigos HID (`KEY_TAP 0xNN`). Este módulo traduce entre ambos.
//!
//! Referencia: USB HID Usage Tables v1.12, sección 10 (Keyboard/Keypad Page 0x07).

use anyhow::{bail, Result};

/// Convierte un nombre de tecla (case-insensitive) al código HID correspondiente.
///
/// Soporta:
/// - Letras: `A`–`Z`
/// - Dígitos: `0`–`9`
/// - Función: `F1`–`F12`
/// - Especiales: `Space`, `Enter`, `Escape`/`Esc`, `Tab`, `Backspace`
/// - Formato explícito: `0xNN` (hex) para teclas no listadas
pub fn parse(name: &str) -> Result<u8> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("nombre de tecla vacío");
    }

    // Formato hex explícito: "0x3A" → 0x3A
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        return u8::from_str_radix(hex, 16)
            .map_err(|_| anyhow::anyhow!("código HID hex inválido: '{}'", trimmed));
    }

    let upper = trimmed.to_ascii_uppercase();

    // Letras A-Z → 0x04..=0x1D
    if upper.len() == 1 {
        let c = upper.chars().next().expect("single-char string cannot be empty");
        if c.is_ascii_alphabetic() {
            return Ok(0x04 + (c as u8 - b'A'));
        }
        if c.is_ascii_digit() {
            // Dígitos: '1'..='9' → 0x1E..=0x26, '0' → 0x27
            return Ok(match c {
                '0' => 0x27,
                _   => 0x1E + (c as u8 - b'1'),
            });
        }
    }

    // Teclas de función F1..F12 → 0x3A..=0x45
    if let Some(rest) = upper.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u8>() {
            if (1..=12).contains(&n) {
                return Ok(0x3A + (n - 1));
            }
        }
    }

    // Numpad 0..9 → 0x62, 0x59..=0x61 (según HID Usage Tables)
    if let Some(rest) = upper.strip_prefix("NUMPAD").or_else(|| upper.strip_prefix("KP")) {
        if let Ok(n) = rest.parse::<u8>() {
            return Ok(match n {
                0     => 0x62,
                1..=9 => 0x59 + (n - 1),
                _     => bail!("Numpad{} fuera de rango (0..9)", n),
            });
        }
    }

    // Especiales
    let code = match upper.as_str() {
        "SPACE"      => 0x2C,
        "ENTER" | "RETURN" => 0x28,
        "ESCAPE" | "ESC"    => 0x29,
        "TAB"        => 0x2B,
        "BACKSPACE"  => 0x2A,
        // Flechas (0x4F..=0x52 en HID Usage Tables)
        "RIGHT" | "RIGHTARROW" => 0x4F,
        "LEFT"  | "LEFTARROW"  => 0x50,
        "DOWN"  | "DOWNARROW"  => 0x51,
        "UP"    | "UPARROW"    => 0x52,
        // Navegación (0x49..=0x4E en HID Usage Tables)
        "INSERT" | "INS"       => 0x49,
        "HOME"                 => 0x4A,
        "PAGEUP"   | "PAGE_UP"   | "AVPAG" | "RETROCEDERPAG" => 0x4B,
        "DELETE" | "DEL"       => 0x4C,
        "END"                  => 0x4D,
        "PAGEDOWN" | "PAGE_DOWN" | "REPAG" | "AVANZARPAG"    => 0x4E,
        // Capslock y modificadores útiles
        "CAPSLOCK" | "CAPS"    => 0x39,
        _ => bail!("nombre de tecla desconocido: '{}'", trimmed),
    };
    Ok(code)
}

/// Convierte un caracter ASCII a su código HID correspondiente, SIN modifier.
/// Usado por `bot.say()` de Lua para tipear strings carácter a carácter.
///
/// Soporta un subset "seguro" que NO requiere Shift:
/// - Letras minúsculas `a-z` → 0x04..0x1D
/// - Dígitos `0-9` → 0x27, 0x1E..0x26
/// - Espacio → 0x2C
/// - Enter (`\n` y `\r`) → 0x28
///
/// **Intencionalmente no soporta mayúsculas ni símbolos** — requerirían
/// enviar Shift+key en una sola operación, lo cual el protocolo actual del
/// bridge (KEY_TAP por comando) no permite. Los nombres de NPCs y comandos
/// de Tibia son case-insensitive, así que "hi trade" funciona igual que "Hi Trade".
pub fn ascii_to_hid(c: char) -> Option<u8> {
    // Letras: solo minúsculas (mayúsculas requerirían Shift).
    if c.is_ascii_lowercase() {
        return Some(0x04 + (c as u8 - b'a'));
    }
    // Dígitos.
    if c.is_ascii_digit() {
        return Some(match c {
            '0' => 0x27,
            _   => 0x1E + (c as u8 - b'1'),
        });
    }
    // Especiales sin modifier.
    match c {
        ' '          => Some(0x2C),   // Space
        '\n' | '\r'  => Some(0x28),   // Enter
        _            => None,         // cualquier otro → ignorado
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_keys() {
        assert_eq!(parse("F1").unwrap(), 0x3A);
        assert_eq!(parse("f1").unwrap(), 0x3A);
        assert_eq!(parse("F12").unwrap(), 0x45);
    }

    #[test]
    fn function_key_out_of_range() {
        assert!(parse("F13").is_err());
        assert!(parse("F0").is_err());
    }

    #[test]
    fn letters() {
        assert_eq!(parse("A").unwrap(), 0x04);
        assert_eq!(parse("a").unwrap(), 0x04);
        assert_eq!(parse("Z").unwrap(), 0x1D);
    }

    #[test]
    fn digits() {
        assert_eq!(parse("1").unwrap(), 0x1E);
        assert_eq!(parse("9").unwrap(), 0x26);
        assert_eq!(parse("0").unwrap(), 0x27);
    }

    #[test]
    fn specials() {
        assert_eq!(parse("Space").unwrap(), 0x2C);
        assert_eq!(parse("Enter").unwrap(), 0x28);
        assert_eq!(parse("esc").unwrap(), 0x29);
        assert_eq!(parse("Escape").unwrap(), 0x29);
    }

    #[test]
    fn hex_override() {
        assert_eq!(parse("0x3A").unwrap(), 0x3A);
        assert_eq!(parse("0xFF").unwrap(), 0xFF);
    }

    #[test]
    fn arrows() {
        assert_eq!(parse("Up").unwrap(), 0x52);
        assert_eq!(parse("Down").unwrap(), 0x51);
        assert_eq!(parse("Left").unwrap(), 0x50);
        assert_eq!(parse("Right").unwrap(), 0x4F);
        // Formas alternativas
        assert_eq!(parse("UpArrow").unwrap(), 0x52);
        assert_eq!(parse("rightarrow").unwrap(), 0x4F);
    }

    #[test]
    fn ascii_to_hid_lowercase_letters() {
        assert_eq!(ascii_to_hid('a'), Some(0x04));
        assert_eq!(ascii_to_hid('h'), Some(0x0B));
        assert_eq!(ascii_to_hid('z'), Some(0x1D));
    }

    #[test]
    fn ascii_to_hid_digits() {
        assert_eq!(ascii_to_hid('1'), Some(0x1E));
        assert_eq!(ascii_to_hid('9'), Some(0x26));
        assert_eq!(ascii_to_hid('0'), Some(0x27));
    }

    #[test]
    fn ascii_to_hid_specials() {
        assert_eq!(ascii_to_hid(' '),  Some(0x2C));
        assert_eq!(ascii_to_hid('\n'), Some(0x28));
        assert_eq!(ascii_to_hid('\r'), Some(0x28));
    }

    #[test]
    fn ascii_to_hid_unsupported_returns_none() {
        // Mayúsculas: intencionalmente no soportadas.
        assert_eq!(ascii_to_hid('A'), None);
        // Símbolos requieren Shift.
        assert_eq!(ascii_to_hid('!'), None);
        assert_eq!(ascii_to_hid('@'), None);
        assert_eq!(ascii_to_hid('.'), None);
    }

    #[test]
    fn ascii_to_hid_matches_parse_letters() {
        // Las letras minúsculas mediante ascii_to_hid deben coincidir con
        // las mayúsculas mediante parse() (ambas apuntan al mismo HID).
        for ch in 'a'..='z' {
            let from_ascii = ascii_to_hid(ch).unwrap();
            let upper = ch.to_ascii_uppercase().to_string();
            let from_parse = parse(&upper).unwrap();
            assert_eq!(from_ascii, from_parse, "mismatch for '{}'", ch);
        }
    }

    #[test]
    fn navigation_keys() {
        // Navegación estándar
        assert_eq!(parse("Insert").unwrap(),   0x49);
        assert_eq!(parse("Home").unwrap(),     0x4A);
        assert_eq!(parse("PageUp").unwrap(),   0x4B);
        assert_eq!(parse("Delete").unwrap(),   0x4C);
        assert_eq!(parse("End").unwrap(),      0x4D);
        assert_eq!(parse("PageDown").unwrap(), 0x4E);
        // Alias en español (AvPág / RePág)
        assert_eq!(parse("AvPag").unwrap(),    0x4B);
        assert_eq!(parse("avpag").unwrap(),    0x4B);
        assert_eq!(parse("RePag").unwrap(),    0x4E);
        // Aliases variantes
        assert_eq!(parse("Page_Up").unwrap(),  0x4B);
        assert_eq!(parse("Ins").unwrap(),      0x49);
        assert_eq!(parse("Del").unwrap(),      0x4C);
    }

    #[test]
    fn numpad() {
        assert_eq!(parse("Numpad0").unwrap(), 0x62);
        assert_eq!(parse("Numpad1").unwrap(), 0x59);
        assert_eq!(parse("Numpad8").unwrap(), 0x60);
        assert_eq!(parse("Numpad9").unwrap(), 0x61);
        // Alias KP
        assert_eq!(parse("KP5").unwrap(), 0x5D);
        assert_eq!(parse("kp2").unwrap(), 0x5A);
    }

    #[test]
    fn numpad_out_of_range() {
        assert!(parse("Numpad10").is_err());
    }

    #[test]
    fn empty_and_unknown() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
        assert!(parse("FunkyKey").is_err());
    }
}
