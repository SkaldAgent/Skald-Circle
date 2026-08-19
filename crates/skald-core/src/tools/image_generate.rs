use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::image_generate::ImageGeneratorManager;
use crate::tools::{
    SimpleExecution, Tool, ToolCategory, ToolContext, ToolDescriptionLength, ToolExecution,
    ToolResult, truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL,
};

// ── image_generate_providers_list ─────────────────────────────────────────────

pub struct ImageGenerateProvidersList {
    pub mgr: Arc<ImageGeneratorManager>,
}

impl Tool for ImageGenerateProvidersList {
    fn name(&self) -> &str { "image_generate_providers_list" }
    fn category(&self) -> ToolCategory { ToolCategory::Introspection }

    fn description(&self) -> &str {
        "List all registered image generation providers. \
         Returns an array of {id, name} objects. \
         Use the id with image_generate to pick a provider."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn describe(&self, _args: &Value, _length: ToolDescriptionLength) -> String {
        "list image providers".to_string()
    }

    fn execute_async<'a>(&'a self, _args: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        let mgr = Arc::clone(&self.mgr);
        Box::pin(async move {
            let providers = mgr.list().await;
            Ok(serde_json::to_string_pretty(&providers)?)
        })
    }
}

// ── image_generate ────────────────────────────────────────────────────────────

pub struct ImageGenerateTool {
    pub mgr: Arc<ImageGeneratorManager>,
}

impl Tool for ImageGenerateTool {
    fn name(&self) -> &str { "image_generate" }
    fn display_name(&self) -> &str { "Generate Image" }
    fn icon(&self) -> &str { "image" }
    fn category(&self) -> ToolCategory { ToolCategory::Config }

    fn description(&self) -> &str {
        "Generate an image from a text prompt. Blocks until the image is ready, then saves \
         it into your own workspace and returns `{path, url}`. `path` (under `uploads/…`, \
         relative to your home) is in your usual vocabulary — pass it to show_file_to_user, \
         send_attachment, read_file or execute_cmd. `url` renders the image inline in the \
         web and mobile chat if you embed it as a Markdown image, ![](url); it is a web \
         link, so on a channel without Markdown (Telegram) send the file itself instead."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["provider_id", "prompt"],
            "properties": {
                "provider_id": {
                    "type":        "string",
                    "description": "ID of the image generation provider (from image_generate_providers_list)"
                },
                "prompt": {
                    "type":        "string",
                    "description": "Text prompt describing the image to generate"
                },
                "extra_params": {
                    "type":        "object",
                    "description": "Optional provider-specific parameters (e.g. width, height, steps). \
                                    See extra_params_schema in image_generate_providers_list for valid fields."
                }
            }
        })
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let provider = args["provider_id"].as_str().unwrap_or("?");
        let prompt   = args["prompt"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => truncate_label(&format!("generate image ({provider})"), MAX_LABEL_SHORT),
            ToolDescriptionLength::Full  => truncate_label(&format!("generate image ({provider}): {prompt}"), MAX_LABEL_FULL),
        }
    }

    /// The generated image is written into the **caller's** workspace, so the path
    /// this returns is the one agent vocabulary every consumer already speaks:
    /// `send_attachment` (Telegram), `show_file_to_user` (web/mobile), the fs-tools
    /// and `execute_cmd` all resolve it through the same [`UserFs`]. It previously
    /// returned a path under the server's own data root, which none of them could
    /// reach — the model got a file it could not hand to anyone.
    ///
    /// `uploads/{session}/` rather than a directory of its own: that is the single
    /// placement seam (`uploads::save_to_home`, collision-safe naming included) and
    /// the one directory the media inliner is authorized to read from, so a vision
    /// model can be shown the image it just made.
    ///
    /// A `url` rides alongside the path because the chat renders Markdown images —
    /// so `![](url)` in the answer shows the picture inline instead of naming a file
    /// the user then has to open.
    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let mgr        = Arc::clone(&self.mgr);
        let fs         = Arc::clone(&ctx.fs);
        let session_id = ctx.session_id;

        Box::new(SimpleExecution::new(Box::pin(async move {
            let provider_id = args["provider_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing provider_id"))?
                .to_string();
            let prompt = args["prompt"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing prompt"))?
                .to_string();
            let extra_params = match &args["extra_params"] {
                Value::Object(_) => Some(args["extra_params"].clone()),
                _                => None,
            };

            let bytes = mgr.generate_bytes(&provider_id, &prompt, extra_params.as_ref()).await?;

            // The extension is sniffed, never assumed: providers return png, jpeg or
            // webp, and it is the extension that decides whether Telegram sends the
            // file inline as a photo or as a nondescript document.
            let mime = crate::session::handler::media::sniff_mime(&bytes[..bytes.len().min(16)])
                .unwrap_or("image/png");
            let name = file_name_for(&prompt, mime);

            let att = crate::uploads::save_to_home(
                &fs, session_id, &name, Some(mime.to_string()), &bytes,
            ).await?;

            let url = file_url(&att.path);
            Ok(ToolResult::Text(json!({ "path": att.path, "url": url }).to_string()))
        })))
    }

    /// Generation needs the caller's workspace to put the image in, and only
    /// [`run_with`](Self::run_with) is handed one. Same shape as `execute_cmd`:
    /// the context-free path fails loudly rather than writing somewhere nobody
    /// can read.
    fn execute_async<'a>(&'a self, _args: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "image_generate needs a session context to save the image into your workspace"
            ))
        })
    }
}

/// Web URL for a file in the caller's workspace: the per-user file endpoint, the
/// same one the viewer and the explorer read through.
///
/// Deliberately an **ownership-scoped** endpoint. Generated images used to be
/// served from one instance-wide directory by id, behind `require_auth` alone —
/// authenticated but with no notion of who owned the image, the same shape as the
/// `/data` static mount that was removed for exactly that. `/api/file` resolves
/// the path through the caller's own `UserFs`, so a link that leaks reveals
/// nothing to anyone not already entitled to it. (That id-based route is gone: it
/// had no writer left once placement moved into the workspace.)
fn file_url(agent_path: &str) -> String {
    // Same idiom as `mcp::oauth`: a throwaway URL's query is a correctly
    // percent-encoded `path=…`, without hand-rolling an encoder.
    let query = reqwest::Url::parse_with_params("http://local/", &[("path", agent_path)])
        .ok()
        .and_then(|u| u.query().map(str::to_owned))
        .unwrap_or_else(|| format!("path={agent_path}"));

    format!("/api/file?{query}")
}

/// Builds a file name from the prompt, so the user sees `a-red-bicycle.png` in
/// their files and in Telegram rather than a random id. Collisions are the upload
/// seam's problem (it appends `_1`, `_2`, …), so this need not be unique.
fn file_name_for(prompt: &str, mime: &str) -> String {
    let ext = match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif"  => "gif",
        _            => "png",
    };

    let mut slug = String::new();
    for ch in prompt.chars() {
        if slug.len() >= 48 { break; }
        if ch.is_ascii_alphanumeric() {
            slug.extend(ch.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let stem = if slug.is_empty() { "image" } else { slug };

    format!("{stem}.{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_points_at_the_per_user_file_endpoint_and_encodes_the_path() {
        assert_eq!(
            file_url("uploads/7/a-red-bicycle.png"),
            "/api/file?path=uploads%2F7%2Fa-red-bicycle.png",
        );
        // A name the slug rules cannot produce, but that the endpoint must still
        // receive intact rather than as two query params.
        assert_eq!(
            file_url("uploads/7/a&b c.png"),
            "/api/file?path=uploads%2F7%2Fa%26b+c.png",
        );
    }

    #[test]
    fn file_name_slugs_the_prompt_and_keys_the_extension_on_the_mime() {
        assert_eq!(file_name_for("A red bicycle", "image/png"), "a-red-bicycle.png");
        assert_eq!(file_name_for("A red bicycle", "image/jpeg"), "a-red-bicycle.jpg");
        assert_eq!(file_name_for("A red bicycle", "image/webp"), "a-red-bicycle.webp");
        // An unrecognized type still produces a usable image name.
        assert_eq!(file_name_for("A red bicycle", "application/octet-stream"), "a-red-bicycle.png");
    }

    #[test]
    fn file_name_never_carries_path_or_shell_syntax_out_of_the_prompt() {
        // Everything non-alphanumeric collapses to a single dash, so a prompt can
        // neither escape the uploads directory nor smuggle syntax into a later
        // `execute_cmd` on the returned path.
        assert_eq!(file_name_for("../../etc/passwd", "image/png"), "etc-passwd.png");
        assert_eq!(file_name_for("a $(whoami) cat", "image/png"), "a-whoami-cat.png");
        // A prompt with nothing usable still yields a name.
        assert_eq!(file_name_for("...", "image/png"), "image.png");
        assert_eq!(file_name_for("", "image/png"), "image.png");
    }

    #[test]
    fn file_name_stays_short_for_a_long_prompt() {
        let name = file_name_for(&"word ".repeat(60), "image/png");
        assert!(name.len() <= 53, "{name}");
        assert!(name.ends_with(".png"));
        assert!(!name.starts_with('-') && !name.contains("-."));
    }
}
