//! Dynamic tool loading (DTL) — the wire PROTOCOL lives in the crate
//! (blueprint D15), the catalog and persistence stay with the host.
//!
//! Three rendering modes ([`ToolRendering`]) decide how dynamically-activated
//! tools reach the model without invalidating the prompt-cache prefix:
//!
//! - `Inline`: active tools go in the `tools` array (every activation changes
//!   the array — no cache).
//! - `DeferredToolReference`: all activatable tools are declared upfront with
//!   `defer_loading: true`; an activation's tool result carries a
//!   `_tool_references` marker the Anthropic client converts to
//!   `tool_reference` blocks.
//! - `SystemToolBlock`: activated tools never touch the `tools` array; a
//!   `{role:"system", tools:[…]}` message is appended after the activation's
//!   tool-result group (Kimi/Moonshot speaks this natively).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::ids::MessageId;
use crate::tool::{Tool, ToolCtx, ToolFailure, ToolOutput};

/// How dynamically-activated tools are rendered on the wire. On
/// [`crate::model::ModelInfo`]; read by `ToolSet::defs` and assemblers,
/// consumed by the shipped clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolRendering {
    /// Only the currently-active tools in the `tools` array.
    #[default]
    Inline,
    /// Anthropic: all activatable tools `defer_loading: true` + tool_reference
    /// blocks in activation results.
    DeferredToolReference,
    /// Kimi K3: `{role:"system", tools:[defs]}` appended after the activation
    /// (append-only, cache-safe).
    SystemToolBlock,
}

/// One activation: the defs of the groups activated at a given anchor message.
#[derive(Debug, Clone)]
pub struct Activation {
    pub anchor: MessageId,
    /// OpenAI-shaped tool defs of the groups activated at `anchor`.
    pub defs:   Vec<Value>,
}

/// Catalog + persistence of activations — implemented by the host. Consulted
/// by assemblers (injection) and by host `ToolSet`s (array rendering).
#[async_trait]
pub trait ActivationSource: Send + Sync {
    /// The activations in force for a frame, ordered by anchor.
    async fn activations(&self, frame: crate::ids::FrameId) -> crate::Result<Vec<Activation>>;
}

/// Backend of the shipped [`ActivateToolsTool`]: validates the groups, mutates
/// the grants, persists the activation (anchored at the current message via
/// `ctx`). Returns the confirmation text shown to the model.
#[async_trait]
pub trait ToolActivator: Send + Sync {
    async fn activate(&self, groups: Vec<String>, ctx: &ToolCtx) -> Result<String, ToolFailure>;
}

/// The shipped `activate_tools` tool. To the kernel it's a tool like any
/// other — the defs re-read at the next round makes the new grants visible.
pub struct ActivateToolsTool {
    activator: Arc<dyn ToolActivator>,
}

impl ActivateToolsTool {
    pub fn new(activator: Arc<dyn ToolActivator>) -> Self { Self { activator } }
}

#[async_trait]
impl Tool for ActivateToolsTool {
    fn name(&self) -> &str { "activate_tools" }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "activate_tools",
                "description": "Load additional tool groups on demand. Activated tools \
                                become available from the next step of this conversation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "groups": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Names of the tool groups to activate"
                        }
                    },
                    "required": ["groups"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        let groups: Vec<String> = args["groups"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if groups.is_empty() {
            return Err(ToolFailure::Failed("activate_tools: no groups given".into()));
        }
        let text = self.activator.activate(groups, ctx).await?;
        Ok(ToolOutput::Text(text))
    }
}
