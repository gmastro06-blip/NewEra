# tibia-bot

Bot de automatización para Tibia con arquitectura distribuida en dos máquinas.

```
PC GAMING (Windows 11)                        PC PROCESADOR (Linux x86_64)
┌──────────────────────────────┐              ┌──────────────────────────────┐
│  Tibia                       │              │  tibia-bot (Rust)            │
│  OBS + DistroAV ─────NDI LAN─┼─────────────▶│    sense/ndi_receiver        │
│                              │              │    core/loop (30 Hz)         │
│  NewEra-bridge.exe             │              │    act/pico_link ────────────┼──┐
│    TCP :9000 ◀───────────────┼─────────────┤                              │  │
│    ↕ serial CDC              │              │  HTTP :8080                  │  │
│                              │              └──────────────────────────────┘  │
│  Raspberry Pi Pico 2 (USB)   │◀─────────────────────────────────────────────┘
│    HID mouse abs + teclado   │
└──────────────────────────────┘
```

## Latencias objetivo

| Segmento | Objetivo |
|---|---|
| NDI PC gaming → bot | ≤ 80 ms |
| Procesamiento bot | ≤ 30 ms |
| Comando → bridge → Pico → HID | ≤ 15 ms |
| **Total end-to-end** | **≤ 130 ms** |

---

## PARTE A — Setup PC gaming (Windows)

### A1. Instalar OBS Studio 30+

Descargar desde https://obsproject.com — versión 30 o superior.

### A2. Instalar plugin DistroAV

Descargar desde https://github.com/DistroAV/DistroAV/releases  
Copiar el `.dll` en la carpeta de plugins de OBS:
```
C:\Program Files\obs-studio\obs-plugins\64bit\
```
Reiniciar OBS.

### A3. Instalar NDI Runtime

Descargar el **NDI Runtime** (no el SDK completo) desde https://ndi.video/tools/  
Esto instala el driver NDI que DistroAV necesita para emitir y que el bot necesita para recibir.  
Reiniciar Windows tras la instalación.

### A4. Configurar Game Capture sobre Tibia

En OBS:
1. Sources → `+` → **Game Capture**
2. Mode: **Capture specific window**
3. Window: seleccionar el ejecutable de Tibia
4. **Importante**: activar "Allow Transparency" para que BattlEye no bloquee la captura

> OBS está en la whitelist de BattlEye. `Game Capture` es el único método compatible.
> `Window Capture` y `Display Capture` son bloqueados por el anticheat.

### A5. Habilitar NDI Output con nombre "TIBIA-BOT"

En OBS:
1. Tools → **DistroAV** (o NDI Output Settings)
2. Activar "Main Output"
3. En "Main Output Name" escribir exactamente: `TIBIA-BOT`
4. OK — OBS ahora emite el stream como fuente NDI visible en la LAN

### A6. Verificar con NDI Studio Monitor

En el PC procesador (o cualquier dispositivo de la LAN):
```bash
# Instalar NDI Tools desde https://ndi.video/tools/
# Abrir NDI Studio Monitor y buscar la fuente "TIBIA-BOT"
```
Si aparece, el pipeline NDI está funcionando.

---

## PARTE B — Compilar y configurar el bridge en Windows

### B1. Instalar Rust en Windows

```powershell
winget install Rustlang.Rustup
rustup toolchain install stable
```

### B2. Compilar el bridge

```cmd
cd tibia-bot\bridge
cargo build --release
```
El binario quedará en `target\release\NewEra-bridge.exe`.

### B3. Configurar bridge_config.toml

```cmd
copy bridge_config.toml.example bridge_config.toml
notepad bridge_config.toml
```
Editar el puerto COM de la Pico (ver Parte C paso C7).

```toml
[serial]
port = "COM5"   # <- ajustar al COM real
baud = 115200

[tcp]
listen_addr = "0.0.0.0:9000"
```

### B4. Ejecutar el bridge

```cmd
cd tibia-bot\bridge
target\release\NewEra-bridge.exe
```

### B5. Verificar log de arranque

El bridge debe imprimir en consola:
```
INFO  Escuchando en 0.0.0.0:9000 — esperando cliente TCP...
INFO  Puerto serial COM5 abierto a 115200 baud
```

---

## PARTE C — Setup Pico 2

### C1. Instalar Arduino IDE y el core arduino-pico

1. Descargar Arduino IDE 2.x desde https://arduino.cc
2. File → Preferences → Additional boards manager URLs:
   ```
   https://github.com/earlephilhower/arduino-pico/releases/download/global/package_rp2040_index.json
   ```
3. Tools → Board → Boards Manager → buscar **"Pico"** → instalar "Raspberry Pi Pico/RP2040/RP2350"

### C2. Instalar Adafruit_TinyUSB_Arduino

Tools → Manage Libraries → buscar **"Adafruit TinyUSB"** → Instalar

### C3. Seleccionar board

Tools → Board → Raspberry Pi RP2040/RP2350 Boards → **"Raspberry Pi Pico 2"**

### C4. Seleccionar USB Stack

Tools → USB Stack → **"Adafruit TinyUSB"**

### C5. Compilar y flashear

1. Abrir `firmware/pico2_hid/pico2_hid.ino` en Arduino IDE
2. Mantener pulsado el botón **BOOTSEL** de la Pico
3. Conectar USB al PC gaming mientras se mantiene BOOTSEL
4. La Pico aparece como disco USB (`RPI-RP2`)
5. Sketch → **Upload** (o Ctrl+U)

### C6. Conectar al PC gaming

Una vez flasheada, conectar la Pico por USB al PC gaming (sin mantener BOOTSEL).

### C7. Verificar en Administrador de dispositivos

Deben aparecer **tres** dispositivos nuevos:
- Dispositivos de interfaz humana → **TibiaBot Pico2 HID+CDC (Mouse)**
- Dispositivos de interfaz humana → **TibiaBot Pico2 HID+CDC (Keyboard)**
- Puertos (COM y LPT) → **TibiaBot Pico2 HID+CDC (COMx)**

Anotar el número `COMx`.

### C8. Configurar bridge_config.toml con el COM real

```toml
[serial]
port = "COM7"   # <- el número que apareció en el paso anterior
```

Reiniciar `NewEra-bridge.exe`.

### C9. Test manual desde otro PC

```bash
# Reemplazar IP_GAMING con la IP del PC gaming
telnet IP_GAMING 9000
PING
# Debe responder: PONG
MOUSE_MOVE 16000 8000
# El cursor del PC gaming debe moverse al centro-izquierda
```

---

## PARTE D — Setup PC procesador (Linux)

### D1. Instalar Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
```

### D2. Instalar el NDI SDK

```bash
# Descargar el SDK de https://ndi.video/sdk/
# (requiere registro gratuito)
# El archivo descargado es un .sh de instalación

chmod +x Install_NDI_SDK_v6_Linux.sh
sudo ./Install_NDI_SDK_v6_Linux.sh

# El SDK se instala en /usr/local/lib/libndi.so y /usr/local/include/
```

### D3. Exportar NDI_SDK_DIR

```bash
# Añadir a ~/.bashrc o ~/.zshrc:
export NDI_SDK_DIR=/usr/local
export LD_LIBRARY_PATH=/usr/local/lib:$LD_LIBRARY_PATH
```

Verificar:
```bash
ls $NDI_SDK_DIR/include/Processing.NDI.Lib.h   # debe existir
```

### D4. Configurar el bot

```bash
cd tibia-bot/bot
cp config.toml.example config.toml
nano config.toml
```

Editar al menos:
```toml
[ndi]
source_name = "TIBIA-BOT"

[pico]
bridge_addr = "192.168.1.100:9000"   # IP del PC gaming

[coords]
# Calibrar según posición real de la ventana de Tibia
tibia_window_x = 0
tibia_window_y = 0
tibia_window_w = 1920
tibia_window_h = 1080
```

### D5. Compilar

```bash
cargo build --release
```

### D6. Ejecutar

```bash
cargo run --release
# o directamente:
./target/release/tibia-bot
```

---

## PARTE E — Test de aceptación end-to-end

### E1. Verificar logs del bot

```
INFO  NDI source encontrada: TIBIA-BOT
INFO  PicoLink: conectado a 192.168.1.100:9000
INFO  PicoLink: PONG recibido — pipeline OK
INFO  Game loop arrancando a 30 Hz (presupuesto/tick = 33.3ms)
```

### E2. Status completo

```bash
curl http://PC_PROCESADOR:8080/status | jq
```
Respuesta esperada:
```json
{
  "tick": 1500,
  "is_paused": false,
  "ticks_total": 1500,
  "ticks_overrun": 0,
  "ndi_latency_ms": 45.2,
  "pico_latency_ms": 8.1,
  "bot_proc_ms": 1.3,
  "has_frame": true
}
```

### E3. Capturar frame de Tibia

```bash
curl -o frame.png http://PC_PROCESADOR:8080/test/grab
# Abrir frame.png — debe mostrar la pantalla de Tibia
```

### E4. Test de click en el viewport

```bash
curl -X POST http://PC_PROCESADOR:8080/test/click \
     -H 'Content-Type: application/json' \
     -d '{"x": 100, "y": 100}'
# El cursor del PC gaming debe moverse y hacer click en esas coords del viewport
```

### E5. Test de ping a la Pico

```bash
curl -X POST http://PC_PROCESADOR:8080/test/pico/ping | jq
```
Respuesta esperada:
```json
{
  "ok": true,
  "reply": "PONG",
  "latency_ms": 7.4
}
```

---

## Estructura del proyecto

```
tibia-bot/
├── Cargo.toml              # Workspace (bot + bridge)
├── bot/
│   ├── Cargo.toml
│   ├── build.rs            # Linkeo con NDI SDK
│   ├── config.toml.example
│   └── src/
│       ├── main.rs         # Bootstrap
│       ├── config.rs       # Carga de config TOML
│       ├── core/
│       │   ├── fsm.rs      # Máquina de estados por prioridad
│       │   ├── loop_.rs    # Game loop 30 Hz con tick budgeting
│       │   └── state.rs    # GameState, SharedState, Metrics
│       ├── sense/
│       │   ├── ndi_receiver.rs  # Thread NDI con reconexión
│       │   ├── frame_buffer.rs  # ArcSwap<Frame> lock-free
│       │   ├── vision.rs        # stub
│       │   └── parse.rs         # stub
│       ├── act/
│       │   ├── mod.rs      # Actuator de alto nivel
│       │   ├── coords.rs   # Conversión viewport→desktop→HID (con tests)
│       │   └── pico_link.rs # Cliente TCP con reconexión y backoff
│       ├── remote/
│       │   └── http.rs     # axum: /status /pause /resume /test/*
│       ├── waypoints/mod.rs # stub
│       ├── scripting/mod.rs # stub (Lua futuro)
│       └── safety/mod.rs    # stub
├── bridge/
│   ├── Cargo.toml
│   ├── bridge_config.toml.example
│   └── src/main.rs         # Proxy TCP↔serial completo
└── firmware/
    └── pico2_hid/
        ├── pico2_hid.ino   # Arduino: HID compuesto + CDC + parser
        └── README.md
```

## Protocolo bot ↔ Pico 2

```
Comando ASCII \n          Respuesta
─────────────────────     ─────────
PING                  →   PONG
HEARTBEAT             →   OK
RESET                 →   OK
STATUS                →   OK uptime=N cmds=N
MOUSE_MOVE <x> <y>    →   OK         (x,y: 0..32767)
MOUSE_CLICK <L|R|M>   →   OK
MOUSE_DOWN  <L|R|M>   →   OK
MOUSE_UP    <L|R|M>   →   OK
KEY_TAP   <hidcode>   →   OK         (ej: 0x04 = 'a')
KEY_DOWN  <hidcode>   →   OK
KEY_UP    <hidcode>   →   OK
TYPE <texto>          →   OK
```

Timeout por comando: **100 ms**. Si la Pico no responde en ese tiempo,
el bot loggea warning y continúa (sin reintentar).

Watchdog de la Pico: si no llega ningún byte en **5 segundos**, ejecuta
RESET internamente.

## Decisiones de diseño

| Decisión | Razón |
|---|---|
| `ndi` crate en lugar de `grafton-ndi` | API de alto nivel más clara en el codebase; si grafton-ndi está más actualizado, el swap es en 5 líneas |
| `loop_.rs` (con underscore) | `loop` es keyword en Rust |
| `ArcSwap<Option<Frame>>` | Acceso lock-free al frame desde game loop y HTTP simultaneamente |
| Un cliente TCP a la vez en el bridge | La Pico solo habla con un cliente; dos clientes generarían comandos entrelazados |
| `ProxyExit` enum en el bridge | Permite diferenciar qué lado cayó y tomar la acción correcta (serial → reabrir serial; TCP → solo reaccept) |
| HID Report ID 1 = mouse, ID 2 = keyboard | Estándar más común; los drivers de Windows los reconocen sin drivers adicionales |
| `HID_LOGICAL_MAX_N(32767, 2)` | 32767 = 0x7FFF; necesitamos 2 bytes porque supera los 127 del encoding de 1 byte |
