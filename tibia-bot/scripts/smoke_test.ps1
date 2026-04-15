# smoke_test.ps1 — Versión PowerShell del runbook automation (Fases B/C/D/F).
#
# Ejecutar después de que el bot esté corriendo (Fase A manual).
# Requiere curl.exe y jq en PATH (ambos shipados con Windows 10+).
#
# Uso:
#   .\scripts\smoke_test.ps1
#
# Output:
#   test_frames\      — 10 frames de referencia
#   smoke_report.txt  — resumen de validaciones

$ErrorActionPreference = "Stop"

$BotUrl = if ($env:BOT_URL) { $env:BOT_URL } else { "http://localhost:8080" }
$FramesDir = if ($env:FRAMES_DIR) { $env:FRAMES_DIR } else { "test_frames" }
$Report = "smoke_report.txt"

"==========================================" | Out-File $Report
"Smoke test report — $(Get-Date)" | Out-File $Report -Append
"==========================================" | Out-File $Report -Append
"" | Out-File $Report -Append

function Log($msg) {
    $line = "[$(Get-Date -Format 'HH:mm:ss')] $msg"
    Write-Host $line
    $line | Out-File $Report -Append
}

function Fail($msg) {
    "FAIL: $msg" | Tee-Object -FilePath $Report -Append
    Write-Error $msg
    exit 1
}

function Pass($msg) {
    "PASS: $msg" | Tee-Object -FilePath $Report -Append
}

# ── Preliminary: bot alive? ─────────────────────────────────────────
Log "Verificando que el bot esté respondiendo en $BotUrl..."
try {
    $null = Invoke-RestMethod -Uri "$BotUrl/status" -TimeoutSec 3
    Pass "Bot responde OK"
} catch {
    Fail "Bot no responde en $BotUrl. Arrancar el bot primero."
}

# ── Fase A ──────────────────────────────────────────────────────────
Log "Fase A — verificando status..."
$status = Invoke-RestMethod -Uri "$BotUrl/status"
"  has_frame: $($status.has_frame)" | Out-File $Report -Append
"  vision_calibrated: $($status.vision_calibrated)" | Out-File $Report -Append
"  tick: $($status.tick)" | Out-File $Report -Append
"  ticks_overrun: $($status.ticks_overrun)" | Out-File $Report -Append

if (-not $status.has_frame) {
    Fail "Fase A: has_frame=false (NDI no conectado?)"
}
Pass "Fase A: bot + NDI OK"

# ── Fase B — capturar 10 frames ─────────────────────────────────────
Log "Fase B — capturando 10 frames a $FramesDir/..."
New-Item -ItemType Directory -Force -Path $FramesDir | Out-Null
$framesOk = 0
for ($i = 1; $i -le 10; $i++) {
    try {
        Invoke-WebRequest -Uri "$BotUrl/test/grab" -OutFile "$FramesDir\frame_$i.png" -UseBasicParsing
        $size = (Get-Item "$FramesDir\frame_$i.png").Length
        if ($size -gt 100000) {
            $framesOk++
        }
    } catch {
        # skip
    }
    Start-Sleep -Milliseconds 500
}
if ($framesOk -lt 8) {
    Fail "Fase B: solo $framesOk/10 frames válidos (>100KB)"
}
Pass "Fase B: $framesOk/10 frames capturados"

# ── Fase C — inventory grid ─────────────────────────────────────────
Log "Fase C — capturando inventory overlay..."
Invoke-WebRequest -Uri "$BotUrl/vision/grab/inventory" -OutFile "$FramesDir\inventory_overlay.png" -UseBasicParsing
$invSize = (Get-Item "$FramesDir\inventory_overlay.png").Length
if ($invSize -lt 50000) {
    Fail "Fase C: inventory overlay vacío ($invSize bytes)"
}

$invJson = Invoke-RestMethod -Uri "$BotUrl/vision/inventory"
"  slot_count: $($invJson.slot_count)" | Out-File $Report -Append
"  items detected: $($invJson.counts.PSObject.Properties.Count)" | Out-File $Report -Append

if ($invJson.slot_count -eq 0) {
    Fail "Fase C: inventory_grid no tiene slots configurados"
}
Pass "Fase C: grid=$($invJson.slot_count) slots, detected=$($invJson.counts.PSObject.Properties.Count) items."
Log "VERIFICAR visualmente: $FramesDir\inventory_overlay.png"

# ── Fase D — validate_templates ─────────────────────────────────────
if (-not (Test-Path "assets\templates\inventory")) {
    Log "assets/templates/inventory no existe, saltando Fase D"
} else {
    Log "Fase D — corriendo validate_templates..."
    $grid = "1760,420,4,5,32,2"
    if ($invJson.grid) {
        $grid = "$($invJson.grid.x),$($invJson.grid.y),$($invJson.grid.cols),$($invJson.grid.rows),$($invJson.grid.slot_size),$($invJson.grid.gap)"
    }
    Log "  grid: $grid"

    $validateOut = "$FramesDir\validate_report.txt"
    & cargo run --release --bin validate_templates -- `
        --frames $FramesDir `
        --templates "assets\templates\inventory" `
        --grid $grid `
        --thresholds "0.05,0.10,0.15,0.20,0.25,0.30" `
        *> $validateOut

    if ($LASTEXITCODE -ne 0) {
        Fail "Fase D: validate_templates falló. Ver $validateOut"
    }

    $matched = (Select-String -Path $validateOut -Pattern "Threshold mínimo con match" -SimpleMatch).Count
    $total = (Select-String -Path $validateOut -Pattern "^Template:").Count
    "  total templates: $total" | Out-File $Report -Append
    "  templates with matches: $matched" | Out-File $Report -Append

    if ($matched -eq 0) {
        Fail "Fase D: NINGÚN template matchea. Ver $validateOut"
    }
    Pass "Fase D: $matched/$total templates con match. Report: $validateOut"
}

# ── Fase F — lintear scripts ────────────────────────────────────────
Log "Fase F — linting all cavebot scripts..."
$lintFail = 0
$lintPass = 0
Get-ChildItem -Path "assets\cavebot\*.toml" | ForEach-Object {
    $out = & cargo run --release --bin lint_cavebot -- $_.FullName 2>&1
    $errLine = $out | Select-String -Pattern "^\d+ errors" | Select-Object -First 1
    $errs = if ($errLine) { [int]($errLine -replace '^(\d+) errors.*', '$1') } else { 0 }
    if ($errs -gt 0) {
        "  FAIL $($_.Name): $errs errors" | Out-File $Report -Append
        $lintFail++
    } else {
        "  OK $($_.Name)" | Out-File $Report -Append
        $lintPass++
    }
}

if ($lintFail -gt 0) {
    Fail "Fase F: $lintFail scripts con errors"
}
Pass "Fase F: $lintPass scripts sin errors"

# ── Resumen ─────────────────────────────────────────────────────────
"" | Out-File $Report -Append
"==========================================" | Out-File $Report -Append
"Smoke test complete — $(Get-Date)" | Out-File $Report -Append
"==========================================" | Out-File $Report -Append

Write-Host ""
Write-Host "Smoke test PASSED. Report: $Report" -ForegroundColor Green
Write-Host "Frames: $FramesDir\"
Write-Host ""
Write-Host "Siguientes pasos manuales:"
Write-Host "  1. Abrir $FramesDir\inventory_overlay.png y verificar slots alineados"
Write-Host "  2. Ejecutar Fase G (calibrar deposit/buy_item)"
Write-Host "  3. Ejecutar Fase H (10 min de hunt)"
