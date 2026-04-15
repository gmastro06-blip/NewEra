# Production runbook + smoke test checklist

Runbook ejecutable para validar el bot en vivo con NDI + HID bridge reales. Diseñado
para correr en orden de arriba a abajo en una sola sesión (~2-3h).

Cada fase tiene **comandos exactos** + **criterios pass/fail**. Si una fase falla,
parar y diagnosticar antes de seguir.

## Status (última sesión: 2026-04-15 session 3)

**Validado en vivo (end-to-end)**:
- Vision pipeline: HP/mana/battle list/inventory leyendo en tiempo real, HP range 50-100%
- **game_coords tracking con MinimapMatcher** (SSDNormalized), live validated en (32659, 31683, 7)
- FSM transitions Walking ↔ Fighting ↔ Idle durante combate real
- Lua healer druida level 11 (`zz_abdendriel_wasps_druid.lua`) con F3 exura + F1 mana pot
  validado por usuario visualmente en cliente Tibia
- Arduino Leonardo HID bridge (3.23ms RTT, reemplazo del Pico 2)
- Safety gates: `focus:tibia_not_foreground` + `prompt:*` + `break:*`
- Anchor tracking sidebar_top (score 0.05-0.06 vs umbral 0.30)
- Recording append mode 78049 snapshots / 43 min sin corrupción
- Hot-reload `/scripts/reload` (confirmado, bug previo era falso)
- Death recovery hook `on_fsm_state_change` para alertar muerte/disconnect
- **380+ lib tests pass**, 0 regresiones, build limpio

**Bugs fixados (Phase A + B de single-session plan)**:
- `recorder.rs`: `OpenOptions::append` (sobrevive restart del bot)
- `loop_.rs` SpellContext: fallback HP 0.5 (seguro ante vitals sostenido None)
- `loop_.rs` dead match arm: warn silencioso en lugar de panic
- `scripting/mod.rs`: reload confirmado working (bug reportado era falso)
- `calibration.toml` anchor `expected_roi = (1700, 0, 80, 70)` match exacto a make_anchors
- `game_coords.rs` **MinimapMatcher** SSDNormalized template matching reemplaza el
  dHash legacy que era frágil al anti-aliasing del cliente Tibia 12
- `game_coords.rs` `build_template` usa RGBA byte order correcto (byte[0]=R)
- `config.toml` `ndi_tile_scale = 2` (valor real empírico, no 5 como asumía el default)
- Re-validación periódica cada 30 detects para anti stuck-in-false-positive

**Ready for production**:
- ✅ Modo "assist healer": usuario mueve char manualmente, bot detecta vitals + dispara
  heal/mana pot/attack. 100% funcional hoy.
- ⏳ Modo "full cavebot": cavebot código listo pero pending live validation de
  node navigation (E.2) + refill loop (D.2+D.3+E.3). ~3-4h trabajo restante.

**No blockers** — todos los bugs críticos resueltos.

---

## Operational runbook (start/monitor/stop)

Use case: sesión de 30-120 min con modo "assist healer" (sin cavebot) o con cavebot
(cuando D.2+D.3 estén calibrados).

### Arranque

```powershell
# PC Gaming: Tibia + OBS con DistroAV NDI source activo
# Arduino Leonardo conectado en COM8 (verificar: arduino-cli board list)

cd C:\Users\gmast\Documents\GitHub\NewEra\tibia-bot

# 1. Bridge (console #1)
.\target\release\pico_bridge.exe bridge\bridge_config.toml

# 2. Bot (console #2)
.\target\release\tibia_bot.exe bot\config.toml assets

# 3. Monitoring (opcional, otro console)
cd monitoring ; docker-compose up -d
# Grafana en http://localhost:3000 admin/admin, dashboard "tibia-bot"

# 4. Start session script (opcional, wraps lo anterior)
.\scripts\start_session.ps1 -Label "hunt-20260415"
```

### Verificación post-arranque (primeros 30 seg)

```powershell
# Health check
curl http://localhost:8080/health

# Esperado: {"ok":true,"reason":"ok","details":{"has_frame":true,...}}
# Si reason="no_frame" → NDI no conectada. Verificar DistroAV.
# Si reason="paused_*" → check safety_pause_reason, resolver manualmente.

# Vitals + game_coords
curl http://localhost:8080/vision/perception | jq '.hp_percent, .mana_percent, .game_coords'

# Esperado: hp ~100, mana ~100, game_coords una tupla (x, y, z) NO null

# Anchor score (sidebar stability)
# Buscar en logs del bot: "Ancla: score=0.XX (umbral=0.30) FOUND"
# Score debe ser < 0.30 (SSD lower=better). Si > 0.30, regenerar template:
.\scripts\regen_anchor.ps1
```

### Monitoring durante la sesión

Los siguientes checks son útiles periódicamente (cada 15 min) o cuando algo parece raro:

```powershell
# Dispatch stats (cuántos heals/attacks han ido al cliente)
curl http://localhost:8080/dispatch/stats

# FSM debug (estado actual + cooldowns)
curl http://localhost:8080/fsm/debug

# Latencias de procesamiento
curl http://localhost:8080/health | jq '.details.bot_proc_ms, .details.ticks_overrun'
# Esperado: bot_proc_ms < 10ms typical, ticks_overrun crece lento (<1% del total)

# Script errors (si los hay, el bot queda funcional pero hay un bug Lua)
curl http://localhost:8080/scripts/status | jq '.last_errors'
```

### Protocolo de emergencia

**Escenario 1: HP cae peligrosamente**
1. `curl -X POST http://localhost:8080/pause` — pausa inmediata
2. Resolver manualmente en el cliente (drink potions, retreat)
3. `curl -X POST http://localhost:8080/resume` o restart bot si es grave

**Escenario 2: Char muerto (prompt:char_select)**
1. El death recovery hook Lua ya loggea el evento con `bot.log("error", ...)`
2. Bot queda en safety_pause automáticamente
3. Relogear el char manualmente en el cliente
4. El bot detecta que `prompt:char_select` ya no está, resume automáticamente (o curl /resume)
5. **Postmortem obligatorio**: `.\scripts\postmortem.ps1 sessions\<latest>.jsonl`

**Escenario 3: Bot stuck (nunca sale de un estado)**
1. `curl http://localhost:8080/fsm/debug` — ver current state + tick counters
2. Si cavebot stuck: `curl -X POST http://localhost:8080/cavebot/pause`
3. Si waypoints stuck: `curl -X POST http://localhost:8080/waypoints/pause`
4. Si FSM stuck en Fighting sin enemies reales: vision reader fallando. Restart bot.

**Escenario 4: Bridge desconectado**
Síntoma: logs muestran `PicoLink: error de conexión`
1. Verificar que `pico_bridge.exe` sigue corriendo
2. Si cayó: relanzarlo. El bot reconnectará automáticamente (backoff exponencial)
3. Si COM port cambió (post flash de Arduino, etc): actualizar `bridge/bridge_config.toml`
   y relanzar bridge

### Parada

```powershell
# 1. Pausar bot (seguro — no pierde la sesión)
curl -X POST http://localhost:8080/pause

# 2. Stop recording (si estaba activo)
curl -X POST http://localhost:8080/recording/stop

# 3. Postmortem
.\scripts\postmortem.ps1 sessions\<latest>.jsonl

# 4. Kill processes
taskkill /F /IM tibia_bot.exe
taskkill /F /IM pico_bridge.exe

# O con script:
.\scripts\stop_session.ps1
```

### Qué revisar en el postmortem

```
HP p50/p95 + min observed  → char health distribution
Mana p50/p95 + min observed → mana efficiency
In combat %                → hunt intensity
Max enemies                → peaks (swarm incidents)
Unique coords              → movement tracking (cavebot only)
Inventory peak (slots)     → supplies consumed
```

Red flags:
- HP min < 20% → healer late o muy stressed
- Ticks overrun > 5% → performance issue
- Unique coords = 1 (no movement) con recording largo → bot stuck
- Recording tamaño 0 bytes → recorder falló (check permissions)

---

## Ruta express: single-session ready-for-2h

Si solo querés validar el stack rápidamente sin pasar por todo el runbook:

```bash
# 1. Build clean (2-4 min primera vez)
cargo build --release && cargo test --release

# 2. Bridge arriba (PC gaming) — Arduino Leonardo en COM8
.\target\release\pico_bridge.exe bridge\bridge_config.toml

# 3. Bot arriba (PC bot)
./target/release/tibia_bot bot/config.toml assets

# 4. Health check
curl -s http://localhost:8080/health | jq .

# 5. Recording start + passive test (30 min)
curl -X POST "http://localhost:8080/recording/start?path=sessions/smoke.jsonl"
# Usuario mueve char manualmente, se deja pegar
# Observar: HP < 70% → F3 exura / Mana < 40% → F1 / HP < 25% → F2
curl -X POST http://localhost:8080/recording/stop

# 6. Postmortem
./target/release/replay_perception --input sessions/smoke.jsonl --summary
```

Si los 6 pasos anteriores pasan sin intervención, el bot está listo para
sesiones de 30-60 min supervisadas. Para sesiones 2h/día necesita además las
calibraciones de Phase G (click coords deposit/buy_item) y cavebot con nodes
(bloqueado por game_coords).

## Prerequisitos

- [ ] **PC gaming** (Windows) con Tibia + OBS + DistroAV corriendo
- [ ] **OBS NDI source** activa y visible en la red
- [ ] **Bridge binary** compilado (`cargo build --release -p pico-bridge`)
- [ ] **Raspberry Pi Pico 2** conectado al puerto serial, con firmware flasheado
- [ ] **PC bot** (Linux o Windows) en la misma LAN que el gaming PC
- [ ] `bot/config.toml` y `bridge/bridge_config.toml` creados desde los `.example`
- [ ] `assets/map_index.bin` generado (ver paso 0 si no existe)

---

## Fase 0 — Setup inicial (10 min)

### 0.1 Build release de ambos binarios

```bash
cd tibia-bot
cargo build --release
```

**Pass**: Compila sin errores ni warnings nuevos.
**Fail**: Stop. Fijar errores antes de seguir.

### 0.2 Verificar map index para tile-hashing

```bash
ls -lh assets/map_index.bin
```

**Pass**: Archivo existe, ~0.9 MB o más.
**Fail**: Generar:
```bash
cargo run --release --bin build_map_index -- \
    --map-dir assets/minimap/minimap \
    --output assets/map_index.bin \
    --floors 6,7,8
```

### 0.3 Configurar bot/config.toml

Editar:
```toml
[ndi]
source_name = "OBS Tibia"   # o como se llame tu source en DistroAV

[pico]
bridge_addr = "192.168.1.50:9000"   # IP del PC gaming

[game_coords]
map_index_path = "assets/map_index.bin"
```

### 0.4 Configurar bridge/bridge_config.toml

En el PC gaming, editar `bridge/bridge_config.toml`:
```toml
[tcp]
listen_addr = "0.0.0.0:9000"

[serial]
port = "COM3"    # verificar en Device Manager
baud = 115200

[input]
mode = "sendinput"    # o "serial" si usas Pico físico

[focus]
enabled = true
window_title = "Tibia"
```

---

## Fase A — Arranque básico (15 min)

### A.1 Arrancar bridge (PC gaming)

```powershell
# Windows
.\target\release\pico_bridge.exe bridge\bridge_config.toml
```

**Pass**: Log muestra:
- `Bridge listening on 0.0.0.0:9000`
- `Serial port COM3 opened at 115200 baud` (si mode=serial)
- `Focus gate enabled for "Tibia"` (si focus habilitado)

**Fail**:
- "COM3 not found" → verificar Device Manager, ajustar port
- "Bind failed" → puerto 9000 ya en uso, cerrar proceso previo

### A.2 Arrancar bot (PC bot)

```bash
cd tibia-bot
./target/release/tibia_bot bot/config.toml assets
```

**Pass**: Log muestra:
- `NDI receiver: conectado a "OBS Tibia"`
- `HTTP server escuchando en 0.0.0.0:8080`
- `PicoLink: conectado a 192.168.1.50:9000`
- `Cavebot cargado` (si tu config tiene path)
- No hay `ERROR` ni `panic`

**Fail**:
- `No se encontró la fuente NDI` → verificar DistroAV + network
- `PicoLink: error de conexión` → verificar IP del bridge, firewall
- `calibration.toml no disponible` → OK, pero visión limitada

### A.3 Verificar status vía HTTP

```bash
curl -s http://localhost:8080/status | jq .
```

**Pass**:
```json
{
  "has_frame": true,
  "vision_calibrated": true,
  "pico_latency_ms": 2.5,     // <10 ms
  "ndi_latency_ms": 35,        // <80 ms
  "bot_proc_ms": 5,            // <30 ms
  "ticks_overrun": 0,          // o muy pocos
  "fsm_state": "Idle",
  "is_paused": false
}
```

**Fail**: Cualquier campo en rojo indica problema de Fase A. Parar.

---

## Fase B — Capturar frames de referencia (10 min)

Posiciona el personaje en **un hunt spot típico** con:
- Combate activo (al menos 1 enemigo visible)
- Backpack con items variados (mana potions, UH runes, etc.)
- Depot chest visible en pantalla (si hunt cerca de ciudad)

### B.1 Capturar 10 frames

```bash
mkdir -p test_frames
for i in $(seq 1 10); do
    curl -s http://localhost:8080/test/grab -o test_frames/frame_$i.png
    sleep 1
done
ls -lh test_frames/
```

**Pass**: 10 PNGs de 2-4 MB cada uno.
**Fail**:
- <100 KB → frame vacío, revisar NDI
- 0 archivos → HTTP no responde, revisar arranque del bot

### B.2 Capturar frames específicos para calibración

```bash
# Frame con depot chest visible (para calibrar deposit)
curl -s http://localhost:8080/test/grab -o test_frames/depot_chest.png

# Frame con NPC trade window abierta (después de "hi" → "trade")
curl -s http://localhost:8080/test/grab -o test_frames/trade_window.png

# Frame con slot de mana potions con stack count visible
curl -s http://localhost:8080/test/grab -o test_frames/inventory_stack.png
```

---

## Fase C — Calibrar inventory grid (15 min)

### C.1 Verificar visualmente

```bash
curl -s http://localhost:8080/vision/grab/inventory -o inv_overlay.png
```

Abrir `inv_overlay.png` en un visor de imágenes.

**Pass**: Los 20 rectángulos amarillos cubren exactamente los 20 slots del backpack.

**Fail**: Editar `assets/calibration.toml`:
```toml
[inventory_grid]
x = <top-left X del primer slot, medido con GIMP>
y = <top-left Y>
slot_size = 32
gap = 2
cols = 4
rows = 5
```

Re-arrancar bot → re-testear.

### C.2 Verificar detección de items

```bash
curl -s http://localhost:8080/vision/inventory | jq .counts
```

**Pass**: Retorna items reales:
```json
{
  "mana_potion": 3,
  "ultimate_healing_rune": 2
}
```

**Fail**:
- Counts vacío con backpack lleno → thresholds mal, ir a Fase D
- Counts con items equivocados → templates mal, reemplazar con capturas reales (ver Fase D fallback)

---

## Fase D — Validar thresholds de templates (15 min)

### D.1 Correr validator con frames reales

```bash
cargo run --release --bin validate_templates -- \
    --frames test_frames \
    --templates assets/templates/inventory \
    --grid 1760,420,4,5,32,2 \
    --thresholds 0.05,0.10,0.15,0.20,0.25,0.30
```

**Pass**: Para cada item que tienes en el backpack, algún threshold produce hits ≥1 en varios frames. Ejemplo:
```
Template: mana_potion
  Best score observed: 0.08
  0.05:   0 hits (0.0%)
  0.10:  10 hits (2.5%)   ← primer threshold con match
  0.15:  10 hits (2.5%)
  → Threshold mínimo con match: 0.10
```

### D.2 Ajustar MATCH_THRESHOLD si necesario

Si el mejor score observado >0.15 para items reales:

Editar `bot/src/sense/vision/inventory.rs:30`:
```rust
const MATCH_THRESHOLD: f32 = 0.25;  // subir de 0.15
```

Re-compilar + re-arrancar + re-verificar `/vision/inventory`.

### D.3 Fallback — templates descargados no matchean

Si los templates del wiki (descargados de tibia.fandom.com) no matchean con tu NDI:

1. Screenshot del slot con mana potion real en el game
2. En GIMP: recortar exactamente el icono 32×32 (sin bordes del slot)
3. Guardar como `assets/templates/inventory/mana_potion.png` (sobreescribe el del wiki)
4. Repetir para cada item que uses

---

## Fase E — Calibrar digit templates para has_stack (20 min)

Solo si quieres usar `has_stack()` (precisión unidad-a-unidad). Si con `has_item()` ya te vale, puedes saltar esta fase.

### E.1 Identificar un slot con stack visible

En Tibia, mira un slot con al menos 2 dígitos visibles (ej. stack de 50 mana potions).

### E.2 Medir coords del número en GIMP

1. Abrir `test_frames/inventory_stack.png` en GIMP
2. Zoom al slot con el stack
3. Anotar la **esquina inferior-derecha del slot** donde empieza el número
4. Medir el rectángulo que contiene el número completo (típicamente 12×8 px)

Ejemplo: slot en `(1780, 420)` de 32×32 → número en aprox `(1792, 446, 18, 8)`.

### E.3 Correr calibration_helper

```bash
cargo run --release --bin calibration_helper -- \
    --frame test_frames/inventory_stack.png \
    --area 1792,446,18,8 \
    --digits 50 \
    --output assets/templates/digits
```

**Pass**: Output muestra:
```
Auto-segmentation found 2 column segments:
  0: x=0, w=4
  1: x=7, w=4

✓ Saved digit 5 → assets/templates/digits/5.png (4×6)
✓ Saved digit 0 → assets/templates/digits/0.png (4×6)
```

**Fail**: 
- Segments ≠ digit count → ajustar `--area` (más estrecho o más ancho)
- No segments → pixels muy débiles, ajustar contrast o zona exacta

### E.4 Repetir para cada dígito 0-9

Busca slots con diferentes stack counts hasta tener los 10:
- Slot "123" → captura digits 1, 2, 3
- Slot "456" → captura digits 4, 5, 6
- Slot "789" → captura digits 7, 8, 9
- Slot "10" o "100" → captura digit 0 (si no lo tienes ya)

**Pass**: `ls assets/templates/digits/` muestra 10 PNGs (0.png a 9.png).

### E.5 Re-arrancar bot y verificar

```bash
# Parar bot (Ctrl+C en su terminal)
# Re-arrancar
./target/release/tibia_bot bot/config.toml assets

# Verificar que los templates se cargan
grep "InventoryReader" logs.txt    # o consola del bot
# Expected: "InventoryReader: habilitado (20 slots)"
# Opcional: agregar log en load_digit_templates si hace falta
```

---

## Fase F — Lintear todos los scripts (5 min)

```bash
for script in assets/cavebot/*.toml; do
    echo "===== $script ====="
    cargo run --release --bin lint_cavebot -- "$script" 2>&1 | grep -E "errors|warnings|✗|⚠"
done
```

**Pass**: Todos los scripts con `0 errors`. Warnings aceptables.

**Fail**: Fijar los errors del script que vayas a usar:
- `has_item('xxx') — template not found` → typo o falta template PNG
- `Cycle without emitter` → loop infinito, revisar gotos
- `check_supplies ... label not found` → typo en `on_fail`

---

## Fase G — Calibrar coords de click en el script elegido (30 min)

Elegir un script para el live test (ej. `abdendriel_wasps.toml`).

### G.1 Calibrar deposit coords

1. Ir al depot chest manualmente con el personaje
2. Capturar frame:
   ```bash
   curl -s http://localhost:8080/test/grab -o depot_frame.png
   ```
3. Abrir en GIMP, medir centro del chest visible → `chest_vx, chest_vy`
4. Right-click manual en el chest, anotar dónde aparece "Stow all" → `stow_vx, stow_vy`
5. Editar el script:
   ```toml
   [[step]]
   kind = "deposit"
   chest_vx = <X medido>
   chest_vy = <Y medido>
   stow_vx  = <X stow all>
   stow_vy  = <Y stow all>
   ```

### G.2 Calibrar buy_item coords

1. Manualmente hablar con el NPC: `hi` → `trade`
2. Capturar con la trade window abierta:
   ```bash
   curl -s http://localhost:8080/test/grab -o trade_frame.png
   ```
3. En GIMP medir:
   - Centro del item "Mana Potion" en la lista → `item_vx, item_vy`
   - Botón "Buy 1" (o equivalente) → `confirm_vx, confirm_vy`
4. Editar el script con esas 4 coords

### G.3 Lintear el script modificado

```bash
cargo run --release --bin lint_cavebot -- assets/cavebot/tu_script.toml
```

**Pass**: `0 errors`.

---

## Fase H — Smoke test del ciclo completo (30 min)

### H.1 Cargar el script via HTTP

```bash
curl -X POST "http://localhost:8080/cavebot/load?path=assets/cavebot/tu_script.toml&enabled=true"
```

**Pass**:
```bash
curl -s http://localhost:8080/cavebot/status | jq .
# {
#   "cavebot_loaded": true,
#   "cavebot_enabled": true,
#   "cavebot_total_steps": 76,
#   ...
# }
```

### H.2 Monitorear durante 10 minutos

Terminal 1 — status:
```bash
watch -n 3 'curl -s http://localhost:8080/status | jq "{
  tick, fsm_state, ticks_overrun,
  cavebot_step: .cavebot_step,
  cavebot_kind: .cavebot_kind,
  safety: .safety_pause_reason
}"'
```

Terminal 2 — combat events:
```bash
watch -n 5 'curl -s http://localhost:8080/combat/events | jq ".events | last"'
```

Terminal 3 — inventory changes:
```bash
watch -n 10 'curl -s http://localhost:8080/vision/inventory | jq .counts'
```

### H.3 Pass criteria (tras 10 min de hunt activo)

- [ ] `ticks_overrun / ticks_total < 0.05` (menos 5% overrun rate)
- [ ] `cavebot_step` cambia coherentemente (no atascado en 1 step >60s)
- [ ] `fsm_state` cicla entre Idle, Fighting, Walking, Emergency (si HP baja)
- [ ] `hp_ratio` estable >30% (heal funciona)
- [ ] `mana_ratio` visible, se recupera con potions
- [ ] `inventory.counts` decrece a medida que usas potions
- [ ] Si el script tiene `check_supplies` → cuando se agotan, salta a `refill`
- [ ] `deposit` emite right-click + stow click en el momento correcto
- [ ] `buy_item` emite select + N confirms
- [ ] No errores `WARN` repetidos (>3 del mismo tipo)
- [ ] No `tick overrun >100ms` en logs

### H.4 Fail criteria (parar y diagnosticar)

- [ ] Personaje muere → heal threshold mal, o spell wrong hidcode
- [ ] Personaje atascado en un tile >30s → node navigation falla
- [ ] `ticks_overrun` crece rápido (>10%/s) → performance problem
- [ ] Bot emite clicks en posiciones aleatorias → calibración mal
- [ ] `PicoLink: conexión perdida` repetido → network issue
- [ ] `Cavebot: más de 64 saltos en un tick` → loop infinito en script

---

## Fase I — Métricas y dashboards (opcional, 20 min)

### I.1 Verificar Prometheus endpoint

```bash
curl -s http://localhost:8080/metrics | head -30
```

**Pass**: Output en formato Prometheus con métricas `tibia_bot_*`.

### I.2 (Opcional) Montar Grafana

```yaml
# docker-compose.yml
services:
  prometheus:
    image: prom/prometheus
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml
    ports: ["9090:9090"]
  grafana:
    image: grafana/grafana
    ports: ["3000:3000"]
```

```yaml
# prometheus.yml
scrape_configs:
  - job_name: tibia_bot
    static_configs:
      - targets: ['host.docker.internal:8080']
    metrics_path: /metrics
    scrape_interval: 5s
```

Métricas recomendadas para graficar:
- `rate(tibia_bot_ticks_total[1m])` — ticks/sec (debe ser ~30)
- `rate(tibia_bot_ticks_overrun_total[1m]) / rate(tibia_bot_ticks_total[1m])` — overrun rate %
- `tibia_bot_ndi_latency_ms`, `tibia_bot_pico_latency_ms`, `tibia_bot_proc_ms`
- `tibia_bot_hp_ratio`, `tibia_bot_mana_ratio` (timeseries)
- `tibia_bot_inventory_slots{item="mana_potion"}` (por item)

---

## Fase J — Sesión extendida (opcional, 1-2h)

Tras pasar Fase H exitosamente, correr el bot durante 1-2 horas de hunt real.

### Verificar cada 15 min

```bash
curl -s http://localhost:8080/status | jq "{
  ticks_total,
  overrun_pct: (.ticks_overrun / .ticks_total * 100),
  fsm_state,
  latencies: {ndi: .ndi_latency_ms, pico: .pico_latency_ms, proc: .bot_proc_ms}
}"
```

**Target**:
- `overrun_pct < 2%`
- Latencias estables (no crecen con el tiempo)
- Memoria del proceso estable (sin leak)

### Si algo raro pasa

```bash
# Pausar bot
curl -X POST http://localhost:8080/pause

# Capturar debug snapshots
curl -s http://localhost:8080/vision/grab/debug -o debug_$(date +%s).png
curl -s http://localhost:8080/fsm/debug | jq . > fsm_debug.json
curl -s http://localhost:8080/combat/events | jq . > combat_log.json

# Revisar logs del bot
tail -100 bot.log | grep -E "WARN|ERROR"

# Resumir
curl -X POST http://localhost:8080/resume  # solo si diagnóstico OK
```

---

## Criterios de "99% alcanzado"

Tras completar todas las fases A-H sin fallos:

- [x] Fase A: bot + bridge arrancan sin error
- [x] Fase B: 10 frames capturables con calidad
- [x] Fase C: inventory grid calibrado, counts correctos
- [x] Fase D: thresholds ajustados a templates reales (o fallback con templates reales del game)
- [ ] Fase E: digit templates calibrados (opcional — para `has_stack`)
- [x] Fase F: todos los scripts lintean sin errors
- [x] Fase G: deposit + buy_item coords calibradas
- [x] Fase H: 10 min de hunt exitoso con todos los pass criteria
- [ ] Fase J: 1-2h de sesión extendida sin degradación

**99% real** = A-H ✓ + J ✓

---

## Rollback rápido

Si algo va muy mal durante el live test:

```bash
# 1. Pausar bot inmediatamente
curl -X POST http://localhost:8080/pause

# 2. Limpiar estado
curl -X POST http://localhost:8080/cavebot/clear

# 3. Si sigues con problemas, kill el proceso
# Windows: Ctrl+C o kill vía Task Manager
# Linux: Ctrl+C

# 4. Kill del bridge en el PC gaming también

# 5. El personaje queda donde estaba. Manual para moverlo si es crítico.
```

---

## Recursos

- **Runbook básico** (este archivo)
- **CLAUDE.md**: referencia completa de arquitectura + modules
- **config.toml.example**: todos los campos documentados
- **calibration.toml**: ROIs + grids
- **Binarios diagnóstico**: `validate_templates`, `lint_cavebot`, `calibration_helper`, `inspect_pixel`, `rgb_dump`, `diff_frames`
- **HTTP API**: ver sección "HTTP Diagnostics" en CLAUDE.md para todos los endpoints

## Post-live-test feedback loop

Cuando termines el live test, reporta:

1. **Qué fase falló** (si alguna)
2. **Thresholds finales** que terminaste usando (inventory, status, anchors)
3. **Coords finales** para deposit/buy_item (para docs futuros)
4. **Tiempo de overrun medio** tras 1h
5. **Items/ideas no cubiertos** por el runbook

---

## Emergency stop protocol

Durante operación 2h/día, si hay que abortar una sesión inmediatamente:

```powershell
# Option 1: pausa graceful (el bridge sigue vivo, bot para de actuar)
curl -X POST http://localhost:8080/pause

# Option 2: kill proceso (si HTTP no responde)
taskkill /F /IM tibia_bot.exe
taskkill /F /IM pico_bridge.exe

# Option 3: alt-tab fuera de Tibia (el bot detecta focus:tibia_not_foreground
# y pausa automáticamente dentro de 100ms). Esto NO mata el bot, solo lo frena
# hasta que Tibia vuelva a tener foco.
```

Después de cualquier emergency stop:
1. Revisar el último snapshot de recording: `replay_perception --input <latest.jsonl> --filter hp_below:30`
2. Revisar logs del bot (stderr): buscar ERROR/WARN repetidos
3. Si hubo muerte del char: NO reanudar hasta revisar por qué el healer no salvó

---

## Game_coords troubleshooting (parked investigation)

Si `curl /vision/perception | jq .game_coords` retorna `null`:

```bash
# 1. Capturar frame con un coord conocido (mirá el char en el juego,
#    leé su posición en el minimap UI de Tibia, anotala).
curl http://localhost:8080/test/grab -o frame_for_diag.png

# 2. Correr el diagnóstico pixel-a-pixel.
./target/release/diff_minimap_pixels \
    --frame frame_for_diag.png \
    --minimap-roi <x,y,w,h> \
    --char-coord <x,y,z> \
    --map-dir assets/minimap/minimap \
    --scale 5 \
    --output diag.png

# 3. El tool imprime un diagnóstico automático:
#    - "shift uniforme" → problema de color space (NDI/OBS)
#    - "variación alta sin shift" → smoothing del cliente Tibia
#    - "shift alto + variación alta" → ROI mal o scale mal
#    - "todo low" → el problema está en dhash(), no en los pixels
```

Mientras no se resuelva game_coords, usar cavebot con `walk` steps temporales
en lugar de `node` steps absolutos. Documentar en el script.

---

## Lua hot-reload (Phase A.4 fix)

Tras el fix de Phase A.4, `/scripts/reload` resetea completamente el Lua
runtime antes de re-cargar los .lua del directorio. Esto garantiza que:

- Cambios en locals (ej `EMERGENCY_COOLDOWN_TICKS = 30`) se apliquen
- Funciones que capturaban upvalues viejos sean reemplazadas
- La API `bot.*` siga disponible (se re-instala automáticamente)

```bash
# Editar el .lua
# ...

# Reload
curl -X POST http://localhost:8080/scripts/reload

# Verificar que cargó sin errores
curl -s http://localhost:8080/scripts/status | jq .
```

Si `/scripts/status` retorna errores, revisar el syntax del .lua y volver a
reload. **NO es necesario reiniciar el bot** para cambios en scripts Lua.

---

## Recording append (Phase A.1 fix)

El `PerceptionRecorder` ahora abre archivos en **append mode** en lugar de
truncate. Esto significa que si el bot se reinicia durante una sesión de 2h,
la data previa se preserva:

```bash
# Primera sesión
curl -X POST "http://localhost:8080/recording/start?path=sessions/day1.jsonl"
# ... bot corre 1h, se reinicia por bug ...

# Segunda sesión (mismo path)
curl -X POST "http://localhost:8080/recording/start?path=sessions/day1.jsonl"
# Los records nuevos se appendean, no sobrescriben

# Postmortem incluye TODO el histórico
./target/release/replay_perception --input sessions/day1.jsonl --summary
```

Para separar sesiones físicamente, usar paths con timestamp:

```powershell
$ts = Get-Date -Format "yyyy-MM-dd_HHmm"
curl -X POST "http://localhost:8080/recording/start?path=sessions/session_$ts.jsonl"
```
