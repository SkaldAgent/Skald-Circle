use anyhow::Result;
use serde_json::{Value, json};
use tracing::info;

use crate::core::tools::{Tool, ToolDescriptionLength};

pub struct Restart;

impl Tool for Restart {
    fn name(&self) -> &str { crate::core::tools::tool_names::RESTART }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Shell }

    fn description(&self) -> &str {
        "Restart the skald process. \
         In headless (dev) mode exits with code -1, signalling the run.sh supervisor to rebuild \
         (cargo build) and relaunch — use this after editing the source code to load the new version. \
         In desktop (Tauri bundle) mode there is no source tree to rebuild, so the process is simply \
         restarted (cleanup + respawn) — use this to apply config.yml / database changes that are \
         only read at startup. \
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
        // Desktop (Tauri bundle) mode: respawn the process manually. The bundled
        // binary is read-only (no source tree to rebuild), so "restart" means:
        // run Tauri-side teardown, spawn a fresh copy of the current exe, exit.
        // This mirrors what `tauri-plugin-process`'s JS `restart` does internally.
        #[cfg(feature = "desktop")]
        {
            if let Some(handle) = crate::desktop::app_handle() {
                info!("restart requested — desktop mode: respawning process");
                let exe = std::env::current_exe()
                    .map_err(|e| anyhow::anyhow!("failed to resolve current_exe: {e}"))?;
                // Tauri-side teardown (webview, tray, event loop, windows).
                handle.cleanup_before_exit();
                // Spawn a fresh copy of the current binary (detached).
                let _ = std::process::Command::new(exe).spawn();
                std::process::exit(0);
            }
            // Fall through if (somehow) the AppHandle isn't set yet — treat as
            // headless and use the exit-code path.
        }

        // Headless (dev) mode: exit with code -1 (= 255 on Unix). run.sh
        // supervisor sees 255, runs `cargo build`, and relaunches. Use
        // `_exit()` instead of `exit()` to skip C atexit handlers (e.g. Metal
        // GPU cleanup in whisper-rs which crashes with SIGABRT and produces
        // exit code 134 instead of 255).
        info!("restart requested — headless mode: exit(-1) → supervisor rebuilds");
        unsafe { libc::_exit(-1) }
    }
}
