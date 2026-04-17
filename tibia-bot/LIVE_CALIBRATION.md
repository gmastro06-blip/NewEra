# Live calibration checklist

Tareas de calibración que requieren Tibia corriendo + char en-game.
Recomendado hacer todas antes de la primera sesión productiva de 4h+.

## Pre-requisitos

- Bot + bridge running (`NewEra.exe` + `NewEra-bridge.exe`)
- Char logueado en Tibia
- Terminal con `curl` + `python` disponibles para helpers

Abrir Windows PowerShell en el repo:

```powershell
cd C:\Users\gmast\Documents\GitHub\NewEra\tibia-bot
```

---

## 1. Starting coord (tile-hashing seed)

**Qué**: el `[game_coords].starting_coord` en `bot/config.toml` debe ser
una coord donde el char está al arrancar el bot. Sin seed el matcher da
false positives (validado live 2026-04-17 en Ab'dendriel).

**Cómo calibrar**:

1. Posicionar el char en su "home tile" (típicamente el depot donde
   arranca el cavebot).
2. Ejecutar el bot + habilitar tile-hashing temporariamente sin seed:
   ```toml
   [game_coords]
   map_index_path = "assets/map_index.bin"
   starting_coord = [32681, 31686, 6]  # seed temporal razonable
   ```
3. Start bot. `curl http://localhost:8080/vision/perception | jq .game_coords`.
   Si reporta `[X, Y, Z]` razonable (±3 tiles del esperado) → listo, usar
   esos valores en config (o mantener los actuales si son cerca).
4. Si reporta coord muy distinta (ej 1000 tiles off) → el seed no ayudó;
   el sector visualmente matches otro sector del mismo piso. Mover el char
   a otro tile y re-intentar, o pedir ajuste en `MAX_JUMP` filter.

Verificar:
```powershell
curl http://localhost:8080/vision/matcher/stats | jq
# narrow_searches > 0, full_searches == 1 (cold boot), misses controlados
```

---

## 2. Digit OCR templates (0-9.png)

**Qué**: para que `has_stack(mana_potion, 100)` funcione preciso, el bot
necesita 10 templates PNG de los dígitos 0-9 en el formato del stack count
de Tibia (pixel font, aprox 4×6 px).

**Workflow con `calibration_helper.exe`**:

1. Abrir bag con un stack conocido (ej: 50 mana potions).
2. Grab un frame:
   ```powershell
   curl http://localhost:8080/test/grab -o /tmp/stack_50.png
   ```
3. En GIMP, medir la esquina inferior-derecha del slot donde está el
   número "50" (ej: `x=1820, y=445, w=16, h=8`).
4. Ejecutar el helper:
   ```powershell
   .\target\release\calibration_helper.exe `
       --frame /tmp/stack_50.png `
       --area 1820,445,16,8 `
       --digits 50 `
       --output assets/templates/digits
   ```
   Genera `5.png` y `0.png`.
5. Repetir con otros stacks para cubrir los 10 dígitos:
   - Stack de 1 mana → extrae `1.png` (otras áreas)
   - Stack de 23 → extrae `2.png` + `3.png`
   - Stack de 456 → extrae `4.png` + `5.png` + `6.png` (3 dígitos)
   - etc.
6. Validar con el bot corriendo:
   ```powershell
   curl http://localhost:8080/vision/inventory | jq .stack_totals
   # Esperar valores reales en vez de 1/slot
   ```

**Checkpoint**: los 10 dígitos calibrados → `has_stack` funciona
correctamente y `/vision/inventory` reporta `stack_totals` distinto de
`slot_counts`.

Sin estos templates, `has_stack` hace fallback a `has_item` (1 unidad
por slot), lo cual funciona OK para thresholds pequeños (< 5 manas) pero
subestima counts grandes (un stack de 100 se reporta como 1).

---

## 3. Wasp sting template

**Qué**: `assets/templates/inventory/wasp_sting.png` (32×32 RGBA). Drop
raro de Wasps. No disponible en CDNs públicas (tibiascape no lo tiene).

**Workflow**:

1. Matar varias wasps en Ab'dendriel cave hasta dropear un sting.
2. Abrir el bag con el sting visible.
3. `curl http://localhost:8080/test/grab -o /tmp/frame_sting.png`.
4. En GIMP, recortar el slot con el sting (32×24 o 32×32 según tu
   convención — los otros inventory templates del repo son 32×24).
5. Save as `assets/templates/inventory/wasp_sting.png`.
6. Actualizar `assets/hunts/abdendriel_wasps.toml`:
   ```toml
   [loot]
   stackables = ["gold_coin", "honeycomb", "mana_potion", "wasp_sting"]
   ```
7. Validar con `validate_hunt_profile`:
   ```powershell
   .\target\release\validate_hunt_profile.exe assets\hunts\abdendriel_wasps.toml
   # Summary: 0 errors, 0 warnings
   ```

**Opcional**: lo mismo para otros drops raros del hunt (armor, rings,
spellbooks).

---

## 4. Cross-floor ladder coords

**Qué**: el cavebot tiene 8 ladders/ropes en el hunt loop. Solo el primero
(z=6→z=7, depot descent) está instrumentado con verify cross-floor.
Replicar el pattern a los otros 7.

**Workflow**: una vez que el primer ladder verify se valide live (ver
`assets/cavebot/abdendriel_wasps.toml` línea ~350), replicar:

```toml
[[step]]
kind = "ladder"        # o "rope"

[[step]]
kind = "wait"
duration_ms = 1500

[step.verify]
condition  = "near_coord(X_LADDER, Y_LADDER, Z_NEW, 3)"
timeout_ms = 3000
on_fail    = "safety_pause"
```

Donde:
- `X_LADDER, Y_LADDER` = coords del tile donde está la escalera (el node
  inmediatamente antes del ladder/rope).
- `Z_NEW` = piso destino (z-1 para descent, z+1 para ascent via rope).

Lista completa de ladders/ropes del cavebot:

| Line # | Tipo | Transición | Coords esperadas tras cambio |
|--------|------|-----------|------------------------------|
| ~350   | ladder | z=6 → z=7 | (32656, 31674, 7) ✅ ya hecho |
| ~419   | ladder | z=7 → z=8 | (32612, 31703, 8) |
| ~448   | ladder | z=8 → z=9 | (32612, 31683, 9) |
| ~470   | ladder | z=9 → z=10 | (32618, 31706, 10) |
| ~518   | rope | z=10 → z=9 | (32612, 31683, 9) |
| ~596   | rope | z=9 → z=8 | (32622, 31691, 8) |
| ~645   | rope | z=8 → z=7 | (32603, 31704, 7) |
| ~718   | ladder | z=7 → z=6 | (32656, 31674, 6) |

---

## 5. Buy coord refinement

**Qué**: las coords del buy_item están calibradas 2026-04-17 con
hover-GetCursorPos para el NPC Shiriel en Ab'dendriel. Si cambiás de NPC
o de layout visual, re-calibrar.

**Workflow con `click_live`**:

1. Abrir manualmente la trade window del NPC.
2. Hover mouse sobre cada botón en Tibia y anotar coords via
   `GetCursorPos` (helper PS script en el repo):
   - mana_potion_row
   - amount_field
   - buy_button
   - bye_button
3. Test cada coord con `click_live` (con bot pausado):
   ```powershell
   .\target\release\click_live.exe `
       --coord 394,292 `
       --verify-template npc_trade `
       --template-dir assets\templates\prompts `
       --expect present `
       --timeout-ms 2000
   # Exit 0 = click landed + template still visible (amount field OK)
   ```

---

## 6. Validation tools pre-session

Ejecutar **antes** de cada sesión live para catch errores de config:

```powershell
# Validar cavebot TOML
.\target\release\lint_cavebot.exe assets\cavebot\abdendriel_wasps.toml

# Validar hunt profile
.\target\release\validate_hunt_profile.exe assets\hunts\abdendriel_wasps.toml

# Ambos deben terminar con "0 errors"
```

---

## 7. Checklist pre-launch

Antes del primer hunt autónomo de 4h:

- [ ] `bot/config.toml` tiene `starting_coord` matching el depot de la sesión.
- [ ] `/vision/matcher/stats` reporta sectors_loaded > 0 + misses bajo.
- [ ] `/vision/perception | jq .game_coords` devuelve coord razonable.
- [ ] `/cavebot/status | jq .hunt_profile` devuelve "abdendriel_wasps"
      (o el profile que cargaste).
- [ ] `/cavebot/status | jq .verifying` devuelve false al inicio.
- [ ] `lint_cavebot` y `validate_hunt_profile` ambos 0 errors.
- [ ] Templates críticos presentes: mana_potion, gold_coin, honeycomb,
      health_potion (ver `assets/templates/inventory/`).
- [ ] Si usás stow: char con al menos 1 item del whitelist
      (profile.loot.stackables), sino el pre-check skipea sin iterar.
- [ ] Bridge conectado al Arduino (log: "Serial abierto en COM8" o similar).
- [ ] Bot pausado al load, `curl /cavebot/load?path=...&enabled=true` OK,
      `curl /resume` OK.

Durante el run, monitoring util:

```powershell
# Status breve cada 2s
while ($true) {
  curl -s http://localhost:8080/cavebot/status | python -c "import json,sys;d=json.load(sys.stdin);print(f'idx={d[\"current_index\"]} kind={d[\"current_kind\"]} label={d[\"current_label\"]} verifying={d[\"verifying\"]}')"
  Start-Sleep -Seconds 2
}
```

Si disparó safety_pause: `curl /status | jq .safety_pause_reason` te dice
exactamente qué verify falló + step + check + timeout.
