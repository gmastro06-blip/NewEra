# Changelog

All notable changes to tibia-bot are documented in this file.

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Version scheme follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Planned for next release:
- **D.2** Click coords calibration (depot chest + NPC shopkeeper)
- **D.3** Cavebot entry point update
- **E.2** Short cavebot test (5-node navigation)
- **E.3** Medium cavebot hunt (30 min refill loop)
- **F4.4** 2h endurance validation session

## [0.2.0] — 2026-04-15

End-to-end validation + game_coords resolved + offline hardening.

### Highlights

- **game_coords tracking**: resolved blocker from v0.1.0. `MinimapMatcher` with
  SSDNormalized template matching replaces the fragile dHash. Live validated
  at coord `(32659, 31683, 7)` matching the user's ground truth.
- **Stack end-to-end validated in live combat**: 78049 snapshots over 43 min,
  HP min observed 50%, 23.4% time in combat, 0 script errors. User visually
  confirmed exura + mana pot drinking in Tibia client.
- **6+ bugs fixed** from v0.1.0 via Phase A + B of single-session plan.
- **Observability**: new Prometheus metrics + HTTP endpoint for matcher stats,
  stuck detection for game_coords + is_moving mismatch.
- **Test coverage**: 380 unit tests + 7 integration tests + 2 doctests,
  zero regressions.

### Added

- `MinimapMatcher` in `bot/src/sense/vision/game_coords.rs`: template matching
  with SSDNormalized. Supports narrow (9 sectors, ~80-160ms) and full brute
  force (~1-4s) modes. Periodic re-validation every 30 detects to recover
  from stuck-in-false-positive.
- `MatcherStats` + `MatcherStatsSnapshot` with atomic counters: narrow/full
  searches, misses, last duration, last score, sectors loaded per floor.
- HTTP endpoint `GET /vision/matcher/stats` returning JSON with matcher
  runtime state.
- Prometheus metrics exported on `/metrics`:
  `tibia_matcher_detects_total{mode="narrow|full"}`,
  `tibia_matcher_misses_total`, `tibia_matcher_last_duration_ms`,
  `tibia_matcher_last_score`, `tibia_matcher_sectors_loaded`.
- Stuck detection: Vision warns when `game_coords` stale > 60s while
  `is_moving=true` (matcher bug, char blocked, or path broken).
- Death recovery hook: Lua `on_fsm_state_change(new_state, reason)` handler
  in `zz_abdendriel_wasps_druid.lua`. Reacts to `prompt:char_select`,
  `prompt:login`, `break:*` with appropriate log levels.
- Recording append mode in `recorder.rs`: sessions survive bot restart
  without truncating the JSONL file.
- Cavebot fixture `assets/cavebot/test_simple.toml` for E.2 short test.
  5-node square pattern, ready to edit with user's coord.
- PowerShell script `scripts/regen_anchor.ps1` automating anchor template
  regeneration (capture frame + make_anchors + backup).
- Production runbook section in `PRODUCTION_CHECKLIST.md` covering arranque,
  verificación, monitoring, protocolo de emergencia, parada, postmortem.
- Diagnostic binaries:
  - `diff_minimap_pixels`: pixel-level comparison NDI minimap vs reference.
  - `find_minimap_ground_truth`: brute-force SSD matching across all floors.
- Integration tests in `bot/tests/e2e_smoke.rs`:
  `vision_matcher_stats_reflect_detection_activity` and
  `matcher_stats_snapshot_serializable`.
- 7 unit tests for `MinimapMatcher` (empty, finds known position, narrow
  vs full floors, rejects above threshold, stats tracking).
- 10 fuzz tests for cavebot parser covering malformed TOML, missing fields,
  wrong types, very long strings, deeply nested structures.
- 2 doctests for `MinimapMatcher` showing usage + stats API.

### Changed

- `bot/config.toml` `ndi_tile_scale` default changed from 5 to 2 (empirical
  value for Tibia 12 with default minimap zoom, validated via
  `find_minimap_ground_truth`).
- `bot/config.toml` new `[game_coords]` fields: `minimap_dir`,
  `matcher_threshold`, `matcher_floors`.
- `assets/calibration.toml` anchor `expected_roi` changed from
  `{x=0, y=0, w=180, h=100}` to `{x=1700, y=0, w=80, h=70}` to match
  `make_anchors` crop exactly, eliminating a +120 offset that broke
  minimap ROI capture.
- `SpellContext` HP/mana fallback from `1.0` to `0.5` when vitals return
  `None` for 5+ consecutive frames (safer: doesn't assume full health,
  doesn't stop heals).
- Cavebot parser rejects empty label names (previously accepted silently).
- Vision hot path simplified: dHash step 1 removed from `detect_position`
  (always failed due to Tibia 12 anti-aliasing, min hamming 14-20 bits
  vs threshold 3). Legacy symbols kept for `build_map_index` bin compat.
- Scripts reload (`/scripts/reload`) confirmed working via live testing.
  Previous suspected "reload doesn't apply" was a false positive.

### Fixed

- **game_coords scale mismatch**: real NDI minimap is 2 px/tile, not 5 as
  the default assumed. Now configurable + documented.
- **game_coords dHash fragility**: dHash can't handle the anti-aliasing of
  Tibia 12 minimap. Replaced with SSD template matching.
- **Anchor score stuck at 0.0**: `expected_roi` didn't match where
  `make_anchors` cropped the template. Fixed to match exactly.
- **Anchor offset broke minimap ROI**: the +120 px shift from anchor
  mismatch pushed minimap ROI out of frame bounds (`capture_minimap`
  returned `None` → `detect_position` never ran).
- **Recording lost on bot restart**: `File::create` truncated the JSONL.
  Now uses `OpenOptions::append`.
- **Dead `ReloadScripts` match arm** in `loop_.rs` had `debug_assert!(false)`;
  now logs a warning quietly.
- **Empty label name** in cavebot was accepted silently; now errors out
  with clear message (discovered by fuzz tests).
- **`PromptDetector`** only loaded `NpcTrade` despite `login.png` and
  `char_select.png` existing on disk.
- **`load_digit_templates()`** was never called at boot; stack count OCR
  fell back to "1 unit per slot" silently.

### Infrastructure

- Arduino Leonardo HID bridge replacing Raspberry Pi Pico 2 (3.23ms RTT,
  HID-Project library, COM8). Sketch in `arduino/tibia_hid_bridge.ino`.
- Bridge config supports `mode = "sendinput"` or `"serial"`.
- 14 MB RAM for MinimapMatcher reference PNGs (floors 6/7/8 only).

### Known Limitations

- Cavebot `node` navigation validated in unit tests only; pending live
  test in E.2 (next session).
- F2 emergency heal pot (HP < 25%) not tested live; healer code is
  conservative enough that HP rarely dropped below 50% in E.1.
- MinimapMatcher `detect` blocks the tick for 80-160ms (narrow) or
  1-4s (full). Observed 0 critical overruns in E.1 but sensible to load.
  Background thread refactor deferred until measured as problematic.
- Click coords for `deposit` + `buy_item` in `abdendriel_wasps.toml` are
  placeholders. Calibration procedure documented at top of file.

## [0.1.0] — 2026-04-14

Initial public commit to GitHub.

### Highlights

- Distributed architecture across PC Gaming (Windows) + PC Processor.
- NDI vision pipeline + FSM + Lua scripting + cavebot + A* pathfinding.
- 364 lib tests pass.

### Included

- `bot/`: Rust game loop (30 Hz), vision readers (HP/mana/battle/
  inventory/minimap/anchor), FSM priority state machine, Lua 5.4
  sandboxed engine, cavebot DSL, waypoints (legacy), safety
  humanization, multi-floor A* pathfinding, HTTP REST API, Perception
  recorder to JSONL.
- `bridge/`: TCP ↔ serial proxy with SendInput + Pico HID modes.
- `arduino/`: Leonardo HID sketch.
- `monitoring/`: Prometheus + Grafana dashboard with 9 panels, 6 alerts.
- `scripts/`: PowerShell runbook (start/stop/check/postmortem).
- Arduino Leonardo HID validated end-to-end (3.23 ms RTT).
- Vision pipeline live-validated in combat (HP/mana/enemies/items).
- Lua healer druid level 11 live-validated.
- Calibrated for Ab'dendriel wasps hunt.

### Known Blockers (resolved in 0.2.0)

- `game_coords` tile-hashing min_hamming 10-17 bits regardless of scale.
- Recording lost on bot restart.
- Anchor template stale.

## Links

- [GitHub repo](https://github.com/gmastro06-blip/NewEra)
- [0.2.0 commit](https://github.com/gmastro06-blip/NewEra/tree/v0.2.0)
- [0.1.0 commit](https://github.com/gmastro06-blip/NewEra/commit/6a4c5e5)
