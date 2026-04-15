# Test Coverage Report

Reporte de cobertura de pruebas del proyecto **tibia-bot** tras las Fases 1-5.

## Resumen ejecutivo

| Métrica | Valor |
|---|---|
| **Unit tests** (cargo test) | **142 / 142 passed** |
| **Integration tests** (Python suite) | **11 / 11 passed** |
| **Clippy errors** | 0 |
| **Clippy warnings nuevos** | 0 (solo pre-existentes) |
| **Release build** | ✓ bot + bridge + synth_frames + calibrate + 6 diagnóstico |
| **Tick rate medido** | 30.3 Hz (budget 30 Hz) |
| **bot_proc_ms medido** | 0.150 ms (budget 33 ms, **220× headroom**) |
| **Reaction delay medido** | 323 ms end-to-end (target 150–350 ms humano) |
| **Overruns en 2s de stress** | 0 |

---

## 1. Qué está completamente validado (automático)

### 1.1 Lógica pura (142 unit tests)
| Módulo | Tests | Cobertura |
|---|---|---|
| `act/coords.rs` | 10 | Viewport → Desktop → HID transforms |
| `act/keycode.rs` | 8 | Parseo de hotkey names a códigos HID |
| `core/fsm.rs` | 19 | Cooldowns, reaction, priority logic, integración waypoint+combat |
| `safety/timing.rs` | 7 | GaussSampler, 3σ clamp, determinismo si stddev=0 |
| `safety/reaction.rs` | 6 | ReactionGate rising/falling edge, reset, state persist |
| `safety/rate_limit.rs` | 4 | Sliding window, cap, expiry |
| `safety/variation.rs` | 5 | WeightedChoice distribution |
| `safety/breaks.rs` | 4 | Multi-level scheduler state machine |
| `safety/human_noise.rs` | 5 | Emisión con intervalo gaussiano |
| `scripting/mod.rs` | 9 | Sandbox, hooks, error capture, load_dir |
| `sense/vision/prompts.rs` | 6 | Template matching SSD normalizado |
| `sense/vision/*` | 30+ | HP/mana, battle list, status icons, minimap, calibration |
| `waypoints/mod.rs` | 15 | Step iteration, stuck watchdog, reset |

### 1.2 Pipeline end-to-end (11 integration tests)

Ejecutados con `python test_smoke/integration_tests.py` contra un bot corriendo + fake bridge + frames sintéticos inyectados vía `POST /test/inject_frame`.

| # | Test | Qué valida | Resultado medido |
|---|---|---|---|
| **01** | heal on HP critical | Inject HP=20% → FSM Emergency → heal key al bridge | `hp=0.199`, 2 heals, `KEY_TAP 0x3B` |
| **02** | attack on enemy | Inject 2 enemigos → FSM Fighting → attack al bridge | 2 entries detectadas, 1 `KEY_TAP 0x2C` |
| **03** | emergency beats fighting | HP crítico + enemigos → heal prioritario | primer key = heal, no attack |
| **04** | nominal → idle | Inject nominal → FSM Idle, sin keys residuales | HP=1.00, 0 keys |
| **05** | reaction time HP critical | Medir delay inject→primer heal | **323 ms** (target 150–350 ms) |
| **06** | scripts healthy | Lua scripts cargados sin errores | 2 files loaded, 0 errors |
| **07** | tick rate stable | 2 seg de ejecución continua | **30.3 Hz, 0 overruns** |
| **08** | bot_proc_ms under budget | Tiempo interno de tick | **0.150 ms** (budget 33 ms) |
| **09** | waypoints load and run | Hot-reload + iterator avance | 4 steps, avanzó a `walk_south` |
| **10** | inject rejects garbage | Endpoint valida PNG | PNG inválido rechazado correctamente |
| **11** | safety fields in /status | Schema del JSON | `safety_pause_reason` + `safety_rate_dropped` presentes |

---

## 2. Lo que la infra de test permite validar (disponible)

Con el endpoint `POST /test/inject_frame` + el binario `synth_frames`, **todas estas verificaciones son ahora automatizables**:

- ✅ Vision → HP/mana reading end-to-end
- ✅ Vision → battle list detection (Monster border)
- ✅ Vision → prompts detection (login / char_select / npc_trade) cuando haya templates
- ✅ FSM state transitions (Idle ↔ Walking ↔ Fighting ↔ Emergency)
- ✅ FSM priority ordering (Emergency > Fighting > Walking > Idle)
- ✅ Reaction gates — delay real medible desde inject hasta bridge.log
- ✅ Cooldowns jittered — variar distribución entre runs
- ✅ Waypoints — carga, ejecución, stuck watchdog, interrupción por combat
- ✅ Scripting Lua — hooks disparados por frames reales, override de heal
- ✅ Rate limiter — burst de acciones vía inyección rápida
- ✅ Break scheduler — con overrides de config para intervalos cortos
- ✅ Bridge — tráfico KEY_TAP/PONG via fake_bridge.py

**Cómo re-ejecutar la suite** (desde `tibia-bot/`):
```bash
# 1. Arrancar servers (idealmente vía preview_start en Claude Code)
# 2. Generar frames (una vez):
./target/release/synth_frames --out test_smoke/frames/nominal.png \
  --calibration assets/calibration.toml
./target/release/synth_frames --out test_smoke/frames/low_hp.png \
  --hp-ratio 0.20 --calibration assets/calibration.toml
./target/release/synth_frames --out test_smoke/frames/combat.png \
  --enemies 2 --calibration assets/calibration.toml
./target/release/synth_frames --out test_smoke/frames/combat_low_hp.png \
  --hp-ratio 0.10 --enemies 1 --calibration assets/calibration.toml

# 3. Ejecutar suite:
python test_smoke/integration_tests.py
```

---

## 3. Lo que SOLO puede validarse con hardware físico

Tras la infra de test añadida en esta iteración, la lista de verificaciones "hardware-only" se reduce a estos **5 items irreducibles**:

### 3.1 Pico 2 + Bridge — conectividad física
| # | Verificación | Estimación |
|---|---|---|
| 1 | Pico 2 flasheado con firmware HID correcto, COM port abierto, `pico_bridge.exe` arranca | 10 min primera vez |
| 2 | `KEY_TAP 0x3A` → Pico → Windows recibe F1 físico (verificar con cualquier app que muestre keystrokes) | 5 min |
| 3 | `MOUSE_MOVE` + `MOUSE_CLICK` llegan al desktop dual-monitor 3840×1080 con coordenadas correctas | 10 min |

### 3.2 OBS + NDI — capture física
| # | Verificación | Estimación |
|---|---|---|
| 4 | OBS + DistroAV con Tibia abierto, source NDI coincide con `config.toml`, el bot recibe frames reales | 10 min |

### 3.3 Tibia in-game
| # | Verificación | Estimación |
|---|---|---|
| 5 | Con los 4 items anteriores operativos: ejecutar 30 min de hunt observando `/status` y el comportamiento in-game. Si no hay overruns y el bot cura+ataca+camina, el pipeline completo funciona | 30 min |

**Total estimado: ~70 minutos de verificación manual una sola vez.**

---

## 4. Lo que NO se puede validar (por naturaleza del problema)

### 4.1 No detectabilidad contra BattleEye / CipSoft
Este es el objetivo final de Fase 5. **No existe un test binario** — solo se sabe con uso prolongado sin ban. Las medidas implementadas:

- ✅ Arquitectura physically-separate elimina vectores client-side
- ✅ Distribución gaussiana en cooldowns (no uniforme)
- ✅ Reaction delays ~180±40ms (humanos reales)
- ✅ Pre-send jitter ~45±15ms
- ✅ Rate limiter hard cap 8 actions/s
- ✅ Heal variation (spell/potion ponderado)
- ✅ Break scheduler multi-nivel opt-in
- ✅ Human noise emitter opt-in
- ✅ Prompt detection (auto-pausa, nunca auto-respuesta)
- ✅ Bridge watchdog (kill switch tras crash)

La única forma de validar esto es **tiempo de uso real sin ban**. Semanas, no horas.

### 4.2 Latencia end-to-end incluyendo NDI real
El budget ≤130 ms se compone de:
- NDI capture: ≤80 ms (solo medible con setup OBS real)
- Bot processing: ≤30 ms ✓ **medido: 0.15 ms**
- Command → Pico → HID: ≤15 ms (solo medible con Pico real)

La parte **bot processing** está validada. Las otras dos dependen del hardware.

---

## 5. Limitaciones documentadas (no son bugs)

| # | Limitación | Estado |
|---|---|---|
| 1 | Sin navegación espacial (waypoints son temporales, no usan coords de mundo) | **Diferido** — requiere OCR del minimap o tile hashing (~20–40h) |
| 2 | Restart-full tras combate, no resume-partial | **Aceptado** — mitigado con steps cortos. Depende de #1 |
| 3 | Sin OCR numérico de HP/mana (solo ratios) | **Aceptado** — pixel counting suficiente para umbrales |
| 4 | `on_enemy_found`, `on_waypoint_reached`, `on_status_gained` no implementados | **Aceptado** — MVP con `on_tick` + `on_low_hp` |
| 5 | Mouse humanization (Bezier) no implementada | **Aceptado** — el bot casi no usa clicks |

---

## 6. Quirks conocidos (cosméticos)

| Quirk | Causa | Impacto |
|---|---|---|
| `/test/pico/ping` falla primer intento (os error 10053) | Windows loopback TCP idle disconnect | Cosmético — la reconexión automática del PicoLink lo recupera |
| `fake_bridge.py` spam de errores HTTP | Algo polea :9000 como HTTP; los threads manejan excepciones | Solo ruido en logs, bot funciona |
| `vision_calibrated=false` sin frame | El flag depende de tener una perception con HP válido | Consistente con diseño |

---

## 7. Roadmap completado

| Fase | Estado | Tests nuevos |
|---|---|---|
| **Fase 1** — Plumbing (NDI + FSM + HTTP + vision) | ✅ pre-2026-04-08 | ~60 |
| **Fase 2** — Combate & Emergency | ✅ | +18 |
| **Fase 3** — Waypoints & stuck watchdog | ✅ | +18 |
| **Fase 4** — Scripting Lua | ✅ | +9 |
| **Fase 5** — Safety & anti-detection | ✅ | +37 |
| **Fase A** — Test infra (inject_frame, synth_frames, integration suite) | ✅ | +11 integration |
| **Total** | **142 unit + 11 integration** | |

---

## 8. Archivos de esta iteración

| Archivo | Rol |
|---|---|
| `bot/src/remote/http.rs` | +`POST /test/inject_frame` endpoint (PNG → FrameBuffer) |
| `bot/src/bin/synth_frames.rs` | **Nuevo** — generador de PNGs sintéticos desde calibration ROIs |
| `bot/Cargo.toml` | +binario `synth_frames` |
| `test_smoke/frames/*.png` | 4 frames base: nominal, low_hp, combat, combat_low_hp |
| `test_smoke/integration_tests.py` | Suite Python de 11 tests automáticos |
| `test_smoke/integration_results.json` | Output JSON con pass/fail + métricas |
| `test_smoke/COVERAGE.md` | **Este archivo** |

---

## 9. Siguiente paso recomendado

La **única acción restante** antes de usar el bot en producción es la **sesión de calibración hardware** (~70 min, una sola vez). Tras eso:

1. Captura 3 screenshots reales de:
   - `login.png` (pantalla de login del cliente)
   - `char_select.png` (lista de personajes)
   - `npc_trade.png` (modal buy/sell de cualquier shopkeeper NPC)

   Ponlos en `assets/templates/prompts/`. Los 3 son mutuamente exclusivos — no pueden aparecer al mismo tiempo.

   **Nota**: Tibia no tiene captchas, ni death screens, ni dialogs bloqueantes para deposit gold (es conversación por texto). Ver sección "Prompt detection" en `CLAUDE.md`.
2. Añade las 3 ROIs correspondientes a `calibration.toml` (`prompt_login`, `prompt_char_select`, `prompt_npc_trade`)
3. Re-ejecuta `python test_smoke/integration_tests.py` para confirmar no-regresión
4. Arranca el bot con hardware real y monitoriza `/status` durante la primera sesión corta (5–10 min)

Todo el código, todos los tests, y toda la infra de validación automatizable están **listos y en verde**.
