# check_session.ps1 — Health check rápido durante una sesión activa.
#
# Imprime el estado de salud y sale con código:
#   0: todo OK (el bot está procesando frames y respondiendo)
#   1: unhealthy (no_frame, stale_frame, proc_slow, not_started)
#   2: paused (login, char_select, npc_trade, manual, safety)
#
# Apto para usar en loops de monitoring:
#   while ($true) {
#       .\scripts\check_session.ps1
#       if ($LASTEXITCODE -ne 0) { ...alerta... }
#       Start-Sleep 30
#   }

$ErrorActionPreference = "Stop"

$BotUrl = if ($env:BOT_URL) { $env:BOT_URL } else { "http://localhost:8080" }

try {
    $resp = Invoke-WebRequest -Uri "$BotUrl/health" -TimeoutSec 3 -ErrorAction SilentlyContinue -SkipHttpErrorCheck
    $health = $resp.Content | ConvertFrom-Json
} catch {
    Write-Host "DOWN  $BotUrl unreachable: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

$ts = Get-Date -Format "HH:mm:ss"
$ok = $health.ok
$reason = $health.reason

# Line resumen coloreado según estado.
$prefix = "[$ts]"
if ($ok) {
    Write-Host "$prefix OK    reason=$reason ticks=$($health.details.ticks_total) proc_ms=$([math]::Round($health.details.bot_proc_ms, 1)) frame_age=$($health.details.frame_age_ms)ms" -ForegroundColor Green
    exit 0
}

switch ($reason) {
    { $_ -in "paused_manual", "paused_login", "paused_char_select", "paused_npc_trade", "paused_safety" } {
        Write-Host "$prefix PAUSE reason=$reason safety=$($health.details.safety_pause_reason)" -ForegroundColor Yellow
        exit 2
    }
    default {
        Write-Host "$prefix DOWN  reason=$reason has_frame=$($health.details.has_frame) frame_age=$($health.details.frame_age_ms)ms proc_ms=$([math]::Round($health.details.bot_proc_ms, 1))" -ForegroundColor Red
        exit 1
    }
}
