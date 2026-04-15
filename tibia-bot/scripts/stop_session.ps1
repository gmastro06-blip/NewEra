# stop_session.ps1 — Finaliza una sesión supervisada del tibia-bot.
#
# Acciones:
#   1. Pausa el bot (safety first)
#   2. Detiene el recording
#   3. Imprime stats rápidos del /metrics + /health
#   4. Deja el session file listo para postmortem
#
# Uso:
#   .\scripts\stop_session.ps1

$ErrorActionPreference = "Stop"

$BotUrl      = if ($env:BOT_URL)      { $env:BOT_URL }      else { "http://localhost:8080" }
$SessionsDir = if ($env:SESSIONS_DIR) { $env:SESSIONS_DIR } else { "sessions" }

function Log($msg, $color = "White") {
    Write-Host "[$(Get-Date -Format 'HH:mm:ss')] $msg" -ForegroundColor $color
}

# ── 1. Pausar el bot ──────────────────────────────────────────────────────────

Log "Pausing bot..." "Cyan"
try {
    Invoke-WebRequest -Uri "$BotUrl/pause" -Method POST -TimeoutSec 5 | Out-Null
    Log "  bot paused" "Gray"
} catch {
    Log "WARN: pause failed — $($_.Exception.Message)" "Yellow"
}

# ── 2. Detener recording ──────────────────────────────────────────────────────

Log "Stopping recording..." "Cyan"
try {
    Invoke-WebRequest -Uri "$BotUrl/recording/stop" -Method POST -TimeoutSec 5 | Out-Null
    Log "  recording stopped & flushed" "Gray"
} catch {
    Log "WARN: recording stop failed — $($_.Exception.Message)" "Yellow"
}

# ── 3. Stats del /status ──────────────────────────────────────────────────────

Log "Fetching end-of-session stats..." "Cyan"
try {
    $status = (Invoke-WebRequest -Uri "$BotUrl/status" -TimeoutSec 5).Content | ConvertFrom-Json
    Log "  ticks_total:    $($status.ticks_total)" "Gray"
    Log "  ticks_overrun:  $($status.ticks_overrun)" "Gray"
    Log "  proc_ms (avg):  $([math]::Round($status.bot_proc_ms, 2))" "Gray"
    Log "  ndi_latency_ms: $([math]::Round($status.ndi_latency_ms, 2))" "Gray"
    Log "  pico_latency_ms:$([math]::Round($status.pico_latency_ms, 2))" "Gray"
    if ($status.safety_pause_reason) {
        Log "  safety_pause:   $($status.safety_pause_reason)" "Yellow"
    }
} catch {
    Log "WARN: status fetch failed" "Yellow"
}

# ── 4. Resolve session file ───────────────────────────────────────────────────

$markerFile = Join-Path $SessionsDir ".current_session"
if (Test-Path $markerFile) {
    $sessionFile = (Get-Content $markerFile -First 1).Trim()
    Remove-Item $markerFile -Force
    Log "" "White"
    Log "Session ended: $sessionFile" "Green"
    if (Test-Path $sessionFile) {
        $size = (Get-Item $sessionFile).Length
        $lines = (Get-Content $sessionFile | Measure-Object -Line).Lines
        Log "  $lines snapshots, $([math]::Round($size/1024, 1)) KB" "Gray"
        Log "" "White"
        Log "Postmortem: .\scripts\postmortem.ps1 $sessionFile" "White"
    } else {
        Log "WARN: session file not found on disk" "Yellow"
    }
} else {
    Log "(No active session marker — manual recording stop only.)" "Gray"
}
