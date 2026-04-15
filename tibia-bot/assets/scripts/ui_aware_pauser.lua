-- ui_aware_pauser.lua — Logging + detección de UI abierta.
--
-- Demuestra el uso de ctx.ui para detectar cuando ciertas ventanas
-- UI están abiertas (depot_chest, stow_menu, etc). Útil para:
--   1. Saber cuándo el cavebot está interactuando con UI
--   2. Adaptar decisiones en on_tick según el contexto
--   3. Logging / métricas de tiempo pasado con cada UI abierta
--
-- Los templates de UI viven en `assets/templates/ui/*.png` y se
-- cargan automáticamente al arrancar. `ctx.ui["depot_chest"]` será
-- `true` si ese template matchea en el frame actual.

-- ── Counters para métricas ────────────────────────────────────────
local depot_open_ticks = 0
local stow_menu_open_ticks = 0
local last_report_tick = 0

function on_tick(ctx)
    -- Count ticks with each UI visible.
    if ctx.ui["depot_chest"] then
        depot_open_ticks = depot_open_ticks + 1
    end
    if ctx.ui["stow_menu"] then
        stow_menu_open_ticks = stow_menu_open_ticks + 1
    end

    -- Report periodic stats (cada 5 min @ 30 Hz = 9000 ticks).
    if ctx.tick - last_report_tick >= 9000 then
        last_report_tick = ctx.tick
        bot.log("info", string.format(
            "UI stats: depot_open=%d stow_open=%d total_ticks=%d",
            depot_open_ticks, stow_menu_open_ticks, ctx.tick
        ))
    end

    -- Ejemplo de uso: si el depot_chest está abierto y el char tiene
    -- enemigos en pantalla → algo raro, loggear warning.
    if ctx.ui["depot_chest"] and ctx.enemies > 0 then
        -- Usar un rate limit propio para no spammear.
        if ctx.tick % 60 == 0 then  -- 1x por 2s
            bot.log("warn", string.format(
                "depot abierto con %d enemigos visibles!", ctx.enemies
            ))
        end
    end

    -- Este script NO emite acciones — solo observa.
    return nil
end

-- No hook on_low_hp: dejamos al FSM manejar emergencias.
