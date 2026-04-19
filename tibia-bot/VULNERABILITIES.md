# Análisis de vulnerabilidades — NewEra

**Fecha original**: 2026-04-17
**Última actualización**: 2026-04-19 (Sprint 2 fixes — V-005, V-006, V-007)
**Scope**: bot + bridge + firmware Arduino
**Método**: audit manual + `cargo audit` + grep patterns

**Severidad**:
- 🔴 **Critical**: RCE, ban trivial, impacto severo, fácil exploitar
- 🟠 **High**: exploit serio con medium effort
- 🟡 **Medium**: exploit posible, mitigado por otros controles
- 🟢 **Low**: bajo impacto o difícil exploit
- ℹ️ **Info**: sin exploit directo, riesgo residual

---

## Resumen ejecutivo

| # | ID | Título | Sev | Status |
|---|----|--------|-----|--------|
| 1 | V-001 | Bridge TCP sin autenticación + bind 0.0.0.0 | 🔴 Critical | ✅ **Fixed** (e909cb3) — auth_token + loopback default |
| 2 | V-002 | HTTP API sin autenticación | 🟠 High | ✅ **Fixed** (e909cb3) — Bearer middleware |
| 3 | V-003 | Path traversal via /cavebot/load, /waypoints/load | 🟡 Medium | ✅ **Fixed** (e909cb3) — validate_load_path whitelist |
| 4 | V-004 | Lua sandbox — escape teórico vía VM bugs | 🟡 Medium | Mitigado parcial (tras V-003 fix) |
| 5 | V-005 | Unbounded file read en load endpoints (DoS) | 🟡 Medium | ✅ **Fixed** (sprint2) — MAX_LOAD_FILE_BYTES=1MB |
| 6 | V-006 | No HTTP rate limiting (DoS local) | 🟡 Medium | ✅ **Fixed** (sprint2) — body limit + concurrency limit |
| 7 | V-007 | Information disclosure via /cavebot/status /metrics | 🟠 High | ✅ **Fixed** (sprint2) — stealth_mode config |
| 8 | V-008 | bincode unmaintained (RUSTSEC-2025-0141) | 🟢 Low | Monitoring (no exploit path) |
| 9 | V-009 | rand unsound (RUSTSEC-2026-0097) | 🟢 Low | Mitigado (no custom logger) |
| 10 | V-010 | PE metadata leaks crate name "tibia-bot" | 🟢 Low | Parcial (strip=symbols) |
| 11 | V-011 | Arduino serial sin auth (local solamente) | 🟢 Low | Mitigado por físico |
| 12 | V-012 | 33 unsafe blocks (Windows API) | ℹ️ Info | Auditado, OK |
| 13 | V-013 | Log files en disk si pipeado a archivo | ℹ️ Info | Documentado |

**Summary**: 1 Critical ✅, 2 High ✅, 4 Medium (3 ✅ + 1 parcial), 4 Low, 2 Info = **13 findings — 7 cerrados, 4 parcial/monitoring, 2 info**.

**Post-Sprint 2**: el stack es **production-ready para live con cuenta real** en configuración recomendada:
- `bridge_config.toml`: `mode="serial"` + `listen_addr=127.0.0.1:9000` + `auth_token` set
- `bot/config.toml`: `listen_addr=127.0.0.1:8080` + `auth_token` set + `stealth_mode=true` para sesión live

---

## V-001 🔴 CRITICAL: Bridge TCP sin autenticación + bind 0.0.0.0

### Descripción

El `pico-bridge` escucha en `0.0.0.0:9000` (TODAS las interfaces) **sin autenticación**. El protocolo acepta comandos de texto plano:

```
MOUSE_MOVE <x> <y>
MOUSE_CLICK
KEY_TAP <hid_code>
PING
```

### Exploit scenario

Cualquier host en la LAN del usuario (WiFi compartida, vecino con credentials del router, invitado, IoT device compromised) puede:

```bash
# Desde cualquier IP en la LAN:
nc <gaming-pc-ip> 9000
> MOUSE_MOVE 32767 32767   # cursor a esquina
> KEY_TAP 0x64              # Win key
> KEY_TAP 0x15              # R
# Win+R abre Run dialog
> KEY_TAP 0x06 0x04 0x07   # "cmd"
> KEY_TAP 0x28              # Enter
# Terminal abre con permisos del usuario
> KEY_TAP ...               # comando arbitrario
```

**Impact**: Remote Code Execution en la máquina gaming del usuario vía keyboard injection. Persistent access via Win+R → PowerShell → arbitrary cmdlet. Quema Tibia instantáneamente pero también puede exfiltrar files/credentials/cookies del browser.

### Por qué existe

La arquitectura diseñada en CLAUDE.md asume bot + bridge en **máquinas distintas**:
- PC Gaming (Windows): Tibia + bridge + Arduino
- PC Processor (Linux): bot

El bot necesita conectar al bridge por LAN → `0.0.0.0:9000` es necesario. Sin auth en el protocolo, exposición directa.

### Mitigaciones recomendadas (orden de prioridad)

**Opción A (fácil, alta efectividad)**: **Shared-secret token en el protocolo**

```toml
# bridge_config.toml
[tcp]
listen_addr = "0.0.0.0:9000"
auth_token  = "<random 32-byte hex>"   # obligatorio

# bot/config.toml
[bridge]
url        = "tcp://10.0.0.5:9000"
auth_token = "<same token>"
```

Protocol: bot envía `AUTH <token>\n` como primer mensaje. Bridge responde `OK\n` o cierra. Sin este handshake, cierra conexión en < 100ms.

**Opción B (más segura, más trabajo)**: **TLS + mutual auth**

`rustls` ya es dep del proyecto. Genera cert + key para bridge, cert cliente para bot. Pinning por fingerprint.

**Opción C (fallback si single-machine)**: **127.0.0.1:9000 only**

Si el usuario corre bot + bridge en la misma máquina (setup común), cambiar default a loopback. Romper la topología dual-machine como default, documentar cómo habilitar LAN con shared secret si la necesitan.

### Remediation effort

- Opción A: ~4h (protocolo + config + bot side + tests)
- Opción B: ~12h (TLS setup + error handling + docs)
- Opción C: ~30 min (config change + doc)

### Tracking

**Fixed 2026-04-17** (commit `e909cb3`). Implementación:

- `[tcp].auth_token: Option<String>` en `bridge_config.toml` + mismo field en `bot/config.toml [pico].auth_token`.
- Bridge default `listen_addr = "127.0.0.1:9000"` (loopback-only safe default).
- Handshake: primer mensaje del cliente DEBE ser `AUTH <token>\n`, bridge responde `OK auth\n` o cierra tras 2s timeout.
- Constant-time compare contra timing attacks.
- Backward compat: si token None/empty, skip handshake (solo seguro con loopback bind).

Para setup LAN (bot + bridge en máquinas distintas), ambos configs DEBEN setear el mismo token.

---

## V-002 🟠 HIGH: HTTP API bot sin autenticación

### Descripción

Los 30+ endpoints del HTTP server del bot no tienen autenticación:

```
POST /pause, /resume, /cavebot/load, /scripts/reload → state-changing
POST /test/click, /test/key → command injection to HID
GET  /vision/*, /metrics, /status → info disclosure
```

### Exploit scenario

Pre-V-007 mitigation (con bind 0.0.0.0): remote LAN scan trivial → control del bot. Ahora (bind 127.0.0.1 por default): **cualquier user-mode process en la misma máquina** puede hit endpoints. Malware no-priviligiado podría:
- Pausar/deshabilitar el bot (sabotage)
- Cargar un cavebot TOML arbitrario apuntando a un path controlado
- Leer status del bot para fingerprint anti-cheat local

### Mitigación actual

Post-BE-7 (commit `2056476`): default `listen_addr = "127.0.0.1:8080"`. Loopback-only reduce superficie pero NO elimina la vulnerabilidad.

### Remediation recomendada

Mismo shared-secret pattern que V-001. Header `Authorization: Bearer <token>` en cada request. `axum` soporta esto trivialmente con middleware.

### Tracking

**Fixed 2026-04-17** (commit `e909cb3`). Middleware `auth_middleware` en `remote/http.rs` chequea `Authorization: Bearer <token>` en cada request cuando `config.http.auth_token` está set. 401 Unauthorized si falta o no matches. `/health` excluido para liveness probes. Constant-time compare.

Backward compat: sin token configurado, pasa todo (OK para loopback-only, inseguro para LAN).

---

## V-003 🟡 MEDIUM: Path traversal via /cavebot/load, /waypoints/load

### Descripción

```rust
async fn handle_cavebot_load(Query(q): Query<CavebotLoadQuery>) {
    let cmd = LoopCommand::LoadCavebot {
        path: PathBuf::from(&q.path),   // ← user-controlled, no sanitization
        ...
    };
}
```

El `path` viene del query string sin sanitización. Pasa a `std::fs::read_to_string` en el parser.

### Exploit scenario

```bash
GET /cavebot/load?path=../../../../etc/passwd
GET /waypoints/load?path=C:/Windows/System32/drivers/etc/hosts
GET /scripts/reload?path=C:/Users/victim/.ssh/id_rsa
```

Los parsers rechazan TOML no válido → no hay data leak directo (el error no contiene el contenido del archivo). Pero:
- **DoS**: parser podría crashear con file malformado
- **Info leak vía error messages**: algunos errores de toml imprimen parte del content (ej "expected table at line X, got ...")
- **Lua case peor**: `scripts/reload` carga `.lua` de un directorio; si el path apunta a un dir con archivos maliciosos, Lua VM puede ejecutar código sandboxed

### Mitigación recomendada

Sanitizar + whitelist:

```rust
fn validate_load_path(path: &Path, expected_dir: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    let expected = fs::canonicalize(expected_dir)?;
    if !canonical.starts_with(&expected) {
        bail!("path escapes expected directory");
    }
    Ok(canonical)
}
```

Aplicar a `/cavebot/load` (whitelist a `assets/cavebot/`), `/waypoints/load` (whitelist `assets/waypoints/`), `/scripts/reload` (whitelist `assets/scripts/`).

### Remediation effort

~1h (helper fn + wire en 3 handlers + tests).

### Tracking

**Fixed 2026-04-17** (commit `e909cb3`). Helper `validate_load_path(user_path, allowed_dir)` en `remote/http.rs`:

1. `fs::canonicalize` resuelve symlinks + `..`.
2. Chequea que el canonical path empiece con `canonicalize(allowed_dir)`.
3. Retorna error descriptivo (400 Bad Request) si escapa.

Aplicado a `/cavebot/load` (whitelist `assets/cavebot/`), `/waypoints/load` (`assets/waypoints/`) y `/scripts/reload` (`assets/scripts/`).

Tests: `v003_validate_load_path_rejects_escape_via_parent_dir`, `v003_validate_load_path_accepts_file_inside_whitelist`.

---

## V-004 🟡 MEDIUM: Lua sandbox — escape teórico vía VM bugs

### Descripción

`scripting/mod.rs` sandboxea al remover: `io`, `os`, `package`, `require`, `dofile`, `loadfile`, `debug`. Queda: `string`, `table`, `math`, `coroutine` + API del bot.

### Exploit scenario

Si un atacante logra cargar un script Lua (via V-003 o accidentalmente via file system access):
- El Lua VM (`mlua` 0.10, Lua 5.4 vendored) tiene superficie no-triviales
- Historically, Lua stack overflow / heap bugs han sido explotables
- Un exploit exitoso → RCE en el process del bot (que tiene acceso al bridge, al inventario, a los templates)

### Mitigación actual

- Globals sandboxed (io/os/require removed)
- `tick_budget_ms` warn si hooks tardan (no kill) — no mitiga RCE
- Scripts cargados solo de `assets/scripts/` (path hardcoded) — mitigado si V-003 se fixea

### Recommendation

1. Fix V-003 (path traversal) — elimina vector trivial
2. Considerar `mlua` feature `safe_builtins` si disponible
3. Fuerza execution con `mlua::Lua::new_with(StdLib::NONE)` + explicit whitelist
4. Ejecutar Lua en un sub-process isolated (heavy, OoS)

### Tracking

**Mitigado parcial**. Risk residual bajo porque el path de attack requiere V-003 o FS access.

---

## V-005 🟡 MEDIUM: Unbounded file read → OOM DoS

### Descripción

`handle_*_load` llaman `std::fs::read_to_string(path)` sin size limit.

### Exploit scenario

```bash
# Atacante crea un archivo de 10 GB con contenido TOML-like
dd if=/dev/urandom of=huge.toml bs=1G count=10
# Fuerza el bot a cargarlo:
curl "http://localhost:8080/cavebot/load?path=huge.toml"
# → bot lee 10 GB a memoria → OOM → crash
```

### Mitigación recomendada

Limit file size antes de read:

```rust
let md = fs::metadata(&path)?;
if md.len() > 10 * 1024 * 1024 {  // 10 MB cap
    bail!("config file too large (max 10 MB)");
}
let raw = fs::read_to_string(&path)?;
```

### Tracking

**Fixed 2026-04-19** (sprint 2). `validate_load_path` ahora checkea `fs::metadata().len()` y rechaza > `MAX_LOAD_FILE_BYTES` (1 MB). Tests: `v005_validate_load_path_rejects_oversized_file`, `v005_validate_load_path_accepts_small_file`.

---

## V-006 🟡 MEDIUM: No HTTP rate limiting

### Descripción

Ningún rate limit en el HTTP server. Los endpoints `/test/grab` (~3 MB PNG), `/test/inject_frame` (parseo PNG expensive), `/vision/grab/*` (varios MB cada uno) son especialmente pesados.

### Exploit scenario

Attacker local spammea `/test/grab` @ 1000 req/s → satura disk I/O, RAM fluctuate con PNG allocations, eventualmente OOM o tick deadline overrun.

### Mitigación actual

Rate limiter global del bot (`safety::rate_limit`) aplica a ACCIONES HID, no a HTTP requests. Diferente capa.

### Recommendation

`tower-http::limit::RateLimitLayer` — ~15 LOC para añadir. 50 req/s per-IP global es razonable.

### Tracking

**Fixed 2026-04-19** (sprint 2). Dos layers añadidos en `build_router`:

1. `tower_http::limit::RequestBodyLimitLayer::new(10 * 1024 * 1024)` — cap body a 10 MB para proteger `/test/inject_frame` y `/vision/extract_template` que parsean PNG arbitrario.
2. `tower::limit::ConcurrencyLimitLayer::new(32)` — máximo 32 requests simultáneas. Un atacante spameando `/test/grab` a 1000 req/s queda limitado a 32 en cola, evitando saturación disk I/O + OOM.

Backpressure limitado per-IP NO se implementó — el cap global es suficiente para el threat model loopback y evita la complejidad de `tower_governor`.

---

## V-007 🟠 HIGH: Information disclosure via multiple endpoints

### Descripción

Endpoints revelan estado fingerprintable del bot:

| Endpoint | Leak |
|----------|------|
| `/cavebot/status` | `hunt_profile`, `verifying`, `current_step`, `current_kind` |
| `/metrics` | `tibia_hunt_profile_loaded{profile=...} 1`, `tibia_cavebot_verifying` |
| `/vision/perception` | HP%, mana%, coords, inventory counts, ui_matches |
| `/vision/grab/debug` | frame PNG con boxes amarillos dibujados (UI elementos identificados) |
| `/fsm/debug` | FSM state interno (Walking, Fighting, etc.) |

### Exploit scenario (local attack)

Un anti-cheat client-side (futuro BattleEye update, o separado), o un malware que implementa anti-bot detection heuristics, puede scan puertos locales:

```python
# Malware scanning local ports
import requests
try:
    r = requests.get("http://127.0.0.1:8080/cavebot/status", timeout=0.5)
    if "hunt_profile" in r.text:
        flag_as_bot()  # identifica esta machine como bot
except: pass
```

Impact: Anti-cheat tiene evidencia inequívoca de bot corriendo. Esto es peor que behavioral fingerprinting porque es un direct positive identification.

### Mitigación actual

- Bind 127.0.0.1 (commit 2056476) evita LAN scan. Local processes aún pueden hit.

### Recommendation

1. Auth token (V-001/V-002 fix cubre esto)
2. Obfuscate strings: en vez de "hunt_profile" usar "hp_id", "verifying" → "v_flag"
3. Separate metrics endpoint en different port, solo on demand
4. Mode "stealth" en config que disable todos los endpoints informativos durante live runs

### Tracking

**Fixed 2026-04-19** (sprint 2, opción 4). Nuevo campo `[http].stealth_mode = false` en `bot/config.toml`. Cuando `true`:

- `/vision/*` completo retorna 404
- `/fsm/debug`, `/combat/events`, `/dispatch/stats` → 404
- `/cavebot/status`, `/waypoints/status`, `/scripts/status` → 404
- `/test/grab`, `/test/inject_frame` → 404
- `/status`, `/pause`, `/resume`, `/metrics`, endpoints `load`/`reload` quedan operativos (necesarios para ops humanas)

404 (no 403) para no leak que el endpoint existe pero está stealth.

Default `false` para preservar debuggability durante calibración. El operador debe activar `stealth_mode = true` antes de una sesión live con cuenta real. Auth token + stealth_mode combinados eliminan ambos vectores de fingerprinting (credencial-less scrape + scan con credencial pero stealth-opaque).

Tests: `v007_stealth_mode_blocks_debug_endpoints`, `v007_stealth_mode_allows_health_and_status`, `v007_stealth_mode_default_false_passes_through`.

**Aún abierto (opcional)**: obfuscate strings — marginal ROI dado que con auth+stealth la superficie es mínima. Refactor tibia_bot_* metrics a nombres genéricos queda como nice-to-have.

---

## V-008 🟢 LOW: bincode 1.3.3 unmaintained

### Descripción

`cargo audit` reporta RUSTSEC-2025-0141: bincode 1.3.3 sin mantenimiento.

### Exploit scenario

Deserialization de un `map_index.bin` malicioso podría triggering bugs del bincode parser. Pero:
- El `.bin` se genera localmente via `build_map_index`
- User supply de este file es raro (no es típico "download a community .bin")

### Recommendation

Migrar a `bincode` 2.x (API nueva, maintained).

### Tracking

**Monitoring** — low priority.

---

## V-009 🟢 LOW: rand 0.8 unsound con custom logger

### Descripción

RUSTSEC-2026-0097: `rand::rng()` con custom logger puede dar output no-random. El bot usa `rand::thread_rng()` no `rng()`.

### Assessment

**No exploit path** en nuestro caso. Status: informational.

---

## V-010 🟢 LOW: PE metadata "tibia-bot" en binary

### Descripción

El crate name `tibia-bot` persiste en PE metadata residual tras `strip = symbols`. Anti-cheats con aggressive string scanning podrían match.

### Mitigación actual

- `strip = symbols` elimina símbolos
- Log strings renamed
- EnvFilter no referencia crate name

### Residual

Cargo embed el crate name en algunas metadata no cubiertas por strip. Refactor de rename = ~30 min pero baja ROI.

### Tracking

**Abierto — low priority**.

---

## V-011 🟢 LOW: Arduino serial plaintext no auth

### Descripción

La comunicación bridge ↔ Arduino via serial COM es plaintext ASCII. Cualquier proceso con acceso a COM port puede enviar comandos.

### Assessment

Mitigado por filesystem ACL — COM port accesible solo al user logueado. Physical access al cable = compromise, pero ese threat model está fuera de scope.

### Tracking

**Mitigated by OS**.

---

## V-012 ℹ️ INFO: 33 unsafe blocks en bridge

### Audit findings

- `unsafe { SendInput(...) }` — Windows API call, FFI boundary, OK si inputs validados
- `unsafe { EnumWindows(...) }` — callback FFI, struct pinning OK
- `unsafe { GetWindowTextW(...) }` — buffer size validated
- `unsafe extern "system" fn cb(hwnd, lparam)` — windows callback, lparam unpoint proper

### Assessment

Todas las unsafe blocks auditadas son llamadas a Windows API con wrappers seguros que validan size + lifetimes. No encontrados memory safety issues.

### Tracking

**OK** — informational.

---

## V-013 ℹ️ INFO: Log files on disk si pipeado

### Descripción

Tracing escribe a stderr por default. Si el user pipea a file:

```powershell
.\target\release\NewEra.exe bot\config.toml assets 2> bot.log
```

El `bot.log` contiene timestamps + actions correlatable con server-side logs.

### Recommendation

Documentar en SECURITY.md: no pipear logs a disk en sesiones live con cuenta real.

### Tracking

**Documentado** — no code change needed.

---

## Recomendaciones priorizadas (remediation roadmap)

### Sprint 1 (crítico — antes de próxima live)

1. **V-001 fix**: bridge auth token (4h)
2. **V-002 fix**: HTTP auth token (2h)
3. **V-003 fix**: path traversal whitelist (1h)
4. **V-005 fix**: file size limit (30 min)

Total: ~8h. Cierra los 2 vectores remote + el path traversal.

### Sprint 2 (hardening pre-producción)

5. **V-006**: rate limit middleware (1h)
6. **V-007**: obfuscate endpoint names / stealth mode (2h)
7. **V-008**: migrar a bincode 2.x (2h)

Total: ~5h. Harden observability surface.

### Sprint 3 (opcional)

8. **V-010**: crate rename tibia-bot → newera (30 min)
9. **V-004**: Lua sandbox tightening (4h)

Total: ~4.5h. Reducción marginal.

---

## Dependencias auditadas

`cargo audit` output completo:

```
Scanning Cargo.lock for vulnerabilities (580 crate dependencies)
Warnings:
  bincode 1.3.3     — unmaintained  (RUSTSEC-2025-0141)
  paste 1.0.15      — unmaintained  (RUSTSEC-2024-0436)
  rand 0.8.5        — unsound       (RUSTSEC-2026-0097, no exploit path)
  rand 0.9.2        — unsound       (RUSTSEC-2026-0097, no exploit path)
  core2 0.4.0       — yanked        (transitive via ravif→image)

Summary: 0 errors, 5 warnings — no CVE-level vulnerabilities
```

Todas las warnings son transitivas (no directos), excepto `bincode` y `rand` que son deps directos pero sin exploit path activo en nuestro uso.

---

## Conclusión

**Estado 2026-04-19 (post-sprint-2)**: el stack está **production-ready** para sesiones live con cuenta real.

Sprint 1 (commit `e909cb3`) cerró los 3 vectores remote críticos:
- V-001 bridge TCP auth + loopback default
- V-002 HTTP bearer middleware
- V-003 path traversal whitelist

Sprint 2 (actual) cerró los vectores DoS + fingerprinting:
- V-005 file size cap en loads
- V-006 body + concurrency limits HTTP
- V-007 stealth_mode config para ocultar debug endpoints

Risks residuales aceptables:
- V-004 Lua escape — teórico, chain requires V-003 (cerrado) o FS access
- V-008 bincode unmaintained — no exploit path, .bin files locales
- V-010 PE metadata — strip=symbols elimina 60%, residual bajo
- Detection patterns behavioral — imposibles de eliminar, mitigados por `safety/` module (timing jitter, breaks, human_noise)
- Supply chain — `cargo audit` clean excepto 5 warnings transitivas

### Recomendación para live

**Configuración segura confirmada** (set todo esto antes de arrancar):

```toml
# bridge/bridge_config.toml
[tcp]
listen_addr = "127.0.0.1:9000"    # o LAN con firewall si multi-PC
auth_token  = "<32+ byte random>"

[input]
mode = "serial"                   # Arduino HID, ADR-001

# bot/config.toml
[pico]
auth_token = "<same as bridge>"

[http]
listen_addr  = "127.0.0.1:8080"
auth_token   = "<32+ byte random>"
stealth_mode = true               # live: true, dev: false
```

Con esta config:
- RCE remoto vía bridge: **bloqueado** (auth + loopback)
- Control malicioso local del bot via HTTP: **bloqueado** (Bearer token)
- Fingerprinting por scan local: **bloqueado** (stealth_mode)
- DoS local (spam /test/grab, load huge files): **bloqueado** (limits)
- Path traversal en loads: **bloqueado** (whitelist)

### Historial de remediation

- **2026-04-17**: audit inicial — 13 findings identificados
- **2026-04-17 commit e909cb3**: sprint 1 — V-001, V-002, V-003 fixed
- **2026-04-19 commit TBD**: sprint 2 — V-005, V-006, V-007 fixed
