//! safety/mod.rs — Humanización y anti-detection behavioral.
//!
//! **Filosofía del módulo:** la arquitectura del bot (hardware HID real vía
//! Pico 2 + captura pasiva NDI + ejecución en PC separado del cliente)
//! elimina todos los vectores de detección *client-side*. Lo que queda son
//! los vectores **behavioral** del lado del servidor: timing perfecto,
//! reacciones sobrehumanas, sesiones continuas, patterns mecánicos.
//!
//! Este módulo implementa las contra-medidas:
//!
//! - [`timing`]: muestreo gaussiano (`N(μ, σ)`) para jittear cooldowns y
//!   retrasos pre-send del Actuator. Reemplaza valores constantes.
//! - [`reaction`]: delay realista de reacción humana (~180±40ms) cuando
//!   aparece una nueva amenaza (enemigo, HP crítico).
//! - [`breaks`]: scheduler multi-nivel de AFKs (micro/medium/long) con
//!   distribuciones que reproducen sesiones humanas caóticas.
//! - [`rate_limit`]: hard cap de actions/s como red de seguridad contra
//!   bursts accidentales del bot (bugs lógicos no deben producir spam).
//! - [`variation`]: selección aleatoria entre acciones equivalentes
//!   (p.ej. spell vs potion) con distribución configurable.
//! - [`human_noise`]: emisión ocasional de teclas "inútiles" (mirar stats,
//!   abrir menú) para imitar micro-interacciones humanas.
//!
//! Los módulos están **desacoplados** — cada uno es testeable por separado
//! y se compone en `BotLoop` o `Actuator` en el punto correcto del tick.

pub mod timing;
pub mod reaction;
pub mod rate_limit;
pub mod variation;
pub mod breaks;
pub mod human_noise;

pub use reaction::ReactionGate;
pub use rate_limit::RateLimiter;
pub use variation::WeightedChoice;
pub use breaks::BreakScheduler;
pub use human_noise::HumanNoise;
