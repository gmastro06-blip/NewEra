/**
 * pico2_hid.ino — Firmware para Raspberry Pi Pico 2 (RP2350)
 *
 * Dispositivo USB compuesto:
 *   Interface 0: HID — Mouse absoluto (Report ID 1) + Teclado (Report ID 2)
 *   Interface 1: CDC ACM (puerto serie virtual)
 *
 * Protocolo serial: comandos ASCII terminados en \n.
 * La Pico responde "OK\n" o "ERR <razon>\n".
 *
 * Compilación:
 *   Arduino IDE 2.x
 *   Board: "Raspberry Pi Pico 2" (arduino-pico 3.x+)
 *   USB Stack: "Adafruit TinyUSB"
 *   Librería: Adafruit_TinyUSB_Arduino (desde Library Manager)
 *
 * Pinout utilizado:
 *   LED onboard: GPIO 25 (igual que Pico 1)
 *
 * Indicadores LED:
 *   Apagado        = esperando que el host abra el CDC
 *   Parpadeo lento = CDC abierto, sin tráfico reciente (idle)
 *   Parpadeo rápido= ejecutando comandos
 *   Fijo encendido = error de parsing repetido (se limpia con RESET)
 */

#include <Adafruit_TinyUSB.h>

// ── Descriptor HID combinado: Mouse absoluto (ID=1) + Teclado (ID=2) ─────────
//
// El descriptor sigue el formato HID 1.11.
// Mouse absoluto: 3 botones + 5 bits padding + X uint16 + Y uint16
//   Logical Max 32767 para que coincida con las coords HID del bot.
// Teclado: modifier byte + reserved + 6 keycodes (boot-protocol compatible).
//
// Referencia: HID Usage Tables for USB, v1.3, sección 4 (Generic Desktop).

static const uint8_t hid_report_descriptor[] = {
  // ── Report ID 1: Mouse absoluto ──────────────────────────────────────────
  HID_USAGE_PAGE(HID_USAGE_PAGE_DESKTOP),
  HID_USAGE(HID_USAGE_DESKTOP_MOUSE),
  HID_COLLECTION(HID_COLLECTION_APPLICATION),
    HID_REPORT_ID(1)
    HID_COLLECTION(HID_COLLECTION_PHYSICAL),

      // Botones L, R, M (1 bit cada uno)
      HID_USAGE_PAGE(HID_USAGE_PAGE_BUTTON),
      HID_USAGE_MIN(1),
      HID_USAGE_MAX(3),
      HID_LOGICAL_MIN(0),
      HID_LOGICAL_MAX(1),
      HID_REPORT_COUNT(3),
      HID_REPORT_SIZE(1),
      HID_INPUT(HID_DATA | HID_VARIABLE | HID_ABSOLUTE),

      // Padding: 5 bits para alinear a 1 byte
      HID_REPORT_COUNT(1),
      HID_REPORT_SIZE(5),
      HID_INPUT(HID_CONSTANT),

      // Coordenadas absolutas X e Y (16 bits cada una, 0..32767)
      HID_USAGE_PAGE(HID_USAGE_PAGE_DESKTOP),
      HID_USAGE(HID_USAGE_DESKTOP_X),
      HID_USAGE(HID_USAGE_DESKTOP_Y),
      HID_LOGICAL_MIN_N(0, 2),
      HID_LOGICAL_MAX_N(32767, 2),
      HID_PHYSICAL_MIN(0),
      HID_PHYSICAL_MAX_N(32767, 2),
      HID_REPORT_COUNT(2),
      HID_REPORT_SIZE(16),
      HID_INPUT(HID_DATA | HID_VARIABLE | HID_ABSOLUTE),

    HID_COLLECTION_END,
  HID_COLLECTION_END,

  // ── Report ID 2: Teclado ──────────────────────────────────────────────────
  HID_USAGE_PAGE(HID_USAGE_PAGE_DESKTOP),
  HID_USAGE(HID_USAGE_DESKTOP_KEYBOARD),
  HID_COLLECTION(HID_COLLECTION_APPLICATION),
    HID_REPORT_ID(2)

    // Byte de modifier (8 teclas modificadoras como bits individuales)
    HID_USAGE_PAGE(HID_USAGE_PAGE_KEYBOARD),
    HID_USAGE_MIN(HID_KEY_CONTROL_LEFT),
    HID_USAGE_MAX(HID_KEY_GUI_RIGHT),
    HID_LOGICAL_MIN(0),
    HID_LOGICAL_MAX(1),
    HID_REPORT_COUNT(8),
    HID_REPORT_SIZE(1),
    HID_INPUT(HID_DATA | HID_VARIABLE | HID_ABSOLUTE),

    // Byte reservado
    HID_REPORT_COUNT(1),
    HID_REPORT_SIZE(8),
    HID_INPUT(HID_CONSTANT),

    // 6 keycodes simultáneos
    HID_USAGE_PAGE(HID_USAGE_PAGE_KEYBOARD),
    HID_USAGE_MIN(0),
    HID_USAGE_MAX(255),
    HID_LOGICAL_MIN(0),
    HID_LOGICAL_MAX_N(255, 2),
    HID_REPORT_COUNT(6),
    HID_REPORT_SIZE(8),
    HID_INPUT(HID_DATA | HID_ARRAY | HID_ABSOLUTE),

  HID_COLLECTION_END,
};

// ── Instancias TinyUSB ────────────────────────────────────────────────────────
Adafruit_USBD_HID usb_hid(hid_report_descriptor,
                           sizeof(hid_report_descriptor),
                           HID_ITF_PROTOCOL_NONE,
                           2,      // intervalo de polling en ms
                           false); // no en boot mode

// ── Estado del mouse y teclado ────────────────────────────────────────────────
// Mantenemos el estado completo para poder emitir reports parciales.
struct MouseState {
  uint8_t  buttons; // bitmask: bit0=L bit1=R bit2=M
  uint16_t x;
  uint16_t y;
};

struct KeyboardState {
  uint8_t modifier;
  uint8_t keycodes[6];
  uint8_t count; // cuántos keycodes activos (0..6)
};

static MouseState    mouse_state    = {0, 0, 0};
static KeyboardState keyboard_state = {0, {0,0,0,0,0,0}, 0};

// ── Watchdog ──────────────────────────────────────────────────────────────────
static unsigned long last_activity_ms = 0;
#define WATCHDOG_TIMEOUT_MS 5000

// ── LED onboard ───────────────────────────────────────────────────────────────
#define LED_PIN 25
enum LedMode { LED_OFF, LED_SLOW_BLINK, LED_FAST_BLINK, LED_SOLID };
static LedMode led_mode = LED_OFF;
static unsigned long last_cmd_ms = 0;

// ── Buffer de línea ───────────────────────────────────────────────────────────
#define LINE_BUFFER_SIZE 256
static char line_buf[LINE_BUFFER_SIZE];
static int  line_pos = 0;
static bool line_overflow = false;

// ── Contadores de diagnóstico ─────────────────────────────────────────────────
static unsigned long cmd_count = 0;
static unsigned long uptime_start_ms = 0;

// ─────────────────────────────────────────────────────────────────────────────
// setup()
// ─────────────────────────────────────────────────────────────────────────────
void setup() {
  // LED como output de estado.
  pinMode(LED_PIN, OUTPUT);
  digitalWrite(LED_PIN, LOW);

  // ── TinyUSB: configurar strings del dispositivo ───────────────────────────
  // Importante: llamar antes de usb_hid.begin() y Serial.begin().
  TinyUSBDevice.setManufacturerDescriptor("TibiaBot");
  TinyUSBDevice.setProductDescriptor("Pico2 HID+CDC");

  // ── Iniciar HID ──────────────────────────────────────────────────────────
  usb_hid.begin();

  // ── Iniciar CDC (Serial) ─────────────────────────────────────────────────
  // Con arduino-pico + TinyUSB, Serial == USB CDC.
  Serial.begin(115200);

  // Esperar a que el host enumere el dispositivo (máx 3s para no bloquearse).
  unsigned long t = millis();
  while (!TinyUSBDevice.mounted() && (millis() - t) < 3000) {
    delay(10);
  }

  uptime_start_ms = millis();
  last_activity_ms = millis();

  // Emitir report inicial en cero para que el OS registre el mouse.
  send_mouse_report();
  send_keyboard_report();
}

// ─────────────────────────────────────────────────────────────────────────────
// loop()
// ─────────────────────────────────────────────────────────────────────────────
void loop() {
  // Procesar tareas internas de TinyUSB (necesario con arduino-pico).
#if defined(ARDUINO_ARCH_RP2040)
  // tud_task() es llamado automáticamente por el core de arduino-pico,
  // pero lo llamamos explícitamente por si acaso para garantizar responsividad.
#endif

  // ── Leer bytes del CDC ────────────────────────────────────────────────────
  while (Serial.available()) {
    int c = Serial.read();
    if (c < 0) break;

    last_activity_ms = millis(); // resetear watchdog de software

    if (c == '\n' || c == '\r') {
      if (line_pos > 0) {
        line_buf[line_pos] = '\0';
        if (line_overflow) {
          // Línea demasiado larga — descartamos silenciosamente.
          Serial.print("ERR linea demasiado larga\n");
          line_overflow = false;
        } else {
          process_line(line_buf, line_pos);
        }
        line_pos = 0;
      }
    } else {
      if (line_pos < LINE_BUFFER_SIZE - 1) {
        line_buf[line_pos++] = (char)c;
      } else {
        // Overflow: seguimos leyendo hasta \n pero marcamos la línea.
        line_overflow = true;
      }
    }
  }

  // ── Watchdog de software: RESET si no hay actividad ──────────────────────
  if ((millis() - last_activity_ms) > WATCHDOG_TIMEOUT_MS) {
    do_reset();
    last_activity_ms = millis();
  }

  // ── Actualizar LED ────────────────────────────────────────────────────────
  update_led();
}

// ─────────────────────────────────────────────────────────────────────────────
// process_line() — parser del protocolo
// ─────────────────────────────────────────────────────────────────────────────
void process_line(const char* line, int len) {
  cmd_count++;
  last_cmd_ms = millis();

  // Parsear el primer token (comando).
  char cmd[32];
  char arg1[64];
  char arg2[64];
  int parsed = sscanf(line, "%31s %63s %63s", cmd, arg1, arg2);

  // ── PING ────────────────────────────────────────────────────────────────
  if (strcmp(cmd, "PING") == 0) {
    Serial.print("PONG\n");
    return;
  }

  // ── HEARTBEAT ───────────────────────────────────────────────────────────
  if (strcmp(cmd, "HEARTBEAT") == 0) {
    Serial.print("OK\n");
    return;
  }

  // ── RESET ───────────────────────────────────────────────────────────────
  if (strcmp(cmd, "RESET") == 0) {
    do_reset();
    Serial.print("OK\n");
    return;
  }

  // ── STATUS ──────────────────────────────────────────────────────────────
  if (strcmp(cmd, "STATUS") == 0) {
    unsigned long uptime_ms = millis() - uptime_start_ms;
    // free_heap no está disponible de forma directa en arduino-pico,
    // pero podemos usar rp2040.getFreeHeap() si existe en la versión del core.
    char buf[128];
    snprintf(buf, sizeof(buf), "OK uptime=%lu cmds=%lu\n",
             uptime_ms, cmd_count);
    Serial.print(buf);
    return;
  }

  // ── MOUSE_MOVE <x> <y> ─────────────────────────────────────────────────
  if (strcmp(cmd, "MOUSE_MOVE") == 0) {
    if (parsed < 3) { Serial.print("ERR faltan args\n"); return; }
    int x = atoi(arg1);
    int y = atoi(arg2);
    if (x < 0 || x > 32767 || y < 0 || y > 32767) {
      Serial.print("ERR coords fuera de rango 0-32767\n");
      return;
    }
    mouse_state.x = (uint16_t)x;
    mouse_state.y = (uint16_t)y;
    if (!send_mouse_report()) { Serial.print("ERR hid_send_failed\n"); return; }
    Serial.print("OK\n");
    return;
  }

  // ── MOUSE_CLICK <L|R|M> ────────────────────────────────────────────────
  if (strcmp(cmd, "MOUSE_CLICK") == 0) {
    if (parsed < 2) { Serial.print("ERR falta boton\n"); return; }
    uint8_t btn = parse_button(arg1);
    if (btn == 0) { Serial.print("ERR boton invalido (L R M)\n"); return; }
    // Press
    mouse_state.buttons |= btn;
    send_mouse_report();
    delay(20); // mínimo 20ms para que el OS registre el click
    // Release
    mouse_state.buttons &= ~btn;
    if (!send_mouse_report()) { Serial.print("ERR hid_send_failed\n"); return; }
    Serial.print("OK\n");
    return;
  }

  // ── MOUSE_DOWN <L|R|M> ────────────────────────────────────────────────
  if (strcmp(cmd, "MOUSE_DOWN") == 0) {
    if (parsed < 2) { Serial.print("ERR falta boton\n"); return; }
    uint8_t btn = parse_button(arg1);
    if (btn == 0) { Serial.print("ERR boton invalido\n"); return; }
    mouse_state.buttons |= btn;
    if (!send_mouse_report()) { Serial.print("ERR hid_send_failed\n"); return; }
    Serial.print("OK\n");
    return;
  }

  // ── MOUSE_UP <L|R|M> ─────────────────────────────────────────────────
  if (strcmp(cmd, "MOUSE_UP") == 0) {
    if (parsed < 2) { Serial.print("ERR falta boton\n"); return; }
    uint8_t btn = parse_button(arg1);
    if (btn == 0) { Serial.print("ERR boton invalido\n"); return; }
    mouse_state.buttons &= ~btn;
    if (!send_mouse_report()) { Serial.print("ERR hid_send_failed\n"); return; }
    Serial.print("OK\n");
    return;
  }

  // ── KEY_TAP <hidcode> ─────────────────────────────────────────────────
  // hidcode puede ser decimal (65) o hex (0x41)
  if (strcmp(cmd, "KEY_TAP") == 0) {
    if (parsed < 2) { Serial.print("ERR falta keycode\n"); return; }
    uint8_t kc = (uint8_t)strtoul(arg1, NULL, 0);
    if (!key_press(kc)) { Serial.print("ERR hid_send_failed\n"); return; }
    delay(20);
    if (!key_release(kc)) { Serial.print("ERR hid_send_failed\n"); return; }
    Serial.print("OK\n");
    return;
  }

  // ── KEY_DOWN <hidcode> ────────────────────────────────────────────────
  if (strcmp(cmd, "KEY_DOWN") == 0) {
    if (parsed < 2) { Serial.print("ERR falta keycode\n"); return; }
    uint8_t kc = (uint8_t)strtoul(arg1, NULL, 0);
    if (!key_press(kc)) { Serial.print("ERR hid_send_failed\n"); return; }
    Serial.print("OK\n");
    return;
  }

  // ── KEY_UP <hidcode> ─────────────────────────────────────────────────
  if (strcmp(cmd, "KEY_UP") == 0) {
    if (parsed < 2) { Serial.print("ERR falta keycode\n"); return; }
    uint8_t kc = (uint8_t)strtoul(arg1, NULL, 0);
    if (!key_release(kc)) { Serial.print("ERR hid_send_failed\n"); return; }
    Serial.print("OK\n");
    return;
  }

  // ── TYPE <texto> ──────────────────────────────────────────────────────
  // Escribe texto ASCII literal carácter por carácter.
  // Soporta solo ASCII imprimible 0x20-0x7E. Ignora el resto.
  if (strcmp(cmd, "TYPE") == 0) {
    // El texto comienza después del primer espacio en la línea original.
    const char* text_start = NULL;
    for (int i = 0; i < len; i++) {
      if (line[i] == ' ') {
        text_start = line + i + 1;
        break;
      }
    }
    if (text_start == NULL || *text_start == '\0') {
      Serial.print("ERR falta texto\n");
      return;
    }
    bool ok = true;
    for (const char* p = text_start; *p && ok; p++) {
      HidChar hc = ascii_to_hid(*p);
      if (hc.keycode != 0) {
        if (hc.needs_shift) keyboard_state.modifier |= 0x02; // Left Shift
        ok = key_press(hc.keycode);
        delay(10);
        ok = ok && key_release(hc.keycode);
        if (hc.needs_shift) {
          keyboard_state.modifier &= ~0x02;
          send_keyboard_report();
        }
        delay(10);
      }
    }
    if (!ok) { Serial.print("ERR hid_send_failed\n"); return; }
    Serial.print("OK\n");
    return;
  }

  // ── Comando desconocido ───────────────────────────────────────────────
  Serial.print("ERR cmd desconocido\n");
}

// ─────────────────────────────────────────────────────────────────────────────
// Funciones HID
// ─────────────────────────────────────────────────────────────────────────────

bool send_mouse_report() {
  if (!usb_hid.ready()) return false;
  // Report ID 1: buttons(1B) + x(2B) + y(2B) = 5 bytes
  uint8_t report[5];
  report[0] = mouse_state.buttons;
  report[1] = mouse_state.x & 0xFF;
  report[2] = (mouse_state.x >> 8) & 0xFF;
  report[3] = mouse_state.y & 0xFF;
  report[4] = (mouse_state.y >> 8) & 0xFF;
  return usb_hid.sendReport(1, report, sizeof(report));
}

bool send_keyboard_report() {
  if (!usb_hid.ready()) return false;
  // Report ID 2: modifier(1B) + reserved(1B) + keycodes(6B) = 8 bytes
  uint8_t report[8] = {0};
  report[0] = keyboard_state.modifier;
  report[1] = 0; // reservado
  for (int i = 0; i < keyboard_state.count && i < 6; i++) {
    report[2 + i] = keyboard_state.keycodes[i];
  }
  return usb_hid.sendReport(2, report, sizeof(report));
}

bool key_press(uint8_t keycode) {
  // Buscar si ya está presionada.
  for (int i = 0; i < keyboard_state.count; i++) {
    if (keyboard_state.keycodes[i] == keycode) return true; // ya presionada
  }
  // Buscar slot libre (máx 6 teclas simultáneas).
  if (keyboard_state.count >= 6) return false; // overflow de N-key rollover
  keyboard_state.keycodes[keyboard_state.count++] = keycode;
  return send_keyboard_report();
}

bool key_release(uint8_t keycode) {
  // Eliminar keycode del array compactando.
  int found = -1;
  for (int i = 0; i < keyboard_state.count; i++) {
    if (keyboard_state.keycodes[i] == keycode) { found = i; break; }
  }
  if (found < 0) return true; // ya estaba liberada
  // Compactar: mover el último al slot liberado.
  keyboard_state.count--;
  if (found < keyboard_state.count) {
    keyboard_state.keycodes[found] = keyboard_state.keycodes[keyboard_state.count];
  }
  keyboard_state.keycodes[keyboard_state.count] = 0;
  return send_keyboard_report();
}

void do_reset() {
  // Liberar todos los botones del mouse.
  mouse_state.buttons = 0;
  send_mouse_report();
  // Liberar todas las teclas del teclado.
  keyboard_state.modifier = 0;
  keyboard_state.count    = 0;
  memset(keyboard_state.keycodes, 0, sizeof(keyboard_state.keycodes));
  send_keyboard_report();
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

uint8_t parse_button(const char* s) {
  if (s[0] == 'L' && s[1] == '\0') return 0x01;
  if (s[0] == 'R' && s[1] == '\0') return 0x02;
  if (s[0] == 'M' && s[1] == '\0') return 0x04;
  return 0; // inválido
}

/// Resultado de convertir un carácter ASCII a HID.
struct HidChar {
  uint8_t keycode;   // 0 = no soportado
  bool    needs_shift;
};

/// Convierte carácter ASCII imprimible al HID keycode + modifier necesario.
/// Cubre letras (mayúsculas y minúsculas), dígitos y símbolos comunes del
/// teclado US-QWERTY.
HidChar ascii_to_hid(char c) {
  // Letras minúsculas: 'a'=0x04 .. 'z'=0x1D
  if (c >= 'a' && c <= 'z') return { (uint8_t)(c - 'a' + 0x04), false };
  // Letras mayúsculas: mismo keycode, Shift activado
  if (c >= 'A' && c <= 'Z') return { (uint8_t)(c - 'A' + 0x04), true };
  // Dígitos sin Shift: '1'=0x1E .. '9'=0x26, '0'=0x27
  if (c >= '1' && c <= '9') return { (uint8_t)(c - '1' + 0x1E), false };
  if (c == '0') return { 0x27, false };
  // Símbolos sin Shift
  if (c == ' ')  return { 0x2C, false };
  if (c == '\n') return { 0x28, false };
  if (c == '-')  return { 0x2D, false };
  if (c == '=')  return { 0x2E, false };
  if (c == '[')  return { 0x2F, false };
  if (c == ']')  return { 0x30, false };
  if (c == '\\') return { 0x31, false };
  if (c == ';')  return { 0x33, false };
  if (c == '\'') return { 0x34, false };
  if (c == '`')  return { 0x35, false };
  if (c == ',')  return { 0x36, false };
  if (c == '.')  return { 0x37, false };
  if (c == '/')  return { 0x38, false };
  // Símbolos que requieren Shift (US-QWERTY)
  if (c == '!') return { 0x1E, true };
  if (c == '@') return { 0x1F, true };
  if (c == '#') return { 0x20, true };
  if (c == '$') return { 0x21, true };
  if (c == '%') return { 0x22, true };
  if (c == '^') return { 0x23, true };
  if (c == '&') return { 0x24, true };
  if (c == '*') return { 0x25, true };
  if (c == '(') return { 0x26, true };
  if (c == ')') return { 0x27, true };
  if (c == '_') return { 0x2D, true };
  if (c == '+') return { 0x2E, true };
  if (c == '{') return { 0x2F, true };
  if (c == '}') return { 0x30, true };
  if (c == '|') return { 0x31, true };
  if (c == ':') return { 0x33, true };
  if (c == '"') return { 0x34, true };
  if (c == '~') return { 0x35, true };
  if (c == '<') return { 0x36, true };
  if (c == '>') return { 0x37, true };
  if (c == '?') return { 0x38, true };
  return { 0, false }; // no soportado
}

// Mantener alias sin Shift para compatibilidad con código existente que lo llame.
uint8_t ascii_to_hid_keycode(char c) { return ascii_to_hid(c).keycode; }

// ── LED ───────────────────────────────────────────────────────────────────────
void update_led() {
  static unsigned long last_toggle = 0;
  static bool led_on = false;
  unsigned long now = millis();

  // Decidir modo según actividad.
  if (!TinyUSBDevice.mounted()) {
    led_mode = LED_OFF;
  } else if ((now - last_cmd_ms) < 200) {
    led_mode = LED_FAST_BLINK; // actividad reciente
  } else if (Serial) {
    led_mode = LED_SLOW_BLINK; // CDC abierto pero idle
  } else {
    led_mode = LED_OFF;
  }

  switch (led_mode) {
    case LED_OFF:
      digitalWrite(LED_PIN, LOW);
      break;
    case LED_SLOW_BLINK:
      if (now - last_toggle > 800) {
        led_on = !led_on;
        digitalWrite(LED_PIN, led_on ? HIGH : LOW);
        last_toggle = now;
      }
      break;
    case LED_FAST_BLINK:
      if (now - last_toggle > 100) {
        led_on = !led_on;
        digitalWrite(LED_PIN, led_on ? HIGH : LOW);
        last_toggle = now;
      }
      break;
    case LED_SOLID:
      digitalWrite(LED_PIN, HIGH);
      break;
  }
}
