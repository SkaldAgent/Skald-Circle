use std::sync::OnceLock;

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::tools::{Tool, ToolDescriptionLength};

/// How to restart, when exiting for a supervisor is not the answer.
///
/// A shell with no supervisor watching its exit code would need to tear itself
/// down and respawn on its own. That is knowledge about the process shell, which
/// the core does not have — so such a shell installs it here. The default server
/// shell has a supervisor (`run.sh`) and installs no handler, so `restart` falls
/// back to the supervisor protocol below.
///
/// Returns only on failure; a successful handler never comes back.
pub type RestartHandler = Box<dyn Fn() -> Result<()> + Send + Sync>;

static HANDLER: OnceLock<RestartHandler> = OnceLock::new();

/// Called once by the process shell during startup, before any tool can run.
pub fn set_restart_handler(handler: RestartHandler) {
    if HANDLER.set(handler).is_err() {
        warn!("a restart handler is already installed — ignoring this one");
    }
}

pub struct Restart;

impl Tool for Restart {
    fn name(&self) -> &str { crate::tools::tool_names::RESTART }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Shell }

    fn description(&self) -> &str {
        "Restart the skald process. Nothing is recompiled: the same binary is re-executed, \
         so this applies config.yml and database changes, which are only read at startup. \
         To load new code, build first (./build.sh), then restart. \
         Requires user approval."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    fn describe(&self, _args: &Value, _length: ToolDescriptionLength) -> String {
        "restart skald".to_string()
    }

    fn execute(&self, _args: Value) -> Result<String> {
        // A shell that installed its own teardown-and-respawn handles it here.
        // Normally this never returns.
        if let Some(handler) = HANDLER.get() {
            info!("restart requested — delegating to the installed handler");
            handler()?;
            warn!("the restart handler returned without restarting — falling back");
        }

        // Headless: exit with code -1 (= 255 on Unix), which `run.sh` reads as
        // "re-execute me". `_exit()` rather than `exit()` skips C atexit handlers
        // — whisper-rs's Metal GPU cleanup aborts with SIGABRT, turning the exit
        // code into 134 and stopping the supervisor instead of restarting it.
        info!("restart requested — exit(-1) → supervisor re-executes the binary");
        unsafe { libc::_exit(-1) }
    }
}
