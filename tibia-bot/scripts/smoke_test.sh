#!/usr/bin/env bash
# smoke_test.sh — Automatiza Fases B/C/D/F del PRODUCTION_CHECKLIST.md.
#
# Ejecutar DESPUÉS de que el bot esté corriendo y arrancado (Fase A).
# Asume curl, jq, y cargo disponibles.
#
# Uso:
#   ./scripts/smoke_test.sh [--skip-build]
#
# Output:
#   test_frames/      — 10 frames de referencia
#   smoke_report.txt  — resumen de validaciones

set -e

BOT_URL="${BOT_URL:-http://localhost:8080}"
FRAMES_DIR="${FRAMES_DIR:-test_frames}"
REPORT="smoke_report.txt"

echo "==========================================" > "$REPORT"
echo "Smoke test report — $(date)" >> "$REPORT"
echo "==========================================" >> "$REPORT"
echo >> "$REPORT"

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$REPORT"; }

fail() {
    echo "❌ FAIL: $*" | tee -a "$REPORT"
    exit 1
}

pass() {
    echo "✅ PASS: $*" | tee -a "$REPORT"
}

# ── Fase preliminar: bot alive? ─────────────────────────────────────
log "Verificando que el bot esté respondiendo en $BOT_URL..."
if ! curl -sf "$BOT_URL/status" > /dev/null; then
    fail "Bot no responde en $BOT_URL. Arrancar el bot primero."
fi
pass "Bot responde OK"

# ── Fase A verification (sanity check) ──────────────────────────────
log "Fase A — verificando status..."
STATUS=$(curl -sf "$BOT_URL/status")
# JSON parsing via python (más portable que jq).
json_get() {
    echo "$1" | python -c "import sys, json; d = json.load(sys.stdin); print(d$2)" 2>/dev/null
}
HAS_FRAME=$(json_get "$STATUS" '["has_frame"]')
VISION_CAL=$(json_get "$STATUS" '["vision_calibrated"]')
TICK=$(json_get "$STATUS" '["tick"]')
OVERRUN=$(json_get "$STATUS" '["ticks_overrun"]')

echo "  has_frame: $HAS_FRAME" >> "$REPORT"
echo "  vision_calibrated: $VISION_CAL" >> "$REPORT"
echo "  tick: $TICK" >> "$REPORT"
echo "  ticks_overrun: $OVERRUN" >> "$REPORT"

if [ "$HAS_FRAME" != "True" ] && [ "$HAS_FRAME" != "true" ]; then
    fail "Fase A: has_frame=false (NDI no conectado?)"
fi
pass "Fase A: bot + NDI OK"

# ── Fase B — capturar frames de referencia ──────────────────────────
log "Fase B — capturando 10 frames a $FRAMES_DIR/..."
mkdir -p "$FRAMES_DIR"
FRAMES_OK=0
for i in $(seq 1 10); do
    if curl -sf "$BOT_URL/test/grab" -o "$FRAMES_DIR/frame_$i.png"; then
        SIZE=$(stat -c%s "$FRAMES_DIR/frame_$i.png" 2>/dev/null || stat -f%z "$FRAMES_DIR/frame_$i.png" 2>/dev/null)
        if [ "$SIZE" -gt 100000 ]; then
            FRAMES_OK=$((FRAMES_OK + 1))
        fi
    fi
    sleep 0.5
done

if [ "$FRAMES_OK" -lt 8 ]; then
    fail "Fase B: solo $FRAMES_OK/10 frames válidos (>100KB)"
fi
pass "Fase B: $FRAMES_OK/10 frames capturados"

# ── Fase C — verificar inventory grid ───────────────────────────────
log "Fase C — capturando inventory overlay..."
if ! curl -sf "$BOT_URL/vision/grab/inventory" -o "$FRAMES_DIR/inventory_overlay.png"; then
    fail "Fase C: /vision/grab/inventory no responde"
fi
INV_SIZE=$(stat -c%s "$FRAMES_DIR/inventory_overlay.png" 2>/dev/null || stat -f%z "$FRAMES_DIR/inventory_overlay.png" 2>/dev/null)
if [ "$INV_SIZE" -lt 50000 ]; then
    fail "Fase C: inventory overlay vacío ($INV_SIZE bytes)"
fi

log "Fase C — fetching inventory counts..."
INV_JSON=$(curl -sf "$BOT_URL/vision/inventory")
SLOT_COUNT=$(json_get "$INV_JSON" '["slot_count"]')
COUNTS_LEN=$(echo "$INV_JSON" | python -c 'import sys,json; print(len(json.load(sys.stdin)["counts"]))' 2>/dev/null)
echo "  slot_count: $SLOT_COUNT" >> "$REPORT"
echo "  items detected: $COUNTS_LEN" >> "$REPORT"
echo "  raw: $INV_JSON" >> "$REPORT"

if [ "$SLOT_COUNT" -eq 0 ]; then
    fail "Fase C: inventory_grid no tiene slots configurados"
fi
pass "Fase C: grid=$SLOT_COUNT slots, detected=$COUNTS_LEN items. VERIFICAR visualmente $FRAMES_DIR/inventory_overlay.png"

# ── Fase D — validate_templates sobre los frames capturados ────────
log "Fase D — corriendo validate_templates sobre frames..."
if [ ! -d "assets/templates/inventory" ]; then
    log "  assets/templates/inventory no existe, saltando Fase D"
else
    # Obtener grid config del endpoint (si existe) o usar default.
    GRID=$(echo "$INV_JSON" | python -c 'import sys,json;d=json.load(sys.stdin);g=d.get("grid");print(f"{g[\"x\"]},{g[\"y\"]},{g[\"cols\"]},{g[\"rows\"]},{g[\"slot_size\"]},{g[\"gap\"]}") if g else print("1760,420,4,5,32,2")' 2>/dev/null || echo "1760,420,4,5,32,2")
    log "  grid: $GRID"

    VALIDATE_OUT="$FRAMES_DIR/validate_report.txt"
    if cargo run --release --bin validate_templates -- \
        --frames "$FRAMES_DIR" \
        --templates "assets/templates/inventory" \
        --grid "$GRID" \
        --thresholds "0.05,0.10,0.15,0.20,0.25,0.30" \
        > "$VALIDATE_OUT" 2>&1; then

        TOTAL_TEMPLATES=$(grep -c "^Template:" "$VALIDATE_OUT" 2>/dev/null)
        TOTAL_TEMPLATES=${TOTAL_TEMPLATES:-0}
        MATCHED=$(grep -c "Threshold mínimo con match" "$VALIDATE_OUT" 2>/dev/null)
        MATCHED=${MATCHED:-0}
        echo "  total templates tested: $TOTAL_TEMPLATES" >> "$REPORT"
        echo "  templates with matches: $MATCHED" >> "$REPORT"

        if [ "$MATCHED" -eq 0 ]; then
            fail "Fase D: NINGÚN template matchea. Ver $VALIDATE_OUT. Considera reemplazar templates del wiki con capturas reales."
        fi
        pass "Fase D: $MATCHED/$TOTAL_TEMPLATES templates con match. Report: $VALIDATE_OUT"
    else
        fail "Fase D: validate_templates falló. Ver $VALIDATE_OUT"
    fi
fi

# ── Fase F — lintear todos los cavebot scripts ──────────────────────
log "Fase F — linting all cavebot scripts..."
LINT_FAIL=0
LINT_PASS=0
for script in assets/cavebot/*.toml; do
    if [ -f "$script" ]; then
        OUT=$(cargo run --release --bin lint_cavebot -- "$script" 2>&1 | tail -5)
        ERRS=$(echo "$OUT" | grep -oE "^[0-9]+ errors" | head -1 | grep -oE "^[0-9]+")
        WARNS=$(echo "$OUT" | grep -oE "[0-9]+ warnings" | head -1 | grep -oE "^[0-9]+")
        if [ "${ERRS:-0}" -gt 0 ]; then
            echo "  ❌ $script: ${ERRS} errors, ${WARNS:-0} warnings" >> "$REPORT"
            LINT_FAIL=$((LINT_FAIL + 1))
        else
            echo "  ✓ $script: 0 errors, ${WARNS:-0} warnings" >> "$REPORT"
            LINT_PASS=$((LINT_PASS + 1))
        fi
    fi
done

if [ "$LINT_FAIL" -gt 0 ]; then
    fail "Fase F: $LINT_FAIL scripts con errors. Ver $REPORT"
fi
pass "Fase F: $LINT_PASS scripts sin errors"

# ── Resumen final ────────────────────────────────────────────────────
echo >> "$REPORT"
echo "==========================================" >> "$REPORT"
echo "Smoke test complete — $(date)" >> "$REPORT"
echo "==========================================" >> "$REPORT"

echo
echo "Smoke test PASSED. Report: $REPORT"
echo "Frames: $FRAMES_DIR/"
echo
echo "Siguientes pasos manuales:"
echo "  1. Abrir $FRAMES_DIR/inventory_overlay.png y verificar slots alineados"
echo "  2. Ejecutar Fase G (calibrar deposit/buy_item) — ver PRODUCTION_CHECKLIST.md"
echo "  3. Ejecutar Fase H (10 min de hunt) — ver PRODUCTION_CHECKLIST.md"
