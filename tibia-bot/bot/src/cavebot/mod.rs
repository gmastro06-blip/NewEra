//! cavebot/ — Sistema completo de hunt automatizado (Fase C del plan).
//!
//! Reemplaza y extiende el módulo `waypoints` legacy. Mientras que `waypoints`
//! solo soporta secuencias temporales planas (walk/wait/hotkey), `cavebot`
//! añade:
//!
//! - **Labels + goto** — saltos nombrados para loops y branches
//! - **Conditional branching** — `goto_if hp_below(0.4)` para ir a refill
//! - **Stand until** — quedarse atacando mobs hasta matar N, HP full, etc
//! - **Loot** — click en coordenada para looter corpses
//! - **SkipIfBlocked** — recovery local si el char no puede avanzar
//! - **Multi-section** — hunt / refill / emergency en un solo archivo
//! - **Hot reload** — cambiar el cavebot sin reiniciar el bot
//!
//! ## Arquitectura
//!
//! - `step.rs` — tipos de datos: `Step`, `StepKind`, `Condition`, `StandUntil`
//! - `parser.rs` — deserialización TOML + resolución de labels a índices
//! - `runner.rs` — ejecución tick a tick con `TickContext`
//!
//! ## Integración con el FSM
//!
//! El cavebot corre **debajo** del FSM principal. Cada tick:
//! 1. El loop llama `cavebot.tick(&ctx)` que retorna `CavebotAction`
//! 2. Si `CavebotAction::Emit(..)`, el loop construye un `WaypointHint::Active`
//! 3. El FSM decide: Emergency > Fighting > Walking (cavebot) > Idle
//! 4. Si el FSM emite la walking action, se dispatcha; si no, el cavebot
//!    se congela (Standing/Walking internos no avanzan el timer)
//!
//! ## Compatibilidad con `waypoints` legacy
//!
//! Ambos coexisten. El loop escoge cuál usar en función del formato del
//! archivo cargado: si tiene `[cavebot]` section, es cavebot v2. Si es
//! `[[step]]` plano, es legacy. `POST /cavebot/load` fuerza cavebot;
//! `POST /waypoints/load` usa legacy.

pub mod hunt_profile;
pub mod parser;
pub mod runner;
pub mod step;

pub use runner::{Cavebot, CavebotAction, TickContext};
