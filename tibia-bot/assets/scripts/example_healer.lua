-- example_healer.lua — Ejemplo de hook de curación custom.
--
-- Hooks soportados por el bot:
--   on_tick(ctx)        -- cada tick, tras visión, antes de la FSM.
--                          ctx = { tick, hp, mana, enemies, fsm }
--                          El return puede ser nil (noop) o una string con
--                          nombre de tecla — el bot la emite como acción
--                          proactiva si no está en Emergency y rate limiter
--                          lo permite. Útil para multi-threshold heal.
--
--   on_low_hp(ctx)      -- cuando HP < HP_CRITICAL_RATIO (30%).
--                          ctx = { tick, hp, mana, enemies, fsm } (igual).
--                          Retornar:
--                          · nil    → bot usa heal_spell default
--                          · string → nombre de tecla ("F1", "F2"...)
--
-- API expuesta:
--   bot.log(level, msg)  -- level ∈ "error"|"warn"|"info"|"debug"
--
-- Sandbox: io, os, package, require, dofile, loadfile, debug están nil.
-- Los scripts no pueden leer archivos ni ejecutar comandos del sistema.

-- Ejemplo: usar poción en vez del spell cuando HP < 15% o mana < 20%
-- (para no gastar el último mana).
function on_low_hp(ctx)
    if ctx.hp < 0.15 then
        bot.log("info", "HP crítico, usando heal_potion")
        return "F2"  -- heal_potion hotkey
    end
    if ctx.mana ~= nil and ctx.mana < 0.20 then
        bot.log("info", "Mana bajo, usando heal_potion en vez de spell")
        return "F2"
    end
    -- HP entre 15% y 30% con mana OK: usar el heal_spell default.
    return nil
end

-- Ejemplo: contador simple para ver que on_tick se dispara.
-- Imprime cada 150 ticks (~5s a 30 Hz). Cuidado con el budget (5ms/hook).
local tick_counter = 0
function on_tick(ctx)
    tick_counter = tick_counter + 1
    if tick_counter % 150 == 0 then
        bot.log("info", string.format(
            "[heartbeat] tick=%d hp=%s mana=%s enemies=%d fsm=%s",
            ctx.tick,
            tostring(ctx.hp),
            tostring(ctx.mana),
            ctx.enemies,
            ctx.fsm
        ))
    end
end
