use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::cron::TaskManager;
use crate::core::tools::{Tool, ToolDescriptionLength};

// ── execute_task ──────────────────────────────────────────────────────────────
//
// This struct is NOT registered in the global ToolRegistry. Instead it is
// injected as an InterfaceTool (with the session_id captured in a closure)
// by the session handler for interactive sessions (web, telegram).
// Background sessions (cron, async) receive `execute_subtask` instead.
//
// The struct is public so skald.rs can call build_execute_task_interface_tool().

pub struct ExecuteTask(pub Arc<TaskManager>);

impl ExecuteTask {
    fn description_text() -> &'static str {
        "Create and run a task. Three modes:\n\
         • mode=cron — scheduled by a 7-field cron expression (sec min hour dom month dow year, \
           Europe/London timezone). Returns task_id and next scheduled run. Recurring unless the \
           expression can only fire once.\n\
         • mode=sync — run immediately, block until the agent finishes, and return the result inline. \
           Best for short tasks (a few seconds to a few minutes).\n\
         • mode=async — start the task in the background and return the task_id immediately. \
           When the task completes its result will be delivered back to this chat automatically."
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["mode", "title", "prompt", "agent_id"],
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["cron", "sync", "async"],
                    "description": "cron=scheduled; sync=run now and wait for result; async=run in background, result comes back to this chat"
                },
                "title":       { "type": "string",  "description": "Short name for this task" },
                "description": { "type": "string",  "description": "What this task does" },
                "cron":        { "type": "string",  "description": "7-field cron expression — required when mode=cron (times in Europe/London). E.g. '0 0 9 * * * *' = every day at 09:00" },
                "prompt":      { "type": "string",  "description": "Prompt sent to the agent at each run" },
                "agent_id":    { "type": "string",  "description": "Task agent to run (required; e.g. software-engineer, researcher, generalist). Must be a `task` agent — chat/system agents are rejected." }
            }
        })
    }

    pub fn execute_with_session(&self, args: &Value, session_id: i64, run_context: Option<String>) -> Result<String> {
        let mode     = args["mode"].as_str().unwrap_or("").trim().to_string();
        let title    = args["title"].as_str().unwrap_or("").trim().to_string();
        let desc     = args["description"].as_str().unwrap_or("").trim().to_string();
        let cron     = args["cron"].as_str().unwrap_or("").trim().to_string();
        let prompt   = args["prompt"].as_str().unwrap_or("").trim().to_string();
        // No default: agent_id is required and validated as a `task` agent inside
        // TaskManager (require_task_agent) for every mode.
        let agent_id = args["agent_id"].as_str().unwrap_or("").trim().to_string();
        let rc_id    = run_context.as_deref();

        if title.is_empty()  { anyhow::bail!("title is required"); }
        if prompt.is_empty() { anyhow::bail!("prompt is required"); }

        match mode.as_str() {
            "cron" => {
                if cron.is_empty() { anyhow::bail!("cron expression is required for mode=cron"); }
                let job = self.0.add_job(&title, &desc, &cron, &prompt, &agent_id, false, "cron", None, rc_id)?;
                let kind = if job.single_run { "one-shot" } else { "recurring" };
                Ok(serde_json::to_string(&json!({
                    "task_id":    job.id,
                    "mode":       "cron",
                    "recurring":  !job.single_run,
                    "next_run_at": job.next_run_at,
                    "message": format!("Created {} cron task {} — '{}'", kind, job.id, job.title),
                }))?)
            }
            "sync" => {
                let result = self.0.add_job_sync(&title, &desc, &prompt, &agent_id, rc_id)?;
                Ok(result)
            }
            "async" => {
                let job = self.0.add_job_async(&title, &desc, &prompt, &agent_id, session_id, rc_id)?;
                Ok(serde_json::to_string(&json!({
                    "task_id": job.id,
                    "status":  "started",
                    "message": format!(
                        "Task {} ('{}') is running in the background. \
                         The system will automatically deliver the result to this conversation when complete. \
                         Do NOT call read_agent_result or read_notifications — no polling needed. \
                         Continue the conversation normally.",
                        job.id, job.title
                    ),
                }))?)
            }
            _ => anyhow::bail!("mode must be one of: cron, sync, async"),
        }
    }
}

/// Builds the execute_task InterfaceTool with the session_id captured in a closure.
/// Called from the session handler when building AgentRunConfig for interactive sessions.
pub fn build_execute_task_interface_tool(
    task_mgr:     Arc<TaskManager>,
    session_id:   i64,
    run_context:  Option<String>,
) -> crate::core::session::handler::InterfaceTool {
    use crate::core::session::handler::{InterfaceTool, ToolFuture};

    let tool = Arc::new(ExecuteTask(task_mgr));

    InterfaceTool {
        definition: json!({
            "type": "function",
            "function": {
                "name": "execute_task",
                "description": ExecuteTask::description_text(),
                "parameters": ExecuteTask::schema(),
            }
        }),
        handler: Arc::new(move |args: Value| -> ToolFuture {
            let tool_clone  = Arc::clone(&tool);
            let run_context = run_context.clone();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    tool_clone.execute_with_session(&args, session_id, run_context)
                })
                .await
                .map_err(|e| anyhow::anyhow!("execute_task panicked: {e}"))?
            })
        }),
    }
}

// ── delete_cron_job ───────────────────────────────────────────────────────────

pub struct DeleteCronJob(pub Arc<TaskManager>);

impl Tool for DeleteCronJob {
    fn name(&self) -> &str { "delete_cron_job" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Permanently delete a scheduled task or cron job by its numeric id."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "integer", "description": "Task id from list_items (type=cron)" }
            }
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let id = args["id"].as_i64().map(|n| n.to_string()).unwrap_or_else(|| "?".to_string());
        format!("delete cron job #{id}")
    }

    fn execute(&self, args: Value) -> Result<String> {
        let id = args["id"].as_i64().ok_or_else(|| anyhow::anyhow!("id must be an integer"))?;
        if self.0.delete_job(id)? {
            Ok(format!("Task {id} deleted."))
        } else {
            Ok(format!("No task with id {id}."))
        }
    }
}
