-- combat_aware_mana.lua — Gestión inteligente de mana según combat state.
--
-- Strategy:
--   · Sin combate: no gastar mana en nada (full regen natural)
--   · En combate con mana alto: cast spells ofensivos (ej. exori via F5)
--   · Mana medio: priorizar heals
--   · Mana bajo: switch a potions, cancelar ofensivas
--
-- Complementa el heal_spell del FSM sin sustituirlo. Delega las heals
-- críticas al FSM (on_low_hp returns nil), solo añade comportamiento
-- ofensivo proactivo.

-- ── Config ────────────────────────────────────────────────────────
local MANA_HIGH      = 0.70   -- >70% = lanza ofensivas
local MANA_LOW       = 0.25   -- <25% = switch to potions
local ATTACK_COOLDOWN_TICKS = 60  -- 2s entre spells ofensivos

-- ── State ─────────────────────────────────────────────────────────
local last_attack_tick = 0

function on_tick(ctx)
    local hp = ctx.hp
    local mana = ctx.mana
    if hp == nil or mana == nil then return nil end

    -- Sin combate: no emitir nada, dejar que el char regen.
    if ctx.enemies == 0 then
        return nil
    end

    -- Mana alto + combate + cooldown ok → lanzar ataque ofensivo.
    -- F5 asume que tienes un spell como exori en esa tecla.
    if mana > MANA_HIGH and hp > 0.50 then
        if ctx.tick - last_attack_tick >= ATTACK_COOLDOWN_TICKS then
            last_attack_tick = ctx.tick
            bot.log("debug", string.format(
                "offensive cast @ hp=%.0f%% mana=%.0f%% enemies=%d",
                hp * 100, mana * 100, ctx.enemies
            ))
            return "F5"
        end
    end

    -- Mana medio + HP bajo: priorizar heal (dejar al FSM)
    -- Mana bajo: no hacer nada proactivo, conservar para emergency
    return nil
end

-- ── on_low_hp: switch a potion si mana es crítico ────────────────
function on_low_hp(ctx)
    local mana = ctx.mana or 1.0
    if mana < MANA_LOW then
        -- Mana muy bajo, el heal_spell no va a funcionar bien.
        -- Usar potion directa en vez.
        bot.log("warn", string.format(
            "mana=%.0f%% too low for spell → potion", mana * 100
        ))
        return "F4"  -- health potion hotkey
    end
    return nil  -- usar heal_spell default del FSM
end
