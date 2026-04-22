# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Working Rules

- Think before acting. Read files before writing code.
- Edit only what changes — never rewrite entire files.
- Do not re-read files already read unless they changed.
- Do not repeat unchanged code in responses.
- No preambles, no trailing summaries, no explaining the obvious.
- Test before marking anything as done.

## Tercer ojo abierto y honestidad brutal (regla maestra)

Antes de responder o commitear, **abrí el tercer ojo**: verificar activamente
qué es mecánicamente verificado vs qué es scaffolding / data inventada / no
testeado. Aplicá siempre al cerrar una tarea:

1. **Corré el comando que validaría la afirmación antes de afirmarla.**
   - Si decís "tests pasan" → `cargo test` corrió y viste el output.
   - Si decís "compila" → `cargo check --features ...` corrió con exit 0.
   - Si decís "funciona live" → el bot corrió contra NDI + Tibia real.
   - Si no se corrió, decí "NO testeado".

2. **Separar siempre 3 categorías en cada entrega:**
   - **Mecánico verificado**: commits, builds, tests cuyo output vi.
   - **Data generada por mí** (wiki, memoria, estimación): listar fuente.
   - **No testeado**: sin eufemismos. "Pendiente validación X".

3. **No narrar scaffolding como "cerrado".**
   - "Fase cerrada al 100%" es mentira si es sólo código + unit tests sintéticos.
   - Usar: "archivos creados, tests unit pasan, pendiente validación runtime".

4. **No inventar números.**
   - Thresholds, latencias, tamaños: si no los medí, decirlo.
   - "+10 MB al binary" solo si corriste `ls -la target/release/NewEra`.
   - "~5ms/tick overhead" solo con benchmark real.

5. **Flaggear errores de compile / API no validados.**
   - Al usar crate nuevo (ej. `ort v2`), `cargo check --features X` debe correr
     antes de commitear el código que lo usa. Si no, decir explícitamente
     "no probé build con feature X".

6. **Cuando el user pida "estado real sin mentir"**, listar explícitamente
   qué afirmaciones previas fueron exageradas/engañosas. No defender.

7. **Si una tarea no se puede completar** (falta dependency, requiere live,
   requiere modelo entrenado, etc.), **listarla explícitamente**. No
   substituir con scaffolding que parece completado.

Ref: memorias `feedback_honest_status_reporting.md` y
`feedback_no_live_until_trust.md` acumulan ejemplos concretos de violaciones
de esta regla.

## Project Overview

**tibia-bot** is a distributed Tibia game automation system across two machines:
- **PC Gaming (Windows)**: Runs Tibia + OBS with DistroAV NDI output + the `bridge` binary + Arduino Leonardo via USB (firmware en `arduino/tibia_hid_bridge/`, usa librería HID-Project `AbsoluteMouse`)
- **PC Processor (Linux)**: Runs the main `bot` binary — video capture, vision, FSM, actuator commands

All code is Rust in a Cargo workspace with two members: `bot/` and `bridge/`.

## Build & Run

```bash
# Build both members (default = sin ML)
cargo build --release

# Build con ML runtime opt-in (ort + ndarray, +~10 MB binary).
# Requiere libonnxruntime instalada en sistema y ORT_LIB_LOCATION env var.
cargo build --release --bin NewEra --features ml-runtime

# Run the bot (from tibia-bot/)
cd tibia-bot && ./target/release/NewEra bot/config.toml assets

# Run the bridge (on Windows gaming PC)
./target/release/NewEra-bridge bridge/bridge_config.toml

# Lint / format
cargo clippy
cargo fmt
```

**Config setup**: Copy `bot/config.toml.example` → `bot/config.toml` and `bridge/bridge_config.toml.example` → `bridge/bridge_config.toml` before first run.

## Tests

```bash
cargo test --release
# Coordinate transform tests (most comprehensive):
cargo test coords -- --nocapture
```

## Diagnostic Binaries

```bash
cargo run --release --bin calibrate      # egui GUI for ROI calibration
cargo run --release --bin scan_rois      # automated ROI scanning
cargo run --release --bin test_vision    # run vision pipeline on a live frame
cargo run --release --bin make_anchors   # generate anchor template PNGs
cargo run --release --bin inspect_pixel  # single pixel color lookup
cargo run --release --bin rgb_dump       # dump pixel RGB for a region
cargo run --release --bin diff_frames    # frame difference analysis
cargo run --release --bin synth_frames   # generate synthetic test frames (HP ratio, enemy count, etc.)
cargo run --release --bin build_map_index -- --map-dir <path> --output assets/map_index.bin [--floors 6,7,8]
                                          # build dHash index for tile-hashing (TibiaMaps.io PNGs)
cargo run --release --bin validate_templates -- --frames <dir> --templates <dir> \
                                          # sweep thresholds over real frames, report hits%
  --grid 1760,420,4,5,32,2 --thresholds 0.05,0.10,0.15,0.20,0.25,0.30
cargo run --release --bin lint_cavebot -- <script.toml>
                                          # static analysis: orphan labels, bad coords, missing templates
cargo run --release --bin calibration_helper -- --frame frame.png --area x,y,w,h \
                                          # extract digit templates from a real frame for has_stack()
  --digits 50 --output assets/templates/digits
cargo run --release --bin replay_perception -- --input session.jsonl [--summary | --trace | --filter X]
                                          # offline analysis of recorded perception JSONL (F1)
cargo run --release --bin path_finder -- --walkability assets/walkability.bin \
                                          # multi-floor A* between 2 absolute coords (F4)
  --from X,Y,Z --to X,Y,Z [--simplify] [--overrides assets/pathfinding_overrides.toml] [--output snippet.toml]
cargo run --release --bin extract_inventory_slots -- --frame frame.png --output /tmp/slots
                                          # recorta 16 slots del inventory_backpack_strip para calibrar templates live
cargo run --release --bin tune_inventory_strip -- --frame frame.png --x 1567 --y 22 ...
                                          # dibuja overlay de slots sobre frame para calibrar backpack_h/offsets
cargo run --release --bin label_dataset -- --manifest datasets/v1/manifest.csv \
                                          # CLI etiquetado offline para dataset ML (Fase 2.3)
  --classes vial,golden_backpack,empty [--relabel]
cargo run --release --bin click_live -- --coord X,Y --button L [--verify-template NAME]
                                          # standalone click validator (Fase 1 ADR-003)
```

To also build the walkability grid for pathfinding, add `--walkability assets/walkability.bin` to the `build_map_index` command — it parses `Minimap_WaypointCost_*.png` from the same map dir.

**Benchmarks** (criterion):
```bash
cargo bench --bench template_matching     # UiDetector template performance
cargo bench --bench pathfinding           # A* performance sobre walkability grid
cargo bench --bench fase1_overhead        # Fase 1.4 shift-tolerance + Fase 1.5 region_monitor
```

## Architecture

### Data flow (30 Hz game loop)

```
NDI thread          Game loop (30 Hz)              Arduino bridge
──────────          ─────────────────              ──────────────
OBS NDI stream      ┌──────────────────┐           bridge binary
    ↓               │ 1. Sense         │           (TCP :9000 ↔
FrameBuffer ──────► │    read frame    │           serial COM)
(ArcSwap)           │    run vision    │               ↑
                    │ 2. Think         │           TCP commands
                    │    update FSM    │               │
                    │ 3. Act ──────────┼──────────►PicoLink
                    │    send command  │           (pico_link.rs — nombre legacy)
                    └──────────────────┘
                           ↕
                    SharedState (RwLock)
                           ↕
                    HTTP server :8080 (axum)
```

### Key modules

| File | Role |
|------|------|
| `bot/src/core/loop_.rs` | 30 Hz absolute-deadline scheduler — tick = Sense→Think→Act |
| `bot/src/core/fsm.rs` | Priority FSM: Pause → Emergency → Combat → Waypoints → Idle |
| `bot/src/core/state.rs` | `GameState` / `SharedState` (all RwLock-guarded), `Metrics` |
| `bot/src/sense/ndi_receiver.rs` | Loads NDI runtime via `libloading`, auto-reconnects |
| `bot/src/sense/frame_buffer.rs` | `ArcSwap<Option<Frame>>` — lock-free single-producer, N-consumer |
| `bot/src/sense/vision/mod.rs` | Vision orchestrator: calibration + anchors + templates → `Perception` |
| `bot/src/sense/vision/hp_mana.rs` | Pixel counting on color bars (not edge detection) |
| `bot/src/sense/vision/anchors.rs` | Template matching for reference points; detects window drift |
| `bot/src/sense/vision/game_coords.rs` | Tile-hashing of minimap → absolute (x,y,z) via dHash + MapIndex |
| `bot/src/sense/vision/inventory.rs` | Template matching SSE + per-template thresholds + NMS + stack strip + shift tolerance (Fase 1.1-1.4) |
| `bot/src/sense/vision/inventory_ml.rs` | ML classifier opcional (ONNX via ort, feature `ml-runtime`); fallback transparente a SSE matcher |
| `bot/src/sense/vision/inventory_ocr.rs` | Digit OCR bottom-right 16×8 px para stack counts (1..999) |
| `bot/src/sense/vision/region_monitor.rs` | Framework diff L1 per-región frame-a-frame (Fase 1.5) |
| `bot/src/sense/dataset_recorder.rs` | Captura crops 32×32 para training ML; manifest CSV + PNGs en `datasets/` (Fase 2.1) |
| `bot/src/act/pico_link.rs` | Async TCP client with exponential backoff and 100ms per-command timeout (nombre legacy — hoy habla con Arduino Leonardo) |
| `bot/src/act/coords.rs` | Viewport → Desktop → HID absolute coordinate transforms |
| `bot/src/remote/http.rs` | axum REST API: `/status`, `/pause`, `/resume`, `/vision/*`, `/test/*` |
| `bot/src/cavebot/` | Hunt automation: labels, goto jumps, conditionals, stand_until |
| `bot/src/safety/` | Behavioral humanization: timing jitter, reaction gates, rate limits, breaks |
| `bot/src/scripting/mod.rs` | Lua 5.4 engine (sandboxed, non-Send): `on_tick`, `on_low_hp` hooks |
| `bridge/src/main.rs` | Single-file bidirectional TCP↔serial proxy |

### Coordinate system

Three-stage transform (all unit-tested in `act/coords.rs`):
1. **Viewport coords** — pixel position within the NDI-captured crop
2. **Desktop coords** — add window offset from `CoordsConfig`
3. **HID absolute** — scale to 0–32767 range for Arduino HID reports

### NDI pixel format

Frames from DistroAV+OBS arrive as **RGBA** (byte[0]=R, byte[2]=B). The `fourcc` field determines actual layout; hardcoded RGBA is confirmed for this setup.

### Vision: HP/mana bars

Count total matching pixels, not edge detection. Edge detection breaks when text overlays the bars. ~5% error from overlays is acceptable for FSM thresholds.

### Calibration & anchors

- ROI coordinates live in `assets/calibration.toml` (TOML-based, all optional — vision degrades gracefully)
- Anchor PNGs live in `assets/anchors/` (reference templates for window position tracking)
- Workflow: add one manual anchor → validate live → expand. Do not block on calibration GUI features.

## Latency Budget

| Segment | Target |
|---------|--------|
| NDI capture (gaming → bot) | ≤ 80 ms |
| Bot processing per tick | ≤ 30 ms |
| Command → bridge → Arduino → HID | ≤ 15 ms |
| **End-to-end** | **≤ 130 ms** |

## HID Bridge Command Protocol

ASCII line-based over TCP (bot → bridge → Arduino serial at 115200 baud):

```
MOUSE_MOVE <x> <y>
MOUSE_CLICK
KEY_TAP <hid_code>
PING
```

Reply is `OK\n` or `PONG\n`. Timeout per command: 100 ms with exponential backoff on reconnect.

## Input modes — sendinput vs serial (Arduino HID)

El bridge soporta dos caminos de inyección de input, configurables via `bridge_config.toml`:

```toml
[input]
mode = "sendinput"   # Windows SendInput API, sin hardware extra
# mode = "serial"    # Arduino Leonardo via USB HID
```

### Cuándo usar cada uno

| Caso | Modo recomendado | Razón |
|------|------------------|-------|
| Setup single-monitor | sendinput | simple, sin hardware |
| Multi-monitor + Tibia en primary | serial (Arduino) | hardware input indistinguible de mouse físico — inmune a anti-injection hooks; viewport sigue OK en ambos modos, pero algunos widgets del sidebar pueden rechazar SendInput |
| Multi-monitor + Tibia en secondary | sendinput (con `MOUSEEVENTF_VIRTUALDESK`) | Arduino HID no llega al secondary (ver abajo) |

### Requisito crítico del modo serial: Tibia en primary monitor

El descriptor HID del Arduino (via librería HID-Project `AbsoluteMouse`) solo targetea el **primary monitor** de Windows. Si Tibia está en un monitor secundario, los clicks nunca llegan a la ventana.

Al boot, el bot hace una safety check: si el centro de la ventana de Tibia cae fuera del vscreen reportado por el bridge, pausa con reason `tibia_off_mapped_screen` y un mensaje indicando que hay que mover Tibia al primary. En modo serial el bridge reporta `vscreen = primary monitor` para que el check natural funcione.

**Cómo mover Tibia a primary**:
1. Windows → Configuración → Sistema → Pantalla → seleccionar el monitor donde está Tibia → "Convertir en pantalla principal", o
2. Arrastrar la ventana de Tibia al monitor que ya es primary.

### HID descriptor signed: firmware offset

El firmware `arduino/tibia_hid_bridge/tibia_hid_bridge.ino` aplica offset `x*2 - 32768` en el handler `MOUSE_MOVE` para mapear el rango unsigned `0..32767` del protocolo del bot al rango **signed int16 (`-32768..32767`)** que espera el descriptor HID. Sin este offset, solo cubría el cuadrante inferior-derecho del monitor.

Ver session note `Obsidian/tibia-bot/sessions/2026-04-16-V7-unblocked.md` para el diagnóstico completo.

## HTTP Diagnostics (port 8080)

```
GET  /status                  — JSON: tick count, FSM state, latencies, metrics
POST /pause | /resume         — pause/resume bot

# Test / diagnostics
POST /test/pico/ping          — ping HID bridge (endpoint nombre legacy), measure RTT
GET  /test/grab               — current NDI frame as PNG
POST /test/click              — test click at viewport coords {"x":N,"y":N}
POST /test/heal               — test heal action
POST /test/key                — test key tap
POST /test/inject_frame       — inject a test frame into the pipeline

# Vision
GET  /vision/perception       — current Perception JSON
GET  /vision/vitals           — HP/mana values
GET  /vision/battle           — battle list
GET  /vision/status           — active status conditions
GET  /vision/grab/anchors     — PNG with ROIs and anchors overlaid (debug)
GET  /vision/grab/battle      — cropped battle list ROI PNG (3× scale)
GET  /vision/grab/debug       — annotated full debug frame PNG
GET  /vision/battle/debug     — per-slot battle diagnostics JSON
GET  /vision/target/debug     — target detection debug
GET  /vision/loot/debug       — loot detection debug JSON
GET  /vision/loot/grab        — loot area crop PNG
GET  /vision/grab/inventory   — frame with backpack slots drawn (yellow boxes)
GET  /vision/inventory        — JSON: slot count + detected item counts + grid config
GET  /vision/region_monitor   — JSON: diff ratio per-región (Fase 1.5 wire)

# FSM / combat
GET  /fsm/debug               — FSM internal state
GET  /combat/events           — combat event log
GET  /dispatch/stats          — action counters (attacks, heals, etc.)

# Waypoints
POST /waypoints/load?path=    — hot-reload waypoint file
GET  /waypoints/status        — waypoint engine state
POST /waypoints/pause|resume  — pause/resume waypoints
POST /waypoints/clear         — clear waypoint list

# Cavebot
POST /cavebot/load?path=      — hot-reload cavebot script
GET  /cavebot/status          — cavebot engine state
POST /cavebot/pause|resume    — pause/resume cavebot
POST /cavebot/clear           — clear cavebot script

# Scripting
POST /scripts/reload          — hot-reload Lua scripts
GET  /scripts/status          — Lua engine status

# Monitoring
GET  /metrics                 — Prometheus/OpenMetrics format (ticks, latencies, HP/mana, inventory)

# Recording (F1)
POST /recording/start?path=X  — start writing perception snapshots to X.jsonl
POST /recording/stop          — stop writing and flush

# Dataset capture para training ML (Fase 2.2)
POST /dataset/start?dir=X&interval=N&tag=S  — start capturing inventory slots crops
POST /dataset/stop            — stop capture + flush manifest CSV
GET  /dataset/status          — {"active":bool,"crops_total":N,"dir":"..."}
```

## Monitoring stack (Prometheus + Grafana)

`monitoring/docker-compose.yml` levanta Prometheus + Grafana con un dashboard `tibia-bot` pre-cargado (9 paneles: status, NDI, tick proc, bridge RTT, enemies, HP/Mana over time, latencies, throughput, inventory slots).

```bash
cd monitoring/
docker-compose up -d
# Prometheus en http://localhost:9090
# Grafana en http://localhost:3000 (admin/admin), dashboard "tibia-bot" auto-cargado
```

Prometheus scrapea `host.docker.internal:8080/metrics` cada 5s. Funciona out-of-the-box en Windows/Mac. En Linux, `extra_hosts: host.docker.internal:host-gateway` ya está configurado.

## Recording & replay (offline debugging)

`bot/src/sense/recorder.rs` records `PerceptionSnapshot` to a JSONL file every N ticks. Use it to capture a live session and analyze it offline without the bot running.

**Enable** in `config.toml`:
```toml
[recording]
enabled = true
path = "session.jsonl"
interval_ticks = 30   # 1 snapshot/sec at 30 Hz
```

**Or trigger via HTTP**:
```
POST /recording/start?path=session.jsonl
POST /recording/stop
```

**Analyze**:
```bash
# Aggregate stats: tick range, HP/mana p50/p95, combat %, item peak, unique coords
cargo run --release --bin replay_perception -- --input session.jsonl --summary

# Line-by-line trace
cargo run --release --bin replay_perception -- --input session.jsonl --trace

# Filter only "danger" frames
cargo run --release --bin replay_perception -- --input session.jsonl --filter hp_below:30
cargo run --release --bin replay_perception -- --input session.jsonl --filter in_combat
cargo run --release --bin replay_perception -- --input session.jsonl --filter has_item:mana_potion
```

The snapshot only includes derived perception (HP/mana ratios, battle list, coords, inventory counts) — it does NOT include the raw NDI frame buffer (too heavy). Replay can verify FSM logic and detection coherence, not pixel-level vision.

## Cavebot hot-reload (label-aware)

`POST /cavebot/load?path=script.toml` smoothly reloads a cavebot script while preserving position. If the OLD cavebot was at (or after) a label that ALSO exists in the NEW script, the new runner jumps to that label instead of restarting at step 0. This lets you iterate on a cavebot script during a live session without losing your hunt position.

If the label doesn't exist in the new script (or the old runner was at a step before any label), the new runner starts at step 0.

## Pathfinding A* (multi-floor)

`bot/src/pathfinding/` calculates routes between absolute tile coordinates using A* over a [`WalkabilityGrid`] built from `Minimap_WaypointCost_*.png` (1083 files in TibiaMaps.io's minimap-without-markers ZIP). Supports automatic multi-floor pathfinding via stair/ramp detection.

**Build the walkability grid** (one-time, ~5 seconds, output ~230 MB for full map):
```bash
cargo run --release --bin build_map_index -- \
    --map-dir <path/to/Tibia/minimap> \
    --output assets/map_index.bin \
    --walkability assets/walkability.bin
```

**Generate a path** for a cavebot script:
```bash
cargo run --release --bin path_finder -- \
    --walkability assets/walkability.bin \
    --from 32015,32212,7 \
    --to   32100,32300,6 \
    --simplify \
    --output hunt_snippet.toml
```

The output is a sequence of cavebot `node` steps. Floor changes are commented in the snippet (`# floor change from z=7 to z=6 (stair/ramp/rope expected)`) so you can verify the bot will actually be able to traverse them.

**Multi-floor**:
- Auto-detects transitions where the same `(x,y)` is walkable on two adjacent floors → marks them as stairs/ramps.
- A* uses 6-connectivity at transition tiles (4 horizontal + up + down).
- Floor changes carry a `FLOOR_CHANGE_PENALTY=500` so A* prefers same-floor paths when possible.

**Manual overrides** for false positives (bridges, rooftops) and false negatives (ropes, ladders, holes — auto-detect can't see them): copy `assets/pathfinding_overrides.toml.example` → `assets/pathfinding_overrides.toml`, edit, and pass `--overrides assets/pathfinding_overrides.toml` to `path_finder`.

**Limitations**:
- Detección de rope/hole por color queda fuera de scope. Para esos casos usa overrides manuales.
- Cuando A* genera un path "raro" que cambia de piso innecesariamente, casi siempre es un falso positivo en bridges → añadir entry en `remove`.

## Cavebot — navegación y requisitos

**Navegación relativa por nodes**: cada `node` es una coord absoluta `(x, y, z)` pero internamente el cavebot computa `dx/dy` desde el node ANTERIOR para saber qué clicks emitir al minimap. Implicación: todo el cavebot es una cadena de desplazamientos relativos — si la baseline es incorrecta, todo descarrila.

**Seed inicial** (commit `9cc5ab8`): al activar el cavebot, el primer Node intenta semillar `prev_node` desde `ctx.game_coords` (tile-hashing real). Requiere map index cargado (`[game_coords].map_index_path` en config). Si tile-hashing no produce match tras ~2s, fallback legacy: registra target como baseline (el char DEBE estar ahí) y advance. Con map index cargado, el cavebot puede arrancar desde cualquier posición — el primer Node camina al target.

**Validación de z** (commit `9cc5ab8`): cada branch de "arrived" en Node compara `ctx.game_coords.z` contra `target.z`. Si mismatch, el cavebot emite `CavebotAction::SafetyPause { reason: "node_z_mismatch: ..." }` que el game loop traduce a `is_paused=true` con reason explícita. Previene cadenas de acciones fantasma en piso equivocado (ej 12 right-clicks de stow en z=7 cuando depot está en z=6).

**Cross-floor navigation**: el minimap-click de Node hace auto-pathfind via Tibia (incluyendo ladders visibles en el path). Pero ladders NO visibles en minimap (holes, ropes, parches) requieren steps explícitos `ladder`/`rope`. Si el cavebot llega a arrival pero z no matchea, el SafetyPause dispara.

## Cavebot — hunt automation

`bot/src/cavebot/` is the structured hunt system. Unlike waypoints (flat temporal sequences), cavebot scripts support control flow:

- **Labels + `goto`** — named targets for loops and branches
- **`goto_if`** — conditional jump (e.g. `goto_if hp_below(0.4) refill`)
- **`stand_until`** — stay in place attacking until condition met (N kills, HP full, etc.)
- **`loot`** — click a coordinate to pick up corpses
- **`skip_if_blocked`** — local recovery for blocked steps
- **`node`** — minimap-click navigation by absolute tile coordinates (delta from prev node)
- **`deposit`** — right-click depot chest + click "Stow all" in context menu
- **`buy_item`** — click item + N confirm clicks in open trade window
- **`check_supplies`** — assert inventory has N slots matching each item template; jump on fail

Scripts are TOML files in `assets/cavebot/`. Hot-reload via `POST /cavebot/load?path=...`. Cavebot emits `WaypointHint` that the FSM accepts/rejects based on current priority (e.g. combat blocks walking).

### Conditions for `goto_if` / `stand until`

- `hp_below(ratio)`, `mana_below(ratio)`, `kills_gte(n)`, `no_combat`, `enemies_gte(n)`, `loot_visible`, `is_moving`, `is_stuck`
- `timer_ticks(n)` — ticks since current step started
- `ui_visible(name)` — UI template from `assets/templates/ui/` matches this frame
- `at_coord(x, y, z)` — tile-hashing reports exact coord (requires map index)
- `near_coord(x, y, z, range)` — Manhattan distance ≤ range
- `has_item(name, min_count)` — ≥ N slots match `assets/templates/inventory/<name>.png`
- `has_stack(name, min_units)` — ≥ N total units, summing OCR-read stack counts (requires digit templates)
- `not:<any>` — negation

### Node tuning (configurable, see `[cavebot]` in config.toml)

Node navigation has 10 tunable parameters via `NodeTuning` (runner.rs). Defaults are set for Tibia 12 @ 1920×1080 with minimap zoom = 1. Override individual fields in `[cavebot]`:
`pixels_per_tile=2`, `displacement_tolerance=4`, `arrived_idle_ticks=10`, `reclick_idle_ticks=60`, `max_reclicks=3`, `timeout_ticks=900`.

**Cavebot vs Waypoints:** use Cavebot for new hunts (labels and conditionals); Waypoints (`bot/src/waypoints/mod.rs`) are the legacy system kept for simple refill loops already written in that format.

### StepVerify — postcondition verification per step

Every cavebot step can declare an optional `[step.verify]` subtable that the runner enforces AFTER the step's natural completion. This is the ADR-003 fix for "fire-and-forget" bugs where clicks emit without effect and the bot advances through fake progress. If the postcondition fails within `timeout_ms`, the runner applies `on_fail`.

```toml
[[step]]
kind = "open_npc_trade"
greeting_phrases = ["hi"]
bag_button_vx = 163
bag_button_vy = 301

[step.verify]
template   = "npc_trade"      # VerifyCheck::TemplateVisible
timeout_ms = 3000             # default 3000
on_fail    = "safety_pause"   # | "advance" | "goto:<label>"
```

**Check variants** (exactly one per `[step.verify]`):
- `template = "<name>"` — UiDetector template `<name>` visible (uses cached `ctx.ui_matches`, up to 500ms stale)
- `absent_template = "<name>"` — template NOT visible (for close/bye actions)
- `condition = "<expr>"` — same grammar as `goto_if.when` (hp_below, has_item, at_coord, etc.)
- `inventory_delta = { item = "mana_potion", min_abs_delta = 50, require_positive = true }` — inventory changed by ≥N units since step start. `require_positive=true` rejects decreases; false accepts either direction. Snapshot captured lazily on step entry.

**Fail actions**:
- `safety_pause` (default) — emit `CavebotAction::SafetyPause { reason: "verify_failed: step[N]=<label> check=<...> timeout=<ms>" }`. Bot pauses with diagnosable reason.
- `advance` — skip the failed verify and move to next step (best-effort steps like optional loot).
- `goto:<label>` — jump to a recovery label. Resolved at TOML load time like `goto`/`check_supplies.on_fail`.

**Runner mechanics**:
- `Cavebot.advance()` is a verify-aware wrapper; if `step.verify.is_some()` and we haven't verified yet, transitions to `verifying: Some(VerifyingState)` instead of advancing.
- Each tick while verifying, `evaluate_verify()` runs the check. Pass → `do_advance()` + continue. Timeout → apply `on_fail`.
- `jump_to` clears verifying (goto bypasses origin's verify).

**Not verified via StepVerify** (use dedicated mechanisms):
- Z arrival after Node steps — already covered by built-in z validation (SafetyPause with `node_z_mismatch`).
- Cross-floor ladder/rope physical movement — covered by subsequent Node's z check.

## click_live — standalone click validator (Fase 1 ADR-003)

Binary `bot/src/bin/click_live.rs` — CLI tool to test a single click against Tibia (via bot's HTTP `/test/click` + `/test/grab`) and verify postcondition before spending 15 min on a full cavebot rebuild. Feedback loop: ~10s vs ~15min.

```bash
# Click at (117, 298), verify npc_trade template disappears, baseline BEFORE click:
cargo run --release --bin click_live -- \
    --coord 117,298 --button L \
    --verify-template npc_trade \
    --template-dir assets/templates/prompts \
    --baseline-first --expect absent --timeout-ms 2000
```

Exit codes: 0=pass, 1=fail (postcondition not met), 2=error (bot not reachable, template missing, etc.). Use in CI or manual calibration.

## Hunt profiles

`assets/hunts/<name>.toml` centralizes data for a specific hunt (loot stackables, supplies thresholds, monster lists, metrics baselines). Loaded via `HuntProfile::load_by_name(Path::new("assets/hunts"), "abdendriel_wasps")`.

Profile schema: `HuntProfile { name, description, level_range, vocation, loot, supplies, monsters, metrics, calibration_hints }`. See `assets/hunts/abdendriel_wasps.toml` for a complete example.

### Cavebot integration via `from_profile`

A cavebot TOML references a profile at the top-level:

```toml
[cavebot]
loop         = true
hunt_profile = "abdendriel_wasps"   # loads assets/hunts/abdendriel_wasps.toml
```

The parser expects the convention `<assets>/cavebot/*.toml` + `<assets>/hunts/*.toml` (sibling dirs). If the cavebot file lives elsewhere, hunts_dir can't be derived and `hunt_profile = "..."` errors.

Once declared, specific steps can consume the profile with `from_profile = true`:

**`check_supplies from_profile = true`** — reads `[supplies]` thresholds instead of an inline `requirements` array:
```toml
[[step]]
kind        = "check_supplies"
on_fail     = "refill"
from_profile = true                 # uses profile.supplies_list() — items with `min` field
```
Only supplies that have an `assets/templates/inventory/<name>.png` template can be checked; uncheckable supplies (ropes, shovels not pictured) should be omitted from the profile's `[supplies]` table.

**`stow_all_items from_profile = true`** — uses `[loot].stackables` as a whitelist with two effects:
1. **Pre-check skip**: if the inventory contains *zero* items from the whitelist, the step advances without iterating (saves the 2 phantom iterations that the stash_full detector would otherwise log as "no change"). Useful on cold-boot when the char has only gear in the bag.
2. **Enhanced stash_full reason**: if the detector fires, the `SafetyPause` reason includes `expected_stackables=[...]` so the operator knows which items the step was looking for.

```toml
[[step]]
kind         = "stow_all_items"
slot_vx      = 1636
slot_vy      = 157
# ... standard fields ...
from_profile = true                 # uses profile.loot.stackables
```

**Mutual exclusion**: within a single step, `from_profile = true` and inline `requirements = [...]` (for check_supplies) are mutually exclusive — having both is an error.

Other intended profile consumers (not yet wired): battle-list validator against `[monsters]` for lure protection, `/metrics` comparisons against `[metrics.expected_xp_per_hour]`.

### Observability

`GET /cavebot/status` exposes the loaded profile name:
```json
{
  "loaded": true,
  "enabled": true,
  "hunt_profile": "abdendriel_wasps",
  "verifying": false,
  ...
}
```
`verifying = true` when the runner is polling a step's postcondition (see StepVerify section).

## Tile-hashing — absolute position from minimap

`bot/src/sense/vision/game_coords.rs` compares the captured minimap against a pre-computed index of Tibia's own minimap PNG files to determine the player's exact `(x, y, z)` coordinates.

**Pipeline**:
1. Extract a 32×32 patch from the minimap corner (away from player crosshair)
2. Compute an 8×8 difference hash (64 bits) of the patch
3. Look up in `MapIndex` (HashMap<u64, Vec<MapPos>>) — exact match first, then fuzzy (hamming ≤ 3)
4. Validate with a second patch from the opposite corner to disambiguate collisions
5. Report `Perception.game_coords: Option<(i32, i32, i32)>`

**Build the index** (one-time, offline):
1. Download from [TibiaMaps.io](https://tibiamaps.io/downloads) — `minimap-without-markers.zip` (~6 MB)
2. Extract PNGs to any directory
3. `cargo run --release --bin build_map_index -- --map-dir <path> --output assets/map_index.bin [--floors 6,7,8]`
4. Set `map_index_path = "assets/map_index.bin"` in `[game_coords]` section of config.toml

**Runtime cost**: detection runs every 15 frames (~500ms) to stay within tick budget. O(1) hash lookup.

**Enables**: `at_coord`, `near_coord` conditions, `stand until reached(x,y,z)`.

## Inventory vision

`bot/src/sense/vision/inventory.rs` template-matches item icons against each slot of the backpack. Cadence: every 15 frames.

**Tres opciones de config** en `calibration.toml` (prioridad: `inventory_backpack_strip` > `inventory_grid` > `inventory_slot`):

**A) Backpack strip** (recomendada para cavebot — N backpacks stacked, 1 row cada uno):
```toml
[inventory_backpack_strip]
x              = 1567   # top-left del primer backpack
y              = 22
backpack_w     = 174
backpack_h     = 67     # title + 1 row + capacity bar
backpack_count = 8      # número de backpacks stacked
slot_x_offset  = 6      # margen interno al 1er slot
slot_y_offset  = 18     # margen interno bajo title bar
slot_size      = 32
slot_gap       = 2
slot_cols      = 4
slot_rows      = 1      # compact view
```
Expande a `backpack_count × slot_cols × slot_rows` slots (default 8×4×1 = 32).

**B) Grid contiguo** (backpack único con grid):
```toml
[inventory_grid]
x = 1760 ; y = 420 ; slot_size = 32 ; gap = 2 ; cols = 4 ; rows = 5
```

**C) Slots manuales** (`[[inventory_slot]]` array para layouts custom).

**Tuning visual del strip**:
```bash
# 1. Capturar un frame con los backpacks abiertos
curl http://localhost:8080/test/grab -o frame.png

# 2. Dibujar el layout propuesto sobre el frame
cargo run --release --bin tune_inventory_strip -- \
    --frame frame.png \
    --x 1567 --y 22 --backpack-w 174 --backpack-h 67 --count 8 \
    --slot-x-offset 6 --slot-y-offset 18 \
    --output tuned.png

# 3. Abrir tuned.png, verificar que los rectángulos amarillos caen sobre los iconos
# 4. Ajustar slot-x-offset/slot-y-offset e iterar hasta que matcheen
# 5. Pegar el bloque TOML que imprime en calibration.toml
```

**Templates**: `assets/templates/inventory/<name>.png` — 70+ PNGs (live-extracted with `extract_inventory_slots` + 4 wiki-origin quarantined en `_wiki_unmatched/`). Match threshold = **0.80 por-default, per-template override** via `assets/templates/inventory/thresholds.toml`:
```toml
# Template con tendencia a false positives → threshold estricto
dragon_ham = 0.95
vial       = 0.85
# Templates no listados usan MATCH_THRESHOLD (0.80)
```
(Commit `fed9387` Fase 1.1 — sube threshold para sprites sparse o de alta similitud cromática con fondo.)

**Fase 1 SSE mejoras** (all opt-in, backward compatible):
- **Per-template thresholds** (1.1): ver bloque arriba.
- **NMS cross-slot dedup** (1.2): si 4 slots matchean el mismo template, filtra los scores fuera del `NMS_SCORE_GAP=0.08` del top. Reduce FPs por mobs similares entre sí.
- **Stack count strip** (1.3): antes del matching, excluye las `STACK_COUNT_HEIGHT_PX=8` rows inferiores (donde se renderiza el número). Evita mismatch entre template "vial×3" y slot "vial×21".
- **Shift-tolerant matching ±2 px** (1.4): extract slot con `SHIFT_TOLERANCE_PX=2` de padding, `match_template` sliding window cubre 5×5 posiciones. Tolera drift de calibración sin perder matches. **Cost empírico** (bench `fase1_overhead`): 16 slots × 5 templates exact = 121 µs, con shift ±2 px = 1.62 ms (**13× overhead**, no 5× como se estimó inicialmente). Amortizado a cadence 1/15 ticks = ~0.1 ms/tick, dentro del budget 33 ms.

**Verification**: `GET /vision/grab/inventory` returns the current frame with yellow rectangles drawn on each slot ROI. `GET /vision/inventory` returns JSON with current per-item counts.

**Two reading modes**:
- `read()` → `HashMap<String, u32>` with slot counts (one entry per slot that matches an item icon)
- `read_with_stacks()` → `InventoryReading { slot_counts, stack_totals }` where `stack_totals` uses digit OCR

**Digit OCR** (`bot/src/sense/vision/inventory_ocr.rs`):
- Scans the bottom-right 16×8 px corner of each slot
- Template-matches each digit position (4×6 px) against `assets/templates/digits/*.png`
- Reconstructs u32 (max 3 digits, Tibia stacks ≤ 999)
- Without calibrated digit templates → fallback to `slot_counts` (1 unit per slot)

**Calibrating digit templates**: extract 10 PNGs (4×6 px) of digits 0-9 as Tibia renders them in the stack count corner. Use `inspect_pixel` or `rgb_dump` on a real frame, save as `assets/templates/digits/0.png` ... `9.png`. The reader auto-loads them at startup.

**Conditions**:
- `has_item(name, N)` — ≥ N slots with the item icon matching
- `has_stack(name, N)` — ≥ N total units via OCR stack count (falls back to has_item if no digit templates)

## Inventory ML classifier (Fase 2 — opt-in)

`bot/src/sense/vision/inventory_ml.rs` puede reemplazar el matcher SSE del inventory por un classifier CNN entrenado con `ml/train_inventory_classifier.py` (Python). Feature flag `ml-runtime` controla si se compila ort runtime:

```bash
# Build default (SSE matcher, ort NO linkeado)
cargo build --release --bin NewEra

# Build con ML (ort + ndarray linkeados, +~10 MB)
cargo build --release --bin NewEra --features ml-runtime
```

Config en `config.toml`:
```toml
[ml]
use_ml               = false                                # default off
model_path           = "ml/models/inventory_v1.onnx"        # vacío = off
classes_path         = "ml/models/inventory_v1.classes.json"
confidence_threshold = 0.80                                 # softmax max ≥ esto
```

**Runtime behavior** cuando `use_ml=true` + modelo cargado + feature activa:
- `Vision::tick` llama `read_with_stacks_ml(frame, Some(&mut ml_reader))`.
- Por slot: primero intenta `infer_slot` (32×32 RGB tensor → softmax → argmax).
- Si confidence ≥ threshold → usa ML class name.
- Si infer_slot devuelve None → fallback automático al SSE matcher (`best_match_for_slot`).
- Sin feature `ml-runtime` o sin modelo → 100% fallback SSE (comportamiento identico al previo).

**Dataset capture workflow** (requerido antes de entrenar):
```bash
# 1. Captura live (bot + Tibia + OBS activos)
TOKEN="<bearer del config>"
curl -X POST -H "Authorization: Bearer $TOKEN" \
    "http://localhost:8080/dataset/start?dir=datasets/v1&interval=15&tag=hunt1"
# ...sesión hunt diversa...
curl -X POST -H "Authorization: Bearer $TOKEN" http://localhost:8080/dataset/stop
curl -s  -H "Authorization: Bearer $TOKEN" http://localhost:8080/dataset/status
# → {"active":false,"crops_total":1234,"dir":"datasets/v1"}

# 2. Etiquetado offline (abre cada PNG en OS viewer, prompts clase)
cargo run --release --bin label_dataset -- \
    --manifest datasets/v1/manifest.csv \
    --classes vial,golden_backpack,green_backpack,white_key,dragon_ham,empty
# Atajos: 0..9/a..z selección rápida, s=skip, u=undo, q=quit+save

# 3. Training (Python, fuera de Rust)
cd tibia-bot/ml
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python train_inventory_classifier.py \
    --manifest ../datasets/v1/manifest.csv \
    --output models/inventory_v1.onnx --epochs 30
# → models/inventory_v1.onnx + .classes.json + .metrics.json

# 4. Runtime: build con feature + set [ml].use_ml=true
cargo build --release --bin NewEra --features ml-runtime
```

Ver `ml/README.md` para detalles del pipeline y arquitectura del modelo (CNN 3 conv + 2 dense, ~150K params, ONNX ~600 KB).

## Region monitor — diff genérico per-región (Fase 1.5)

`bot/src/sense/vision/region_monitor.rs` captura snapshots BGRA de ROIs registradas y computa diff L1 normalizado frame-a-frame. Wired en `BotLoop::new()` con 3 regiones default (battle_list, minimap, viewport) con thresholds (0.05, 0.05, 0.10).

**Endpoint** `GET /vision/region_monitor`:
```bash
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8080/vision/region_monitor
# [{"name":"battle_list","change_ratio":0.012,"above_threshold":false,"first_tick":false},
#  {"name":"minimap","change_ratio":0.003,"above_threshold":false,"first_tick":false},
#  {"name":"viewport","change_ratio":0.087,"above_threshold":false,"first_tick":false}]
```

Útil para diagnóstico: `battle_list change_ratio` salta a 0.5 cuando un mob entra/sale; `viewport` > 0.10 sostenido sugiere transición de escena sin template match (prompt sin calibrar).

Framework genérico — otros consumers pueden `add_region(name, roi, threshold)` y leer `last_change_tick()`. No trigger automático del FSM.

**Cost empírico** (bench `fase1_overhead`): tick() con 3 regiones sobre frame 1920×1080 = **~3 ms** (battle_list 171×997 + minimap 107×110 + viewport 967×719). 9% del budget 33 ms/tick. Si el viewport diff se vuelve caro en el futuro, opciones: sub-sampling stride o cadence < 30 Hz.

## Attack highlight detection (Tibia 12 — Fase post-V7)

`bot/src/sense/vision/color.rs::is_attack_highlight()` detecta el cuadrado cyan/purple que Tibia 12 pinta alrededor del icono del slot atacado (vs el borde rojo clásico). Usado por `detect_is_being_attacked` como **fuente 2** (dual-source):

1. Clientes clásicos: `red_hits ≥ 2` en borde izq del slot (legacy).
2. Tibia 12: ≥ `ATTACK_HIGHLIGHT_MIN_HITS=10` pixels cyan/purple en area del icono (first `ICON_AREA_WIDTH_PX=25` cols).

Patrón cyan: `B ≥ 180 && B > R + 40 && G ≥ 130` (distingue de `is_player_blue` que tiene `G ≤ 80`). Evidencia empírica: pixels típicos `(146,146,209), (75,196,225), (91,203,245)`.

## Scripting — Lua hooks

`bot/src/scripting/mod.rs` embeds Lua 5.4 (via `mlua`, vendored, non-Send — lives in the game loop thread):

- **Hooks:** `on_tick(ctx)` called every tick; `on_low_hp(ratio)` called when HP drops below threshold
- **TickContext table:** read-only snapshot of HP, mana, battle list, FSM state
- **`bot.say(text)`:** queued and typed out at a humanized pace
- **Sandbox:** `io`, `os`, `package`, `require`, `debug` are removed
- **Budget:** warns (does not kill) if hook exceeds `tick_budget_ms`

Scripts live in `[scripting].script_dir` (default `assets/scripts/`). Hot-reload via `POST /scripts/reload`.

## Safety — behavioral humanization

`bot/src/safety/` is the anti-detection layer, decoupled from FSM logic:

| Submodule | What it does |
|-----------|-------------|
| `timing.rs` | Gaussian-sampled cooldowns: N(μ, σ) per action |
| `reaction.rs` | `ReactionGate` — realistic delay (~180±40ms) before responding to new threats |
| `rate_limit.rs` | Hard cap on actions/sec to prevent burst bugs |
| `variation.rs` | `WeightedChoice` — randomizes equivalent actions (spell vs potion) |
| `breaks.rs` | `BreakScheduler` — multi-level AFK: micro (seconds), medium (minutes), long (hours) |
| `human_noise.rs` | Occasional useless key presses (stats screen, menus) to mimic idle micro-interactions |

Enabled via `[safety].humanize_timing = true` and `presend_jitter_mean/std` in config.

## Waypoints — scope and known limitations

The waypoint system in `bot/src/waypoints/mod.rs` uses **temporal step sequences**, not spatial navigation. A step is `{ key, duration_ms, interval_ms }`:
- `walk`: re-emits a directional key every `interval_ms` for `duration_ms` total.
- `wait`: `key=""` + `duration_ms>0` — no emit, just lets time pass.
- `hotkey`: `duration_ms=0` — one-shot tap that advances immediately.

See `assets/waypoints/example.toml` for a working example. Steps are loaded at startup from `[waypoints].path` and can be hot-reloaded via `POST /waypoints/load?path=...`.

### Known limitations (intentional, documented)

**No spatial/minimap-based navigation in Waypoints.** The legacy waypoint system cannot target an absolute tile like `(1024, 512, 7)`. For absolute positioning use **Cavebot with `node` steps** + tile-hashing (see "Tile-hashing" section). Waypoints remain for simple refill loops already written in that format.

**Post-combat restart is full, not partial.** When the FSM exits Emergency or Fighting and returns to Walking, the current step is **restarted from tick 0** rather than resumed from the midpoint. Reasoning: during combat the character can drift from its expected position, so resuming a "walk N 5s" at tick 3/5 would emit taps toward the wrong direction.

**Mitigation**: use **short steps** (≤3s) in hunt areas where combat is frequent. The drift after a restart is bounded by the step duration. A proper "resume with position fixup" requires spatial navigation (see above) and is not worth the complexity until that exists.

**Stuck detection is time-based only.** `WaypointList::tick_stuck_check` fires a warning (and pauses waypoints) when the iterator hasn't advanced to a different step for `stuck_threshold_ticks` (default 1800 ≈ 60s). This catches *perpetual combat interruption* and *infinite loops* but **not** "character blocked against a wall while the step timer keeps ticking". Blocking detection via minimap diff is a candidate for Fase 5 (safety).

## Prompt detection (login / char select / npc trade)

`bot/src/sense/vision/prompts.rs` detects 3 blocking screens/modals via template matching. The list is **evidence-based from Tibia documentation** — each one was verified to exist and block the bot:

| Prompt | What it is | Detection |
|---|---|---|
| `login` | Client login screen (after disconnect/crash/kick/server save at 10:00 CET) | Template match in `prompt_login` ROI |
| `char_select` | Character selection list — appears after login AND after character death (Tibia has no separate "death screen") | Template match in `prompt_char_select` ROI |
| `npc_trade` | NPC shopkeeper buy/sell modal — opens on `hi` → `trade` to a shopkeeper. Character cannot walk while open | Template match in `prompt_npc_trade` ROI |

**What we intentionally do NOT cover:**
- **Captchas**: Tibia does not use captchas. BattleEye (since Feb 2017) is kernel-level anti-cheat, not a visual prompt.
- **Death screen**: doesn't exist as a separate screen in Tibia — death goes directly to char_select.
- **Deposit/withdraw gold**: these are *text conversations* with banker NPCs in the chat console, not modal windows. A Lua script handles them by sending strings.
- **Depot chest / containers**: non-modal windows, don't block walking.
- **Party invites / player trade requests**: dismissable popups, resolved with ESC.
- **Market window**: modal but only relevant if the bot uses Market features (not in MVP).

When a prompt is detected the FSM force-pauses with `safety_pause_reason = "prompt:<kind>"`. The bot **never auto-responds** to prompts — that would be detectable. The operator must resolve manually.

Templates live in `assets/templates/prompts/` (login.png, char_select.png, npc_trade.png) and are user-provided. Without templates the detector is no-op.

