//! Declarative OpenAI-compatible LLM providers, loaded at boot from a runtime
//! YAML file ([`PROVIDERS_FILE`], resolved against the process working
//! directory — the same convention as `config.yml`). Editing the file and
//! restarting picks up changes without a rebuild; a provider entry that fails
//! validation is logged and skipped, never fatal.
//!
//! One engine, [`DeclaredProvider`], implements [`ApiProvider`] for every
//! entry: the YAML carries identity, endpoints, per-model JSON field mapping,
//! id-prefix enrichment rules and the reasoning knob, while providers whose
//! behavior is not OpenAI-compatible (Anthropic, Ollama) or too bespoke
//! (OpenRouter's pricing/reasoning parsing, OpenAI's extra TTS/transcribe
//! services) stay native Rust and register alongside.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

use crate::chatbot::openai::OpenAiClient;
use crate::llm::providers::{extra_with_reasoning, RemoteLlmModelInfo};
use crate::llm::{LlmModelRecord, LlmProviderRecord};
use crate::provider::{
    ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ReasoningMode, ServiceType,
};

/// Runtime catalog of declarative providers, relative to the process cwd.
pub const PROVIDERS_FILE: &str = "providers.yaml";

const LLM_ONLY: &[ServiceType] = &[ServiceType::Llm];

// ── YAML spec ────────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct ProvidersFile {
    #[serde(default)]
    providers: Vec<serde_yaml::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct ProviderSpec {
    id:      String,
    name:    String,
    /// Only `openai_compatible` exists today; the key is accepted (and
    /// validated) so the file format can grow other kinds later.
    kind:    Option<String>,
    base_url: String,
    /// When true, the instance-level `base_url` stored in the DB overrides
    /// `base_url` (local providers like LM Studio).
    #[serde(default)]
    base_url_overridable: bool,
    #[serde(default)]
    api_key: ApiKeySpec,
    #[serde(default)]
    prompt_cache: bool,
    ui:      UiSpec,
    #[serde(default)]
    fields:  Vec<FieldSpec>,
    models:  Option<ModelsSpec>,
    reasoning: Option<ReasoningSpec>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ApiKeySpec {
    #[default]
    Required,
    Optional,
    None,
}

#[derive(Debug, serde::Deserialize)]
struct UiSpec {
    color:       String,
    icon:        String,
    description: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct FieldSpec {
    key:   String,
    label: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    secret: bool,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ModelsSpec {
    /// GET path joined to `models_url`; the response must be the OpenAI
    /// `{ "data": [...] }` envelope. Mutually exclusive with `static`.
    endpoint: Option<String>,
    /// Base for the listing endpoint; defaults to the resolved `base_url`.
    models_url: Option<String>,
    /// Defaults to `bearer` unless `api_key: none`.
    auth: Option<AuthSpec>,
    /// Static model-id catalog (provider exposes no listing endpoint).
    #[serde(rename = "static")]
    static_models: Option<Vec<String>>,
    #[serde(default)]
    map: MapSpec,
    #[serde(default)]
    base_capabilities: Vec<String>,
    #[serde(default)]
    defaults: DefaultsSpec,
    /// First-matching rule wins.
    #[serde(default)]
    enrich: Vec<EnrichRule>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum AuthSpec {
    Bearer,
    None,
}

/// Per-model JSON field names → `RemoteLlmModelInfo` fields. Absent mappings
/// leave the corresponding field `None` (id defaults to `"id"`, name to id).
#[derive(Debug, Default, serde::Deserialize)]
struct MapSpec {
    id: Option<String>,
    name: Option<String>,
    context_length: Option<String>,
    max_completion_tokens: Option<String>,
    knowledge_cutoff: Option<String>,
    /// Boolean JSON field; when true it also adds the `vision` capability.
    vision: Option<String>,
    price_input_per_million: Option<String>,
    price_output_per_million: Option<String>,
    /// capability name → boolean JSON field that enables it.
    #[serde(default)]
    capability_flags: HashMap<String, String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct DefaultsSpec {
    vision: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
struct EnrichRule {
    #[serde(rename = "match")]
    glob: String,
    #[serde(default)]
    mode: EnrichMode,
    context_length: Option<u64>,
    max_completion_tokens: Option<u64>,
    vision: Option<bool>,
    #[serde(default)]
    add_capabilities: Vec<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum EnrichMode {
    /// Only fill fields the endpoint/static list left unset (endpoint wins).
    #[default]
    Fill,
    /// Overwrite whatever the endpoint returned (rule wins).
    Override,
}

#[derive(Debug, serde::Deserialize)]
struct ReasoningSpec {
    request: ReasoningRequestSpec,
    /// First-matching rule wins.
    #[serde(default)]
    modes: Vec<ReasoningModeRule>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReasoningRequestSpec {
    /// `{"reasoning_effort": value}`, with optional value remapping
    /// (e.g. `disabled` → `none`).
    Effort {
        #[serde(default)]
        remap: HashMap<String, String>,
    },
    /// `disabled`/`enabled` toggle via `{"thinking": {...}}`; any other value
    /// also carries `reasoning_effort`.
    Thinking,
}

#[derive(Debug, serde::Deserialize)]
struct ReasoningModeRule {
    #[serde(default)]
    when: WhenSpec,
    values: Vec<String>,
    default: Option<String>,
}

/// Selector for a reasoning mode: with both keys present the model matches on
/// either (OR); with neither, it matches every model.
#[derive(Debug, Default, serde::Deserialize)]
struct WhenSpec {
    models: Option<Vec<String>>,
    capability: Option<String>,
}

impl WhenSpec {
    fn matches(&self, model_id: &str, capabilities: &[String]) -> bool {
        let id_ok = self
            .models
            .as_ref()
            .is_some_and(|gs| gs.iter().any(|g| glob_match(g, model_id)));
        let cap_ok = self
            .capability
            .as_ref()
            .is_some_and(|c| capabilities.iter().any(|x| x == c));
        match (self.models.is_some(), self.capability.is_some()) {
            (false, false) => true,
            _ => id_ok || cap_ok,
        }
    }
}

// ── DeclaredProvider ─────────────────────────────────────────────────────────

/// [`ApiProvider`] strings are `&'static str`, but the spec is runtime data:
/// the handful of strings a provider exposes are leaked once at boot. The set
/// of declared providers is fixed for the process lifetime and tiny, so the
/// leak is bounded and intentional.
struct LeakedMeta {
    id:          &'static str,
    name:        &'static str,
    description: Option<&'static str>,
    color:       &'static str,
    icon:        &'static str,
    fields:      &'static [ProviderField],
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

pub struct DeclaredProvider {
    spec: ProviderSpec,
    meta: LeakedMeta,
    /// Built on first use: with the `rustls-no-provider` reqwest feature a
    /// `Client` can only be constructed after the process installs a crypto
    /// provider (done by the shell at startup), so building one per provider
    /// at registration time would panic outside the server binary (tests).
    http: std::sync::OnceLock<reqwest::Client>,
}

impl DeclaredProvider {
    fn new(spec: ProviderSpec) -> Self {
        let fields: Vec<ProviderField> = spec
            .fields
            .iter()
            .map(|f| ProviderField {
                key:      leak_str(&f.key),
                label:    leak_str(&f.label),
                required: f.required,
                secret:   f.secret,
            })
            .collect();
        let meta = LeakedMeta {
            id:          leak_str(&spec.id),
            name:        leak_str(&spec.name),
            description: spec.ui.description.as_deref().map(leak_str),
            color:       leak_str(&spec.ui.color),
            icon:        leak_str(&spec.ui.icon),
            fields:      Box::leak(fields.into_boxed_slice()),
        };
        Self { spec, meta, http: std::sync::OnceLock::new() }
    }

    fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(reqwest::Client::new)
    }

    fn base_url(&self, record: &LlmProviderRecord) -> String {
        if self.spec.base_url_overridable {
            record
                .base_url
                .clone()
                .unwrap_or_else(|| self.spec.base_url.clone())
        } else {
            self.spec.base_url.clone()
        }
    }

    fn models_url(&self, models: &ModelsSpec, record: &LlmProviderRecord) -> String {
        models
            .models_url
            .clone()
            .unwrap_or_else(|| self.base_url(record))
    }

    /// Bearer key for the listing endpoint, or `None` when the entry is
    /// unauthenticated. Errors when auth is required but the instance has no key.
    fn auth_key(&self, models: &ModelsSpec, record: &LlmProviderRecord) -> Result<Option<String>> {
        let bearer = match models.auth {
            Some(AuthSpec::Bearer) => true,
            Some(AuthSpec::None) => false,
            None => !matches!(self.spec.api_key, ApiKeySpec::None),
        };
        if !bearer {
            return Ok(None);
        }
        let key = record.api_key.clone().ok_or_else(|| {
            anyhow!(
                "provider '{}': api_key required for {} model listing",
                record.name,
                self.meta.name
            )
        })?;
        Ok(Some(key))
    }

    fn blank_info(&self, id: &str, models: &ModelsSpec) -> RemoteLlmModelInfo {
        RemoteLlmModelInfo {
            id:                       id.to_string(),
            name:                     id.to_string(),
            context_length:           None,
            max_completion_tokens:    None,
            knowledge_cutoff:         None,
            capabilities:             models.base_capabilities.clone(),
            vision:                   models.defaults.vision,
            price_input_per_million:  None,
            price_output_per_million: None,
            reasoning:                None,
        }
    }

    fn map_model(&self, m: &serde_json::Value, models: &ModelsSpec) -> Option<RemoteLlmModelInfo> {
        let map = &models.map;
        let get = |f: &Option<String>| f.as_deref().map(|k| &m[k]);
        let id = get(&map.id)
            .or_else(|| Some(&m["id"]))
            .and_then(|v| v.as_str())?
            .to_string();
        let name = get(&map.name)
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        let mut vision = get(&map.vision).and_then(|v| v.as_bool());
        if vision.is_none() {
            vision = models.defaults.vision;
        }
        let mut capabilities = models.base_capabilities.clone();
        let mut add_cap = |cap: &str| {
            if !capabilities.iter().any(|c| c == cap) {
                capabilities.push(cap.to_string());
            }
        };
        if vision == Some(true) {
            add_cap("vision");
        }
        for (cap, field) in &map.capability_flags {
            if m[field].as_bool().unwrap_or(false) {
                add_cap(cap);
            }
        }
        Some(RemoteLlmModelInfo {
            id,
            name,
            context_length:        get(&map.context_length).and_then(|v| v.as_u64()),
            max_completion_tokens: get(&map.max_completion_tokens).and_then(|v| v.as_u64()),
            knowledge_cutoff:      get(&map.knowledge_cutoff)
                .and_then(|v| v.as_str())
                .map(String::from),
            capabilities,
            vision,
            price_input_per_million:  get(&map.price_input_per_million).and_then(|v| v.as_f64()),
            price_output_per_million: get(&map.price_output_per_million).and_then(|v| v.as_f64()),
            reasoning: None,
        })
    }

    async fn list_models(&self, record: &LlmProviderRecord) -> Result<Vec<RemoteLlmModelInfo>> {
        let models = self.spec.models.as_ref().expect("checked by caller");
        let mut list = if let Some(ids) = &models.static_models {
            ids.iter().map(|id| self.blank_info(id, models)).collect()
        } else {
            let endpoint = models.endpoint.as_deref().expect("validated");
            let base = self.models_url(models, record);
            let url = format!("{}{}", base.trim_end_matches('/'), endpoint);
            let key = self.auth_key(models, record)?;
            let mut req = self.http().get(&url);
            if let Some(k) = key {
                req = req.bearer_auth(k);
            }
            let who = self.meta.name;
            let resp: serde_json::Value = req
                .send()
                .await
                .map_err(|e| anyhow!("{who} request failed: {e}"))?
                .error_for_status()
                .map_err(|e| anyhow!("{who} error response: {e}"))?
                .json()
                .await
                .map_err(|e| anyhow!("{who} response parse failed: {e}"))?;
            let raw = resp["data"]
                .as_array()
                .cloned()
                .ok_or_else(|| anyhow!("unexpected {who} response shape"))?;
            raw.iter().filter_map(|m| self.map_model(m, models)).collect()
        };
        for info in &mut list {
            apply_enrich(&models.enrich, info);
        }
        Ok(list)
    }
}

/// Applies the first matching enrich rule (later rules are not consulted).
fn apply_enrich(rules: &[EnrichRule], info: &mut RemoteLlmModelInfo) {
    let Some(rule) = rules.iter().find(|r| glob_match(&r.glob, &info.id)) else {
        return;
    };
    let set = |dst: &mut Option<u64>, v: Option<u64>| {
        if let Some(v) = v {
            match rule.mode {
                EnrichMode::Fill => {
                    if dst.is_none() {
                        *dst = Some(v);
                    }
                }
                EnrichMode::Override => *dst = Some(v),
            }
        }
    };
    set(&mut info.context_length, rule.context_length);
    set(&mut info.max_completion_tokens, rule.max_completion_tokens);
    if let Some(v) = rule.vision {
        match rule.mode {
            EnrichMode::Fill => {
                if info.vision.is_none() {
                    info.vision = Some(v);
                }
            }
            EnrichMode::Override => info.vision = Some(v),
        }
    }
    for cap in &rule.add_capabilities {
        if !info.capabilities.iter().any(|c| c == cap) {
            info.capabilities.push(cap.clone());
        }
    }
}

/// Case-insensitive glob where `*` is the only wildcard (any run, empty
/// included): `"k3*"` is a prefix match, `"*reasoner*"` a contains match, and
/// a pattern without `*` is an exact match.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.to_lowercase();
    let t = text.to_lowercase();
    if !p.contains('*') {
        return p == t;
    }
    let anchored_start = !p.starts_with('*');
    let anchored_end = !p.ends_with('*');
    let parts: Vec<&str> = p.split('*').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return true;
    }
    let mut rest = t.as_str();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 && anchored_start {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
            continue;
        }
        match rest.find(part) {
            Some(pos) => rest = &rest[pos + part.len()..],
            None => return false,
        }
    }
    if anchored_end {
        return t.ends_with(parts[parts.len() - 1]);
    }
    true
}

#[async_trait::async_trait]
impl ApiProvider for DeclaredProvider {
    fn type_id(&self) -> &'static str {
        self.meta.id
    }

    fn display_name(&self) -> &'static str {
        self.meta.name
    }

    fn supported_types(&self) -> &'static [ServiceType] {
        LLM_ONLY
    }

    async fn list_llm_models(
        &self,
        record: &LlmProviderRecord,
    ) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        if self.spec.models.is_none() {
            return Ok(None);
        }
        Ok(Some(self.list_models(record).await?))
    }

    fn reasoning_mode(&self, model_id: &str, capabilities: &[String]) -> Option<ReasoningMode> {
        let spec = self.spec.reasoning.as_ref()?;
        let rule = spec
            .modes
            .iter()
            .find(|r| r.when.matches(model_id, capabilities))?;
        Some(ReasoningMode::ValueSet {
            values:  rule.values.clone(),
            default: rule.default.clone(),
        })
    }

    fn reasoning_request(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        let spec = self.spec.reasoning.as_ref()?;
        let s = value.as_str()?;
        match &spec.request {
            ReasoningRequestSpec::Effort { remap } => {
                let v = remap.get(s).map(String::as_str).unwrap_or(s);
                Some(serde_json::json!({ "reasoning_effort": v }))
            }
            ReasoningRequestSpec::Thinking => match s {
                "disabled" => Some(serde_json::json!({ "thinking": { "type": "disabled" } })),
                "enabled" => Some(serde_json::json!({ "thinking": { "type": "enabled" } })),
                effort => Some(serde_json::json!({
                    "thinking":         { "type": "enabled" },
                    "reasoning_effort": effort,
                })),
            },
        }
    }

    fn build_llm(
        &self,
        record: &LlmProviderRecord,
        model: &LlmModelRecord,
    ) -> Option<Result<BuiltLlmClient>> {
        Some((|| {
            let key = match self.spec.api_key {
                ApiKeySpec::Required => record.api_key.clone().with_context(|| {
                    format!(
                        "provider '{}': api_key required for {}",
                        record.name, self.meta.id
                    )
                })?,
                ApiKeySpec::Optional => record.api_key.clone().unwrap_or_default(),
                ApiKeySpec::None => String::new(),
            };
            let extra = extra_with_reasoning(self, model);
            let prompt_cache = self.spec.prompt_cache;
            Ok(BuiltLlmClient {
                client: Arc::new(OpenAiClient::new(self.base_url(record), key, extra, prompt_cache)),
                prompt_cache,
            })
        })())
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      self.meta.id,
            display_name: self.meta.name,
            description:  self.meta.description,
            color:        self.meta.color,
            icon:         self.meta.icon,
            lists_models: self.spec.models.is_some(),
            fields:       self.meta.fields,
        }
    }
}

// ── Loader ───────────────────────────────────────────────────────────────────

/// Loads every valid entry from `path`. A missing file, an unreadable file or
/// a malformed entry is logged and skipped — the native providers always
/// register regardless.
pub fn load(path: &Path) -> Vec<DeclaredProvider> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!(path = %path.display(), "no declarative providers file — skipped");
            return Vec::new();
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "cannot read declarative providers file — skipped");
            return Vec::new();
        }
    };
    let file: ProvidersFile = match serde_yaml::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "invalid declarative providers file — skipped");
            return Vec::new();
        }
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (i, entry) in file.providers.into_iter().enumerate() {
        match parse_entry(entry) {
            Ok(p) => {
                if !seen.insert(p.meta.id) {
                    warn!(type_id = p.meta.id, "duplicate provider id in declarative file — skipped");
                    continue;
                }
                out.push(p);
            }
            Err(e) => {
                warn!(path = %path.display(), entry = i, error = %e, "invalid provider entry — skipped");
            }
        }
    }
    info!(path = %path.display(), count = out.len(), "declarative providers loaded");
    out
}

fn parse_entry(v: serde_yaml::Value) -> Result<DeclaredProvider> {
    let spec: ProviderSpec = serde_yaml::from_value(v)?;
    validate(&spec)?;
    Ok(DeclaredProvider::new(spec))
}

fn validate(spec: &ProviderSpec) -> Result<()> {
    if spec.id.trim().is_empty() {
        return Err(anyhow!("provider id must not be empty"));
    }
    if let Some(kind) = &spec.kind
        && kind != "openai_compatible"
    {
        return Err(anyhow!(
            "provider '{}': unsupported kind '{kind}' (only 'openai_compatible')",
            spec.id
        ));
    }
    if spec.base_url.trim().is_empty() {
        return Err(anyhow!("provider '{}': base_url must not be empty", spec.id));
    }
    if let Some(m) = &spec.models {
        match (m.endpoint.is_some(), m.static_models.is_some()) {
            (true, true) => {
                return Err(anyhow!(
                    "provider '{}': models.endpoint and models.static are mutually exclusive",
                    spec.id
                ));
            }
            (false, false) => {
                return Err(anyhow!(
                    "provider '{}': models needs either endpoint or static",
                    spec.id
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_prefix_contains_exact() {
        assert!(glob_match("k3*", "K3-0718"));
        assert!(glob_match("*coder*", "deepseek-Coder-v2"));
        assert!(glob_match("*128k*", "glm-4-32b-0414-128k"));
        assert!(glob_match("glm-5", "glm-5"));
        assert!(!glob_match("glm-5", "glm-5-turbo"));
        assert!(glob_match("a*b", "axbxb"));
        assert!(!glob_match("a*b", "abx"));
        assert!(glob_match("*b", "ab"));
        assert!(!glob_match("k3*", "kimi-for-coding"));
    }

    fn provider(yaml: &str) -> DeclaredProvider {
        parse_entry(serde_yaml::from_str(yaml).unwrap()).unwrap()
    }

    #[test]
    fn thinking_request_mapping() {
        let p = provider(
            r#"
            id: t
            name: T
            base_url: http://x
            ui: { color: c, icon: i }
            reasoning:
              request: { kind: thinking }
              modes:
                - { values: [disabled, enabled], default: enabled }
            "#,
        );
        let req = |v: &str| p.reasoning_request(&serde_json::json!(v)).unwrap();
        assert_eq!(req("disabled"), serde_json::json!({ "thinking": { "type": "disabled" } }));
        assert_eq!(req("enabled"), serde_json::json!({ "thinking": { "type": "enabled" } }));
        assert_eq!(
            req("high"),
            serde_json::json!({ "thinking": { "type": "enabled" }, "reasoning_effort": "high" })
        );
    }

    #[test]
    fn effort_request_remap() {
        let p = provider(
            r#"
            id: t
            name: T
            base_url: http://x
            ui: { color: c, icon: i }
            reasoning:
              request: { kind: effort, remap: { disabled: none } }
              modes:
                - { values: [disabled, max], default: max }
            "#,
        );
        let req = |v: &str| p.reasoning_request(&serde_json::json!(v)).unwrap();
        assert_eq!(req("disabled"), serde_json::json!({ "reasoning_effort": "none" }));
        assert_eq!(req("max"), serde_json::json!({ "reasoning_effort": "max" }));
    }

    #[test]
    fn reasoning_mode_first_match_wins() {
        let p = provider(
            r#"
            id: t
            name: T
            base_url: http://x
            ui: { color: c, icon: i }
            reasoning:
              request: { kind: thinking }
              modes:
                - when: { models: ["glm-5.2*"] }
                  values: [disabled, max]
                - when: { models: ["glm-5*"] }
                  values: [disabled, enabled]
            "#,
        );
        let mode = |id: &str| p.reasoning_mode(id, &[]);
        assert!(matches!(
            mode("glm-5.2"),
            Some(ReasoningMode::ValueSet { values, .. }) if values == ["disabled", "max"]
        ));
        assert!(matches!(
            mode("glm-5-turbo"),
            Some(ReasoningMode::ValueSet { values, .. }) if values == ["disabled", "enabled"]
        ));
        assert!(mode("glm-4").is_none());
    }

    #[test]
    fn reasoning_mode_capability_or_models() {
        let p = provider(
            r#"
            id: t
            name: T
            base_url: http://x
            ui: { color: c, icon: i }
            reasoning:
              request: { kind: thinking }
              modes:
                - when: { capability: reasoning, models: ["*reasoner*"] }
                  values: [disabled, high]
            "#,
        );
        assert!(p.reasoning_mode("anything", &["reasoning".to_string()]).is_some());
        assert!(p.reasoning_mode("deepseek-reasoner", &[]).is_some());
        assert!(p.reasoning_mode("deepseek-chat", &[]).is_none());
    }

    #[test]
    fn enrich_first_match_fill_vs_override() {
        let rules: Vec<EnrichRule> = serde_yaml::from_str(
            r#"
            - { match: "*coder*", context_length: 16384, mode: override }
            - { match: "k3*", context_length: 1048576, vision: true }
            "#,
        )
        .unwrap();
        let mut info = RemoteLlmModelInfo {
            id: "x-coder".into(),
            name: "x".into(),
            context_length: Some(999),
            max_completion_tokens: None,
            knowledge_cutoff: None,
            capabilities: vec![],
            vision: None,
            price_input_per_million: None,
            price_output_per_million: None,
            reasoning: None,
        };
        apply_enrich(&rules, &mut info);
        assert_eq!(info.context_length, Some(16384)); // override wins over endpoint

        info.id = "k3-pro".into();
        info.context_length = Some(999);
        apply_enrich(&rules, &mut info);
        assert_eq!(info.context_length, Some(999)); // fill keeps the endpoint value
        assert_eq!(info.vision, Some(true));
    }

    /// The catalog shipped at the repository root must always parse: the file
    /// is runtime data, but this test keeps a typo from reaching users.
    #[test]
    fn shipped_providers_yaml_is_valid() {
        let text = include_str!("../../../../../providers.yaml");
        let file: ProvidersFile = serde_yaml::from_str(text).unwrap();
        assert!(!file.providers.is_empty());
        let mut ids = std::collections::HashSet::new();
        for entry in file.providers {
            let p = parse_entry(entry).unwrap();
            assert!(ids.insert(p.meta.id), "duplicate id {}", p.meta.id);
        }
    }
}
