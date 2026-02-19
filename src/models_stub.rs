//! Stub [`ModelsProvider`] for the Temporal TUI binary.
//!
//! Exposes a single fixed model without contacting any remote service.

use codex_core::config::Config;
use codex_core::models_manager::manager::ModelsProvider;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use tokio::sync::TryLockError;

/// A models provider that always returns a single preset built from the
/// model name supplied at construction time.
pub struct FixedModelsProvider {
    preset: ModelPreset,
}

impl FixedModelsProvider {
    pub fn new(model: String) -> Self {
        Self {
            preset: ModelPreset {
                id: model.clone(),
                model: model.clone(),
                display_name: model,
                description: String::new(),
                default_reasoning_effort: ReasoningEffort::Medium,
                supported_reasoning_efforts: vec![],
                supports_personality: false,
                is_default: true,
                upgrade: None,
                show_in_picker: true,
                supported_in_api: true,
                input_modalities: vec![],
            },
        }
    }
}

impl ModelsProvider for FixedModelsProvider {
    fn try_list_models(&self, _config: &Config) -> Result<Vec<ModelPreset>, TryLockError> {
        Ok(vec![self.preset.clone()])
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        vec![]
    }
}
