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

    // ── Abrir trade window via botón bag del greeting ─────────────────
    /// Saluda al NPC tipeando `greeting_phrases` en chat (típicamente
    /// `["hi"]`) y luego CLICKEA el botón de bag del greeting window para
    /// abrir la trade window. Usado para NPCs de Tibia 12 cuyo greeting
    /// muestra un icono de bag que abre la trade window al clickearlo
    /// (ej: Ashari Aelzerand Neeymas en Ab'dendriel — abre trade sin
    /// tipear "trade"/"potions" en chat).
    ///
    /// Flujo de fases:
    /// - Fase 0: tipear `greeting_phrases` una por una (equivalente a
    ///   `NpcDialog`). Cada frase se emite en un tick separado via
    ///   `CavebotAction::Say`.
    /// - Fase 1: esperar `wait_button_ms` (default 800ms) para que Tibia
    ///   renderice el greeting window con el botón de bag.
    /// - Fase 2: emitir un `CavebotAction::Click { vx, vy }` sobre
    ///   `(bag_button_vx, bag_button_vy)`.
    /// - Fase 3: esperar 500ms post-click para que abra la trade window
    ///   y avanzar al siguiente step.
    ///
    /// **Calibración**: abrir el cliente manualmente con el NPC, capturar
    /// el frame con `/test/grab`, medir las coords del botón de bag en
    /// GIMP y pegar en `bag_button_vx`/`bag_button_vy`.
    OpenNpcTrade {
        greeting_phrases: Vec<String>,
        bag_button_vx:    i32,
        bag_button_vy:    i32,
        wait_button_ms:   u64,
    },

    // ── Depot / trading ────────────────────────────────────────────────
    /// Right-click en depot chest, esperar menú, click en "Stow all".
    /// Usado para depositar contenido del backpack tras una hunt session.
    ///
    /// **NOTA 2026-04-16**: En Tibia 12 el workflow moderno es via "Stash"
    /// (Supply Stash del Locker), NO el Depot Chest directo. Este step queda
    /// como legacy para servers antiguos. Para Tibia oficial usar `StowBag`.
    Deposit {
        chest_vx:     i32,
        chest_vy:     i32,
        stow_vx:      i32,
        stow_vy:      i32,
        menu_wait_ms: u64,  // tiempo para que aparezca el menu
        process_ms:   u64,  // tiempo tras click para procesar stow
    },
    /// Deposita TODOS los items stackables del bag al Supply Stash iterando
    /// N veces sobre el slot 0 del bag. Cada iteración hace:
    ///   1. Right-click al slot 0 del bag (el item que esté ahí)
    ///   2. Espera menu_wait_ms
    ///   3. Click en "Stow all items of this type" del menu (offset x/y)
    ///   4. Espera stow_process_ms — bag se reacomoda, siguiente tipo
    ///      de item aparece en slot 0
    ///   5. Repite hasta `max_iterations`
    ///
    /// **Prerequisito**: el char debe estar al lado del depot locker
    /// (proximity) para que la opción "Stow" aparezca en el menu.
    ///
    /// **Comportamiento correcto en Tibia moderno**:
    /// El menu context del CONTAINER no tiene "Stow container's content"
    /// en Tibia oficial. En cambio, se hace right-click sobre un ITEM del
    /// bag y el menu ofrece:
    ///   - "Stow" (stow solo ese item)
    ///   - "Stow all items of this type" (stow todos los del mismo tipo)
    ///
    /// **Comportamiento**:
    /// - Items stackables (gold, potions, runes, loot) → Stash (10k max)
    /// - Items non-stackables (gear con imbue, rares) → quedan en el bag,
    ///   el right-click genera menu sin opción "Stow" → iteración no avanza.
    ///   Por eso tenemos `max_iterations` como safety.
    ///
    /// **Calibración**:
    /// - `slot_vx/vy`: coord del primer slot del bag (típico arriba-izquierda
    ///   del bag panel). Medir con GIMP sobre un frame capturado.
    /// - `menu_offset_x/y`: offset desde el click hasta la línea "Stow all
    ///   items of this type" del menu. El menu aparece a la DERECHA del
    ///   click. Típico: `offset_x=+90` (centro del menu, ~180 px wide),
    ///   `offset_y=+197` (~11 líneas de 18 px cada una).
    /// - `max_iterations`: 4-8 suficiente para loot típico (2-4 tipos
    ///   stackables post-hunt).
    ///
    /// Referencias: [TibiaWiki Supply Stash](https://tibia.fandom.com/wiki/Your_Supply_Stash),
    /// [Depot Locker](https://tibia.fandom.com/wiki/Locker_(Depot)).
    StowAllItems {
        /// X del primer slot del bag en la UI (absolute viewport coord).
        slot_vx:         i32,
        /// Y del primer slot del bag.
        slot_vy:         i32,
        /// Offset lateral desde el click al menu item "Stow all items of
        /// this type". El menu aparece a la derecha del click. Típico +90.
        menu_offset_x:   i32,
        /// Offset vertical al menu item. Típico +197 (línea ~10 del menu).
        menu_offset_y:   i32,
        /// Tiempo para que el menu context se renderice (~300 ms).
        menu_wait_ms:    u64,
        /// Tiempo entre stow iterations para que el bag se reacomode (~800 ms).
        stow_process_ms: u64,
        /// Número máximo de iteraciones. Si los items se acaban antes,
        /// las iteraciones restantes no hacen nada (menu sin "Stow all" para
        /// non-stackables). Default 8.
        max_iterations:  u8,
    },
    /// Escribe texto en un campo de input (no chat) haciendo primero click
    /// sobre el field para activarlo y luego emitiendo un `KeyTap` por cada
    /// caracter del `text`. Pensado para el **search field de la trade
    /// window de Tibia 12** — donde `OpenNpcTrade` abre la ventana y este
    /// step filtra la lista de items por nombre antes del `BuyItem`.
    ///
    /// A diferencia de `NpcDialog` / `OpenNpcTrade`, este step NO tipea en
    /// el chat de Tibia: no wrapea con Enter y no pasa por el typing buffer
    /// de `bot.say()`. Emite los taps directamente al Pico/SendInput.
    ///
    /// Flujo de fases:
    /// - Fase 0: emit `CavebotAction::Click { vx, vy }` sobre `(field_vx,
    ///   field_vy)` para que el input tome focus.
    /// - Fase 1: esperar `wait_after_click_ms` (default 150ms) para que
    ///   Tibia procese el focus.
    /// - Fase 2: loop sobre los caracteres de `text`:
    ///     - Convertir cada char con `crate::act::keycode::ascii_to_hid`.
    ///     - Chars sin mapping (mayúsculas, símbolos) → `tracing::warn!`
    ///       y skip; no panic.
    ///     - Emitir `CavebotAction::KeyTap(hid)` y esperar
    ///       `char_spacing_ms` (default 80ms) antes del próximo tap.
    /// - Fase 3: esperar `wait_after_type_ms` (default 200ms) para que la
    ///   UI termine de aplicar el filtro.
    /// - Fase 4: advance al siguiente step.
    ///
    /// **Limitación**: `ascii_to_hid` solo soporta `a-z`, `0-9`, `space` y
    /// `\n`/`\r`. Mayúsculas y símbolos se ignoran (en la mayoría de casos
    /// el match de la search bar es case-insensitive).
    TypeInField {
        field_vx:            i32,
        field_vy:            i32,
        text:                String,
        wait_after_click_ms: u64,
        wait_after_type_ms:  u64,
        char_spacing_ms:     u64,
    },
    /// Comprar N unidades de un item en la trade window abierta.
    ///
    /// Soporta dos flujos según si `amount_vx`/`amount_vy` están presentes:
    ///
    /// **Legacy (Tibia clásico o NPCs sin Amount field)** — cuando ambos
    /// `amount_vx` y `amount_vy` son `None`:
    ///   1. Click en `(item_vx, item_vy)` para seleccionar la fila del item.
    ///   2. Esperar `spacing_ms`.
    ///   3. Emitir `quantity` left-clicks sobre `(confirm_vx, confirm_vy)`
    ///      con `spacing_ms` de separación (cada click compra 1 unidad).
    ///
    /// **Tibia 12 con Amount field** — cuando ambos están `Some`:
    ///   1. Click en `(item_vx, item_vy)` (selecciona la fila del item).
    ///   2. Esperar 200ms para que la trade window reaccione al select.
    ///   3. Click en `(amount_vx, amount_vy)` (focus al input Amount).
    ///   4. Esperar 150ms para que el input tome focus.
    ///   5. Por cada dígito decimal del string de `quantity` (en orden):
    ///        - Emitir un `KeyTap` con el HID code del dígito.
    ///        - Esperar `spacing_ms/2` entre dígitos (mínimo 1 tick).
    ///   6. Esperar 150ms post-dígitos.
    ///   7. UN solo click sobre `(confirm_vx, confirm_vy)` (compra todas las
    ///      unidades de golpe porque el Amount field ya está escrito).
    ///   8. Esperar `spacing_ms` para que la UI procese.
    ///   9. Avanzar al siguiente step.
    ///
    /// El modo Amount es el correcto para la UI moderna de Tibia 12; el legacy
    /// queda para retrocompatibilidad con scripts antiguos.
    BuyItem {
        item_vx:    i32,
        item_vy:    i32,
        /// Input field "Amount" en la trade window. Si ambos son `Some`, se
        /// usa el flujo moderno (tipeo de dígitos + 1 click); si ambos son
        /// `None`, flujo legacy (N clicks de confirm).
        amount_vx:  Option<i32>,
        amount_vy:  Option<i32>,
        confirm_vx: i32,
        confirm_vy: i32,
        quantity:   u32,
        /// En modo legacy: tiempo entre clicks de confirm.
        /// En modo Amount: tiempo base (dígitos usan `spacing_ms/2`,
        /// post-flujo espera `spacing_ms` antes de avanzar).
        spacing_ms: u64,
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
