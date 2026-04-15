# start_session.ps1 — Arranca una sesión supervisada del tibia-bot.
#
# Asume que el bot y el bridge YA están corriendo (arrancados a mano o por
# servicio). Este script solo configura el estado para una sesión:
#   1. Verifica /health → debe retornar 503 con reason != no_frame
#   2. Activa recording con un path basado en timestamp
#   3. Hace /resume si el bot estaba pausado manualmente
#   4. Imprime URL del dashboard Grafana y tail command del recording
#
# Uso:
#   .\scripts\start_session.ps1
#   .\scripts\start_session.ps1 -Label "dwarves-run-1"
#
# Variables de entorno:
#   BOT_URL        — default http://localhost:8080
#   SESSIONS_DIR   — default sessions/
#   GRAFANA_URL    — default http://localhost:3000

param(
    [string]$Label = ""
)

$ErrorActionPreference = "Stop"

$BotUrl      = if ($env:BOT_URL)      { $env:BOT_URL }      else { "http://localhost:8080" }
$SessionsDir = if ($env:SESSIONS_DIR) { $env:SESSIONS_DIR } else { "sessions" }
$GrafanaUrl  = if ($env:GRAFANA_URL)  { $env:GRAFANA_URL }  else { "http://localhost:3000" }

function Log($msg, $color = "White") {
    Write-Host "[$(Get-Date -Format 'HH:mm:ss')] $msg" -ForegroundColor $color
}

function Fail($msg) {
    Log $msg "Red"
    exit 1
}

# ── 1. Health check ───────────────────────────────────────────────────────────

Log "Checking bot health at $BotUrl/health..." "Cyan"

try {
    $response = Invoke-WebRequest -Uri "$BotUrl/health" -TimeoutSec 5 -ErrorAction SilentlyContinue -SkipHttpErrorCheck
    $health = $response.Content | ConvertFrom-Json
} catch {
    Fail "Cannot reach $BotUrl/health — is the bot running? ($($_.Exception.Message))"
}

Log "  ok=$($health.ok) reason=$($health.reason)" "Gray"
Log "  frame_age_ms=$($health.details.frame_age_ms) proc_ms=$($health.details.bot_proc_ms) ticks=$($health.details.ticks_total)" "Gray"

# Aceptamos ok=true o "paused_manual" (el usuario lo pausó antes de arrancar sesión).
# Rechazamos: no_frame, stale_frame, proc_slow, not_started, paused_login, paused_char_select.
if (-not $health.ok) {
    switch ($health.reason) {
        "paused_manual" { Log "Bot estaba pausado manualmente — se resumirá tras activar recording." "Yellow" }
        "not_started"   { Fail "Bot no ha procesado ningún tick. Verificá NDI source y reinicia." }
        "no_frame"      { Fail "Bot no está recibiendo frames NDI. Verificá OBS/DistroAV." }
        "stale_frame"   { Fail "Último frame NDI tiene $($health.details.frame_age_ms)ms. NDI probablemente colgado." }
        "proc_slow"     { Fail "Tick proc time $($health.details.bot_proc_ms)ms > 50ms. Bot degradado." }
        "paused_login"      { Fail "Pantalla de login detectada. Logueá manualmente antes de iniciar sesión." }
        "paused_char_select"{ Fail "Pantalla char_select detectada. Seleccioná char manualmente." }
        "paused_npc_trade"  { Fail "Ventana de NPC trade abierta. Cerrala antes de iniciar sesión." }
        "paused_safety"     { Fail "Safety pause activa ($($health.details.safety_pause_reason)). Resolvé antes de iniciar." }
        default             { Fail "Health unknown reason: $($health.reason)" }
    }
}

# ── 2. Recording ──────────────────────────────────────────────────────────────

if (-not (Test-Path $SessionsDir)) {
    New-Item -ItemType Directory -Path $SessionsDir | Out-Null
}

$ts = Get-Date -Format "yyyyMMdd-HHmmss"
$suffix = if ($Label) { "_$Label" } else { "" }
$sessionName = "${ts}${suffix}"
$sessionFile = Join-Path $SessionsDir "$sessionName.jsonl"

Log "Starting recording → $sessionFile" "Cyan"

try {
    Invoke-WebRequest -Uri "$BotUrl/recording/start?path=$sessionFile" -Method POST -TimeoutSec 5 | Out-Null
} catch {
    Fail "Recording start failed: $($_.Exception.Message)"
}

# ── 3. Resume si estaba pausado ───────────────────────────────────────────────

if ($health.reason -eq "paused_manual") {
    Log "Resuming bot..." "Cyan"
    try {
        Invoke-WebRequest -Uri "$BotUrl/resume" -Method POST -TimeoutSec 5 | Out-Null
    } catch {
        Fail "Resume failed: $($_.Exception.Message)"
    }
}

# ── 4. Info final ─────────────────────────────────────────────────────────────

Log "Session started: $sessionName" "Green"
Log "" "White"
Log "Monitoring:" "White"
Log "  Dashboard:  $GrafanaUrl/d/tibia-bot-main/tibia-bot" "Gray"
Log "  Status:     curl $BotUrl/status" "Gray"
Log "  Health:     curl $BotUrl/health" "Gray"
Log "" "White"
Log "Stop with:  .\scripts\stop_session.ps1" "White"
Log "Postmortem: .\scripts\postmortem.ps1 $sessionFile" "White"

# Escribir el path del session file en un marker para que stop_session lo lea.
$markerFile = Join-Path $SessionsDir ".current_session"
$sessionFile | Out-File -FilePath $markerFile -Encoding ASCII -Force
