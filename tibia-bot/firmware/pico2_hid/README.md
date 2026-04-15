# Firmware pico2_hid

Firmware Arduino para la Raspberry Pi Pico 2 (RP2350).

## Requisitos

| Herramienta | Versión mínima |
|---|---|
| Arduino IDE | 2.x |
| arduino-pico (Earle Philhower) | 3.x (soporte RP2350) |
| Adafruit_TinyUSB_Arduino | última disponible |

## Instalación del entorno

1. Arduino IDE → Preferences → Additional boards manager URLs:
   ```
   https://github.com/earlephilhower/arduino-pico/releases/download/global/package_rp2040_index.json
   ```
2. Tools → Board → Boards Manager → buscar "Raspberry Pi Pico/RP2040/RP2350" → Instalar
3. Tools → Manage Libraries → buscar "Adafruit TinyUSB" → Instalar

## Compilación y flasheo

1. Tools → Board → "Raspberry Pi Pico 2"
2. Tools → USB Stack → "Adafruit TinyUSB"
3. Compilar (Ctrl+R)
4. Mantener pulsado BOOTSEL en la Pico, conectar USB → aparece como disco USB
5. Sketch → Upload (Ctrl+U) — o arrastrar el .uf2 al disco

## Verificación en Windows

Tras el flasheo, en Administrador de dispositivos deben aparecer:
- **Dispositivos de interfaz humana**: "TibiaBot Pico2 HID+CDC" × 2 (mouse + teclado)
- **Puertos (COM y LPT)**: un nuevo COMx

Anotar el número COM para configurar `bridge_config.toml`.
