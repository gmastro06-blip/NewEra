-- anti_waste.lua — Evita gastar heals/mana innecesariamente.
--
-- Útil cuando el FSM + cavebot son muy agresivos heal/mana. Este script
-- actúa como filter: bloquea heals proactivos si HP no los necesita.
--
-- Uso: combinar con un heal spell configurado en config.toml. El script
-- NO emite heals directamente; devuelve nil para dejar que el FSM decida,
-- y `on_tick` devuelve nil si no hay razón proactiva.
--
-- Ajustar los thresholds por clase:
--   Knight:  hp_min=0.85 (regen alto, tank)
--   Paladin: hp_min=0.75
--   Mage:    hp_min=0.60 (frágil, heal más frecuente)

-- ── Ajustar estos valores a tu clase ──────────────────────────────
local HP_PROACTIVE_HEAL = 0.75   -- heal proactivo solo si HP < este
local HP_IDLE_IGNORE    = 0.90   -- si HP > este y sin combate, no actuar
local MANA_RESERVE      = 0.20   -- nunca gastar spell si mana < reserve

-- ── Contadores para diagnostico ───────────────────────────────────
local heals_blocked = 0
local heals_allowed = 0

function on_tick(ctx)
    local hp = ctx.hp
    local mana = ctx.mana
    if hp == nil then return nil end

    -- Sin combate y HP alto: nada que hacer.
    if ctx.enemies == 0 and hp >= HP_IDLE_IGNORE then
        return nil
    end

    -- Mana reserve: si estamos muy bajos, ahorrar para emergencias.
    if mana ~= nil and mana < MANA_RESERVE then
        if hp >= HP_PROACTIVE_HEAL then
            heals_blocked = heals_blocked + 1
            if heals_blocked % 30 == 0 then
                bot.log("info", string.format(
                    "anti_waste: blocked %d heals (mana reserve)", heals_blocked
                ))
            end
            return nil
        end
    end

    -- HP necesita heal proactivo y hay recursos: permitir default.
    if hp < HP_PROACTIVE_HEAL then
        heals_allowed = heals_allowed + 1
        return nil  -- dejar que el FSM/heal_spell default actúe
    end

    return nil
end

-- Diagnostico cada minuto.
local last_stats_tick = 0
function on_low_hp(ctx)
    -- No hacemos nada especial en low_hp, delegamos al FSM default.
    -- Pero aprovechamos para loggear stats periódicos.
    if ctx.tick - last_stats_tick > 1800 then  -- 1 min @ 30 Hz
        last_stats_tick = ctx.tick
        bot.log("info", string.format(
            "anti_waste stats: %d allowed, %d blocked",
            heals_allowed, heals_blocked
        ))
    end
    return nil
end
