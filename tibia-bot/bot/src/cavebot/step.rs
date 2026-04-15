//! step.rs — Tipos de datos del cavebot.
//!
//! Define `StepKind` (enum de todas las variantes), `Condition` (evaluables
//! contra `TickContext`) y `StandUntil` (criterios de finalización de un
//! step `Stand`).
//!
//! Los labels y gotos se resuelven a índices numéricos en `parser.rs`
//! (pre-computed al load), así que el runtime no hace lookups por nombre.

/// Un step del cavebot: una instrucción que el iterador ejecuta tick a tick.
///
/// El `label_name` es opcional y solo se usa para diagnóstico + para que
/// `Goto/GotoIf` puedan apuntar aquí por nombre en el TOML. Tras `resolve_labels`,
/// los `Goto` contienen el índice absoluto al step target (dentro del flattened
/// array de `Cavebot.steps`).
#[derive(Debug, Clone)]
pub struct Step {
    /// Nombre humano opcional. Si es `Label`, este es el nombre que buscan
    /// los `Goto`. Para otros kinds es solo para logs.
    pub label: Option<String>,
    /// El tipo de acción del step.
    pub kind: StepKind,
}

/// Variantes de pasos del cavebot. Cada variante tiene su propia lógica de
/// tick y sus propios datos estáticos.
///
/// Los `Goto`/`GotoIf` usan `target_idx` — el índice absoluto al step
/// destino, pre-resuelto desde el `label` del TOML por `parser::resolve_labels`.
#[derive(Debug, Clone)]
pub enum StepKind {
    // ── Movimiento ─────────────────────────────────────────────────────
    /// Tecla direccional repetida cada `interval_ms` durante `duration_ms`.
    /// Ejemplo: `walk key=D duration_ms=3000 interval_ms=300` = pulsa D cada
    /// 300ms durante 3s.
    Walk {
        hidcode:     u8,
        duration_ms: u64,
        interval_ms: u64,
    },
    /// Espera N ms sin emitir acción (útil para animaciones de subir escaleras,
    /// esperar por un cooldown, etc).
    Wait {
        duration_ms: u64,
    },
    /// Emite una tecla una vez y avanza inmediatamente (hotkey de rope,
    /// shovel, usar potion, etc).
    Hotkey {
        hidcode: u8,
    },
    /// Quédate en el sitio hasta cumplir una condición. No emite acciones —
    /// simplemente bloquea el avance del cavebot mientras el FSM maneja
    /// combate/heal. Avanza cuando `until` se cumple o se alcanza `max_wait_ms`.
    Stand {
        until:       StandUntil,
        max_wait_ms: u64,
    },

    // ── Control de flujo ───────────────────────────────────────────────
    /// Punto de anclaje nombrado. Solo marca posición para `Goto`. No emite nada.
    /// El field `Step.label` contiene el nombre buscable.
    Label,
    /// Salto incondicional al index pre-resuelto del step destino.
    /// `target_idx` es absoluto dentro del array flattened de `Cavebot.steps`.
    Goto {
        target_label: String,   // preservado para logs + re-resolve
        target_idx:   usize,    // pre-resuelto al load
    },
    /// Salto condicional: si `condition` se evalúa true contra el TickContext,
    /// saltar a `target_idx`. Si no, avanzar al siguiente step.
    GotoIf {
        target_label: String,
        target_idx:   usize,
        condition:    Condition,
    },

    // ── Acción ─────────────────────────────────────────────────────────
    /// Click en coordenada del viewport para lootear un corpse.
    /// `retry_count` es cuántas veces clicar (1-3 típico, por si el primer
    /// click no abre el container por lag).
    Loot {
        vx:          i32,
        vy:          i32,
        retry_count: u8,
    },

    // ── Safety ─────────────────────────────────────────────────────────
    /// Wrapper que saltará al siguiente step si no hay actividad (sin battle
    /// list entries, sin cambio de HP) en `max_wait_ms`. El `inner` es el
    /// step real que se ejecuta hasta detectar stuck.
    ///
    /// Implementación MVP: lo tratamos como un `Walk` normal pero con un
    /// watchdog que mira el `TickContext.last_activity_tick`. Si pasa la
    /// ventana sin actividad, avanzamos forzosamente.
    SkipIfBlocked {
        inner:       Box<StepKind>,
        max_wait_ms: u64,
    },

    // ── Navegación por coordenadas ────────────────────────────────────
    /// Caminar a una posición absoluta (x, y, z) haciendo click en el minimap.
    /// El cliente de Tibia ejecuta pathfinding A* del servidor y el personaje
    /// camina automáticamente. Avanza cuando `is_moving` pasa de true a false
    /// (llegó al destino) o cuando expira `max_wait_ms`.
    ///
    /// El offset se calcula vs el Node anterior: `dx = x - prev.x, dy = y - prev.y`.
    /// El primer Node del script solo registra la posición sin emitir click.
    Node {
        x: i32,
        y: i32,
        z: i32,
        max_wait_ms: u64,
    },

    // ── Cambio de piso ─────────────────────────────────────────────────
    /// Usar rope en un rope hole para subir un piso. El personaje debe estar
    /// parado sobre el agujero (posicionado por un Node anterior). Emite
    /// KeyTap con el hidcode del hotkey de rope (ej: F6) y avanza.
    /// Añadir un Wait posterior para la animación de cambio de piso.
    Rope { hidcode: u8 },
    /// Usar una escalera/rampa. Emite un Click en coordenadas del viewport
    /// (típicamente el centro del game area, donde está el personaje) y avanza.
    /// Añadir un Wait posterior para la animación.
    Ladder { vx: i32, vy: i32 },

    // ── NPC dialog (Fase D) ────────────────────────────────────────────
    /// Tipea una secuencia de frases en el chat de Tibia para hablar con un
    /// NPC (ej: ["hi", "trade"] para abrir una tienda). El step publica
    /// cada frase al `bot.say()` interno del script engine (vía el typing
    /// buffer del loop) y espera hasta que el typing buffer esté drenado
    /// antes de avanzar.
    ///
    /// `wait_prompt_ms`: tras tipear todas las frases, esperar N ms adicionales
    /// para que Tibia responda / abra la ventana de NPC. 0 = avanzar inmediato.
    NpcDialog {
        phrases:        Vec<String>,
        wait_prompt_ms: u64,
    },

    // ── Depot / trading ────────────────────────────────────────────────
    /// Right-click en depot chest, esperar menú, click en "Stow all".
    /// Usado para depositar contenido del backpack tras una hunt session.
    Deposit {
        chest_vx:     i32,
        chest_vy:     i32,
        stow_vx:      i32,
        stow_vy:      i32,
        menu_wait_ms: u64,  // tiempo para que aparezca el menu
        process_ms:   u64,  // tiempo tras click para procesar stow
    },
    /// Comprar N unidades de un item en la trade window abierta.
    /// Emite: left-click en item → wait → N left-clicks en confirm.
    BuyItem {
        item_vx:    i32,
        item_vy:    i32,
        confirm_vx: i32,
        confirm_vy: i32,
        quantity:   u32,
        spacing_ms: u64,    // tiempo entre clicks de confirm
    },
    /// Verifica que todos los items en `requirements` tengan al menos
    /// `min_count` matches en inventario. Si alguno falla, salta a
    /// `on_fail_idx`. Si todos OK, advance.
    CheckSupplies {
        requirements:     Vec<(String, u32)>,
        on_fail_label:    String,
        on_fail_idx:      usize,
    },
}

/// Condición evaluable contra un `TickContext`. Usada por `GotoIf` para
/// decidir si saltar. Las condiciones son PURA LECTURA — no tienen efectos
/// secundarios y son idempotentes para el mismo ctx.
#[derive(Debug, Clone)]
pub enum Condition {
    /// HP ratio menor que el umbral (0.0..1.0). Útil para ir a refill.
    HpBelow(f32),
    /// Mana ratio menor que el umbral.
    ManaBelow(f32),
    /// Contador global de kills ≥ N.
    KillsGte(u64),
    /// Ticks transcurridos desde el inicio del step actual ≥ N.
    /// Útil como timer interno sin necesidad de Wait.
    TimerTicksElapsed(u64),
    /// Battle list vacía ahora mismo.
    NoCombat,
    /// Un elemento de UI con ese nombre está visible en el frame actual.
    /// El nombre corresponde al archivo PNG en `assets/templates/ui/` (sin extensión).
    /// Ejemplo en TOML: `when = "ui_visible(depot_chest)"`
    UiVisible(String),
    /// Cantidad de enemigos en battle list ≥ N. Anti-lure o "esperar N mobs".
    EnemyCountGte(u32),
    /// Sparkles de loot visibles en el viewport (≥ threshold).
    LootVisible,
    /// El personaje se está moviendo (minimapa cambió respecto al frame anterior).
    IsMoving,
    /// El personaje está en la coordenada exacta (x, y, z).
    /// Requiere map index cargado. `goto_if at_coord(32015, 32212, 7)`
    AtCoord(i32, i32, i32),
    /// Distancia Manhattan del personaje a (x, y, z) ≤ N tiles.
    /// `goto_if near_coord(32015, 32212, 7, 5)`
    NearCoord { x: i32, y: i32, z: i32, range: i32 },
    /// Inventario contiene ≥ `min_count` slots del item `name`.
    /// Requiere inventory vision cargada. `goto_if not:has_item(mana_potion, 3)`
    HasItem { name: String, min_count: u32 },
    /// Inventario contiene ≥ `min_units` unidades totales del item `name`,
    /// sumando los stack counts de cada slot. Requiere digit OCR templates
    /// cargados (`assets/templates/digits/`); si no están, se comporta como
    /// `HasItem` (1 unit per slot).
    /// Ejemplo: `goto_if not:has_stack(mana_potion, 50)`
    HasStack { name: String, min_units: u32 },
    /// Negación de cualquier condición (para `goto_if not hp_below(0.9)`).
    Not(Box<Condition>),
}

impl Condition {
    /// Evalúa la condición contra el contexto del tick actual.
    pub fn eval(&self, ctx: &super::runner::TickContext) -> bool {
        match self {
            Condition::HpBelow(th)             => ctx.hp_ratio.map(|r| r < *th).unwrap_or(false),
            Condition::ManaBelow(th)           => ctx.mana_ratio.map(|r| r < *th).unwrap_or(false),
            Condition::KillsGte(n)             => ctx.total_kills >= *n,
            Condition::TimerTicksElapsed(n)    => ctx.ticks_in_current_step >= *n,
            Condition::NoCombat                => !ctx.in_combat,
            Condition::UiVisible(name)         => ctx.ui_matches.contains(name),
            Condition::EnemyCountGte(n)        => ctx.enemy_count >= *n,
            Condition::LootVisible             => ctx.loot_sparkles >= 8,
            Condition::IsMoving                => ctx.is_moving.unwrap_or(false),
            Condition::AtCoord(x, y, z)        => {
                ctx.game_coords.map(|(gx, gy, gz)| gx == *x && gy == *y && gz == *z).unwrap_or(false)
            }
            Condition::NearCoord { x, y, z, range } => {
                ctx.game_coords.map(|(gx, gy, gz)| {
                    gz == *z && (gx - x).abs() + (gy - y).abs() <= *range
                }).unwrap_or(false)
            }
            Condition::HasItem { name, min_count } => {
                ctx.inventory_counts.get(name).copied().unwrap_or(0) >= *min_count
            }
            Condition::HasStack { name, min_units } => {
                ctx.inventory_stacks.get(name).copied().unwrap_or(0) >= *min_units
            }
            Condition::Not(inner)              => !inner.eval(ctx),
        }
    }
}

/// Criterios de finalización para un step `Stand`. El cavebot se queda en el
/// step sin emitir acciones mientras no se cumpla, dejando que el FSM maneje
/// el combate/heal.
#[derive(Debug, Clone)]
pub enum StandUntil {
    /// Mata N mobs desde que el Stand empezó.
    MobsKilled(u32),
    /// HP ratio vuelve a ≥ 0.95.
    HpFull,
    /// Mana ratio vuelve a ≥ 0.95.
    ManaFull,
    /// Simplemente espera N ms.
    TimerMs(u64),
    /// Battle list vacía (no hay mobs a la vista).
    NoCombat,
    /// Esperar hasta que haya ≥ N enemigos en battle list.
    EnemiesGte(u32),
    /// Esperar hasta que el personaje llegue a la coordenada (x, y, z).
    /// Requiere map index cargado. `stand until reached(32015, 32212, 7)`
    ReachedCoord(i32, i32, i32),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cavebot::runner::TickContext;

    fn ctx(hp: Option<f32>, mana: Option<f32>, kills: u64, ticks: u64, in_combat: bool) -> TickContext {
        TickContext {
            tick: 0,
            hp_ratio: hp,
            mana_ratio: mana,
            total_kills: kills,
            ticks_in_current_step: ticks,
            in_combat,
            last_activity_tick: 0,
            ui_matches: vec![],
            is_moving: Some(false),
            enemy_count: if in_combat { 1 } else { 0 },
            loot_sparkles: 0,
            minimap_center: None,
            minimap_displacement: None,
            game_coords: None,
            inventory_counts: std::collections::HashMap::new(),
            inventory_stacks: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn hp_below_condition() {
        let c = Condition::HpBelow(0.5);
        assert!(c.eval(&ctx(Some(0.3), None, 0, 0, false)));
        assert!(!c.eval(&ctx(Some(0.6), None, 0, 0, false)));
        // HP desconocido → no dispara.
        assert!(!c.eval(&ctx(None, None, 0, 0, false)));
    }

    #[test]
    fn kills_gte_condition() {
        let c = Condition::KillsGte(10);
        assert!(c.eval(&ctx(None, None, 10, 0, false)));
        assert!(c.eval(&ctx(None, None, 11, 0, false)));
        assert!(!c.eval(&ctx(None, None, 9, 0, false)));
    }

    #[test]
    fn timer_condition() {
        let c = Condition::TimerTicksElapsed(30);
        assert!(c.eval(&ctx(None, None, 0, 30, false)));
        assert!(!c.eval(&ctx(None, None, 0, 29, false)));
    }

    #[test]
    fn no_combat_condition() {
        let c = Condition::NoCombat;
        assert!(c.eval(&ctx(None, None, 0, 0, false)));
        assert!(!c.eval(&ctx(None, None, 0, 0, true)));
    }

    #[test]
    fn not_condition_inverts() {
        let c = Condition::Not(Box::new(Condition::HpBelow(0.5)));
        assert!(!c.eval(&ctx(Some(0.3), None, 0, 0, false)));
        assert!(c.eval(&ctx(Some(0.8), None, 0, 0, false)));
    }
}
