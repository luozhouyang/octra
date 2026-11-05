//! Local coding-agent discovery and configuration.
//!
//! The module automates the templates in `examples/agents/`: it detects
//! installed agents, renders the Octra provider into their native
//! configuration format, backs up the previous file, and restores it on
//! request. It never proxies agent traffic.

pub mod adapter;
pub mod claude;
pub mod codex;
pub mod control;
pub mod detect;
pub mod error;
pub mod fs;
pub mod opencode;

pub use adapter::{
    AgentAdapter, AgentContext, ApplyProfile, ConfigChange, ConfigField, ConfigView, Detection,
    RenderedConfig,
};
pub use control::{AgentControlState, control_router};
pub use error::{AgentError, AgentResult};
pub use octra_storage::agents::{AgentBackup, AgentProfile, NewAgentBackup, NewAgentProfile};

use std::sync::Arc;

use claude::ClaudeCodeAdapter;
use codex::CodexAdapter;
use opencode::OpenCodeAdapter;

/// The adapters compiled into this build.
#[derive(Clone)]
pub struct AgentRegistry {
    adapters: Vec<Arc<dyn AgentAdapter>>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl AgentRegistry {
    pub fn builtin() -> Self {
        Self {
            adapters: vec![
                Arc::new(ClaudeCodeAdapter),
                Arc::new(CodexAdapter),
                Arc::new(OpenCodeAdapter),
            ],
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.adapters
            .iter()
            .find(|adapter| adapter.id() == id)
            .cloned()
    }

    pub fn all(&self) -> &[Arc<dyn AgentAdapter>] {
        &self.adapters
    }
}

#[cfg(test)]
mod tests {
    use super::AgentRegistry;

    #[test]
    fn registry_exposes_unique_agent_ids() {
        let registry = AgentRegistry::builtin();
        let mut ids: Vec<&str> = registry.all().iter().map(|adapter| adapter.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), registry.all().len());
        assert!(registry.get("claude-code").is_some());
        assert!(registry.get("unknown").is_none());
    }
}
