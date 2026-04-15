; fix_tibia_window.ahk — Fija la ventana de Tibia en (0,0) 1920x1080 sin borde.
;
; Requiere AutoHotkey v2.0 (https://www.autohotkey.com/)
; Uso: doble-click en este archivo mientras Tibia está corriendo.
; Hotkey: F9 para aplicar manualmente.
; La ventana se reposiciona automáticamente cada 2 segundos (por si se mueve).
;
; Ajustar X, Y, W, H si tu setup es diferente.

X := 0
Y := 0
W := 1920
H := 1080

FixWindow() {
    global X, Y, W, H
    if WinExist("ahk_exe Tibia.exe") {
        ; Quitar borde (WS_CAPTION=0xC00000 y WS_THICKFRAME=0x40000)
        WinSetStyle("-0xC40000", "ahk_exe Tibia.exe")
        ; Posicionar y dimensionar
        WinMove(X, Y, W, H, "ahk_exe Tibia.exe")
    }
}

; Aplicar al arrancar
FixWindow()

; Reaplicar cada 2 segundos por si Tibia se mueve
SetTimer(FixWindow, 2000)

; F9 para forzar manualmente
F9::FixWindow()
