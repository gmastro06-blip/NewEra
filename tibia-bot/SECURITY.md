# Anti-detection guide (BattleEye)

Documentación de los vectores de detección que BattleEye (y otros anti-cheats
kernel-level) usan contra software de automation, y cómo NewEra mitiga cada uno.

**Disclaimer importante**: ningún bot es indetectable al 100%. Este documento
describe mitigaciones que **reducen la probabilidad** de detección vs una
implementación naïve. No hay garantía de que BattleEye (o sus updates futuros)
no detecte el bot. Asumir que **eventualmente puede haber ban** y no correrlo en
cuentas irrepetibles.

---

## Vectores de detección de BE y status en NewEra

| Vector | Cómo BE detecta | Status NewEra |
|--------|-----------------|---------------|
| **SendInput / PostMessage a ventana del juego** | Kernel driver hookea las APIs, flaguea procesos que las llaman sobre Tibia | ✅ **Mitigado**: default `mode = "serial"` (Arduino HID). `sendinput` opt-in solo para dev/test. |
| **OpenProcess + ReadProcessMemory sobre Tibia.exe** | Monitor de handles al proceso del juego | ✅ **No aplica**: NewEra nunca abre Tibia.exe. Captura vía NDI stream (OBS → bot). |
| **DirectX/GDI hooks para screen capture** | Monitor de DLL injection en Tibia process | ✅ **No aplica**: captura vía OBS + DistroAV NDI plugin. Zero hooks en Tibia. |
| **High-frequency polling de GetForegroundWindow / GetWindowTextW** | Fingerprint: lectura repetitiva del título de la ventana activa | ✅ **Mitigado**: poll interval 2000ms (default), antes 100ms. |
| **EnumWindows buscando strings ("Tibia", etc.)** | Procesos enumerando ventanas para buscar un target | 🟡 **Parcial**: TTL cache de 10s sobre resultado. Primera query todavía hace EnumWindows. |
| **SetWindowsHookEx (hook global de mouse/keyboard)** | Install de hooks LL_MOUSE / LL_KEYBOARD | ✅ **No aplica**: NewEra no usa hooks. |
| **Process name + PE metadata** | String matching contra nombres conocidos de bots | 🟡 **Parcial**: binarios renombrados a `NewEra.exe` / `NewEra-bridge.exe` (genéricos). PE metadata de Rust Cargo incluye version/authors por default. |
| **String scan de binary** | Busca literales suspechosos en memoria del proceso (`"cavebot"`, `"autoheal"`, `"tibia_bot"`) | 🟡 **Parcial**: `strip = "symbols"` en release profile elimina ~60% de Rust symbols. Literales de log y template paths aún presentes. |
| **Perfect-timing input** | Acciones con intervalos matemáticamente precisos (exactamente 100ms) son no-humanas | ✅ **Mitigado**: Gaussian jitter via `rand_distr::Normal`. `presend_jitter_mean=45ms std=15ms`. Cada action sample independiente. |
| **Uniform typing cadence** | Caracteres tipeados con spacing fijo (ej cada 133ms exacto) | ✅ **Mitigado 2026-04-17 v2**: `TYPING_MIN=3, MAX=8 ticks` uniformly sampled por char → 100-267ms entre teclas, absorbe pattern uniforme. |
| **HTTP endpoints expuestos a LAN** | Puerto abierto con rutas identificables (/cavebot/status, /metrics) | ✅ **Mitigado**: default `listen_addr = 127.0.0.1:8080` (loopback only). Antes `0.0.0.0:8080` permitía LAN scan identificar bot server. |
| **Strings "tibia-bot" / "tibia_bot" en binary** | String match contra naming conventions | 🟡 **Parcial**: startup log string cambiado a "NewEra runtime up". EnvFilter default "info" (sin target prefix). Crate name `tibia-bot` persiste por refactor cost; visible en PE metadata residual pero cubierto por `strip = symbols`. |
| **Pixel-perfect repeated clicks** | Clicks en (X, Y) exactos múltiples veces en la misma sesión | 🟡 **Parcial**: coords del cavebot son fijas por diseño (template-based). Mitigación limitada — se podría añadir jitter ±3px al transform HID, no implementado. |
| **Mouse paths demasiado rectos** | Movimiento cursor punto-a-punto sin curvas | 🟢 **No aplica en este setup**: Arduino HID teletransporta el cursor (AbsoluteMouse), no traza trayectoria. Al no haber path, no hay curva que analizar. Ventaja del modelo vs mouse relativo. |
| **Named pipes / shared memory con Tibia** | IPC sospechoso | ✅ **No aplica**: comunicación bot↔bridge via TCP localhost. |

---

## Setup anti-detection recomendado

### Config mínima safe para live

**`bridge/bridge_config.toml`**:

```toml
[input]
mode = "serial"          # Arduino HID — immune a BE

[focus]
enabled = true
poll_interval_ms = 2000  # NO bajar de 1000ms
debounce_count = 2

[serial]
port = "COM8"            # ajustar al tuyo
baud = 115200
```

**`bot/config.toml`** sección `[safety]`:

```toml
[safety]
humanize_timing       = true     # Gaussian jitter en todas las actions
presend_jitter_mean_ms = 45.0
presend_jitter_std_ms  = 15.0
reaction_hp_mean_ms    = 180.0   # reaction time realista ante HP drop
reaction_hp_std_ms     = 40.0
max_session_hours      = 4.0     # cortar antes de fatigue patterns detectables
```

### Build con stripping

El `[profile.release]` del workspace ya tiene `strip = "symbols"`. Verificar:

```bash
cd tibia-bot
cargo build --release -p tibia-bot -p pico-bridge
# Verificar tamaño más pequeño vs unstripped:
ls -la target/release/NewEra.exe target/release/NewEra-bridge.exe
```

Para debugging, usar el alternativo:

```bash
cargo build --profile release-with-debug -p tibia-bot
# Binary con símbolos completos + backtrace info, para diagnóstico.
# NO usar para live runs.
```

---

## Checklist pre-sesión (anti-detection)

Antes de arrancar una sesión live con cuenta real:

- [ ] `bridge_config.toml`: `mode = "serial"` (no sendinput)
- [ ] Arduino Leonardo flasheado + conectado + puerto COM en config
- [ ] `focus.enabled = true` pero con `poll_interval_ms >= 1000`
- [ ] `safety.humanize_timing = true`
- [ ] `safety.max_session_hours <= 4` para primera corrida
- [ ] Build release con strip activo (verificar tamaño del .exe)
- [ ] Tibia en primary monitor (requisito del Arduino HID descriptor)
- [ ] El bridge NO está corriendo en modo sendinput (audit con Task Manager + grep el config)
- [ ] No hay otros bots / hacks running en la máquina (BE flags co-occurrence)
- [ ] El navegador no está logueado con la misma cuenta de Tibia (session tracking)

Durante la sesión:

- [ ] Evitar minimize/maximize frecuente — da polling patterns
- [ ] Si necesitás pausar para cualquier motivo, usar el endpoint `/pause` del bot, NO alt-tab
- [ ] Anotar timing de breaks (humanos toman agua, mean); `breaks.rs` scheduler lo hace

Post-sesión:

- [ ] Logs del bot NO compartir públicamente (contienen timestamps + acciones que podrían correlacionarse server-side)
- [ ] Si hubo crash o anomaly, revisar si pudo haber generado patterns no-humanos antes del crash

---

## Riesgos residuales

### Behavioral fingerprinting (lo que no podemos mitigar completamente)

1. **Ruta exacta repetitiva**: el cavebot sigue los mismos nodes siempre. Un analizador de patrones server-side podría detectar que este char siempre camina `(32681,31686) → (32670,31660) → ...` en ese orden. Mitigación parcial: múltiples profiles de hunt rotando; no implementado.

2. **Activity patterns 24/7**: char online 8h/día todos los días = pattern no-humano. Mitigación: `BreakScheduler` toma pauses + `max_session_hours` cap, pero el agregado semanal sigue siendo suspicious si no hay offline windows realistas.

3. **Click timing distribution**: aunque individual clicks tienen jitter, la DISTRIBUCIÓN de timing (90% de acciones en 30-60ms window) podría diferir de humans (más varianza, fat tails). Rust usa `rand_distr::Normal` — podríamos migrar a distribution con fat tails (LogNormal o Pareto) pero no urgente.

4. **Perfect resource management**: el bot hereda/drinks potions en threshold exacto (< 40% HP, < 30% mana). Humans tienen mayor varianza. Mitigación: reaction_gate con `reaction_hp_mean_ms` helps, pero tu HP threshold es fijo en config.

### Game-level exploits que BE mira

1. **Multi-char simultáneo desde misma IP**: si corres 2 chars autónomos al mismo tiempo, correlación server-side. Mitigación: 1 char por sesión.

2. **Trade patterns suspicious**: si el bot siempre compra 100 manas al mismo NPC en el mismo orden, server flags. Mitigación parcial: variación de qty/frequency aleatoria (no implementado).

3. **Death recovery patterns**: tras morir, un humano toma varios minutos para organizarse; el bot retoma inmediatamente. Mitigación: NewEra pausa en `prompt_char_select` sin auto-respuesta — no es trivial pero es correcto.

---

## Verificación de mitigaciones

### 1. Confirmar bridge en modo serial

```powershell
# Con el bot corriendo, check logs del bridge.
# Debe decir: "Input mode: serial (puerto COM8)" o similar.
# Si dice "Input mode: sendinput" → CAMBIAR inmediatamente.
```

### 2. Confirmar focus poll interval

```powershell
# Log del bridge cada 2s debería mostrar check de foco.
# Si fires cada 100ms → config mal, editar bridge_config.toml.
```

### 3. Verificar strip en binary

```powershell
# Tamaño típico release stripped: ~4-6 MB.
# Si es >12 MB, strip no está activo.
Get-ChildItem target\release\NewEra.exe | Select-Object Length
```

### 4. Verificar no hay SendInput imports en bridge runtime

```powershell
# Usar dependencias de Windows solo cuando necesario.
# En modo serial, sendinput.rs compila pero nunca se ejecuta.
```

---

## Referencias

- [BattleEye detection techniques (unofficial)](https://www.unknowncheats.me/forum/anti-cheat-bypass/) — forum, anecdotes sobre BE
- [ADR-001 en este repo](../Obsidian/tibia-bot/decisions/ADR-001-input-mode-serial-vs-sendinput.md) — rationale de serial como default
- `bridge/src/sendinput.rs` docs inline — warning sobre detección
- [Arduino HID docs](https://www.arduino.cc/reference/en/libraries/hid-project/) — AbsoluteMouse descriptor

---

## Cuándo NO usar NewEra

- **Char de valor alto** (nivel >100, gear +1M gp): un ban es pérdida grande. Probar primero con char desechable por varias semanas.
- **Servidor con policy estricta contra botting** (ej: PvP servers, tournaments): riesgo alto.
- **Sin Arduino disponible**: modo sendinput tiene detection risk significativo. No usar con cuentas reales.
- **Primera vez que corrés el bot**: hacer 2-3 sesiones cortas (<30 min) observando antes de sesiones largas.

## Cuándo SÍ es razonablemente seguro

- Char desechable / leveling from 1
- Arduino HID configurado + Tibia en primary
- Sesiones < 4h con breaks
- Monitoring activo del bot durante la sesión (no fully unattended)
- Server casual sin reputación de bans agresivos
