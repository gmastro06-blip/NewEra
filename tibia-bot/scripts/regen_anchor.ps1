# regen_anchor.ps1 — Regenera el template del anchor `sidebar_top` desde
# un frame fresco capturado del bot en ejecución.
#
# Este script resuelve el bug "anchor score=0.0" observado en sesión live:
# el template `assets/anchors/sidebar_top.png` quedó desalineado con el
# layout actual de Tibia (posiblemente por cambio de resolución, DPI, o
# versión del cliente). Se reconstruye capturando un frame actual del
# bot y recortando la misma ROI con `make_anchors.rs`.
#
# **Prerequisitos**:
# - Bot corriendo y accesible en http://localhost:8080
# - Char en una zona ESTABLE (no combate, no modales, Tibia fullscreen)
# - El sidebar UI visible sin interrupciones (battle list, skills, etc)
#
# **Uso**:
#   .\scripts\regen_anchor.ps1
#   .\scripts\regen_anchor.ps1 -BotUrl http://192.168.1.100:8080
#   .\scripts\regen_anchor.ps1 -KeepFrame   # conserva frame_reference.png
#
# **Verificación post-ejecución**:
# 1. Reiniciar el bot (para que re-cargue el template)
# 2. `curl http://localhost:8080/vision/perception | jq .anchor`
#    Debe mostrar score > 0.30 y best_pos cerca de (1700, 0)
# 3. Si el score sigue en 0.0, el frame capturado estaba mal (ver
#    prerequisitos arriba).

param(
    [string]$BotUrl = "http://localhost:8080",
    [switch]$KeepFrame
)

$ErrorActionPreference = "Stop"

# Paths relativos al root del workspace tibia-bot
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Split-Path -Parent $ScriptDir
Set-Location $RepoRoot

$FramePath   = Join-Path $RepoRoot "frame_reference.png"
$AnchorsDir  = Join-Path $RepoRoot "assets\anchors"
$OldTemplate = Join-Path $AnchorsDir "sidebar_top.png"
$BinPath     = Join-Path $RepoRoot "target\release\make_anchors.exe"

Write-Host "regen_anchor.ps1 — regeneración de template `sidebar_top`" -ForegroundColor Cyan
Write-Host "Repo root:   $RepoRoot"
Write-Host "Bot URL:     $BotUrl"
Write-Host ""

# 1. Verificar que el bot esté arriba.
Write-Host "[1/5] Pinging bot health endpoint..." -ForegroundColor Yellow
try {
    $health = Invoke-RestMethod -Uri "$BotUrl/health" -Method Get -TimeoutSec 3
    Write-Host "      Bot OK: $($health | ConvertTo-Json -Compress)"
} catch {
    Write-Host "      ERROR: el bot no responde en $BotUrl/health" -ForegroundColor Red
    Write-Host "      Start the bot first: .\target\release\NewEra.exe bot\config.toml assets"
    exit 1
}

# 2. Capturar frame fresh.
Write-Host ""
Write-Host "[2/5] Capturing current NDI frame..." -ForegroundColor Yellow
try {
    Invoke-WebRequest -Uri "$BotUrl/test/grab" -OutFile $FramePath -TimeoutSec 5
    $size = (Get-Item $FramePath).Length
    Write-Host "      Saved: $FramePath ($([math]::Round($size/1024, 1)) KB)"
    if ($size -lt 100000) {
        Write-Host "      WARNING: frame muy chico (<100KB). Puede estar vacío." -ForegroundColor Yellow
    }
} catch {
    Write-Host "      ERROR capturando frame: $_" -ForegroundColor Red
    exit 1
}

# 3. Backup del template viejo (por si la regen falla).
Write-Host ""
Write-Host "[3/5] Backing up old template..." -ForegroundColor Yellow
if (Test-Path $OldTemplate) {
    $backup = "$OldTemplate.bak"
    Copy-Item $OldTemplate $backup -Force
    Write-Host "      Backup: $backup"
} else {
    Write-Host "      No hay template viejo (primera regen)"
}

# 4. Verificar que make_anchors existe, sino compilar.
Write-Host ""
Write-Host "[4/5] Verifying make_anchors binary..." -ForegroundColor Yellow
if (-not (Test-Path $BinPath)) {
    Write-Host "      $BinPath no encontrado, compilando..."
    & cargo build --release --bin make_anchors
    if ($LASTEXITCODE -ne 0) {
        Write-Host "      ERROR compilando make_anchors" -ForegroundColor Red
        exit 1
    }
}
Write-Host "      OK: $BinPath"

# 5. Ejecutar make_anchors sobre el frame capturado.
Write-Host ""
Write-Host "[5/5] Running make_anchors to regenerate template..." -ForegroundColor Yellow
& $BinPath $FramePath "assets"
if ($LASTEXITCODE -ne 0) {
    Write-Host "      ERROR en make_anchors" -ForegroundColor Red
    exit 1
}

# 6. Verificación final.
if (Test-Path $OldTemplate) {
    $newSize = (Get-Item $OldTemplate).Length
    Write-Host ""
    Write-Host "SUCCESS: template regenerado" -ForegroundColor Green
    Write-Host "  Path:  $OldTemplate"
    Write-Host "  Size:  $([math]::Round($newSize/1024, 1)) KB"
    Write-Host ""
    Write-Host "Siguientes pasos:" -ForegroundColor Cyan
    Write-Host "  1. Reiniciar el bot (para re-cargar el template):"
    Write-Host "       taskkill /F /IM NewEra.exe"
    Write-Host "       .\target\release\NewEra.exe bot\config.toml assets"
    Write-Host "  2. Verificar el score del anchor:"
    Write-Host "       curl $BotUrl/vision/perception | jq .anchor"
    Write-Host "     Esperado: score > 0.30, best_pos cerca de (1700, 0)"
} else {
    Write-Host "ERROR: make_anchors no generó $OldTemplate" -ForegroundColor Red
    exit 1
}

# Limpieza del frame intermedio (opcional).
if (-not $KeepFrame) {
    Remove-Item $FramePath -Force
    Write-Host ""
    Write-Host "Frame intermedio eliminado. Pasar -KeepFrame para conservarlo."
}
