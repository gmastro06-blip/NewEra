param(
    [int]$X = 0,
    [int]$Y = 0,
    [int]$W = 1920,
    [int]$H = 1080
)

$code = @'
using System;
using System.Runtime.InteropServices;
public class WinAPI {
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hwnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern long GetWindowLong(IntPtr h, int i);
    [DllImport("user32.dll")] public static extern long SetWindowLong(IntPtr h, int i, long s);
}
'@
Add-Type -TypeDefinition $code

function Fix-TibiaWindow {
    $proc = Get-Process -Name "Tibia" -ErrorAction SilentlyContinue
    if (-not $proc) { Write-Host "Tibia no encontrado, esperando..."; return $false }
    $hwnd = $proc[0].MainWindowHandle
    if ($hwnd -eq [IntPtr]::Zero) { Write-Host "Sin ventana todavia..."; return $false }

    $style = [WinAPI]::GetWindowLong($hwnd, -16)
    $style = $style -band (-bnot (0x00C00000L -bor 0x00040000L -bor 0x00800000L))
    [WinAPI]::SetWindowLong($hwnd, -16, $style) | Out-Null
    [WinAPI]::SetWindowPos($hwnd, [IntPtr]::Zero, $X, $Y, $W, $H, 0x0060) | Out-Null
    Write-Host "Tibia fijada en ($X,$Y) ${W}x${H}"
    return $true
}

$ok = $false
while (-not $ok) {
    $ok = Fix-TibiaWindow
    if (-not $ok) { Start-Sleep 2 }
}

Write-Host "Monitoreando (Ctrl+C para salir)..."
while ($true) {
    Start-Sleep 3
    Fix-TibiaWindow | Out-Null
}
