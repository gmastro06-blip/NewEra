# Cavebot templates — composable building blocks

Plantillas TOML que se copian y completan con coords específicas por hunt.
El objetivo es reducir el tiempo de calibración por hunt a ~5-10 min (solo
coords, el grueso de la lógica ya está).

## Cómo usar

1. Copiar la plantilla relevante a `assets/cavebot/<hunt_name>.toml`
2. Reemplazar los `<PLACEHOLDER>` con coords reales (ver workflow abajo)
3. Lintar: `cargo run --release --bin lint_cavebot -- assets/cavebot/<hunt_name>.toml`
4. Hot-reload en el bot: `POST /cavebot/load?path=assets/cavebot/<hunt_name>.toml`

## Workflow de calibración de coords

Mientras Tibia corre con NDI activo:

```bash
# 1. Ir manualmente al depot chest, posicionarse encima
curl -H "Authorization: Bearer <TOKEN>" http://127.0.0.1:8080/vision/cursor
# → retorna (x, y) del cursor en frame coords → anotar como chest_vx, chest_vy

# 2. Ir al NPC shopkeeper, click derecho
curl ... /vision/cursor  # → item_vx, item_vy del item a comprar

# 3. Para tile coord (nodes del mapa):
curl ... /vision/perception | jq .game_coords
# → [x, y, z] — anotar coord del char → usar en [[step]] kind="node"
```

## Composición típica de un cavebot

```
1. initial_label: start
2. hunt_loop:
   - walk_to_first_spawn (nodes)
   - loop { attack_mobs + loot }
3. check_supplies_low:
   - goto_if hp<30% or mana<30% or has_item<threshold → refill
4. refill:
   - walk_to_depot (nodes)
   - deposit_loot (stow_all_items)
   - walk_to_npc (nodes)
   - open_npc_trade + buy_item (potions, runes)
   - walk_back_to_hunt (nodes)
   - goto hunt_loop
5. end_of_time: goto start (loop forever)
```

## Plantillas disponibles

- [refill_base.toml](refill_base.toml) — loop de refill desde depot+NPC
- [hunt_loop_base.toml](hunt_loop_base.toml) — loop de hunt con nodes+loot
- [depot_base.toml](depot_base.toml) — deposit items en depot chest
