// tibia_hid_bridge.ino — Arduino Leonardo (ATmega32U4) HID bridge para tibia-bot
//
// Reemplaza el Raspberry Pi Pico 2 como hardware HID. El protocolo de línea
// es idéntico al del Pico, así que el bridge Rust (modo serial) funciona sin
// modificaciones — solo cambiar el puerto COM en bridge_config.toml.
//
// ── Hardware ──────────────────────────────────────────────────────────────
//   Arduino Leonardo (ATmega32U4) — USB HID nativo + CDC serial simultáneo.
//   Otros boards compatibles: Pro Micro, Arduino Micro, SparkFun Pro Micro.
//
// ── Librería requerida ────────────────────────────────────────────────────
//   HID-Project by NicoHood
//   Instalación:
//     Arduino IDE → Tools → Manage Libraries → buscar "HID-Project" → Install
//   Repo: https://github.com/NicoHood/HID
//
//   Por qué HID-Project en vez de Mouse.h/Keyboard.h built-in:
//     - BootKeyboard acepta HID usage IDs raw (0x3A para F1, etc.)
//       sin necesidad de mapear a constantes Arduino (KEY_F1, ...)
//     - AbsoluteMouse soporta posicionamiento ABSOLUTO en rango 0-32767,
//       que es lo que el bot envía (no relativo como el Mouse.h built-in).
//
// ── Protocolo serial (115200 baud, 8N1, line-based ASCII) ─────────────────
//   PING\n              → responde "PONG\n"
//   MOUSE_MOVE X Y\n    → mueve mouse a (X,Y) en HID absoluto 0-32767
//                         responde "OK\n"
//   MOUSE_CLICK\n       → click izquierdo (default, mismo que "MOUSE_CLICK L")
//   MOUSE_CLICK L\n     → click izquierdo
//   MOUSE_CLICK R\n     → click derecho (context menu en Tibia)
//   MOUSE_CLICK M\n     → click medio (scroll wheel click)
//                         responde "OK\n"
//   KEY_TAP 0xNN\n      → press + release de la HID usage ID 0xNN
//                         responde "OK\n"
//                         (también acepta decimal: KEY_TAP 58)
//   RESET\n             → re-init del HID stack, responde "OK\n"
//
//   Errores: "ERR <razón>\n" (línea malformada, parámetros inválidos, etc.)
//
// ── Setup paso a paso ─────────────────────────────────────────────────────
//   1. Conectá el Leonardo al PC Gaming via USB (NO al PC Processor).
//   2. Arduino IDE → Tools:
//        - Board: "Arduino Leonardo"
//        - Port: el COM que aparezca al conectar (probablemente COM3-COM7)
//   3. Sketch → Upload (botón flecha)
//   4. Anotá el COM port que ves en Tools → Port → es el que va en
//      bridge_config.toml.
//   5. En el PC Gaming, editá bridge/bridge_config.toml:
//        [input]
//        mode = "serial"
//        [serial]
//        port = "COM5"   ← tu COM real
//        baud = 115200
//   6. Arrancá el bridge: ./pico_bridge bridge/bridge_config.toml
//        Verás: "PicoLink: PONG recibido — pipeline OK"
//
// ── Verificación rápida ───────────────────────────────────────────────────
//   En el PC Processor (donde corre el bot):
//     curl -X POST http://localhost:8080/test/pico/ping
//   Respuesta esperada: { "ok": true, "reply": "PONG", "latency_ms": <5 }

#include <HID-Project.h>
#include <HID-Settings.h>

// ── Configuración ────────────────────────────────────────────────────────
const unsigned long SERIAL_BAUD = 115200;
const unsigned int  CLICK_HOLD_MS = 30;     // delay entre press y release del click
const unsigned int  KEY_HOLD_MS   = 30;     // delay entre press y release de teclas
const size_t        LINE_BUF_SIZE = 64;     // máximo tamaño de comando entrante

// LED interno para feedback visual (parpadea cuando recibe comando).
const int LED_PIN = LED_BUILTIN;

// ── Estado ───────────────────────────────────────────────────────────────
char   line_buf[LINE_BUF_SIZE];
size_t line_pos = 0;

// ── Setup ────────────────────────────────────────────────────────────────
void setup() {
    pinMode(LED_PIN, OUTPUT);
    digitalWrite(LED_PIN, LOW);

    // CDC serial sobre USB para el host.
    Serial.begin(SERIAL_BAUD);
    // No esperamos a Serial.connected() — el bridge puede conectar después.

    // Inicializar HID interfaces.
    BootKeyboard.begin();
    AbsoluteMouse.begin();

    // Pequeño blink de boot-ready (~200ms total).
    digitalWrite(LED_PIN, HIGH); delay(80);
    digitalWrite(LED_PIN, LOW);  delay(40);
    digitalWrite(LED_PIN, HIGH); delay(80);
    digitalWrite(LED_PIN, LOW);
}

// ── Loop principal ───────────────────────────────────────────────────────
void loop() {
    // Leer caracteres uno por uno hasta encontrar '\n'.
    while (Serial.available() > 0) {
        int c = Serial.read();
        if (c < 0) break;
        if (c == '\r') continue;            // ignorar CR (Windows line endings)
        if (c == '\n') {
            line_buf[line_pos] = '\0';
            if (line_pos > 0) {
                process_command(line_buf);
            }
            line_pos = 0;
        } else if (line_pos < LINE_BUF_SIZE - 1) {
            line_buf[line_pos++] = (char)c;
        } else {
            // Línea desbordó el buffer — descartar y reportar.
            line_pos = 0;
            Serial.print(F("ERR line_too_long\n"));
        }
    }
}

// ── Procesador de comandos ───────────────────────────────────────────────
void process_command(const char* cmd) {
    // Blink LED como heartbeat visual.
    digitalWrite(LED_PIN, HIGH);

    // PING / PONG
    if (strcmp(cmd, "PING") == 0) {
        Serial.print(F("PONG\n"));
    }
    // MOUSE_MOVE X Y
    else if (strncmp(cmd, "MOUSE_MOVE ", 11) == 0) {
        long x, y;
        if (sscanf(cmd + 11, "%ld %ld", &x, &y) == 2) {
            // AbsoluteMouse usa rango 0-32767 (= int16 unsigned).
            if (x < 0 || x > 32767 || y < 0 || y > 32767) {
                Serial.print(F("ERR mouse_out_of_range\n"));
            } else {
                AbsoluteMouse.moveTo((int)x, (int)y);
                Serial.print(F("OK\n"));
            }
        } else {
            Serial.print(F("ERR mouse_parse\n"));
        }
    }
    // MOUSE_CLICK [L|R|M]  (default L si no hay argumento)
    //
    // Acepta 3 variantes:
    //   "MOUSE_CLICK"     → left click (compat con firmware pre-2026-04-16)
    //   "MOUSE_CLICK L"   → left click
    //   "MOUSE_CLICK R"   → right click (context menu en Tibia)
    //   "MOUSE_CLICK M"   → middle click
    //
    // Bug fix 2026-04-16: antes el Arduino solo hacía LEFT, rechazando
    // silenciosamente "MOUSE_CLICK R" con "ERR unknown command". Esto
    // rompía todo el flujo de stow_bag / deposit / context menus del bot.
    else if (strcmp(cmd, "MOUSE_CLICK") == 0
          || strcmp(cmd, "MOUSE_CLICK L") == 0) {
        AbsoluteMouse.press(MOUSE_LEFT);
        delay(CLICK_HOLD_MS);
        AbsoluteMouse.release(MOUSE_LEFT);
        Serial.print(F("OK\n"));
    }
    else if (strcmp(cmd, "MOUSE_CLICK R") == 0) {
        AbsoluteMouse.press(MOUSE_RIGHT);
        delay(CLICK_HOLD_MS);
        AbsoluteMouse.release(MOUSE_RIGHT);
        Serial.print(F("OK\n"));
    }
    else if (strcmp(cmd, "MOUSE_CLICK M") == 0) {
        AbsoluteMouse.press(MOUSE_MIDDLE);
        delay(CLICK_HOLD_MS);
        AbsoluteMouse.release(MOUSE_MIDDLE);
        Serial.print(F("OK\n"));
    }
    // KEY_TAP <hex_or_dec>
    else if (strncmp(cmd, "KEY_TAP ", 8) == 0) {
        long code;
        const char* arg = cmd + 8;
        // Acepta hex (0x3A) o decimal (58).
        if (arg[0] == '0' && (arg[1] == 'x' || arg[1] == 'X')) {
            code = strtol(arg, NULL, 16);
        } else {
            code = strtol(arg, NULL, 10);
        }
        if (code <= 0 || code > 0xFF) {
            Serial.print(F("ERR key_out_of_range\n"));
        } else {
            // BootKeyboard.write() convierte a HID report con la usage ID raw.
            // press + release manualmente para dar tiempo de hold.
            BootKeyboard.press((KeyboardKeycode)code);
            delay(KEY_HOLD_MS);
            BootKeyboard.release((KeyboardKeycode)code);
            Serial.print(F("OK\n"));
        }
    }
    // RESET
    else if (strcmp(cmd, "RESET") == 0) {
        BootKeyboard.releaseAll();
        AbsoluteMouse.releaseAll();
        Serial.print(F("OK\n"));
    }
    // Comando desconocido
    else {
        Serial.print(F("ERR unknown_command\n"));
    }

    digitalWrite(LED_PIN, LOW);
}
