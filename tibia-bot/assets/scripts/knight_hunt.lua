-- knight_hunt.lua — Lógica custom para Knight en hunt real.
--
-- Setup del usuario:
--   F1       = Mana Potion
--   F3       = Exura Ico (heal spell)
--   PgDown   = Attack Next Target
--   WASD     = walking
--
-- Hooks disponibles:
--   on_tick(ctx)   → cada tick; retorno nil/string:
--                    nil    → noop (solo observabilidad)
--                    string → acción proactiva (ej. heal a HP<50% antes
--                             de que llegue a Emergency). Se dispatcha
--                             si no estamos en Emergency ni safety pause.
--   on_low_hp(ctx) → solo si HP < 30%. Retorno nil/string igual que on_tick.
--
-- ctx = { tick, hp, mana, enemies, fsm }
--   tick    : u64, tick actual del game loop
--   hp      : 0.0-1.0 o nil si vision no lo lee
--   mana    : igual
--   enemies : u32, count de enemies detectados en battle list
--   fsm     : string, estado del FSM en el tick ANTERIOR (lag de 1 tick)
--
-- API disponible:
--   bot.log(level, msg)  -- "error"/"warn"/"info"/"debug"

-- ══════════════════════════════════════════════════════════════════════════
-- Config de thresholds (ajustar según el vocation/level)
-- ══════════════════════════════════════════════════════════════════════════

local HP_PROACTIVE       = 0.50   -- heal proactivo a 50%
local HP_CRITICAL        = 0.30   -- emergency (coincide con FSM)
local HP_VERY_CRITICAL   = 0.15   -- crítico máximo
local MANA_MIN_FOR_SPELL = 0.20   -- si mana < 20%, no usar Exura Ico (se quedaría seco)

-- Cooldown interno del heal proactivo (para no solapar con el FSM Emergency).
-- 30 ticks @ 30Hz = 1s. Entre heals proactivos consecutivos.
local PROACTIVE_COOLDOWN_TICKS = 30

-- ══════════════════════════════════════════════════════════════════════════
-- Estado interno (persiste entre ticks)
-- ══════════════════════════════════════════════════════════════════════════

local tick_counter = 0

-- Tracking detallado para stats
local stats = {
    heals_emitted         = 0,   -- total heals (on_low_hp)
    proactive_heals       = 0,   -- heals desde on_tick a HP<50%
    low_hp_events         = 0,   -- flancos nuevos de HP<30%
    very_critical_events  = 0,   -- flancos de HP<15%
    low_mana_events       = 0,
    max_enemies_seen      = 0,
    total_kills_estimated = 0,   -- aproximado: flancos de bajada en enemies
    ticks_in_fighting     = 0,
    ticks_in_walking      = 0,
    ticks_in_idle         = 0,
    ticks_in_emergency    = 0,
}

-- Estado para detectar flancos
local last_hp_critical_tick = -9999
local last_proactive_heal_tick = -9999
local last_enemy_count = 0

-- ══════════════════════════════════════════════════════════════════════════
-- Hook: on_tick — observability + proactive heal
-- ══════════════════════════════════════════════════════════════════════════

function on_tick(ctx)
    tick_counter = tick_counter + 1

    -- ── Tracking de tiempo por estado (aprox; ctx.fsm tiene lag de 1 tick) ──
    if ctx.fsm == "Fighting" then
        stats.ticks_in_fighting = stats.ticks_in_fighting + 1
    elseif ctx.fsm == "Walking" then
        stats.ticks_in_walking = stats.ticks_in_walking + 1
    elseif ctx.fsm == "Emergency" then
        stats.ticks_in_emergency = stats.ticks_in_emergency + 1
    else
        stats.ticks_in_idle = stats.ticks_in_idle + 1
    end

    -- ── Tracking de enemies (max visto + kills estimados) ───────────────────
    local enemies = ctx.enemies or 0
    if enemies > stats.max_enemies_seen then
        stats.max_enemies_seen = enemies
    end
    -- Si el count bajó, asumimos un kill (aprox, el debounce del FSM ya filtra noise)
    if enemies < last_enemy_count then
        stats.total_kills_estimated = stats.total_kills_estimated + (last_enemy_count - enemies)
    end
    last_enemy_count = enemies

    -- ── Flanco de HP crítico (para log + stats, NO dispara heal aquí) ──────
    if ctx.hp ~= nil and ctx.hp < HP_CRITICAL then
        if tick_counter - last_hp_critical_tick > 30 then
            stats.low_hp_events = stats.low_hp_events + 1
            if ctx.hp < HP_VERY_CRITICAL then
                stats.very_critical_events = stats.very_critical_events + 1
                bot.log("warn", string.format(
                    "[knight] HP MUY CRITICO: %.0f%% (enemies=%d mana=%.0f%%)",
                    ctx.hp * 100, enemies, (ctx.mana or 1) * 100
                ))
            else
                bot.log("info", string.format(
                    "[knight] HP bajo: %.0f%% (enemies=%d mana=%.0f%%) -> on_low_hp",
                    ctx.hp * 100, enemies, (ctx.mana or 1) * 100
                ))
            end
        end
        last_hp_critical_tick = tick_counter
    end

    -- ── Tracking de mana bajo ──────────────────────────────────────────────
    if ctx.mana ~= nil and ctx.mana < 0.20 then
        if tick_counter % 60 == 0 then
            stats.low_mana_events = stats.low_mana_events + 1
        end
    end

    -- ── Report periódico cada 300 ticks (~10s) ─────────────────────────────
    if tick_counter % 300 == 0 then
        local hp_pct = (ctx.hp or 1) * 100
        local mn_pct = (ctx.mana or 1) * 100
        bot.log("info", string.format(
            "[knight] stats t=%d hp=%.0f%% mn=%.0f%% e=%d " ..
            "| fsm=%s | heals=%d(+%d proactive) killsEst=%d " ..
            "| time[F/W/I/E]=%d/%d/%d/%d maxE=%d",
            ctx.tick, hp_pct, mn_pct, enemies, ctx.fsm,
            stats.heals_emitted, stats.proactive_heals,
            stats.total_kills_estimated,
            stats.ticks_in_fighting, stats.ticks_in_walking,
            stats.ticks_in_idle, stats.ticks_in_emergency,
            stats.max_enemies_seen
        ))
    end

    -- ── Proactive heal a HP<50% (threshold intermedio, antes de Emergency) ─
    -- El FSM solo dispara heal en Emergency (HP<30%). Aquí extendemos con
    -- un heal "suave" cuando HP baja de 50% pero todavía no es crítico.
    --
    -- Condiciones:
    --  (1) HP entre HP_CRITICAL (30%) y HP_PROACTIVE (50%)
    --  (2) Cooldown interno (≥30 ticks = 1s desde el último proactive)
    --  (3) Mana suficiente para el spell (≥20%)
    --  (4) No estamos en Emergency (el FSM ya dispara heal en Emergency)
    if ctx.hp ~= nil
       and ctx.hp < HP_PROACTIVE
       and ctx.hp >= HP_CRITICAL
       and ctx.fsm ~= "Emergency"
       and (ctx.mana or 0) >= MANA_MIN_FOR_SPELL
       and (tick_counter - last_proactive_heal_tick) >= PROACTIVE_COOLDOWN_TICKS
    then
        last_proactive_heal_tick = tick_counter
        stats.proactive_heals = stats.proactive_heals + 1
        bot.log("debug", string.format(
            "[knight] proactive heal: HP=%.0f%% mana=%.0f%%",
            ctx.hp * 100, (ctx.mana or 1) * 100
        ))
        return "F3"  -- Exura Ico
    end

    return nil
end

-- ══════════════════════════════════════════════════════════════════════════
-- Hook: on_low_hp — custom heal decision en Emergency (HP<30%)
-- ══════════════════════════════════════════════════════════════════════════
--
-- Este hook se llama SOLO cuando HP<30% (Emergency). Tiene acceso al ctx
-- completo (hp, mana, enemies) para decidir qué heal usar.
--
-- Lógica:
--   · HP<15% → crítico máximo: forzar F3 (Exura Ico)
--   · HP 15-30% + mana < 20% → no hay mana para spell, usar F1 (mana pot)
--     para recuperarla — el FSM volverá a llamar on_low_hp en el siguiente
--     tick cuando el mana suba.
--   · HP 15-30% + mana OK → F3 (Exura Ico, default)
--
-- NOTA: F1 en el config del usuario es "Mana Potion", no heal. Cuando mana
-- está vacío, usar F1 tiene el efecto de recuperar mana para poder curar
-- en el siguiente tick. Es un workaround hasta que tengas Ultimate Health
-- Potion en otra tecla.

function on_low_hp(ctx)
    stats.heals_emitted = stats.heals_emitted + 1

    if ctx.hp < HP_VERY_CRITICAL then
        -- HP crítico máximo: siempre spell (aunque gaste mana)
        return "F3"
    end

    if (ctx.mana or 1) < MANA_MIN_FOR_SPELL then
        -- Mana insuficiente para Exura Ico — usar mana potion.
        -- El FSM volverá a entrar en Emergency el próximo tick si HP sigue
        -- bajo, y con más mana podremos lanzar el spell.
        bot.log("warn", string.format(
            "[knight] HP=%.0f%% pero mana=%.0f%% → mana potion primero",
            ctx.hp * 100, (ctx.mana or 0) * 100
        ))
        return "F1"  -- Mana Potion
    end

    -- Default: Exura Ico
    return "F3"
end
