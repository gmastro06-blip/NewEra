//! health/system.rs — HealthSystem core (placeholder hasta commit 2).
//!
//! Stub mínimo que permite que el módulo compile. La lógica de evaluación
//! + hysteresis + ArcSwap publish llega en el commit siguiente.

use std::sync::Arc;
use arc_swap::ArcSwap;

use super::{HealthConfig, HealthStatus};

pub struct HealthSystem {
    #[allow(dead_code)]
    config: HealthConfig,
    output: Arc<ArcSwap<HealthStatus>>,
}

impl HealthSystem {
    pub fn new(config: HealthConfig) -> Self {
        Self {
            config,
            output: Arc::new(ArcSwap::from_pointee(HealthStatus::default())),
        }
    }

    pub fn output_handle(&self) -> Arc<ArcSwap<HealthStatus>> {
        Arc::clone(&self.output)
    }

    pub fn last_status(&self) -> Arc<HealthStatus> {
        self.output.load_full()
    }
}
