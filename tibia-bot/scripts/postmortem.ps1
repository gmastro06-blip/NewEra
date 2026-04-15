# postmortem.ps1 — Análisis post-session de un recording JSONL.
#
# Corre:
#   1. replay_perception --summary → stats agregados
#   2. Filtros de "danger frames": hp_below:30, in_combat
#   3. Detección de gaps en el stream de snapshots (indica bot stuck)
#
# Uso:
#   .\scripts\postmortem.ps1 sessions\20260415-143022.jsonl
#   .\scripts\postmortem.ps1 sessions\20260415-143022.jsonl -Verbose

param(
    [Parameter(Mandatory=$true, Position=0)]
    [string]$SessionFile,

    [switch]$VerboseMode
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $SessionFile)) {
    Write-Host "ERROR: session file not found: $SessionFile" -ForegroundColor Red
    exit 1
}

$binary = "target\release\replay_perception.exe"
if (-not (Test-Path $binary)) {
    Write-Host "ERROR: $binary not found. Run 'cargo build --release --bin replay_perception' first." -ForegroundColor Red
    exit 1
}

function Section($title) {
    Write-Host ""
    Write-Host ("=" * 60) -ForegroundColor Cyan
    Write-Host "  $title" -ForegroundColor Cyan
    Write-Host ("=" * 60) -ForegroundColor Cyan
}

# ── 1. Summary ────────────────────────────────────────────────────────────────

Section "Session Summary"
& $binary --input $SessionFile --summary

# ── 2. Danger frames ──────────────────────────────────────────────────────────

Section "Danger frames (HP < 30%)"
& $binary --input $SessionFile --filter hp_below:30 --summary

Section "Combat frames"
& $binary --input $SessionFile --filter in_combat --summary

# ── 3. Gap detection ──────────────────────────────────────────────────────────

Section "Snapshot stream continuity"

$snapshots = Get-Content $SessionFile | Where-Object { $_ -ne "" } | ForEach-Object {
    try { $_ | ConvertFrom-Json } catch { $null }
} | Where-Object { $_ -ne $null }

if ($snapshots.Count -eq 0) {
    Write-Host "  No snapshots parseable." -ForegroundColor Red
    exit 1
}

$ticks = $snapshots | ForEach-Object { $_.tick } | Sort-Object
$first = $ticks[0]
$last  = $ticks[-1]
$span  = $last - $first
$count = $snapshots.Count

Write-Host "  First tick: $first" -ForegroundColor Gray
Write-Host "  Last tick:  $last" -ForegroundColor Gray
Write-Host "  Span:       $span ticks (~$([math]::Round($span/30.0, 1)) seconds at 30Hz)" -ForegroundColor Gray
Write-Host "  Snapshots:  $count" -ForegroundColor Gray
Write-Host "  Expected:   $([math]::Round($span / 30.0, 0)) at 1/sec interval" -ForegroundColor Gray

# Detectar gaps: 2 snapshots consecutivos con tick diff > 60 (2 sec at 30Hz)
$gaps = @()
for ($i = 1; $i -lt $ticks.Count; $i++) {
    $diff = $ticks[$i] - $ticks[$i-1]
    if ($diff -gt 60) {
        $gaps += [pscustomobject]@{ from_tick = $ticks[$i-1]; to_tick = $ticks[$i]; missing_ticks = $diff }
    }
}

if ($gaps.Count -eq 0) {
    Write-Host "  No gaps detected. Bot ran continuously." -ForegroundColor Green
} else {
    Write-Host "  WARN: $($gaps.Count) gap(s) detected in snapshot stream:" -ForegroundColor Yellow
    foreach ($g in $gaps) {
        $sec = [math]::Round($g.missing_ticks / 30.0, 1)
        Write-Host "    tick $($g.from_tick) → $($g.to_tick) ($sec sec gap)" -ForegroundColor Yellow
    }
}

# ── 4. Safety pauses ──────────────────────────────────────────────────────────

Section "Safety events"

$pauses = $snapshots | Where-Object { $_.conditions -and ($_.conditions -match "safety_pause") }
if ($pauses -and $pauses.Count -gt 0) {
    Write-Host "  $($pauses.Count) snapshot(s) durante safety_pause:" -ForegroundColor Yellow
    $pauses | Select-Object -First 5 | ForEach-Object {
        Write-Host "    tick=$($_.tick) hp=$($_.hp_ratio) mana=$($_.mana_ratio)" -ForegroundColor Gray
    }
} else {
    Write-Host "  No safety pauses detected." -ForegroundColor Green
}

Section "Done"
Write-Host "Postmortem complete for: $SessionFile" -ForegroundColor Green
Write-Host ""
Write-Host "Next actions:" -ForegroundColor White
Write-Host "  - If gaps > 0: investigar qué bloqueó el recorder (crash? stall?)" -ForegroundColor Gray
Write-Host "  - If hp_below:30 > 5% of frames: ajustar healer thresholds" -ForegroundColor Gray
Write-Host "  - If ticks_overrun > 0: investigar frames pesados en la sesión" -ForegroundColor Gray
