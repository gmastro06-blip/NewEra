-- abdendriel_wasps_druid.lua — Healer + mana management para druida
-- **level 11** en Ab'dendriel wasps.
--
-- Hotbar real del usuario (confirmado 2026-04-15):
--   F1 = mana potion (16 stack, púrpura) — bebida proactiva si mana < 40%
--   F2 = small health potion (6 stack, roja pequeña) — emergency HP pot
--   F3 = exura spell (heal druid level 8+) — preferido sobre bot.say("exura")
--   F4-F11 = varios (no usados por este script)
--
-- Stats estimados a level 11 druida:
--   HP   ≈ 185  (5 HP por nivel desde base 130)
--   Mana ≈ 140  (30 mana por nivel desde base 50 con wand)
--
-- Wasps hit 15-30 dmg por attack con 60 HP. Peligroso para un druida
-- level 11 porque 2-3 hits pueden bajarte del 70% al 30% en un tick.
--
-- Estrategia CONSERVADORA para level 11:
--   - Wand/Rod attack (Snakebite Rod, 10-16 dmg, sin mana)
--   - exura (40 HP heal, 20 mana) desde 70% HP (margen amplio)
--   - NO exura gran (requiere level 20, no disponible a tu nivel)
--   - Mana potion proactivo desde 40% mana (sostiene ~4 exuras adicionales)
--   - Health potion emergency desde 25% HP (~45 HP remaining)
--   - Si mana agotado AND HP bajo → F2 + log error (operador debe intervenir)
--
-- Spell nativo en Tibia (NO requiere hotkey, se emite vía bot.say):
--   "exura" — light heal, level 8+, druid/sorcerer/paladin, 40 HP / 20 mana

-- ── Parámetros para level 11 (conservadores) ────────────────────────
-- Validación live 2026-04-15 Phase E.1: stack end-to-end validado con
-- thresholds temporariamente a 0.95. User confirma visualmente exura + mana
-- pot drinking en el cliente. Revertido a valores normales de level 11.
local HEAL_THRESHOLD          = 0.70  -- exura a 70% HP (margen amplio)
local EMERGENCY_HP_POT        = 0.25  -- F2 health pot si HP < 25% (~45 HP)
local MANA_POT_THRESHOLD      = 0.40  -- F1 mana pot si mana < 40% (~55 mana)
local MANA_MIN_FOR_HEAL       = 0.15  -- no castear exura si mana < 15%
local HEAL_COOLDOWN_TICKS     = 30    -- 1s entre heals
local MANA_POT_COOLDOWN_TICKS = 60    -- 2s entre mana potions

-- ── Estado del script ───────────────────────────────────────────────
local last_heal_tick      = 0   -- último tick que F3 (exura) se emitió
local last_mana_pot_tick  = 0   -- último tick que F1 (mana pot) se emitió
local last_emergency_tick = 0   -- último tick que F2 (emergency) se emitió

-- Cooldown emergency F2: 30 ticks (1 segundo @ 30Hz). Tibia drink animation
-- toma ~1 segundo, no tiene sentido drink más rápido. Esto evita spam de
-- F2 cuando HP queda atrapado en zona crítica varios ticks seguidos.
local EMERGENCY_COOLDOWN_TICKS = 30

-- ── on_fsm_state_change: alertas de eventos críticos ────────────────
-- El bot llama este hook cuando la FSM transiciona O cuando la safety
-- pause reason cambia. `reason` llega como string con el motivo (ej.
-- "prompt:char_select" = char muerto, "prompt:login" = disconnect,
-- "break:micro" = break iniciado, "focus:tibia_not_foreground" = alt-tab).
--
-- Este hook es informativo: NO puede override la FSM. Solo log/alerta.
function on_fsm_state_change(new_state, reason)
    if reason == "prompt:char_select" then
        -- Char select aparece tras muerte O tras disconnect. En ambos
        -- casos el hunt se detiene hasta intervención humana.
        bot.log("error", "CHAR MUERTO O DESCONECTADO → sesión detenida")
    elseif reason == "prompt:login" then
        bot.log("error", "LOGIN SCREEN detectado → disconnect, sesión detenida")
    elseif reason == "prompt:npc_trade" then
        bot.log("info", "NPC trade window abierto → pausando cavebot")
    elseif reason and string.find(reason, "^break:") then
        bot.log("info", "Safety break iniciado: " .. reason)
    end
end

-- ── on_low_hp: FSM dispara esto cuando HP < 30% (emergencia) ────────
function on_low_hp(ctx)
    local hp   = ctx.hp   or 1.0
    local mana = ctx.mana or 1.0
    local tick = ctx.tick or 0

    -- Ultra-crítico (< 25%): health potion con cooldown.
    if hp < EMERGENCY_HP_POT then
        if tick - last_emergency_tick >= EMERGENCY_COOLDOWN_TICKS then
            last_emergency_tick = tick
            bot.log("error", string.format("HP CRÍTICO %.0f%% → F2 (health potion)", hp * 100))
            return "F2"
        end
        -- En cooldown: skip pero no error log (evita spam de logs).
        return nil
    end

    -- HP bajo con mana disponible: exura via F3 hotkey (con cooldown).
    if mana > MANA_MIN_FOR_HEAL then
        if tick - last_heal_tick >= HEAL_COOLDOWN_TICKS then
            last_heal_tick = tick
            bot.log("warn", string.format("HP %.0f%% → F3 (exura)", hp * 100))
            return "F3"
        end
        return nil
    end

    -- HP bajo Y mana agotado: health potion emergency con cooldown + log.
    if tick - last_emergency_tick >= EMERGENCY_COOLDOWN_TICKS then
        last_emergency_tick = tick
        bot.log("error", string.format("HP %.0f%% Y mana %.0f%% — retreat needed! → F2", hp * 100, mana * 100))
        return "F2"
    end
    return nil
end

-- ── on_tick: decisiones proactivas cada tick ────────────────────────
function on_tick(ctx)
    local hp   = ctx.hp
    local mana = ctx.mana
    if hp == nil or mana == nil then
        return nil
    end

    -- 1. Mana potion proactivo: si mana < 40% Y hay combate, F1.
    --    Rate limit: 1 cada 2 segundos.
    if mana < MANA_POT_THRESHOLD and ctx.enemies > 0 then
        if ctx.tick - last_mana_pot_tick >= MANA_POT_COOLDOWN_TICKS then
            last_mana_pot_tick = ctx.tick
            bot.log("info", string.format("mana pot → F1 (mana=%.0f%%)", mana * 100))
            return "F1"
        end
    end

    -- 2. Pre-heal exura a 70% cuando hay combate. 1 cada segundo.
    if hp < HEAL_THRESHOLD and mana > MANA_MIN_FOR_HEAL and ctx.enemies > 0 then
        if ctx.tick - last_heal_tick >= HEAL_COOLDOWN_TICKS then
            last_heal_tick = ctx.tick
            bot.log("info", string.format("pre-heal F3 exura (HP=%.0f%%)", hp * 100))
            return "F3"
        end
    end

    return nil
end

-- ── on_fsm_state_change: death recovery + alert ─────────────────────
-- El bot loop invoca este hook cada vez que la FSM cambia de estado O el
-- safety_pause_reason cambia. Lo usamos para detectar:
--
--   - Muerte real del char → "prompt:char_select"
--     (Tibia no tiene death screen, va directo a char selection)
--   - Logout forzado → "prompt:login"
--     (kick, server save, disconnect)
--   - Stuck en un paused state por mucho tiempo
--
-- IMPORTANTE: este hook NO puede retornar una hotkey — solo sirve para
-- logging y side-effects. El bot NO responde automáticamente a muerte
-- (sería detectable).
function on_fsm_state_change(new_state, reason)
    -- Ignorar transitions normales Walking↔Fighting↔Idle — solo interesan
    -- los pausados.
    if reason == nil then
        return
    end

    if reason == "prompt:char_select" then
        bot.log("error", "═══ CHAR MUERTO ═══ prompt:char_select detectado")
        bot.log("error", "Bot pausado. Operador debe resolver manualmente:")
        bot.log("error", "  1. Relogear el char")
        bot.log("error", "  2. Revisar postmortem de la sesión")
        bot.log("error", "  3. Curl /recording/stop + replay_perception --summary")
    elseif reason == "prompt:login" then
        bot.log("error", "═══ DISCONNECT ═══ prompt:login detectado (kick/saving/crash)")
        bot.log("error", "Bot pausado esperando relog manual del operador")
    elseif reason == "prompt:npc_trade" then
        bot.log("warn", "NPC trade window abierta — bot pausado hasta que cierres")
    elseif reason == "focus:tibia_not_foreground" then
        -- Este es frecuente (cada alt-tab), mejor log info no error.
        bot.log("info", "Safety: Tibia perdió foco — bot pausado hasta recuperarlo")
    elseif string.sub(reason, 1, 5) == "break" then
        bot.log("info", string.format("Break scheduler: %s", reason))
    else
        -- Cualquier otra razón desconocida — log para investigación.
        bot.log("warn", string.format("FSM → %s  reason=%s", new_state, reason))
    end
end
