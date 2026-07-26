use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, trace, warn};

use core_api::provider::LlmStrength;

const AGENTS_DIR: &str = "agents";

/// The neutral, instance-wide fallback chat agent (§0.1: a stable technical id,
/// never surfaced to the user — the display name lives in its `meta.json`). Used
/// as the last-resort default when a role carries no `attrs.chat_agent` (or its
/// attrs are unreadable). The per-user entry agent is normally resolved from the
/// caller's role — see `db::roles::default_chat_agent_for_user`.
pub const DEFAULT_CHAT_AGENT: &str = "assistant";

/// The role an agent plays, declared by the required `type` field in `meta.json`.
///
/// - `Chat`: a conversational entry-point the user talks to directly (e.g. `main`,
///   `project-coordinator`). Not dispatchable as a sub-agent, not a valid task root.
/// - `Task`: a task executor. Dispatchable by a parent agent **and** a valid root of a
///   scheduled/async task (e.g. `software-engineer`, `researcher`, `generalist`).
/// - `System`: a hidden background agent wired into the runtime by id (e.g. `tic`).
///   Never listed, never user-chattable, never dispatchable from the tool surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    Chat,
    Task,
    System,
}

/// Per-locale overrides for the **user-facing** fields of an agent, keyed by
/// locale in `meta.json` under `i18n`. Only `name` and `friendly_description`
/// are translatable — the routing `description` (read by the orchestrator LLM)
/// and the prompt stay canonical. Any absent field falls back to the top-level
/// (English) value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalizedMeta {
    #[serde(default)]
    pub name:                 Option<String>,
    #[serde(default)]
    pub friendly_description: Option<String>,
}

#[derive(Deserialize)]
struct RawMeta {
    name:          String,
    description:   String,
    #[serde(default)]
    friendly_description: Option<String>,
    /// Translations of the user-facing fields, keyed by locale (`"it"`, `"fr"`…).
    #[serde(default)]
    i18n:          HashMap<String, LocalizedMeta>,
    #[serde(default)]
    instructions:  Option<String>,
    #[serde(default)]
    inject_memory: Vec<String>,
    #[serde(default)]
    client:        Option<String>,
    #[serde(default)]
    strength:      Option<LlmStrength>,
    /// Required: declares the agent's role. A `meta.json` without `type` fails to load.
    #[serde(rename = "type")]
    agent_type:    AgentType,
    #[serde(default = "default_true")]
    inject_skills: bool,
    #[serde(default)]
    icon:          Option<String>,
}

/// Serde default for boolean fields that should be `true` when the key is absent.
fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMeta {
    pub id:            String,
    pub name:          String,
    /// Routing description for the **orchestrator LLM**: "when should I delegate to this
    /// agent, and what does it return?". Injected into `<!-- AGENTS_LIST -->` and returned
    /// by `list_items` (type=agents). Required.
    pub description:   String,
    /// Human-facing blurb shown to the **user** on the frontend Agents page. When absent
    /// the frontend falls back to `description`. Applies to every agent type.
    #[serde(default)]
    pub friendly_description: Option<String>,
    /// Translations of the user-facing fields (`name` / `friendly_description`), keyed
    /// by locale. Resolved server-side by [`AgentMeta::localize`] before the meta is sent
    /// to the frontend — never serialized out, so all translations stay on the box.
    #[serde(default, skip_serializing)]
    pub i18n:          HashMap<String, LocalizedMeta>,
    /// Note for the **calling LLM** on *how* to invoke this agent for the best result
    /// (expected inputs, format, gotchas). Kept short. Only meaningful for `task` agents —
    /// it is surfaced solely via `list_items` (type=agents), which already lists task
    /// agents only, so no extra gating is needed.
    #[serde(default)]
    pub instructions:  Option<String>,
    #[serde(default)]
    pub inject_memory: Vec<String>,
    /// Preferred LLM client name (must exist in the DB, configured via the web app).
    /// If unset, the sub-agent inherits the caller's client.
    #[serde(default)]
    pub client:        Option<String>,
    /// Minimum LLM capability required to run this agent reliably.
    /// AUTO selection skips clients weaker than this threshold.
    #[serde(default)]
    pub strength:      Option<LlmStrength>,
    /// The agent's role (`chat` / `task` / `system`). Only `task` agents are listed in
    /// `list_items` (type=agents) / the AGENTS_LIST injection and are dispatchable or
    /// runnable as a task root; `chat` and `system` are excluded from those paths.
    #[serde(rename = "type")]
    pub agent_type:    AgentType,
    /// When true (the default, including when the key is absent), the skills index
    /// (`skills/index.md`) is injected into this agent's system prompt so it can
    /// discover and use installed skills. Set false for background agents that don't
    /// need them (e.g. TIC) to save tokens.
    #[serde(default = "default_true")]
    pub inject_skills: bool,
    /// Path to the agent's icon image file (relative to the agent's directory).
    /// Defaults to None if no icon is configured.
    #[serde(default)]
    pub icon: Option<String>,
}

impl AgentMeta {
    /// Overwrite the user-facing `name` / `friendly_description` with the
    /// translation for `locale` when `meta.json` provides one, leaving each
    /// field untouched (English fallback) when the locale or the specific field
    /// is absent. The routing `description` and prompt are never localized.
    pub fn localize(&mut self, locale: &str) {
        if let Some(loc) = self.i18n.get(locale).cloned() {
            if let Some(name) = loc.name {
                self.name = name;
            }
            if loc.friendly_description.is_some() {
                self.friendly_description = loc.friendly_description;
            }
        }
    }
}

/// Scan `agents/` and return metadata for every agent that has both
/// `meta.json` and `AGENT.md`. Skips the `common/` directory.
pub fn discover() -> Result<Vec<AgentMeta>> {
    let mut agents = Vec::new();

    let dir = std::fs::read_dir(AGENTS_DIR)
        .with_context(|| format!("Failed to read agents directory '{AGENTS_DIR}'"))?;

    for entry in dir {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() { continue; }

        let id = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if !n.is_empty() && n != "common" => n.to_string(),
            _ => continue,
        };

        let meta_path   = path.join("meta.json");
        let system_path = path.join("AGENT.md");
        if !meta_path.exists() || !system_path.exists() {
            warn!(agent_id = %id, "skipping agent: missing meta.json or AGENT.md");
            continue;
        }

        let raw_str = match std::fs::read_to_string(&meta_path) {
            Ok(s) => s,
            Err(e) => {
                warn!(agent_id = %id, error = %e, "skipping agent: cannot read meta.json");
                continue;
            }
        };
        // A single malformed meta.json (e.g. missing the required `type` field) must not
        // blank the whole roster — warn and skip it, keep discovering the rest.
        let raw: RawMeta = match serde_json::from_str(&raw_str) {
            Ok(r) => r,
            Err(e) => {
                warn!(agent_id = %id, error = %e, "skipping agent: invalid meta.json");
                continue;
            }
        };

        let meta = AgentMeta {
            id,
            name:            raw.name,
            description:     raw.description,
            friendly_description: raw.friendly_description,
            i18n:            raw.i18n,
            instructions:    raw.instructions,
            inject_memory:   raw.inject_memory,
            client:          raw.client,
            strength:        raw.strength,
            agent_type:      raw.agent_type,
            inject_skills:   raw.inject_skills,
            icon:            raw.icon,
        };
        trace!(agent_id = %meta.id, client = ?meta.client, strength = ?meta.strength, "agent meta loaded");
        debug!(agent_id = %meta.id, name = %meta.name, "agent discovered");
        agents.push(meta);
    }

    agents.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(agents)
}

/// Load metadata for a single agent (reads its `meta.json`).
pub fn load_meta(agent_id: &str) -> Result<AgentMeta> {
    let path = format!("{AGENTS_DIR}/{agent_id}/meta.json");
    let raw_str = std::fs::read_to_string(&path)
        .with_context(|| format!("Agent '{agent_id}': meta.json not found at '{path}'"))?;

    let raw: RawMeta = serde_json::from_str(&raw_str)
        .with_context(|| format!("Agent '{agent_id}': failed to parse meta.json"))?;

    Ok(AgentMeta {
        id:              agent_id.to_string(),
        name:            raw.name,
        description:     raw.description,
        friendly_description: raw.friendly_description,
        i18n:            raw.i18n,
        instructions:    raw.instructions,
        inject_memory:   raw.inject_memory,
        client:          raw.client,
        strength:        raw.strength,
        agent_type:      raw.agent_type,
        inject_skills:   raw.inject_skills,
        icon:            raw.icon,
    })
}

/// Load metadata for `agent_id` and assert it is a runnable **task** agent.
/// Errors if the agent does not exist or is a `chat` / `system` agent — i.e. the
/// single gate for "can this agent be dispatched or run as a task root?".
pub fn load_task_meta(agent_id: &str) -> Result<AgentMeta> {
    let meta = load_meta(agent_id)?;
    if meta.agent_type != AgentType::Task {
        anyhow::bail!(
            "agent `{agent_id}` is a {:?} agent and cannot be dispatched or run as a task — only `task` agents can",
            meta.agent_type
        );
    }
    Ok(meta)
}

/// Load and resolve the system prompt for `agent_id` from disk.
/// Called at request time so edits to `.md` files take effect without restart.
pub fn load_prompt(agent_id: &str) -> Result<String> {
    let path = format!("{AGENTS_DIR}/{agent_id}/AGENT.md");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Agent '{agent_id}': AGENT.md not found at '{path}'"))?;
    resolve_includes(&content)
}

fn resolve_includes(content: &str) -> Result<String> {
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(path_raw) = trimmed
            .strip_prefix("<!-- INCLUDE:")
            .and_then(|s| s.strip_suffix("-->"))
        {
            let path = format!("{AGENTS_DIR}/{}", path_raw.trim());
            let included = std::fs::read_to_string(&path)
                .with_context(|| format!("INCLUDE: failed to read '{path}'"))?;
            out.push_str(&format!("<included_file path=\"{path}\">\n"));
            out.push_str(&resolve_includes(&included)?);
            out.push_str("</included_file>\n");
        } else if trimmed == "<!-- AGENTS_LIST -->" {
            out.push_str(&render_agents_list()?);
        } else if trimmed == "<!-- MCP_LIST -->" {
            // Replaced at request time by the system-context source with dynamic
            // active/hidden sections. Leave a sentinel so the injection point
            // is preserved and positioned correctly in the prompt.
            out.push_str("__MCP_LIST__\n");
        } else if let Some(key) = trimmed
            .strip_prefix("<!-- ")
            .and_then(|s| s.strip_suffix(" -->"))
            .filter(|k| k.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
        {
            // Generic runtime substitution: <!-- KEY --> → __KEY__ sentinel.
            // Replaced at request time via SendMessageOptions::system_substitutions.
            out.push_str(&format!("__{key}__\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

fn render_agents_list() -> Result<String> {
    let agents = discover()?;
    let mut out = String::new();
    for agent in agents.iter().filter(|a| a.agent_type == AgentType::Task) {
        out.push_str(&format!("- **{}** — {}\n", agent.id, agent.description));
    }
    Ok(out)
}
