use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use core_api::image_generate::{ImageGenerate, ImageGenerateRegistry};
use core_api::plugin::{Plugin, PluginContext};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{info, warn};

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct PluginConfig {
    #[serde(default = "default_base_url")]
    base_url: String,
    #[serde(default = "default_workflows_dir")]
    workflows_dir: String,
    #[serde(default)]
    default_negative: String,
}

fn default_base_url()      -> String { "http://localhost:8188".into() }
fn default_workflows_dir() -> String { "data/comfyui/workflows".into() }

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            base_url:         default_base_url(),
            workflows_dir:    default_workflows_dir(),
            default_negative: String::new(),
        }
    }
}

// ── _personal_agent metadata ──────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct PersonalAgentMeta {
    name:                        Option<String>,
    description:                 Option<String>,
    prompt_node:                 Option<String>,
    negative_prompt_node:        Option<String>,
    #[serde(default)]
    prompt_field:                Option<String>,
    #[serde(default)]
    prompt_field_extra:          Option<Vec<String>>,
    #[serde(default)]
    negative_prompt_field:       Option<String>,
    #[serde(default)]
    negative_prompt_field_extra: Option<Vec<String>>,
    #[serde(default)]
    extra_params:                ExtraParamNodeMap,
    #[serde(default)]
    input_image_node:            Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ExtraParamNodeMap {
    width_node:  Option<String>,
    height_node: Option<String>,
    steps_node:  Option<String>,
}

// ── Runtime status ────────────────────────────────────────────────────────────

#[derive(Default)]
struct RuntimeStatus {
    comfyui_online:       bool,
    registered_providers: usize,
}

// ── ComfyUiWorkflowGenerator ──────────────────────────────────────────────────

pub struct ComfyUiWorkflowGenerator {
    id:                         String,
    name:                       String,
    description:                Option<String>,
    extra_params_schema:        Option<Value>,
    workflow_path:              PathBuf,
    prompt_node:                Option<String>,
    negative_prompt_node:       Option<String>,
    prompt_field:               Option<String>,
    prompt_field_extra:         Option<Vec<String>>,
    negative_prompt_field:      Option<String>,
    negative_prompt_field_extra: Option<Vec<String>>,
    extra_param_nodes:          ExtraParamNodeMap,
    input_image_node:           Option<String>,
    default_negative:           String,
    base_url:                   String,
    http:                       Arc<reqwest::Client>,
}

impl ComfyUiWorkflowGenerator {
    fn from_file(
        path:   &Path,
        config: &PluginConfig,
        http:   Arc<reqwest::Client>,
    ) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("failed to read {:?}: {e}", path))?;
        let workflow: Value = serde_json::from_str(&content)
            .map_err(|e| anyhow!("invalid JSON in {:?}: {e}", path))?;

        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
        let id   = format!("comfyui-{stem}");

        let meta: PersonalAgentMeta = workflow.get("_personal_agent")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let name = meta.name.unwrap_or_else(|| stem.to_string());

        // Require either a prompt node or an input image node (or both)
        if meta.prompt_node.is_none()
            && find_first_node(&workflow, "CLIPTextEncode").is_none()
            && meta.input_image_node.is_none()
        {
            anyhow::bail!("no CLIPTextEncode or input_image_node in {:?}", path);
        }

        let prompt_node = meta.prompt_node
            .or_else(|| find_first_node(&workflow, "CLIPTextEncode"));

        let extra_params_schema = build_extra_params_schema(&workflow, &meta.extra_params);

        Ok(Self {
            id,
            name,
            description: meta.description,
            extra_params_schema,
            workflow_path: path.to_path_buf(),
            prompt_node,
            negative_prompt_node: meta.negative_prompt_node,
            prompt_field: meta.prompt_field,
            prompt_field_extra: meta.prompt_field_extra,
            negative_prompt_field: meta.negative_prompt_field,
            negative_prompt_field_extra: meta.negative_prompt_field_extra,
            extra_param_nodes: meta.extra_params,
            input_image_node:  meta.input_image_node,
            default_negative: config.default_negative.clone(),
            base_url: config.base_url.clone(),
            http,
        })
    }

    async fn poll_until_done(&self, prompt_id: &str) -> Result<ImageOutputInfo> {
        let url      = format!("{}/history/{prompt_id}", self.base_url.trim_end_matches('/'));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);

        loop {
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("ComfyUI generation timed out after 300s");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;

            let json: Value = self.http.get(&url).send().await
                .map_err(|e| anyhow!("ComfyUI /history request failed: {e}"))?
                .json().await
                .map_err(|e| anyhow!("ComfyUI /history parse failed: {e}"))?;

            // Empty object {} = still in queue
            let Some(entry) = json.get(prompt_id) else { continue };

            let status = &entry["status"];

            // Execution error — extract the message from status.messages.
            // Must be checked BEFORE the "still running" guard: a failed prompt
            // reports `completed: false`, so checking completion first would
            // mask the error and poll until the deadline.
            if status["status_str"].as_str() == Some("error") {
                let error_msg = status["messages"].as_array()
                    .and_then(|msgs| {
                        msgs.iter().find(|m| m[0].as_str() == Some("execution_error"))
                    })
                    .and_then(|m| m.get(1))
                    .map(|details| {
                        let node = details["node_type"].as_str().unwrap_or("unknown");
                        let exc  = details["exception_message"].as_str().unwrap_or("unknown error");
                        format!("{node}: {exc}")
                    })
                    .unwrap_or_else(|| "unknown execution error".to_string());
                anyhow::bail!("ComfyUI execution error: {error_msg}");
            }

            // Still running
            if status["completed"].as_bool() == Some(false) {
                continue;
            }

            if let Some(outputs) = entry["outputs"].as_object() {
                for node_output in outputs.values() {
                    if let Some(img) = node_output["images"].as_array().and_then(|a| a.first()) {
                        return Ok(ImageOutputInfo {
                            filename:  img["filename"].as_str().unwrap_or("").to_string(),
                            subfolder: img["subfolder"].as_str().unwrap_or("").to_string(),
                        });
                    }
                }
            }
            anyhow::bail!("ComfyUI: generation completed but no image in outputs");
        }
    }
}

struct ImageOutputInfo {
    filename:  String,
    subfolder: String,
}

#[async_trait]
impl ImageGenerate for ComfyUiWorkflowGenerator {
    fn id(&self)   -> &str { &self.id }
    fn name(&self) -> &str { &self.name }

    fn description(&self) -> Option<&str> { self.description.as_deref() }

    fn extra_params_schema(&self) -> Option<Value> { self.extra_params_schema.clone() }

    async fn generate(&self, prompt: &str, extra_params: Option<&Value>) -> Result<Vec<u8>> {
        // Re-read from disk so modified workflows are picked up immediately
        let content = tokio::fs::read_to_string(&self.workflow_path).await
            .map_err(|e| anyhow!("failed to read workflow: {e}"))?;
        let mut workflow: Value = serde_json::from_str(&content)
            .map_err(|e| anyhow!("invalid workflow JSON: {e}"))?;

        if let Some(obj) = workflow.as_object_mut() {
            obj.remove("_personal_agent");
        }

        // Inject prompt — skip if the workflow has no prompt node (e.g. upscale-only)
        if let Some(ref node) = self.prompt_node {
            let prompt_field = self.prompt_field.as_deref().unwrap_or("text");
            workflow[node]["inputs"][prompt_field] = json!(prompt);
            if let Some(ref extras) = self.prompt_field_extra {
                for field in extras {
                    workflow[node]["inputs"][field] = json!(prompt);
                }
            }
        }

        // Inject negative prompt
        if let Some(neg_node) = &self.negative_prompt_node {
            if !self.default_negative.is_empty() {
                let neg_field = self.negative_prompt_field.as_deref().unwrap_or("text");
                workflow[neg_node]["inputs"][neg_field] = json!(self.default_negative);
                if let Some(ref extras) = self.negative_prompt_field_extra {
                    for field in extras {
                        workflow[neg_node]["inputs"][field] = json!(self.default_negative);
                    }
                }
            }
        }

        // Inject extra_params into declared nodes
        if let Some(params) = extra_params {
            if let (Some(node), Some(v)) = (&self.extra_param_nodes.width_node,  params["width"].as_i64())  { workflow[node]["inputs"]["width"]  = json!(v); }
            if let (Some(node), Some(v)) = (&self.extra_param_nodes.height_node, params["height"].as_i64()) { workflow[node]["inputs"]["height"] = json!(v); }
            if let (Some(node), Some(v)) = (&self.extra_param_nodes.steps_node,  params["steps"].as_i64())  { workflow[node]["inputs"]["steps"]  = json!(v); }
        }

        // ── Input image (img2img) ──────────────────────────────────────────────────
        if let (Some(node), Some(image_path)) = (&self.input_image_node, extra_params.and_then(|p| p["input_image"].as_str())) {
            // 1. Read the image file
            let image_bytes = tokio::fs::read(image_path).await
                .map_err(|e| anyhow!("failed to read input image '{image_path}': {e}"))?;

            // 2. Get just the filename (strip path)
            let filename = Path::new(image_path)
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow!("invalid image path: {image_path}"))?;

            // 3. Upload to ComfyUI
            let upload_url = format!("{}/upload/image", self.base_url.trim_end_matches('/'));
            let form = reqwest::multipart::Form::new()
                .part("image", reqwest::multipart::Part::bytes(image_bytes)
                    .file_name(filename.to_string())
                    .mime_str("image/png")
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![])))
                .text("overwrite", "true");

            let upload_resp = self.http.post(&upload_url)
                .multipart(form)
                .send().await
                .map_err(|e| anyhow!("ComfyUI /upload/image failed: {e}"))?;

            if !upload_resp.status().is_success() {
                let status = upload_resp.status();
                let body   = upload_resp.text().await.unwrap_or_default();
                anyhow::bail!("ComfyUI /upload/image error {status}: {body}");
            }

            // 4. Set the filename in the LoadImage node
            // Use just the name (ComfyUI prepends its input/ directory)
            workflow[node]["inputs"]["image"] = json!(filename);
        }


        // POST /prompt
        let url = format!("{}/prompt", self.base_url.trim_end_matches('/'));
        let resp = self.http.post(&url)
            .json(&json!({ "prompt": workflow }))
            .send().await
            .map_err(|e| anyhow!("ComfyUI /prompt failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body   = resp.text().await.unwrap_or_default();
            anyhow::bail!("ComfyUI /prompt error {status}: {body}");
        }

        let resp_json: Value = resp.json().await
            .map_err(|e| anyhow!("ComfyUI /prompt response parse failed: {e}"))?;
        let prompt_id = resp_json["prompt_id"].as_str()
            .ok_or_else(|| anyhow!("ComfyUI: no prompt_id in response"))?
            .to_string();

        info!(provider = %self.id, %prompt_id, "ComfyUI: queued");

        let image_info = self.poll_until_done(&prompt_id).await?;

        // Download image
        let view_url = format!(
            "{}/view?filename={}&subfolder={}&type=output",
            self.base_url.trim_end_matches('/'),
            image_info.filename,
            image_info.subfolder,
        );
        let img_resp = self.http.get(&view_url).send().await
            .map_err(|e| anyhow!("ComfyUI /view failed: {e}"))?;

        let bytes = img_resp.bytes().await
            .map_err(|e| anyhow!("ComfyUI /view download failed: {e}"))?;

        info!(provider = %self.id, bytes = bytes.len(), "ComfyUI: generation complete");
        Ok(bytes.to_vec())
    }
}

// ── ComfyUIPlugin ─────────────────────────────────────────────────────────────

pub struct ComfyUIPlugin {
    http:           Arc<reqwest::Client>,
    watcher:        Mutex<Option<JoinHandle<()>>>,
    status:         Arc<Mutex<RuntimeStatus>>,
    /// IDs currently registered in ImageGeneratorManager — shared with watcher task.
    registered_ids: Arc<Mutex<HashSet<String>>>,
    /// Last registry received in reload() — used to unregister on stop/disable.
    last_registry:  Mutex<Option<Arc<dyn ImageGenerateRegistry>>>,
}

impl ComfyUIPlugin {
    pub fn new() -> Self {
        Self {
            http:           Arc::new(reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client")),
            watcher:        Mutex::new(None),
            status:         Arc::new(Mutex::new(RuntimeStatus::default())),
            registered_ids: Arc::new(Mutex::new(HashSet::new())),
            last_registry:  Mutex::new(None),
        }
    }

    async fn stop_watcher_and_cleanup(&self) {
        if let Some(handle) = self.watcher.lock().await.take() {
            handle.abort();
        }
        let ids: Vec<String> = self.registered_ids.lock().await.drain().collect();
        if let Some(reg) = self.last_registry.lock().await.as_ref() {
            for id in &ids {
                reg.unregister(id).await;
            }
        }
        *self.status.lock().await = RuntimeStatus::default();
    }
}

#[async_trait]
impl Plugin for ComfyUIPlugin {
    fn id(&self)          -> &str { "comfyui" }
    fn name(&self)        -> &str { "ComfyUI" }
    fn description(&self) -> &str { "Local image generation via ComfyUI. Each JSON file in data/comfyui/workflows/ becomes a separate provider." }
    fn is_running(&self)  -> bool { self.watcher.try_lock().map_or(false, |g| g.is_some()) }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "base_url":         { "type": "string", "default": "http://localhost:8188", "description": "ComfyUI server URL" },
                "workflows_dir":    { "type": "string", "default": "data/comfyui/workflows", "description": "Directory with workflow JSON files" },
                "default_negative": { "type": "string", "default": "", "description": "Default negative prompt for all workflows" }
            }
        })
    }

    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()> {
        // Abort previous watcher and unregister its providers
        self.stop_watcher_and_cleanup().await;

        // Store registry for future cleanup
        *self.last_registry.lock().await = Some(Arc::clone(&ctx.image_generate_registry));

        if !enabled {
            info!("ComfyUI plugin disabled");
            return Ok(());
        }

        let plugin_config: PluginConfig = serde_json::from_value(config).unwrap_or_default();

        // Ensure workflows directory exists
        tokio::fs::create_dir_all(&plugin_config.workflows_dir).await.ok();

        let registry       = Arc::clone(&ctx.image_generate_registry);
        let http           = Arc::clone(&self.http);
        let status         = Arc::clone(&self.status);
        let registered_ids = Arc::clone(&self.registered_ids);

        let handle = tokio::spawn(watcher_loop(
            registry, plugin_config, http, status, registered_ids,
        ));
        *self.watcher.lock().await = Some(handle);

        info!("ComfyUI plugin started");
        Ok(())
    }

    async fn start(&self, ctx: PluginContext) -> Result<()> {
        self.reload(true, json!({}), ctx).await
    }

    async fn stop(&self) -> Result<()> {
        self.stop_watcher_and_cleanup().await;
        info!("ComfyUI plugin stopped");
        Ok(())
    }

    fn runtime_status(&self) -> Option<Value> {
        self.status.try_lock().ok().map(|s| json!({
            "comfyui_online":       s.comfyui_online,
            "registered_providers": s.registered_providers,
        }))
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }
}

// ── Watcher loop ──────────────────────────────────────────────────────────────

async fn watcher_loop(
    registry:       Arc<dyn ImageGenerateRegistry>,
    config:         PluginConfig,
    http:           Arc<reqwest::Client>,
    status:         Arc<Mutex<RuntimeStatus>>,
    registered_ids: Arc<Mutex<HashSet<String>>>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut was_online = false;
    // path → mtime of the registered version
    let mut known: HashMap<PathBuf, SystemTime> = HashMap::new();

    loop {
        interval.tick().await;

        // ── 1. Health check ───────────────────────────────────────────────────
        let is_online = health_check(&http, &config.base_url).await;

        if !is_online && was_online {
            for path in known.keys() {
                let id = path_to_id(path);
                registry.unregister(&id).await;
                registered_ids.lock().await.remove(&id);
            }
            known.clear();
            warn!("ComfyUI unreachable — all providers unregistered");
        }

        was_online = is_online;
        {
            let mut s = status.lock().await;
            s.comfyui_online       = is_online;
            s.registered_providers = known.len();
        }

        if !is_online { continue; }

        // ── 2. File scan ──────────────────────────────────────────────────────
        let current = match scan_workflows(&config.workflows_dir).await {
            Ok(m)  => m,
            Err(e) => { warn!(error = %e, "ComfyUI: workflow scan failed"); continue; }
        };

        // Removed files
        for path in known.keys() {
            if !current.contains_key(path) {
                let id = path_to_id(path);
                registry.unregister(&id).await;
                registered_ids.lock().await.remove(&id);
                info!(%id, "ComfyUI: workflow removed");
            }
        }

        // New or modified files
        for (path, mtime) in &current {
            let changed = known.get(path).map_or(true, |old| old != mtime);
            if !changed { continue; }

            if known.contains_key(path) {
                // modified — unregister old version first
                let id = path_to_id(path);
                registry.unregister(&id).await;
                registered_ids.lock().await.remove(&id);
            }

            match ComfyUiWorkflowGenerator::from_file(path, &config, Arc::clone(&http)) {
                Ok(provider) => {
                    info!(id = %provider.id, "ComfyUI: workflow registered");
                    registered_ids.lock().await.insert(provider.id.clone());
                    registry.register(Arc::new(provider)).await;
                }
                Err(e) => warn!(path = %path.display(), error = %e, "ComfyUI: skipping workflow"),
            }
        }

        known = current;
        status.lock().await.registered_providers = known.len();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn health_check(http: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{}/system_stats", base_url.trim_end_matches('/'));
    timeout(Duration::from_secs(2), http.get(&url).send())
        .await
        .map(|r| r.map(|r| r.status().is_success()).unwrap_or(false))
        .unwrap_or(false)
}

async fn scan_workflows(dir: &str) -> Result<HashMap<PathBuf, SystemTime>> {
    let mut map = HashMap::new();
    let mut entries = tokio::fs::read_dir(dir).await
        .map_err(|e| anyhow!("cannot read workflows dir '{dir}': {e}"))?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
        if let Ok(meta) = entry.metadata().await {
            if let Ok(mtime) = meta.modified() {
                map.insert(path, mtime);
            }
        }
    }
    Ok(map)
}

fn path_to_id(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    format!("comfyui-{stem}")
}

fn find_first_node(workflow: &Value, class_type: &str) -> Option<String> {
    let obj = workflow.as_object()?;
    let mut keys: Vec<u64> = obj.keys()
        .filter_map(|k| k.parse().ok())
        .collect();
    keys.sort_unstable();
    keys.into_iter()
        .map(|n| n.to_string())
        .find(|k| workflow[k]["class_type"].as_str() == Some(class_type))
}

fn build_extra_params_schema(workflow: &Value, nodes: &ExtraParamNodeMap) -> Option<Value> {
    let mut props = serde_json::Map::new();

    if let Some(id) = &nodes.width_node {
        let default = workflow[id]["inputs"]["width"].as_i64().unwrap_or(512);
        props.insert("width".into(),  json!({ "type": "integer", "default": default }));
    }
    if let Some(id) = &nodes.height_node {
        let default = workflow[id]["inputs"]["height"].as_i64().unwrap_or(512);
        props.insert("height".into(), json!({ "type": "integer", "default": default }));
    }
    if let Some(id) = &nodes.steps_node {
        let default = workflow[id]["inputs"]["steps"].as_i64().unwrap_or(20);
        props.insert("steps".into(),  json!({ "type": "integer", "default": default }));
    }

    if props.is_empty() { None } else { Some(json!({ "type": "object", "properties": props })) }
}
