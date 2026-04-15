-- advanced_healer.lua — Multi-threshold healing con lógica por clase.
--
-- Este script demuestra:
--   1. Multi-threshold heal: diferentes spells según nivel de HP
--   2. Proactive heal via on_tick (no espera al Emergency del FSM)
--   3. Mana-aware: si mana bajo, usa potion en vez de spell
--   4. Rate limiting suave para no spammear heals
--
-- Requiere:
--   - Hotkeys configurados en config.toml:
--     F1 = exura (light heal ~70-150 HP)
--     F2 = exura gran (medium heal ~200-400 HP)
--     F3 = exura ico / supreme healing potion (emergency)
--     F4 = health potion (fallback)

-- ── Estado global del script ──────────────────────────────────────
local last_heal_tick = 0
local heal_cooldown_ticks = 30  -- 1s a 30 Hz entre heals proactivos

-- ── on_low_hp: llamado cuando HP < 30% (HP_CRITICAL_RATIO) ────────
-- Esta función decide QUÉ tecla emitir. El FSM ya garantiza que se
-- llama solo en emergencia. Retorna nil para usar el default.
function on_low_hp(ctx)
    local hp = ctx.hp or 1.0
    local mana = ctx.mana or 1.0

    -- HP ultra-crítico (<10%): emergency potion sin importar mana.
    if hp < 0.10 then
        bot.log("error", string.format("HP CRÍTICO %.0f%% — emergency potion", hp * 100))
        return "F3"  -- exura ico / supreme potion
    end

    -- HP < 20% y mana disponible: exura gran.
    if hp < 0.20 and mana > 0.15 then
        bot.log("warn", string.format("HP bajo %.0f%%, exura gran", hp * 100))
        return "F2"
    end

    -- HP < 30% pero mana casi agotado: potion en vez de spell.
    if mana < 0.10 then
        bot.log("warn", string.format("HP %.0f%%, mana agotado → potion", hp * 100))
        return "F4"  -- health potion
    end

    -- Default: exura simple (F1), via return nil usa heal_spell del config.
    return nil
end

-- ── on_tick: heal proactivo antes del umbral Emergency ────────────
-- Permite "pre-heal" a 50-70% sin esperar al 30% del FSM. Útil contra
-- hits fuertes donde perder un tick = perder el char.
function on_tick(ctx)
    local hp = ctx.hp
    local mana = ctx.mana
    if hp == nil or mana == nil then
        return nil  -- sin lectura fiable, no actuar
    end

    -- Rate limit: no más de 1 pre-heal por segundo.
    if ctx.tick - last_heal_tick < heal_cooldown_ticks then
        return nil
    end

    -- Pre-heal a 50% con exura si hay mana suficiente y hay combate.
    -- Sin combate no hay riesgo inmediato → dejar que HP regenere solo.
    if hp < 0.50 and mana > 0.20 and ctx.enemies > 0 then
        last_heal_tick = ctx.tick
        bot.log("info", string.format("pre-heal exura a HP=%.0f%%", hp * 100))
        return "F1"
    end

    return nil
end
