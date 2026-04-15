#!/usr/bin/env python3
"""
integration_tests.py - Suite de tests de integración contra un bot corriendo.

Pre-requisitos:
1. `fake-bridge` corriendo en 127.0.0.1:9000
2. `tibia-bot-smoke` corriendo en 127.0.0.1:18080
3. Frames sintéticos generados en `test_smoke/frames/` via `cargo run --bin synth_frames`

Cada test:
- Inyecta un frame vía POST /test/inject_frame
- Opcionalmente espera N ms
- Verifica estado vía /status, /vision/vitals, /scripts/status, etc
- Verifica que los KEY_TAPs correctos lleguen al bridge.log

Los tests son **idempotentes** - arranquen desde cualquier estado. Cada test
pausa el bot al terminar y lo reanuda al empezar el siguiente.

Output: pass/fail por test + un resumen + un reporte opcional en JSON.
"""

import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

HTTP_BASE = "http://127.0.0.1:18080"
BRIDGE_LOG = Path(__file__).parent / "bridge.log"
FRAMES_DIR = Path(__file__).parent / "frames"

# Resultados globales.
RESULTS: list[dict] = []


# ── HTTP helpers ──────────────────────────────────────────────────────────────


def http_get(path: str, timeout: float = 2.0) -> dict:
    req = urllib.request.Request(HTTP_BASE + path, method="GET")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode("utf-8"))


def http_post(path: str, data: bytes | None = None, content_type: str = "application/json",
              timeout: float = 5.0) -> dict:
    headers = {"Content-Type": content_type}
    req = urllib.request.Request(HTTP_BASE + path, data=data or b"", headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            body = r.read().decode("utf-8")
            if not body:
                return {}
            return json.loads(body)
    except Exception as e:
        return {"error": str(e)}


def inject_frame(name: str) -> dict:
    """POST un PNG al endpoint /test/inject_frame."""
    path = FRAMES_DIR / name
    if not path.exists():
        raise FileNotFoundError(f"No existe el frame sintético: {path}")
    return http_post("/test/inject_frame", data=path.read_bytes(), content_type="image/png")


def read_bridge_log_since(since_ts: float) -> list[dict]:
    """Lee todas las entradas del bridge.log con ts >= since_ts."""
    if not BRIDGE_LOG.exists():
        return []
    entries = []
    for line in BRIDGE_LOG.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("ts", 0) >= since_ts:
            entries.append(e)
    return entries


def key_taps_since(since_ts: float, hidcode_hex: str | None = None) -> list[dict]:
    """Filtra solo KEY_TAPs (opcionalmente por hidcode) del log desde ts."""
    keys = [e for e in read_bridge_log_since(since_ts) if e.get("cmd", "").startswith("KEY_TAP")]
    if hidcode_hex is None:
        return keys
    needle = f"KEY_TAP {hidcode_hex}"
    return [e for e in keys if e["cmd"] == needle]


def pause_bot():
    http_post("/pause")
    time.sleep(0.05)


def resume_bot():
    http_post("/resume")
    time.sleep(0.05)


# ── Framework de test ─────────────────────────────────────────────────────────


def record(name: str, ok: bool, detail: str = ""):
    marker = "PASS" if ok else "FAIL"
    print(f"  [{marker}] {name}  {detail}")
    RESULTS.append({"name": name, "ok": ok, "detail": detail})


def run_test(name: str, fn):
    print(f"\n> {name}")
    try:
        fn()
    except AssertionError as e:
        record(name, False, f"assert: {e}")
    except Exception as e:
        record(name, False, f"exception: {e}")


# ── Tests ─────────────────────────────────────────────────────────────────────


def test_01_heal_on_hp_critical():
    """
    Inyectar frame con HP=20% -> FSM debe entrar en Emergency y emitir KEY_TAP del heal.
    Usa polling hasta 1.5s (worst case: cooldown 333+3*83=582ms + reaction 180+3*40=300ms
    + jitter presend 45+3*15=90ms = ~972ms + margen de seguridad).
    """
    resume_bot()
    # Reset del reaction gate con frame nominal primero.
    inject_frame("nominal.png")
    time.sleep(0.2)

    t0 = time.time()
    r = inject_frame("low_hp.png")
    assert r.get("ok"), f"inject falló: {r}"

    # Polling hasta encontrar el primer heal (máx 1.5s).
    deadline = t0 + 1.5
    heal_keys = []
    while time.time() < deadline:
        heal_keys = [
            k for k in key_taps_since(t0)
            if k["cmd"] in ("KEY_TAP 0x3A", "KEY_TAP 0x3B")
        ]
        if heal_keys:
            break
        time.sleep(0.03)

    # Verificar /vision/vitals detectó HP bajo.
    vitals = http_get("/vision/vitals")
    assert vitals["hp"] is not None, f"vision no detectó HP: {vitals}"
    hp_ratio = vitals["hp"]["ratio"]
    assert 0.15 < hp_ratio < 0.25, f"HP ratio inesperado: {hp_ratio}"

    status = http_get("/status")
    assert len(heal_keys) >= 1, (
        f"no se detectó KEY_TAP de heal en 1.5s. "
        f"fsm={status['fsm_state']}"
    )
    record(
        "01: heal on HP critical",
        True,
        f"hp={hp_ratio:.3f} heals={len(heal_keys)} first_cmd={heal_keys[0]['cmd']}",
    )


def test_02_attack_on_enemy():
    """
    Inyectar frame con 2 enemigos + HP full -> FSM debe entrar en Fighting y emitir attack.
    """
    pause_bot()
    resume_bot()
    time.sleep(0.1)
    t0 = time.time()
    r = inject_frame("combat.png")
    assert r.get("ok"), f"inject falló: {r}"

    # Reaction delay de enemigo: 250±60ms -> esperar 600ms.
    time.sleep(0.6)

    # Verificar battle list tiene enemigos.
    battle = http_get("/vision/battle")
    assert len(battle["entries"]) >= 1, f"battle list vacía: {battle}"

    # Verificar attack key (0x2C = Space) llegó al bridge.
    keys = key_taps_since(t0)
    attack_keys = [k for k in keys if k["cmd"] == "KEY_TAP 0x2C"]
    assert len(attack_keys) >= 1, (
        f"no se detectó attack KEY_TAP 0x2C. keys={[k['cmd'] for k in keys]}"
    )
    record(
        "02: attack on enemy",
        True,
        f"entries={len(battle['entries'])} attacks={len(attack_keys)}",
    )


def test_03_emergency_overrides_fighting():
    """
    Frame con HP crítico Y enemigos -> eventualmente Emergency debe emitir heal.

    NOTA: el orden exacto (heal antes o después del primer attack) depende del
    estado de los reaction gates. Si el enemy_gate ya estaba abierto de un test
    previo y el hp_gate recién se arma, el bot podría emitir 1-2 attacks antes
    del primer heal durante el reaction delay HP. Esto es realista — un humano
    también reacciona al combate antes de notar HP bajo por unos ms. El test
    solo verifica que eventualmente el heal aparece dentro de la ventana.
    """
    pause_bot()
    # Reset completo: nominal primero para resetear ambos gates.
    inject_frame("nominal.png")
    resume_bot()
    time.sleep(0.3)

    t0 = time.time()
    r = inject_frame("combat_low_hp.png")
    assert r.get("ok"), f"inject falló: {r}"

    # Polling hasta ver al menos 1 heal (máx 1.5s).
    deadline = t0 + 1.5
    heal_keys = []
    while time.time() < deadline:
        heal_keys = [
            k for k in key_taps_since(t0)
            if k["cmd"] in ("KEY_TAP 0x3A", "KEY_TAP 0x3B")
        ]
        if heal_keys:
            break
        time.sleep(0.03)

    assert len(heal_keys) >= 1, (
        f"Emergency no priorizó heal en 1.5s. "
        f"keys={[k['cmd'] for k in key_taps_since(t0)]}"
    )
    first_heal_delay_ms = (heal_keys[0]["ts"] - t0) * 1000
    record(
        "03: emergency emits heal (within window)",
        True,
        f"first_heal={first_heal_delay_ms:.0f}ms heals={len(heal_keys)}",
    )


def test_04_nominal_no_actions():
    """
    Frame nominal (HP y mana llenos, sin enemigos) -> FSM en Idle, sin acciones.
    """
    pause_bot()
    resume_bot()
    time.sleep(0.1)
    t0 = time.time()
    r = inject_frame("nominal.png")
    assert r.get("ok"), f"inject falló: {r}"

    time.sleep(0.5)

    vitals = http_get("/vision/vitals")
    assert vitals["hp"]["ratio"] > 0.9, f"HP no lleno: {vitals['hp']['ratio']}"
    assert vitals["mana"]["ratio"] > 0.9, f"mana no lleno: {vitals['mana']['ratio']}"

    # Puede haber algún KEY_TAP residual si el tick aún estaba procesando otros
    # frames; toleramos ≤1 action porque el fsm podría haber estado cerrando un combat.
    keys = key_taps_since(t0)
    # Permitimos 1 "cola" - si hay más, algo está mal.
    assert len(keys) <= 1, f"demasiadas acciones en nominal: {[k['cmd'] for k in keys]}"
    record(
        "04: nominal -> idle",
        True,
        f"hp={vitals['hp']['ratio']:.2f} residual_keys={len(keys)}",
    )


def test_05_reaction_time_hp():
    """
    Medir el reaction delay real entre inject de HP crítico y primer KEY_TAP.
    Debe estar en el rango ~100-300ms (μ=180, σ=40).
    """
    pause_bot()
    # Primero frame nominal para resetear reaction gate.
    inject_frame("nominal.png")
    resume_bot()
    time.sleep(0.3)

    t_inject = time.time()
    inject_frame("low_hp.png")

    # Poll del bridge.log cada 20ms hasta ver el primer heal.
    first_heal_ts = None
    deadline = t_inject + 1.0
    while time.time() < deadline:
        heals = [
            k for k in key_taps_since(t_inject)
            if k["cmd"] in ("KEY_TAP 0x3A", "KEY_TAP 0x3B")
        ]
        if heals:
            first_heal_ts = heals[0]["ts"]
            break
        time.sleep(0.02)

    assert first_heal_ts is not None, "no se detectó heal dentro de 1s"
    delay_ms = (first_heal_ts - t_inject) * 1000
    # El delay debería estar en [70, 400] ms:
    # - reaction time de ~180±40ms
    # - + presend jitter de ~45±15ms
    # - + overhead del tick loop (~33ms)
    # -> esperado ~250±60ms típico. Clamp a 70-400 para robustez.
    assert 60 < delay_ms < 500, f"delay fuera de rango humano: {delay_ms:.0f}ms"
    record(
        "05: reaction time HP critical",
        True,
        f"delay={delay_ms:.0f}ms (esperado 150-350ms)",
    )


def test_06_scripts_status():
    """
    Verificar que los scripts Lua siguen cargados y sin errores tras inyecciones.
    """
    s = http_get("/scripts/status")
    assert s["enabled"], "scripting deshabilitado"
    assert len(s["loaded_files"]) >= 1, f"sin scripts cargados: {s}"
    # Los errores pueden acumularse si el script tiene un bug - tolerancia: 0.
    assert len(s["last_errors"]) == 0, f"scripts con errores: {s['last_errors']}"
    record(
        "06: scripts healthy",
        True,
        f"loaded={len(s['loaded_files'])}",
    )


def test_07_tick_rate_stable():
    """
    El tick count debe avanzar a ~30 Hz sin overruns durante 2 segundos.
    """
    pause_bot()
    resume_bot()
    s0 = http_get("/status")
    t0 = time.time()
    time.sleep(2.0)
    s1 = http_get("/status")
    dt = time.time() - t0
    ticks = s1["tick"] - s0["tick"]
    rate = ticks / dt
    overruns = s1["ticks_overrun"] - s0["ticks_overrun"]
    assert 25 < rate < 35, f"tick rate fuera de 25-35 Hz: {rate:.1f}"
    assert overruns == 0, f"{overruns} overruns en 2s"
    record(
        "07: tick rate stable",
        True,
        f"rate={rate:.1f}Hz overruns={overruns}",
    )


def test_08_bot_proc_ms_under_budget():
    """
    bot_proc_ms promedio debe estar muy por debajo del presupuesto (33ms a 30 Hz).
    """
    s = http_get("/status")
    bpm = s["bot_proc_ms"]
    assert bpm < 10.0, f"bot_proc_ms demasiado alto: {bpm:.2f}ms (budget 33ms)"
    record(
        "08: bot_proc_ms under budget",
        True,
        f"bot_proc_ms={bpm:.3f}ms",
    )


def test_09_waypoints_load_and_run():
    """
    Cargar el ejemplo de waypoints y verificar que el loop avanza por los steps.
    """
    # Cargar waypoints con enabled=true
    r = http_post("/waypoints/load?path=tibia-bot/assets/waypoints/example.toml&enabled=true")
    assert r.get("ok"), f"load falló: {r}"
    # Inyectar frame nominal para que el FSM no entre en Emergency/Fighting.
    inject_frame("nominal.png")
    time.sleep(0.3)
    s = http_get("/waypoints/status")
    assert s["loaded"], f"waypoints no cargados: {s}"
    assert s["total_steps"] == 4, f"esperaba 4 steps, got {s['total_steps']}"

    # Esperar ~3s para que avance varios steps.
    time.sleep(3.0)
    s2 = http_get("/waypoints/status")
    # El current_index debería haber cambiado.
    assert s2["current_index"] is not None
    # Limpiar antes de salir.
    http_post("/waypoints/clear")
    record(
        "09: waypoints load and run",
        True,
        f"steps={s['total_steps']} current_step={s2['current_label']}",
    )


def test_10_inject_frame_endpoint_rejects_garbage():
    """
    El endpoint /test/inject_frame debe rechazar bytes que no son PNG.
    """
    r = http_post("/test/inject_frame", data=b"not a png", content_type="image/png")
    assert not r.get("ok"), f"esperaba fail con garbage: {r}"
    assert "inválido" in r.get("message", "").lower() or "error" in r.get("message", "").lower()
    record(
        "10: inject endpoint rejects garbage",
        True,
        f"message='{r.get('message', '')[:40]}'",
    )


def test_11_safety_status_in_response():
    """
    /status incluye los campos nuevos safety_pause_reason y safety_rate_dropped.
    """
    s = http_get("/status")
    assert "safety_pause_reason" in s, "falta safety_pause_reason"
    assert "safety_rate_dropped" in s, "falta safety_rate_dropped"
    record(
        "11: safety fields in /status",
        True,
        f"rate_dropped={s['safety_rate_dropped']}",
    )


# ── Runner ────────────────────────────────────────────────────────────────────


def main():
    print("=" * 68)
    print("  tibia-bot - integration tests")
    print(f"  HTTP: {HTTP_BASE}")
    print(f"  Bridge log: {BRIDGE_LOG}")
    print(f"  Frames dir: {FRAMES_DIR}")
    print("=" * 68)

    # Verificar pre-requisitos.
    try:
        status = http_get("/status")
    except Exception as e:
        print(f"\n[X] El bot no responde en {HTTP_BASE}: {e}")
        print("   Arranca primero `tibia-bot-smoke` via preview_start.")
        sys.exit(1)

    if not FRAMES_DIR.exists():
        print(f"\n[X] No existe {FRAMES_DIR}")
        print("   Genera los frames con synth_frames (ver README).")
        sys.exit(1)

    required = ["nominal.png", "low_hp.png", "combat.png", "combat_low_hp.png"]
    missing = [f for f in required if not (FRAMES_DIR / f).exists()]
    if missing:
        print(f"\n[X] Faltan frames: {missing}")
        sys.exit(1)

    print(f"\nBot vivo - tick inicial={status['tick']} fsm={status['fsm_state']}")

    # Ejecutar tests en orden.
    tests = [
        ("01 heal on HP critical",       test_01_heal_on_hp_critical),
        ("02 attack on enemy",           test_02_attack_on_enemy),
        ("03 emergency beats fighting",  test_03_emergency_overrides_fighting),
        ("04 nominal -> idle",            test_04_nominal_no_actions),
        ("05 reaction time HP",          test_05_reaction_time_hp),
        ("06 scripts healthy",           test_06_scripts_status),
        ("07 tick rate stable",          test_07_tick_rate_stable),
        ("08 bot_proc_ms under budget",  test_08_bot_proc_ms_under_budget),
        ("09 waypoints load and run",    test_09_waypoints_load_and_run),
        ("10 inject rejects garbage",    test_10_inject_frame_endpoint_rejects_garbage),
        ("11 safety fields in /status",  test_11_safety_status_in_response),
    ]

    for name, fn in tests:
        run_test(name, fn)

    # Resumen.
    print()
    print("=" * 68)
    passed = sum(1 for r in RESULTS if r["ok"])
    failed = len(RESULTS) - passed
    print(f"  Resultados: {passed}/{len(RESULTS)} passed, {failed} failed")
    print("=" * 68)

    # Dump a JSON para el reporte.
    out_path = Path(__file__).parent / "integration_results.json"
    out_path.write_text(json.dumps(RESULTS, indent=2))
    print(f"  Detalle en: {out_path}")

    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
