//! Config.toml loading for the codex-temporal harness.
//!
//! Provides shared config loading used by both CLI and TUI binaries.
//! Config is loaded client-side (outside the workflow sandbox) and
//! extracted into [`SessionWorkflowInput`] fields that flow through
//! Temporal's serialization boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use codex_core::config::{Config, ConfigBuilder, ConfigOverrides, ConfigToml, find_codex_home};
use codex_core::ModelProviderInfo;
use codex_protocol::config_types::{Personality, ReasoningSummary, WebSearchMode};
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;

use crate::types::{CrewAgentDef, CrewMode, CrewType, SessionWorkflowInput};

/// Holds the result of loading config.toml: a template
/// [`SessionWorkflowInput`] and the resolved model provider info.
pub struct HarnessConfig {
    /// Template workflow input with fields populated from config.toml.
    /// The `user_message` field is left empty — callers fill it in.
    pub base_input: SessionWorkflowInput,
    /// Model provider resolved from config.toml (for the worker/activity).
    pub model_provider: ModelProviderInfo,
}

/// Load the codex config.toml via [`ConfigBuilder`] and extract
/// relevant fields into a [`HarnessConfig`].
///
/// This performs file I/O (reads config layers from disk) and must
/// be called outside the Temporal workflow sandbox.
pub async fn load_harness_config() -> Result<HarnessConfig, Box<dyn std::error::Error>> {
    let config = ConfigBuilder::default().build().await?;

    // --- model ---
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| "gpt-4o".to_string());

    // --- instructions ---
    let instructions = config
        .base_instructions
        .clone()
        .unwrap_or_else(|| "You are a helpful coding assistant.".to_string());

    // --- developer instructions ---
    let developer_instructions = config.developer_instructions.clone();

    // --- approval policy ---
    let approval_policy: AskForApproval = *config.permissions.approval_policy.get();

    // --- web search mode ---
    // Config uses Constrained<WebSearchMode> where Disabled means off.
    // Workflow input uses Option<WebSearchMode> where None means off.
    let web_search_mode = match *config.web_search_mode.get() {
        WebSearchMode::Disabled => None,
        other => Some(other),
    };

    // --- reasoning ---
    let reasoning_effort = config.model_reasoning_effort;
    let reasoning_summary = config.model_reasoning_summary.unwrap_or_default();

    // --- personality ---
    let personality = config.personality;

    // --- model provider ---
    let model_provider = config.model_provider.clone();

    // Populate crew_agents from the built-in default crew so that
    // explorer and worker are available via spawn_agent in every session.
    let default_crew = built_in_default_crew();
    let mut crew_agents = BTreeMap::new();
    for (agent_name, agent_def) in &default_crew.agents {
        if agent_name == &default_crew.main_agent {
            continue; // main agent is not a crew sub-agent
        }
        crew_agents.insert(agent_name.clone(), agent_def.clone());
    }

    let base_input = SessionWorkflowInput {
        user_message: String::new(),
        model,
        instructions,
        approval_policy,
        web_search_mode,
        reasoning_effort,
        reasoning_summary,
        personality,
        developer_instructions,
        model_provider: None, // Set by caller from HarnessConfig.model_provider
        crew_agents,
        continued_state: None,
        max_iterations: None,
    };

    Ok(HarnessConfig {
        base_input,
        model_provider,
    })
}

/// Apply environment variable overrides on top of config.toml values.
///
/// Environment variables take highest priority for backward compatibility
/// with the pre-config.toml workflow.
pub fn apply_env_overrides(input: &mut SessionWorkflowInput) {
    // CODEX_MODEL
    if let Ok(model) = std::env::var("CODEX_MODEL") {
        input.model = model;
    }

    // CODEX_APPROVAL_POLICY
    if let Ok(val) = std::env::var("CODEX_APPROVAL_POLICY") {
        input.approval_policy = match val.as_str() {
            "never" => AskForApproval::Never,
            "untrusted" => AskForApproval::UnlessTrusted,
            "on-failure" => AskForApproval::OnFailure,
            _ => input.approval_policy,
        };
    }

    // CODEX_WEB_SEARCH
    if let Ok(val) = std::env::var("CODEX_WEB_SEARCH") {
        input.web_search_mode = match val.as_str() {
            "live" => Some(WebSearchMode::Live),
            "cached" => Some(WebSearchMode::Cached),
            "disabled" => None,
            _ => input.web_search_mode,
        };
    }

    // CODEX_EFFORT
    if let Ok(val) = std::env::var("CODEX_EFFORT") {
        input.reasoning_effort = match val.as_str() {
            "low" => Some(ReasoningEffort::Low),
            "medium" => Some(ReasoningEffort::Medium),
            "high" => Some(ReasoningEffort::High),
            _ => input.reasoning_effort,
        };
    }

    // CODEX_REASONING_SUMMARY
    if let Ok(val) = std::env::var("CODEX_REASONING_SUMMARY") {
        input.reasoning_summary = match val.as_str() {
            "concise" => ReasoningSummary::Concise,
            "detailed" => ReasoningSummary::Detailed,
            _ => input.reasoning_summary,
        };
    }

    // CODEX_PERSONALITY
    if let Ok(val) = std::env::var("CODEX_PERSONALITY") {
        input.personality = match val.as_str() {
            "friendly" => Some(Personality::Friendly),
            "pragmatic" => Some(Personality::Pragmatic),
            _ => input.personality,
        };
    }
}

// ---------------------------------------------------------------------------
// Crew type discovery & loading
// ---------------------------------------------------------------------------

/// Return the path to the crews directory: `{CODEX_HOME}/crews/`.
fn crews_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home = find_codex_home()?;
    Ok(home.join("crews"))
}

/// Return the built-in `codex-default` crew type.
///
/// This crew groups the built-in `explorer` and `worker` roles as regular
/// crew members so they are available via `spawn_agent` without requiring
/// users to write a crew TOML file.
pub fn built_in_default_crew() -> CrewType {
    let mut agents = BTreeMap::new();

    // Main agent — uses the "default" role with no overrides.
    agents.insert(
        "default".to_string(),
        CrewAgentDef {
            role: Some("default".to_string()),
            model: None,
            instructions: None,
            description: Some("Default agent.".to_string()),
        },
    );

    // Explorer agent.
    agents.insert(
        "explorer".to_string(),
        CrewAgentDef {
            role: Some("explorer".to_string()),
            model: None,
            instructions: None,
            description: Some(
                "Use `explorer` for specific codebase questions. \
                 Explorers are fast and authoritative."
                    .to_string(),
            ),
        },
    );

    // Worker agent.
    agents.insert(
        "worker".to_string(),
        CrewAgentDef {
            role: Some("worker".to_string()),
            model: None,
            instructions: None,
            description: Some(
                "Use for execution and production work. \
                 Typical tasks: implement features, fix tests or bugs, \
                 split large refactors into independent chunks."
                    .to_string(),
            ),
        },
    );

    CrewType {
        name: "codex-default".to_string(),
        description: "Built-in crew with explorer and worker agents.".to_string(),
        mode: CrewMode::Interactive,
        initial_prompt: None,
        inputs: BTreeMap::new(),
        main_agent: "default".to_string(),
        agents,
        approval_policy: None,
    }
}

/// Discover all crew types by scanning `{CODEX_HOME}/crews/*.toml`.
///
/// The built-in `codex-default` crew is always included unless a user-defined
/// `codex-default.toml` file exists (which overrides the built-in).
///
/// Returns an empty vec (not an error) if the crews directory does not exist
/// (the built-in crew is still included).
pub fn discover_crew_types() -> Result<Vec<CrewType>, Box<dyn std::error::Error>> {
    let dir = crews_dir()?;

    let mut crews = Vec::new();
    let mut has_user_default = false;

    if dir.exists() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                let content = std::fs::read_to_string(&path)?;
                let crew: CrewType = toml::from_str(&content).map_err(|e| {
                    format!("failed to parse {}: {e}", path.display())
                })?;
                if crew.name == "codex-default" {
                    has_user_default = true;
                }
                crews.push(crew);
            }
        }
    }

    // Prepend built-in crew if no user-defined override exists.
    if !has_user_default {
        crews.insert(0, built_in_default_crew());
    }

    // Sort by name for stable ordering.
    crews.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(crews)
}

/// Load a single crew type by name from `{CODEX_HOME}/crews/{name}.toml`.
///
/// Falls back to the built-in `codex-default` crew when `name` is
/// `"codex-default"` and no user file exists.
pub fn load_crew_type(name: &str) -> Result<CrewType, Box<dyn std::error::Error>> {
    let dir = crews_dir()?;
    let path = dir.join(format!("{name}.toml"));
    if !path.exists() {
        if name == "codex-default" {
            return Ok(built_in_default_crew());
        }
        return Err(format!("crew type '{}' not found at {}", name, path.display()).into());
    }
    let content = std::fs::read_to_string(&path)?;
    let crew: CrewType = toml::from_str(&content)
        .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    Ok(crew)
}

/// Apply a crew type definition to a [`SessionWorkflowInput`].
///
/// 1. Validates all required inputs are provided (errors on missing).
/// 2. Interpolates `{placeholder}` in `initial_prompt` and agent `instructions`.
/// 3. Applies crew's `approval_policy` override to `base`.
/// 4. Applies main agent's `model` override to `base`.
/// 5. Applies main agent's `instructions` to `base`.
/// 6. Sets `base.user_message` from interpolated `initial_prompt` (autonomous mode).
pub fn apply_crew_type(
    crew: &CrewType,
    inputs: &BTreeMap<String, String>,
    base: &mut SessionWorkflowInput,
) -> Result<(), Box<dyn std::error::Error>> {
    // --- validate required inputs ---
    for (name, spec) in &crew.inputs {
        if spec.required && !inputs.contains_key(name) && spec.default.is_none() {
            return Err(format!(
                "missing required input '{}' for crew '{}'",
                name, crew.name
            )
            .into());
        }
    }

    // Build the interpolation map: user inputs + defaults.
    let mut vars = BTreeMap::new();
    for (name, spec) in &crew.inputs {
        if let Some(val) = inputs.get(name) {
            vars.insert(name.clone(), val.clone());
        } else if let Some(ref default) = spec.default {
            vars.insert(name.clone(), default.clone());
        }
    }

    // --- apply crew approval policy ---
    if let Some(policy) = crew.approval_policy {
        base.approval_policy = policy;
    }

    // --- apply main agent overrides ---
    if let Some(main_agent_def) = crew.agents.get(&crew.main_agent) {
        if let Some(ref model) = main_agent_def.model {
            base.model = model.clone();
        }
        if let Some(ref instructions) = main_agent_def.instructions {
            base.instructions = interpolate(instructions, &vars);
        }
    }

    // --- set user_message from initial_prompt (autonomous mode) ---
    if crew.mode == CrewMode::Autonomous
        && let Some(ref prompt) = crew.initial_prompt
    {
        base.user_message = interpolate(prompt, &vars);
    }

    // --- populate crew_agents with non-main agent definitions ---
    for (agent_name, agent_def) in &crew.agents {
        if agent_name == &crew.main_agent {
            continue; // main agent is handled above via base input
        }
        base.crew_agents.insert(
            agent_name.clone(),
            CrewAgentDef {
                role: agent_def.role.clone(),
                model: agent_def.model.clone(),
                instructions: agent_def
                    .instructions
                    .as_ref()
                    .map(|i| interpolate(i, &vars)),
                description: agent_def.description.clone(),
            },
        );
    }

    Ok(())
}

/// Interpolate `{key}` placeholders in a template string.
fn interpolate(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{key}}}"), value);
    }
    result
}

/// Inject crew agent definitions into a config TOML string so they appear
/// as `[agents.<name>]` entries.
///
/// This makes crew roles visible in `config.agent_roles` when the TOML is
/// parsed downstream via [`config_from_toml`], which in turn causes
/// `build_specs()` to include them in the `spawn_agent` tool description.
///
/// Existing agent entries in the TOML are preserved (crew definitions only
/// fill in missing roles).
pub fn inject_crew_roles_into_toml(
    toml_str: &str,
    crew_agents: &BTreeMap<String, CrewAgentDef>,
) -> Result<String, Box<dyn std::error::Error>> {
    if crew_agents.is_empty() {
        return Ok(toml_str.to_string());
    }

    let mut doc: toml::Value = toml::from_str(toml_str)?;

    // Ensure [agents] table exists.
    let root = doc
        .as_table_mut()
        .ok_or("config TOML root is not a table")?;
    if !root.contains_key("agents") {
        root.insert("agents".to_string(), toml::Value::Table(toml::map::Map::new()));
    }
    let agents_table = root
        .get_mut("agents")
        .and_then(|v| v.as_table_mut())
        .ok_or("agents is not a table")?;

    for (name, def) in crew_agents {
        // Don't overwrite existing user-defined roles.
        if agents_table.contains_key(name) {
            continue;
        }
        let mut entry = toml::map::Map::new();
        if let Some(ref desc) = def.description {
            entry.insert("description".to_string(), toml::Value::String(desc.clone()));
        }
        agents_table.insert(name.clone(), toml::Value::Table(entry));
    }

    Ok(toml::to_string(&doc)?)
}

/// Reconstruct a [`Config`] from a TOML string previously produced by
/// [`ConfigBuilder::build_toml_string()`].
///
/// This helper is used by both the workflow (to build the real Config from
/// the `load_config` activity output) and by tool-execution activities (to
/// reconstruct the Config from the TOML string passed via `ToolExecInput`).
///
/// No file I/O is performed — `user_instructions` (AGENTS.md content) is
/// provided by the caller so it can come from project-context collection.
pub fn config_from_toml(
    toml_str: &str,
    cwd: &Path,
    user_instructions: Option<String>,
) -> Result<Config, Box<dyn std::error::Error>> {
    let config_toml: ConfigToml = toml::from_str(toml_str)?;
    let codex_home = PathBuf::from("/tmp/codex-temporal");
    let overrides = ConfigOverrides {
        cwd: Some(cwd.to_path_buf()),
        ..Default::default()
    };
    Ok(Config::from_toml(config_toml, overrides, codex_home, user_instructions)?)
}
