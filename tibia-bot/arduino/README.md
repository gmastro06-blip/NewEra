# Arduino Leonardo HID bridge — tibia-bot

Reemplazo del Raspberry Pi Pico 2 como hardware HID. Este sketch corre en
un Arduino Leonardo (o cualquier board ATmega32U4 como Pro Micro / Micro)
y emula un mouse + keyboard USB HID que recibe comandos del `pico_bridge`
binary via serial.

## Hardware

- **Arduino Leonardo** (ATmega32U4) — recomendado, USB nativo
- Alternativas compatibles:
  - SparkFun Pro Micro / Pro Micro Clone (ATmega32U4)
  - Arduino Micro (ATmega32U4)
- **NO compatible**: Arduino Uno, Nano, Nano Every (no tienen USB HID)

## Setup paso a paso

### 1. Instalar librería HID-Project

1. Abrir Arduino IDE
2. **Tools → Manage Libraries** (Ctrl+Shift+I)
3. Buscar `HID-Project` (autor: NicoHood)
4. Click **Install** → versión más reciente

### 2. Configurar board

1. **Tools → Board → Arduino AVR Boards → Arduino Leonardo**
2. Conectar el Arduino al PC Gaming via USB
3. **Tools → Port** → seleccionar el COM que aparezca (ej: `COM5`)
   - Anotar este puerto, va en `bridge_config.toml`

### 3. Flash del sketch

1. Abrir `arduino/tibia_hid_bridge/tibia_hid_bridge.ino` desde el repo
2. **Sketch → Upload** (Ctrl+U)
3. Esperar a que diga "Done uploading"
4. El LED del board debería parpadear 2 veces (boot ready)

### 4. Configurar el bridge Rust

Editar `bridge/bridge_config.toml` en el PC Gaming:

```toml
[input]
mode = "serial"

[serial]
port = "COM5"      # ← reemplazar con tu COM real
baud = 115200

[tcp]
listen_addr = "0.0.0.0:9000"

[focus]
enabled = true
window_title_contains = "Tibia"
poll_interval_ms = 100
```

### 5. Arrancar el bridge

```powershell
cd C:/Users/gmast/Documents/GitHub/NewEra/tibia-bot
./target/release/pico_bridge.exe bridge/bridge_config.toml
```

Si todo está OK, verás en el log:
```
PicoLink: conectado a 127.0.0.1:9000
PicoLink: PONG recibido — pipeline OK
```

### 6. Verificar desde el bot

```powershell
curl -X POST http://localhost:8080/test/pico/ping
```

Respuesta esperada:
```json
{"ok": true, "reply": "PONG", "latency_ms": 2.5}
```

## Protocolo serial

Idéntico al del Pico bridge — el sketch implementa:

| Comando | Respuesta | Descripción |
|---|---|---|
| `PING\n` | `PONG\n` | Health check |
| `MOUSE_MOVE X Y\n` | `OK\n` | Mover mouse a (X, Y) en HID absoluto 0-32767 |
| `MOUSE_CLICK\n` | `OK\n` | Click izquierdo (30ms hold) |
| `KEY_TAP 0xNN\n` | `OK\n` | Press+release de HID usage ID (hex o decimal) |
| `RESET\n` | `OK\n` | Liberar todas las teclas/botones |

Errores: `ERR <razón>\n` — línea malformada, params fuera de rango, etc.

## Diferencias vs Pico 2

| Aspecto | Pico 2 (RP2040/RP2350) | Arduino Leonardo (ATmega32U4) |
|---|---|---|
| Lenguaje firmware | C / MicroPython | Arduino C++ |
| HID library | TinyUSB | HID-Project (NicoHood) |
| Boot time | ~500ms | ~200ms |
| USB enumeration | nativo | nativo |
| Latencia HID report | ~1ms | ~1-2ms |
| Compatibilidad | Pico 2 oficial | Leonardo / Pro Micro / Micro |

**No hay diferencias funcionales** — el bot ve ambos como un dispositivo
HID estándar y el bridge usa el mismo protocolo serial.

## Troubleshooting

### El bridge dice "PicoLink: conectando..." pero nunca recibe PONG

- Verifica el COM port en `bridge_config.toml` — debe matchear el que ves
  en Device Manager → Puertos (COM y LPT)
- Asegúrate que el Arduino IDE esté CERRADO (libera el puerto)
- Verifica que el sketch fue flasheado (LED debe parpadear al boot)
- Verifica que el cable USB del Arduino soporta data (no solo carga)

### El char no responde a las teclas

- Verifica con `curl -X POST http://localhost:8080/test/pico/ping` que el
  bridge responde PONG
- Verifica `[focus]` en `bridge_config.toml` — `window_title_contains = "Tibia"`
  debe matchear el título exacto de la ventana del juego
- Prueba `curl -X POST http://localhost:8080/test/key` para enviar una tecla
  de prueba directamente

### Errores de "ERR key_out_of_range" en el log del bridge

- El bot está enviando un HID code fuera del rango 1-255
- Revisa `bot/config.toml` — los hotkeys deben ser valores HID válidos
  (F1=0x3A, F2=0x3B, ..., F12=0x45, PageDown=0x4E, etc.)

### El sketch no compila ("HID-Project.h not found")

- Instala la librería HID-Project en el Arduino IDE
  (Tools → Manage Libraries → buscar "HID-Project")

### Click absoluto no apunta al lugar correcto

- AbsoluteMouse de HID-Project mapea 0-32767 al monitor PRIMARIO
- Si Tibia está en un monitor secundario, mover Tibia al primario o
  ajustar las coords desktop en `bot/config.toml [coords]`
