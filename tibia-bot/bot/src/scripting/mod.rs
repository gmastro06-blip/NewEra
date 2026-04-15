//! scripting/mod.rs — Motor de scripts Lua 5.4 sandboxed.
//!
//! El `ScriptEngine` vive **dentro del thread del game loop** — `mlua::Lua`
//! no es `Send` y no queremos la sobrecarga de la feature `send`. El
//! `BotLoop::run()` crea el engine al arrancar y lo reemplaza en respuesta
//! a `LoopCommand::ReloadScripts`.
//!
//! ## API de scripts
//!
//! Los scripts declaran funciones globales con nombres específicos:
//!
//! ```lua
//! -- Llamado cada tick tras la visión, antes de la FSM.
//! -- `ctx` es una tabla read-only con: tick, hp, mana, enemies, fsm.
//! function on_tick(ctx)
//!   if ctx.enemies > 2 then
//!     bot.log("warn", "¡Muchos enemigos!")
//!   end
//! end
//!
//! -- Llamado cuando HP < HP_CRITICAL_RATIO, ANTES de que la FSM emita heal.
//! -- Retornar `nil` → usar la hotkey default del config.
//! -- Retornar una string → usar esa tecla en lugar del default.
//! function on_low_hp(ratio)
//!   if ratio < 0.15 then
//!     return "F2"  -- pocion en vez del spell
//!   end
//!   return nil     -- dejar default (heal_spell)
//! end
//! ```
//!
//! ## Sandbox
//!
//! Tras cargar los scripts, los globals peligrosos son reemplazados por `nil`:
//! `io`, `os`, `package`, `require`, `dofile`, `loadfile`, `debug`. Esto evita
//! que un script lea archivos, ejecute comandos del sistema, cargue librerías
//! o monkey-patchee el runtime.
//!
//! ## Budget
//!
//! Cada invocación de hook se cronometra. Si excede `tick_budget_ms` se
//! loggea un warning pero el script NO es interrumpido (mlua 0.10 soporta
//! interrupts pero añade complejidad que MVP no justifica).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use std::cell::Cell;
use mlua::{Function, HookTriggers, Lua, Table, Value, VmState};
use tracing::{info, warn};

/// Resultado de invocar un hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptResult {
    /// El hook retornó `nil` o no existe — usar comportamiento default del bot.
    Noop,
    /// El hook retornó una string que se parseó exitosamente a un keycode HID.
    Hotkey(u8),
    /// El hook falló (error de ejecución, tipo de retorno inválido, etc).
    /// El bot debe caer al default y seguir — no crashear.
    Error(String),
}

/// Contexto inmutable pasado al hook `on_tick` como una tabla Lua.
/// Los campos son un subset del `Perception` + `GameState` relevantes para
/// scripting. Se construye en cada tick, así que mantenerlo pequeño.
#[derive(Debug, Clone, Default)]
pub struct TickContext {
    pub tick:        u64,
    pub hp_ratio:    Option<f32>,
    pub mana_ratio:  Option<f32>,
    pub enemy_count: u32,
    pub fsm_state:   String,
    /// Nombres de templates de UI visibles en el frame (ver `assets/templates/ui/`).
    /// Accesible en Lua como `ctx.ui["depot_chest"]` → true/nil.
    pub ui_matches:  Vec<String>,
}

/// Motor de scripts. NO es `Send` — vive en el game loop thread.
pub struct ScriptEngine {
    lua:           Lua,
    /// Archivos .lua cargados en la última llamada a `load_dir`.
    loaded_files:  Vec<PathBuf>,
    /// Errores capturados en el último ciclo de hooks (para /scripts/status).
    last_errors:   Vec<String>,
    /// Budget de tiempo por hook, en ms. 0 desactiva el warning.
    tick_budget_ms: f64,
    /// Cola de strings pendientes de tipear via `bot.say()`. Compartida con
    /// las closures Lua vía `Rc<RefCell<>>` porque mlua `Lua` no es `Send` y
    /// las closures tampoco. El BotLoop drena esta cola cada tick y convierte
    /// cada string en KEY_TAPs paceados.
    say_queue: Rc<RefCell<VecDeque<String>>>,
    /// Deadline absoluto para el hook en ejecución. `None` = sin hook activo.
    /// Leído por el callback de `Lua::set_interrupt` para matar hooks que
    /// excedan `tick_budget_ms`. Rc<Cell<>> porque el interrupt callback no
    /// puede tomar prestado `self`.
    hook_deadline: Rc<Cell<Option<Instant>>>,
}

impl ScriptEngine {
    /// Crea un engine con Lua vacío y sandbox aplicado. Sin scripts cargados.
    pub fn new(tick_budget_ms: f64) -> Result<Self> {
        let lua = Lua::new();
        let say_queue = Rc::new(RefCell::new(VecDeque::new()));
        Self::apply_sandbox(&lua)
            .map_err(|e| anyhow!("Error aplicando sandbox al runtime Lua: {}", e))?;
        Self::install_bot_api(&lua, &say_queue)
            .map_err(|e| anyhow!("Error instalando API `bot` en el runtime Lua: {}", e))?;

        // Registrar hook VM que mata cualquier hook que exceda `hook_deadline`.
        // Lua 5.4 llama al debug hook cada N instrucciones (configurable).
        // Si retornamos Err, el `func.call()` falla con ese error y el
        // script se aborta limpiamente. El overhead de 1 instrucción cada 1000
        // es ~0.1% — negligible para nuestros hooks.
        let hook_deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let deadline_cb = hook_deadline.clone();
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(1000),
            move |_lua, _dbg| {
                if let Some(dl) = deadline_cb.get() {
                    if Instant::now() >= dl {
                        return Err(mlua::Error::RuntimeError(
                            "script hook budget exceeded".into()
                        ));
                    }
                }
                Ok(VmState::Continue)
            },
        );

        Ok(Self {
            lua,
            loaded_files: Vec::new(),
            last_errors:  Vec::new(),
            tick_budget_ms,
            say_queue,
            hook_deadline,
        })
    }

    /// Drena la cola de strings pendientes de tipear. El BotLoop llama a
    /// esta función cada tick después de invocar los hooks Lua y convierte
    /// cada string en una secuencia de KEY_TAPs paceados.
    pub fn drain_say_queue(&self) -> Vec<String> {
        let mut q = self.say_queue.borrow_mut();
        q.drain(..).collect()
    }

    /// Reemplaza los globals peligrosos por `nil`. Debe llamarse antes de
    /// cargar cualquier script de usuario.
    fn apply_sandbox(lua: &Lua) -> mlua::Result<()> {
        let globals = lua.globals();
        for name in &[
            "io", "os", "package", "require",
            "dofile", "loadfile", "debug",
        ] {
            globals.set(*name, Value::Nil)?;
        }
        Ok(())
    }

    /// Instala una tabla `bot` con funciones utilitarias que los scripts
    /// pueden llamar (`bot.log`, `bot.say`).
    fn install_bot_api(lua: &Lua, say_queue: &Rc<RefCell<VecDeque<String>>>) -> mlua::Result<()> {
        let bot_table = lua.create_table()?;

        // bot.log(level, msg) — emite un log en el tracing del bot.
        // Niveles válidos: "error", "warn", "info", "debug".
        let log_fn = lua.create_function(|_, (level, msg): (String, String)| {
            match level.to_lowercase().as_str() {
                "error" => tracing::error!("[lua] {}", msg),
                "warn"  => tracing::warn!("[lua] {}", msg),
                "info"  => tracing::info!("[lua] {}", msg),
                "debug" => tracing::debug!("[lua] {}", msg),
                _       => tracing::info!("[lua] {}", msg),
            }
            Ok(())
        })?;
        bot_table.set("log", log_fn)?;

        // bot.say(text) — encola texto para tipear en el chat del juego.
        // El game loop drena la cola cada tick y convierte cada char en
        // KEY_TAPs paceados. Se antepone/pospone Enter para abrir/cerrar
        // el chat input de Tibia.
        //
        // Soporta letras (mayúsculas y minúsculas), dígitos y símbolos
        // US-QWERTY. El firmware Pico maneja Shift automáticamente por
        // carácter (ver ascii_to_hid en pico2_hid.ino).
        let say_queue_clone = Rc::clone(say_queue);
        let say_fn = lua.create_function(move |_, text: String| {
            if text.is_empty() {
                return Ok(());
            }
            let mut q = say_queue_clone.borrow_mut();
            q.push_back(text);
            Ok(())
        })?;
        bot_table.set("say", say_fn)?;

        lua.globals().set("bot", bot_table)?;
        Ok(())
    }

    /// Resetea completamente el runtime Lua. Crea un nuevo `Lua::new()`,
    /// re-aplica el sandbox, re-instala la API `bot`, y re-registra el hook
    /// de budget. **Preserva** `say_queue` y `hook_deadline` vía `Rc::clone`
    /// para que el BotLoop que ya tiene referencias a ellos siga funcionando.
    ///
    /// Se usa desde `load_dir` para garantizar que los hooks antiguos
    /// (`on_tick`, `on_low_hp`, upvalues capturados) desaparezcan por
    /// completo en un reload. Sin este reset, scripts que tuvieron errores
    /// parciales podrían dejar globals stale en el estado.
    fn reset_lua_state(&mut self) -> Result<()> {
        let new_lua = Lua::new();
        Self::apply_sandbox(&new_lua)
            .map_err(|e| anyhow!("reset: error aplicando sandbox: {}", e))?;
        Self::install_bot_api(&new_lua, &self.say_queue)
            .map_err(|e| anyhow!("reset: error instalando API bot: {}", e))?;

        // Re-registrar el debug hook de budget con el mismo `hook_deadline`.
        let deadline_cb = self.hook_deadline.clone();
        new_lua.set_hook(
            HookTriggers::new().every_nth_instruction(1000),
            move |_lua, _dbg| {
                if let Some(dl) = deadline_cb.get() {
                    if Instant::now() >= dl {
                        return Err(mlua::Error::RuntimeError(
                            "script hook budget exceeded".into()
                        ));
                    }
                }
                Ok(VmState::Continue)
            },
        );

        self.lua = new_lua;
        Ok(())
    }

    /// Carga todos los archivos .lua del directorio dado (no recursivo).
    /// Los archivos se ejecutan en orden alfabético para que el usuario
    /// pueda controlar dependencias con prefijos numéricos.
    ///
    /// **IMPORTANTE**: antes de cargar, el runtime Lua se resetea por
    /// completo (sandbox + bot_api re-aplicados, globals nukeados). Esto
    /// garantiza que un hot-reload sobrescriba funciones viejas incluso
    /// cuando capturan upvalues locales del chunk (patrón común en los
    /// scripts de healer con `local last_heal_tick = 0`). Sin este reset,
    /// la hot-reload era no-op para cambios en locals/cooldowns.
    ///
    /// Si un script falla al cargar, el error se registra en `last_errors`
    /// y los otros scripts siguen cargándose. El engine nunca queda en
    /// estado inconsistente por un script roto.
    pub fn load_dir(&mut self, dir: &Path) -> Result<()> {
        self.loaded_files.clear();
        self.last_errors.clear();
        self.reset_lua_state()
            .with_context(|| "reset Lua state antes de load_dir")?;

        if !dir.exists() {
            anyhow::bail!("script_dir '{}' no existe", dir.display());
        }

        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("No se pudo leer '{}'", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lua"))
            .collect();
        files.sort();

        for path in files {
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("read '{}': {}", path.display(), e);
                    warn!("script load: {}", msg);
                    self.last_errors.push(msg);
                    continue;
                }
            };
            let chunk_name = path.display().to_string();
            match self.lua.load(src).set_name(&chunk_name).exec() {
                Ok(()) => {
                    info!("script cargado: '{}'", path.display());
                    self.loaded_files.push(path);
                }
                Err(e) => {
                    let msg = format!("exec '{}': {}", chunk_name, e);
                    warn!("script load: {}", msg);
                    self.last_errors.push(msg);
                }
            }
        }
        Ok(())
    }

    /// ¿Existe una función global con el nombre dado?
    pub fn has_hook(&self, name: &str) -> bool {
        matches!(
            self.lua.globals().get::<Value>(name),
            Ok(Value::Function(_))
        )
    }

    /// Invoca `on_tick(ctx)`. `ctx` se pasa como tabla Lua con los campos
    /// de `TickContext`. El retorno se ignora (on_tick es para side-effects).
    pub fn fire_on_tick(&mut self, ctx: &TickContext) -> ScriptResult {
        if !self.has_hook("on_tick") {
            return ScriptResult::Noop;
        }
        let tbl = match self.build_ctx_table(ctx) {
            Ok(t) => t,
            Err(e) => return ScriptResult::Error(format!("build ctx: {}", e)),
        };
        let func: Function = match self.lua.globals().get("on_tick") {
            Ok(f)  => f,
            Err(e) => return ScriptResult::Error(format!("get on_tick: {}", e)),
        };
        self.invoke_with_budget("on_tick", move || func.call::<Value>(tbl))
    }

    /// Invoca `on_fsm_state_change(new_state, reason)` cuando el FSM cambia
    /// de estado o el `safety_pause_reason` cambia. El hook es best-effort:
    /// los scripts pueden loggear alertas (char muerto, disconnect, break
    /// iniciado) sin impactar la decisión del FSM.
    ///
    /// **Argumentos pasados**:
    /// - `new_state: string` — nombre del FsmState nuevo ("Paused", "Idle", ...)
    /// - `reason: string | nil` — motivo de safety pause (ej "prompt:char_select",
    ///   "char:dead", "break:micro"); `nil` si no hay pause reason asociada.
    ///
    /// El retorno del hook se IGNORA (no puede override la transición — eso
    /// corrompería la FSM). Solo usar para side-effects (log, say, alerts).
    ///
    /// **Ejemplo de uso en Lua**:
    /// ```lua
    /// function on_fsm_state_change(new_state, reason)
    ///     if reason == "prompt:char_select" then
    ///         bot.log("error", "CHAR MUERTO — sesión detenida")
    ///     end
    /// end
    /// ```
    pub fn fire_on_fsm_state_change(
        &mut self,
        new_state: &str,
        reason: Option<&str>,
    ) -> ScriptResult {
        if !self.has_hook("on_fsm_state_change") {
            return ScriptResult::Noop;
        }
        let func: Function = match self.lua.globals().get("on_fsm_state_change") {
            Ok(f)  => f,
            Err(e) => return ScriptResult::Error(format!("get on_fsm_state_change: {}", e)),
        };
        let state_str = new_state.to_string();
        let reason_opt = reason.map(|s| s.to_string());
        self.invoke_with_budget("on_fsm_state_change", move || {
            match reason_opt {
                Some(r) => func.call::<Value>((state_str, r)),
                None    => func.call::<Value>((state_str, Value::Nil)),
            }
        })
    }

    /// Invoca `on_low_hp(ctx)`. El hook recibe una tabla Lua con el mismo
    /// contexto que `on_tick`: `{tick, hp, mana, enemies, fsm}`.
    ///
    /// **Compatibilidad**: scripts antiguos que declaran
    /// `function on_low_hp(ratio)` siguen funcionando porque Lua ignora
    /// argumentos extra, PERO ahora reciben una tabla como primer argumento
    /// en vez de un float — si hacían aritmética con `ratio` directamente,
    /// romperán. El script `example_healer.lua` y `knight_hunt.lua` de
    /// este proyecto usan la nueva firma.
    ///
    /// El retorno puede ser:
    /// - `nil` → `Noop` (bot usa su heal_spell default)
    /// - string → se parsea con `keycode::parse`; success → `Hotkey(u8)`
    /// - otro tipo → `Error`
    pub fn fire_on_low_hp(&mut self, ctx: &TickContext) -> ScriptResult {
        if !self.has_hook("on_low_hp") {
            return ScriptResult::Noop;
        }
        let tbl = match self.build_ctx_table(ctx) {
            Ok(t) => t,
            Err(e) => return ScriptResult::Error(format!("build ctx: {}", e)),
        };
        let func: Function = match self.lua.globals().get("on_low_hp") {
            Ok(f)  => f,
            Err(e) => return ScriptResult::Error(format!("get on_low_hp: {}", e)),
        };
        self.invoke_with_budget("on_low_hp", move || func.call::<Value>(tbl))
    }

    /// Cronometra una invocación, convierte el `Value` retornado a
    /// `ScriptResult` y emite warning si excede el budget.
    fn invoke_with_budget<F>(&mut self, hook_name: &str, f: F) -> ScriptResult
    where
        F: FnOnce() -> mlua::Result<Value>,
    {
        let start = Instant::now();
        // Armar el deadline para el interrupt callback si hay budget configurado.
        // `Duration::from_secs_f64` puede panic con valores negativos → clamp.
        if self.tick_budget_ms > 0.0 {
            let budget = Duration::from_secs_f64(self.tick_budget_ms.max(0.0) / 1000.0);
            self.hook_deadline.set(Some(start + budget));
        }
        let result = f();
        // Desarmar el deadline para que futuros invoke_with_budget no se vean
        // afectados por el callback si no se re-arman.
        self.hook_deadline.set(None);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        if self.tick_budget_ms > 0.0 && elapsed_ms > self.tick_budget_ms {
            warn!(
                "script hook '{}' excedió budget: {:.2}ms > {:.2}ms",
                hook_name, elapsed_ms, self.tick_budget_ms
            );
        }

        match result {
            Ok(Value::Nil) => ScriptResult::Noop,
            Ok(Value::String(s)) => {
                match s.to_str() {
                    Ok(s_str) => {
                        let s_owned = s_str.to_string();
                        match crate::act::keycode::parse(&s_owned) {
                            Ok(hid) => ScriptResult::Hotkey(hid),
                            Err(e)  => {
                                let msg = format!(
                                    "{} returned invalid key '{}': {}",
                                    hook_name, s_owned, e
                                );
                                self.last_errors.push(msg.clone());
                                ScriptResult::Error(msg)
                            }
                        }
                    }
                    Err(e) => {
                        let msg = format!("{} returned non-utf8 string: {}", hook_name, e);
                        self.last_errors.push(msg.clone());
                        ScriptResult::Error(msg)
                    }
                }
            }
            Ok(_other) => {
                // Retorno desconocido (bool, number, table, etc) → tratamos
                // como Noop. Hooks como on_tick son intencionalmente void.
                ScriptResult::Noop
            }
            Err(e) => {
                let msg = format!("{} runtime error: {}", hook_name, e);
                self.last_errors.push(msg.clone());
                ScriptResult::Error(msg)
            }
        }
    }

    /// Construye una tabla Lua desde el TickContext. Read-only por convención
    /// (Lua no tiene read-only nativo, pero el script no debería mutarla).
    fn build_ctx_table(&self, ctx: &TickContext) -> mlua::Result<Table> {
        let t = self.lua.create_table()?;
        t.set("tick", ctx.tick)?;
        t.set("hp",   ctx.hp_ratio)?;
        t.set("mana", ctx.mana_ratio)?;
        t.set("enemies", ctx.enemy_count)?;
        t.set("fsm",  ctx.fsm_state.as_str())?;
        // ctx.ui["depot_chest"] → true si ese template está visible, nil si no.
        let ui_tbl = self.lua.create_table()?;
        for name in &ctx.ui_matches {
            ui_tbl.set(name.as_str(), true)?;
        }
        t.set("ui", ui_tbl)?;
        Ok(t)
    }

    /// Snapshot de archivos cargados (para /scripts/status).
    pub fn loaded_files(&self) -> &[PathBuf] {
        &self.loaded_files
    }

    /// Snapshot de errores del último ciclo (para /scripts/status).
    pub fn last_errors(&self) -> &[String] {
        &self.last_errors
    }

    /// Limpia los errores registrados.
    pub fn clear_errors(&mut self) {
        self.last_errors.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> ScriptEngine {
        ScriptEngine::new(5.0).expect("engine creation")
    }

    fn load_inline(e: &ScriptEngine, src: &str, name: &str) {
        // Ejecuta código Lua directamente (sin pasar por load_dir).
        e.lua.load(src).set_name(name).exec().expect("lua exec");
    }


    fn low_hp_ctx(ratio: f32) -> TickContext {
        TickContext {
            tick: 0,
            hp_ratio: Some(ratio),
            mana_ratio: Some(1.0),
            enemy_count: 0,
            fsm_state: "Emergency".into(),
            ui_matches: vec![],
        }
    }

    #[test]
    fn new_engine_has_no_hooks() {
        let e = engine();
        assert!(!e.has_hook("on_tick"));
        assert!(!e.has_hook("on_low_hp"));
    }

    #[test]
    fn sandbox_blocks_io_os_and_friends() {
        let e = engine();
        let code = r#"
            if io ~= nil then error("io was not nilled") end
            if os ~= nil then error("os was not nilled") end
            if package ~= nil then error("package was not nilled") end
            if require ~= nil then error("require was not nilled") end
            if dofile ~= nil then error("dofile was not nilled") end
            if loadfile ~= nil then error("loadfile was not nilled") end
            if debug ~= nil then error("debug was not nilled") end
        "#;
        load_inline(&e, code, "sandbox_check");
    }

    #[test]
    fn bot_log_api_is_callable() {
        let e = engine();
        // El test se conforma con que no crashee. El output va a tracing.
        load_inline(&e, r#"bot.log("info", "hola desde lua")"#, "log_test");
    }

    #[test]
    fn bot_say_api_queues_strings() {
        let e = engine();
        // Drain inicial — cola vacía.
        assert!(e.drain_say_queue().is_empty());

        // Un say.
        load_inline(&e, r#"bot.say("hi")"#, "say_test_1");
        let drained = e.drain_say_queue();
        assert_eq!(drained, vec!["hi"]);

        // Tras drain, la cola vuelve a vacía.
        assert!(e.drain_say_queue().is_empty());

        // Múltiples says en secuencia.
        load_inline(&e, r#"
            bot.say("hi")
            bot.say("trade")
            bot.say("bye")
        "#, "say_test_2");
        let drained = e.drain_say_queue();
        assert_eq!(drained, vec!["hi", "trade", "bye"]);
    }

    #[test]
    fn bot_say_ignores_empty_strings() {
        let e = engine();
        load_inline(&e, r#"
            bot.say("")
            bot.say("hi")
            bot.say("")
        "#, "say_empty_test");
        assert_eq!(e.drain_say_queue(), vec!["hi"]);
    }

    #[test]
    fn fire_on_low_hp_with_no_hook_returns_noop() {
        let mut e = engine();
        assert_eq!(e.fire_on_low_hp(&low_hp_ctx(0.10)), ScriptResult::Noop);
    }

    #[test]
    fn fire_on_low_hp_returns_hotkey_from_string() {
        let mut e = engine();
        // Nueva firma: on_low_hp recibe una TABLA con (tick, hp, mana, enemies, fsm).
        load_inline(&e, r#"
            function on_low_hp(ctx)
                if ctx.hp < 0.15 then return "F2" end
                return nil
            end
        "#, "hp_hook");
        assert_eq!(e.fire_on_low_hp(&low_hp_ctx(0.10)), ScriptResult::Hotkey(0x3B)); // F2
        assert_eq!(e.fire_on_low_hp(&low_hp_ctx(0.20)), ScriptResult::Noop);          // nil
    }

    #[test]
    fn fire_on_low_hp_ctx_includes_mana() {
        // Verifica que el ctx en on_low_hp incluye mana (para decisiones
        // "heal solo si tengo mana para spell").
        let mut e = engine();
        load_inline(&e, r#"
            function on_low_hp(ctx)
                if ctx.mana < 0.20 then return "F2" end  -- mana bajo: potion
                return "F3"                                -- mana OK: spell
            end
        "#, "mana_aware_hook");

        // Mana al 10% (bajo) → F2 (potion)
        let ctx_low_mana = TickContext {
            tick: 0,
            hp_ratio: Some(0.25),
            mana_ratio: Some(0.10),
            enemy_count: 0,
            fsm_state: "Emergency".into(),
            ui_matches: vec![],
        };
        assert_eq!(e.fire_on_low_hp(&ctx_low_mana), ScriptResult::Hotkey(0x3B));

        // Mana al 80% (alto) → F3 (spell)
        let ctx_high_mana = TickContext {
            tick: 0,
            hp_ratio: Some(0.25),
            mana_ratio: Some(0.80),
            enemy_count: 0,
            fsm_state: "Emergency".into(),
            ui_matches: vec![],
        };
        assert_eq!(e.fire_on_low_hp(&ctx_high_mana), ScriptResult::Hotkey(0x3C));
    }

    #[test]
    fn fire_on_low_hp_invalid_key_is_error() {
        let mut e = engine();
        load_inline(&e, r#"
            function on_low_hp(_)
                return "NotAKey"
            end
        "#, "bad_hook");
        match e.fire_on_low_hp(&low_hp_ctx(0.10)) {
            ScriptResult::Error(msg) => assert!(msg.contains("NotAKey"), "msg={}", msg),
            other => panic!("esperaba Error, recibí {:?}", other),
        }
        assert!(!e.last_errors().is_empty());
    }

    #[test]
    fn script_runtime_error_is_captured_not_crashed() {
        let mut e = engine();
        load_inline(&e, r#"
            function on_low_hp(_)
                error("explota")
            end
        "#, "crash_hook");
        match e.fire_on_low_hp(&low_hp_ctx(0.10)) {
            ScriptResult::Error(msg) => assert!(msg.contains("explota"), "msg={}", msg),
            other => panic!("esperaba Error, recibí {:?}", other),
        }
    }

    #[test]
    fn fire_on_tick_passes_context_table() {
        let mut e = engine();
        load_inline(&e, r#"
            seen = { tick = nil, hp = nil, enemies = nil, fsm = nil }
            function on_tick(ctx)
                seen.tick    = ctx.tick
                seen.hp      = ctx.hp
                seen.enemies = ctx.enemies
                seen.fsm     = ctx.fsm
            end
        "#, "tick_hook");

        let ctx = TickContext {
            tick: 42,
            hp_ratio: Some(0.75),
            mana_ratio: Some(0.50),
            enemy_count: 3,
            fsm_state: "Walking".into(),
            ui_matches: vec![],
        };
        let r = e.fire_on_tick(&ctx);
        assert_eq!(r, ScriptResult::Noop);

        // Leer el global `seen` y verificar.
        let seen: Table = e.lua.globals().get("seen").unwrap();
        let tick: u64    = seen.get("tick").unwrap();
        let hp:   f32    = seen.get("hp").unwrap();
        let en:   u32    = seen.get("enemies").unwrap();
        let fsm:  String = seen.get("fsm").unwrap();
        assert_eq!(tick, 42);
        assert!((hp - 0.75).abs() < 1e-6);
        assert_eq!(en, 3);
        assert_eq!(fsm, "Walking");
    }

    #[test]
    fn load_dir_reads_all_lua_files() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("tibia_bot_scripts_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let p1 = dir.join("10_ok.lua");
        std::fs::File::create(&p1).unwrap().write_all(
            b"function on_low_hp(r) return \"F1\" end"
        ).unwrap();

        let p2 = dir.join("20_broken.lua");
        std::fs::File::create(&p2).unwrap().write_all(
            b"function on_tick( -- syntax error"
        ).unwrap();

        // Archivo que NO es .lua (se ignora).
        let p3 = dir.join("README.md");
        std::fs::File::create(&p3).unwrap().write_all(b"not lua").unwrap();

        let mut e = engine();
        e.load_dir(&dir).unwrap();

        assert_eq!(e.loaded_files().len(), 1, "solo uno válido");
        assert!(e.loaded_files()[0].ends_with("10_ok.lua"));
        assert_eq!(e.last_errors().len(), 1);
        assert!(e.last_errors()[0].contains("20_broken.lua"));

        // El hook del script válido debe estar accesible.
        assert_eq!(e.fire_on_low_hp(&low_hp_ctx(0.10)), ScriptResult::Hotkey(0x3A));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_budget_hard_interrupt_kills_infinite_loop() {
        // Budget muy bajo (5ms). El script tiene loop infinito.
        // El debug hook debe matarlo antes de que drene el CPU.
        let mut e = ScriptEngine::new(5.0).expect("engine");
        // on_tick con busy loop — no puede salir sin el interrupt.
        let src = r#"
            function on_tick(ctx)
                local n = 0
                while true do
                    n = n + 1
                end
                return nil
            end
        "#;
        e.lua.load(src).set_name("infinite_tick").exec().expect("lua exec");

        let ctx = TickContext {
            tick: 0,
            hp_ratio: Some(1.0),
            mana_ratio: Some(1.0),
            enemy_count: 0,
            fsm_state: "Idle".into(),
            ui_matches: vec![],
        };

        let t0 = std::time::Instant::now();
        let result = e.fire_on_tick(&ctx);
        let elapsed = t0.elapsed();

        // Debe terminar con Error (budget exceeded) en tiempo razonable.
        // Con budget=5ms + hook check cada 1000 instrucciones, el corte
        // suele ocurrir en <100ms. Dejamos 500ms de margen.
        assert!(matches!(result, ScriptResult::Error(_)),
            "esperaba Error por budget, got {:?}", result);
        assert!(elapsed.as_millis() < 500,
            "interrupt tardó demasiado: {}ms", elapsed.as_millis());
        let err_msg = match result {
            ScriptResult::Error(m) => m,
            _ => unreachable!(),
        };
        assert!(err_msg.contains("budget exceeded") || err_msg.contains("runtime"),
            "mensaje inesperado: {}", err_msg);
    }

    #[test]
    fn hook_budget_allows_normal_execution() {
        // Budget amplio (100ms). Script normal debe correr sin problemas.
        let mut e = ScriptEngine::new(100.0).expect("engine");
        let src = r#"
            function on_tick(ctx)
                return "F1"
            end
        "#;
        e.lua.load(src).set_name("normal_tick").exec().expect("lua exec");

        let ctx = TickContext {
            tick: 0,
            hp_ratio: Some(1.0),
            mana_ratio: Some(1.0),
            enemy_count: 0,
            fsm_state: "Idle".into(),
            ui_matches: vec![],
        };

        // Esperamos Hotkey(F1 = 0x3A)
        assert_eq!(e.fire_on_tick(&ctx), ScriptResult::Hotkey(0x3A));
    }

    /// Smoke test: los scripts bundled en assets/scripts/ deben cargar sin
    /// errores de sintaxis ni del sandbox. Si alguien añade un script nuevo
    /// con bug, este test lo captura.
    #[test]
    fn bundled_scripts_load_without_errors() {
        let candidates = [
            "../assets/scripts",
            "assets/scripts",
        ];
        let dir = candidates.iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.exists());
        let Some(dir) = dir else {
            eprintln!("skip: assets/scripts not found from CI path");
            return;
        };

        let mut e = ScriptEngine::new(5.0).expect("engine");
        e.load_dir(&dir).expect("load_dir");

        // Todos los .lua deben haber cargado sin error de parse.
        // Si hay errores, listarlos.
        if !e.last_errors().is_empty() {
            panic!(
                "{} script(s) failed to load:\n{}",
                e.last_errors().len(),
                e.last_errors().join("\n")
            );
        }
        // Verificar que al menos los 4 nuevos de F3 + los 4 existentes cargaron.
        // Contamos los .lua reales (no incluyendo backups, tmp, etc).
        let lua_count = std::fs::read_dir(&dir).unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("lua"))
            .count();
        assert_eq!(e.loaded_files().len(), lua_count,
            "esperaba {} scripts cargados, got {}", lua_count, e.loaded_files().len());
    }

    /// Bug fix (Phase A.4): /scripts/reload no aplicaba cambios en scripts
    /// con upvalues locales. El reset_lua_state en load_dir lo arregla.
    ///
    /// Este test reproduce el escenario: carga un script v1 que retorna F1,
    /// invoca el hook, verifica F1. Luego sobrescribe el archivo con v2 que
    /// retorna F2, vuelve a llamar load_dir, invoca el hook, verifica F2.
    ///
    /// Sin reset_lua_state, el Lua runtime mantenía closures viejas en
    /// upvalues capturados y la nueva versión no se aplicaba correctamente.
    #[test]
    fn reload_replaces_old_function() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("tibia_bot_reload_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("healer.lua");

        // Versión 1: retorna F1 con cooldown corto (upvalue local).
        let v1 = r#"
            local COOLDOWN = 10
            local last = 0
            function on_low_hp(ctx)
                if ctx.tick - last >= COOLDOWN then
                    last = ctx.tick
                    return "F1"
                end
                return nil
            end
        "#;
        std::fs::File::create(&script_path).unwrap().write_all(v1.as_bytes()).unwrap();

        let mut e = engine();
        e.load_dir(&dir).expect("v1 load");
        // F1 = 0x3A
        let mut ctx = low_hp_ctx(0.10);
        ctx.tick = 100;
        assert_eq!(e.fire_on_low_hp(&ctx), ScriptResult::Hotkey(0x3A), "v1 debería emitir F1");

        // Versión 2: retorna F2 con cooldown distinto.
        // Si el reload NO resetea el runtime, la closure v1 seguiría activa
        // y devolvería F1 en lugar de F2.
        let v2 = r#"
            local COOLDOWN = 5
            local last = 0
            function on_low_hp(ctx)
                if ctx.tick - last >= COOLDOWN then
                    last = ctx.tick
                    return "F2"
                end
                return nil
            end
        "#;
        std::fs::File::create(&script_path).unwrap().write_all(v2.as_bytes()).unwrap();
        e.load_dir(&dir).expect("v2 reload");

        // Tras reload, v2 debe estar activa: F2 = 0x3B, Y el cooldown se
        // resetea (last=0), por lo que la primera llamada dispara sin esperar.
        ctx.tick = 200;
        assert_eq!(
            e.fire_on_low_hp(&ctx),
            ScriptResult::Hotkey(0x3B),
            "v2 debería emitir F2 tras reload — si retorna F1, el reset no funcionó",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase C.1: verifica que `fire_on_fsm_state_change` pasa los args
    /// correctos al hook Lua y que el hook puede usar `bot.log`/`bot.say`
    /// para emitir alertas.
    #[test]
    fn fire_on_fsm_state_change_passes_args_to_hook() {
        let mut e = engine();
        // Hook que captura (new_state, reason) en globals para verificar.
        load_inline(&e, r#"
            captured_state = nil
            captured_reason = nil
            function on_fsm_state_change(new_state, reason)
                captured_state = new_state
                captured_reason = reason
                if reason == "prompt:char_select" then
                    bot.say("char_dead_alert")
                end
            end
        "#, "fsm_hook");

        // Transición a Paused con reason = prompt:char_select.
        let r = e.fire_on_fsm_state_change("Paused", Some("prompt:char_select"));
        assert_eq!(r, ScriptResult::Noop, "hook retorna nil → Noop");

        // Verificar los valores capturados por el hook.
        let state: String = e.lua.globals().get("captured_state").unwrap();
        let reason: String = e.lua.globals().get("captured_reason").unwrap();
        assert_eq!(state, "Paused");
        assert_eq!(reason, "prompt:char_select");

        // El hook debe haber llamado bot.say("char_dead_alert").
        assert_eq!(e.drain_say_queue(), vec!["char_dead_alert"]);
    }

    /// Phase C.1: verifica que el hook recibe `nil` cuando no hay reason.
    #[test]
    fn fire_on_fsm_state_change_nil_reason_is_nil_in_lua() {
        let mut e = engine();
        load_inline(&e, r#"
            captured_was_nil = nil
            function on_fsm_state_change(new_state, reason)
                captured_was_nil = (reason == nil)
            end
        "#, "fsm_nil_reason");

        let r = e.fire_on_fsm_state_change("Walking", None);
        assert_eq!(r, ScriptResult::Noop);

        let was_nil: bool = e.lua.globals().get("captured_was_nil").unwrap();
        assert!(was_nil, "el hook debe recibir nil cuando reason es None");
    }

    /// Phase C.1: sin hook definido, fire es no-op.
    #[test]
    fn fire_on_fsm_state_change_no_hook_is_noop() {
        let mut e = engine();
        assert_eq!(e.fire_on_fsm_state_change("Idle", None), ScriptResult::Noop);
        assert_eq!(
            e.fire_on_fsm_state_change("Paused", Some("prompt:char_select")),
            ScriptResult::Noop
        );
    }

    /// Verifica que el reset_lua_state preserva la API `bot` (no rompe
    /// `bot.log` o `bot.say` después de un reload).
    #[test]
    fn reload_preserves_bot_api() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("tibia_bot_reload_api_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("uses_bot.lua");

        let src = r#"
            function on_tick(ctx)
                bot.log("info", "hi from lua")
                bot.say("hello")
            end
        "#;
        std::fs::File::create(&script_path).unwrap().write_all(src.as_bytes()).unwrap();

        let mut e = engine();
        // Primer load
        e.load_dir(&dir).expect("first load");
        let ctx = TickContext {
            tick: 0,
            hp_ratio: Some(1.0),
            mana_ratio: Some(1.0),
            enemy_count: 0,
            fsm_state: "Idle".into(),
            ui_matches: vec![],
        };
        assert_eq!(e.fire_on_tick(&ctx), ScriptResult::Noop);
        assert_eq!(e.drain_say_queue(), vec!["hello"]);

        // Segundo load (reload) — la API bot debe seguir funcionando.
        e.load_dir(&dir).expect("reload");
        assert_eq!(e.fire_on_tick(&ctx), ScriptResult::Noop);
        assert_eq!(e.drain_say_queue(), vec!["hello"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
