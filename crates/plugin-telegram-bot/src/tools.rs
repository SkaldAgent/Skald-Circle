use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use teloxide::prelude::*;
use teloxide::types::InputFile;

use core_api::interface_tool::InterfaceTool;
use core_api::tool::{Tool, ToolCategory, ToolDescriptionLength};
use core_api::tts::{TextToSpeech, TtsProvider};
use core_api::user_files::UserFilesApi;

use super::auth::{Binding, load_config, save_config};
use super::TelegramPlugin;

/// Returns all LLM-callable tools available in a Telegram session.
///
/// Each tool captures `bot` and `chat_id` so its handler can send content
/// back to the user without any additional context.
///
/// `send_voice_message` is included only when at least one TTS provider is active.
///
/// # Adding a new tool
/// Implement a private `fn <name>_tool(bot: Bot, chat_id: ChatId, ...) -> InterfaceTool`
/// and push it into the vec returned by this function.
pub(crate) async fn interface_tools(
    bot:     Bot,
    chat_id: ChatId,
    tts:     &dyn TtsProvider,
    files:   Arc<dyn UserFilesApi>,
) -> Vec<InterfaceTool> {
    let mut tools = vec![send_attachment_tool(bot.clone(), chat_id, files)];

    if let Some(synth) = tts.get().await {
        tools.push(send_voice_tool(bot, chat_id, synth));
    }

    tools
}

// ── send_attachment ───────────────────────────────────────────────────────────

/// What the Bot API accepts in one upload (50 MB). Checked before the file is
/// read, so an oversized one costs a `stat` rather than a rejected 50 MB POST.
const TELEGRAM_UPLOAD_LIMIT: u64 = 50 * 1000 * 1000;

/// The narrower ceiling `sendPhoto` enforces — above it an image is sent as a
/// document instead, which is the same bytes without the inline preview.
const TELEGRAM_PHOTO_LIMIT: u64 = 10 * 1000 * 1000;

/// Sends a file from the **user's** workspace, resolved through
/// [`UserFilesApi`] — the same routing the fs-tools use, so `~/report.pdf`,
/// `uploads/{session}/photo.jpg` and the container-only `/tmp/out.png` all work.
///
/// It used to hand the raw argument to `InputFile::file`, which resolves against
/// the **server process's** working directory: every agent path the model has
/// ever been given (each of them relative to the user's home, or absolute inside
/// their container) failed the `path.exists()` check, and the one class that did
/// not — a name that happens to exist next to the binary — would have sent the
/// wrong file entirely.
fn send_attachment_tool(bot: Bot, chat_id: ChatId, files: Arc<dyn UserFilesApi>) -> InterfaceTool {
    InterfaceTool {
        definition: json!({
            "type": "function",
            "function": {
                "name": "send_attachment",
                "description": "Send a file to the user on Telegram. Images (jpg/png/webp) and videos (mp4/mov/webm) are sent inline by default; any other type is sent as a document. Set as_document=true to force sending as a downloadable file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type":        "string",
                            "description": "Path to the file, in your usual vocabulary: `~/report.pdf`, `uploads/…`, `shared/{folder}/…`, `projects/…`, or an absolute path inside your sandbox (`/tmp/out.png`). Memory notes cannot be sent."
                        },
                        "caption": {
                            "type":        "string",
                            "description": "Optional caption shown below the file."
                        },
                        "as_document": {
                            "type":        "boolean",
                            "description": "Force sending as a downloadable file instead of an inline photo/video (default false)."
                        }
                    },
                    "required": ["file_path"]
                }
            }
        }),
        handler: Arc::new(move |args| {
            let bot     = bot.clone();
            let files   = Arc::clone(&files);
            Box::pin(async move {
                let file_path = args["file_path"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("send_attachment: missing `file_path`"))?;
                let caption     = args["caption"].as_str().map(str::to_string);
                let as_document = args["as_document"].as_bool().unwrap_or(false);

                let read = files.read(file_path, TELEGRAM_UPLOAD_LIMIT).await
                    .map_err(|e| anyhow::anyhow!("send_attachment: {e}"))?;

                // Present images/videos inline by default; everything else (and
                // anything when as_document=true) as a downloadable document.
                let ext = std::path::Path::new(&read.name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let mut kind = if as_document {
                    "document"
                } else {
                    match ext.as_str() {
                        "jpg" | "jpeg" | "png" | "webp" => "photo",
                        "mp4" | "mov" | "webm"          => "video",
                        _                               => "document",
                    }
                };
                // `sendPhoto` caps at 10 MB where `sendDocument` takes 50, so a big
                // image goes out as a file rather than as an API error.
                if kind == "photo" && read.bytes.len() as u64 > TELEGRAM_PHOTO_LIMIT {
                    kind = "document";
                }

                // The bytes are already in hand — a container file has no host path
                // to point Telegram at, and a mounted one would only be re-read.
                let file = InputFile::memory(read.bytes).file_name(read.name);
                let file_path = read.display;
                let result = match kind {
                    "photo" => {
                        let mut req = bot.send_photo(chat_id, file);
                        if let Some(cap) = caption { req = req.caption(cap); }
                        req.await.map(|_| ())
                    }
                    "video" => {
                        let mut req = bot.send_video(chat_id, file);
                        if let Some(cap) = caption { req = req.caption(cap); }
                        req.await.map(|_| ())
                    }
                    _ => {
                        let mut req = bot.send_document(chat_id, file);
                        if let Some(cap) = caption { req = req.caption(cap); }
                        req.await.map(|_| ())
                    }
                };

                result.map_err(|e| anyhow::anyhow!("send_attachment: Telegram error: {e}"))?;
                Ok(format!("File sent ({kind}): {file_path}"))
            })
        }),
    }
}

// ── send_voice_message ────────────────────────────────────────────────────────

fn send_voice_tool(bot: Bot, chat_id: ChatId, synth: Arc<dyn TextToSpeech>) -> InterfaceTool {
    let instructions_hint = synth
        .instructions()
        .map(|i| format!("\n\nVoice instructions: {i}"))
        .unwrap_or_default();

    InterfaceTool {
        definition: json!({
            "type": "function",
            "function": {
                "name": "send_voice_message",
                "description": format!(
                    "Synthesise text to speech and send it to the user as a Telegram voice message. \
                     Use when audio is a better medium than text — e.g. short answers, \
                     confirmations, or when the user asks you to speak.{instructions_hint}"
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": {
                            "type":        "string",
                            "description": "The text to synthesise and send as audio."
                        }
                    },
                    "required": ["text"]
                }
            }
        }),
        handler: Arc::new(move |args| {
            let bot   = bot.clone();
            let synth = Arc::clone(&synth);
            Box::pin(async move {
                let text = args["text"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("send_voice_message: missing `text`"))?;

                let audio = synth
                    .synthesize(text, None)
                    .await
                    .map_err(|e| anyhow::anyhow!("send_voice_message: TTS error: {e}"))?;

                // Telegram only renders Ogg/Opus as a playable voice message, so
                // transcode whatever the synthesiser produced (mp3, wav, raw pcm…).
                let audio = to_ogg_opus(audio, synth.output_format())
                    .await
                    .map_err(|e| anyhow::anyhow!("send_voice_message: audio conversion failed: {e}"))?;

                bot.send_voice(chat_id, InputFile::memory(audio).file_name("voice.ogg"))
                    .await
                    .map_err(|e| anyhow::anyhow!("send_voice_message: Telegram error: {e}"))?;

                Ok("Voice message sent.".to_string())
            })
        }),
    }
}

/// Transcode synthesised audio to Ogg/Opus — the only format Telegram renders as
/// a playable voice message — using ffmpeg over stdin/stdout pipes (no temp files).
///
/// `format` is the synthesiser's [`TextToSpeech::output_format`]. Ogg/Opus input
/// is passed through untouched. Raw `pcm` is headerless, so it is described to
/// ffmpeg as the 24 kHz / mono / s16le stream OpenAI and Gemini TTS emit; every
/// other (self-describing) container is auto-detected by ffmpeg.
async fn to_ogg_opus(audio: Vec<u8>, format: &str) -> anyhow::Result<Vec<u8>> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    // Already a Telegram-native container — nothing to do.
    if matches!(format, "opus" | "ogg") {
        return Ok(audio);
    }

    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error"]);
    if format == "pcm" {
        cmd.args(["-f", "s16le", "-ar", "24000", "-ac", "1"]);
    }
    cmd.args(["-i", "pipe:0", "-c:a", "libopus", "-b:a", "32k", "-f", "ogg", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!(
        "ffmpeg not available (required to convert {format} audio to Telegram Ogg/Opus): {e}"
    ))?;

    // Feed stdin from a separate task so a full stdout pipe can't deadlock the write.
    let mut stdin = child.stdin.take().expect("stdin piped");
    let feeder = tokio::spawn(async move {
        let _ = stdin.write_all(&audio).await;
        let _ = stdin.shutdown().await;
    });

    let out = child.wait_with_output().await
        .map_err(|e| anyhow::anyhow!("ffmpeg execution failed: {e}"))?;
    let _ = feeder.await;

    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg ({format} → Ogg/Opus) exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    Ok(out.stdout)
}

// ── telegram_pairing (registry tool, category Config) ─────────────────────────

/// Tool that binds a Telegram `chat_id` to a Skald `user_id`.
///
/// Category `Config` — excluded from the default tool list, activated
/// explicitly by the admin's agent. The admin calls this after a user reports
/// their pairing code from Telegram.
///
/// The binding is written to the config table (key `"telegram"`); the
/// resulting `ConfigKeyUpdated` event reloads the plugin's in-memory cache
/// instantly.
pub struct TelegramPairingTool {
    plugin: Arc<TelegramPlugin>,
}

impl TelegramPairingTool {
    pub fn new(plugin: Arc<TelegramPlugin>) -> Self {
        Self { plugin }
    }
}

impl Tool for TelegramPairingTool {
    fn name(&self) -> &str { "telegram_pairing" }
    fn category(&self) -> ToolCategory { ToolCategory::Config }

    fn description(&self) -> &str {
        "Bind a Telegram chat to a Skald user so they can chat with the agent via Telegram. \
         Use `action: \"bind\"` with either a `code` (from the pairing message the user received) \
         or a `chat_id` + `user_id`. Use `action: \"list\"` to see current bindings. \
         Use `action: \"unbind\"` with a `chat_id` to remove a binding."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type":        "string",
                    "enum":        ["bind", "unbind", "list"],
                    "description": "bind: create a chat_id→user_id binding. unbind: remove it. list: show all bindings.",
                    "default":     "bind"
                },
                "code": {
                    "type":        "string",
                    "description": "Pairing code shown to the Telegram user (alternative to chat_id+user_id)."
                },
                "chat_id": {
                    "type":        "integer",
                    "description": "Telegram chat id (use when not resolving via code)."
                },
                "user_id": {
                    "type":        "string",
                    "description": "Skald user id to bind to (required for bind when not using code)."
                }
            }
        })
    }

    fn describe(&self, _args: &Value, _length: ToolDescriptionLength) -> String {
        "telegram_pairing".to_string()
    }

    fn execute(&self, args: Value) -> Result<String> {
        let shared = self.plugin.shared()
            .ok_or_else(|| anyhow::anyhow!("telegram: plugin is not running"))?
            .clone();

        let action = args.get("action")
            .and_then(Value::as_str)
            .unwrap_or("bind");

        // Block on since Tool::execute is sync.
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| anyhow::anyhow!("telegram_pairing: no tokio runtime: {e}"))?;

        rt.block_on(async {
            let cfg_api = &*shared.config;

            match action {
                "list" => {
                    let cfg = load_config(cfg_api).await?;
                    if cfg.bindings.is_empty() {
                        return Ok("No Telegram bindings.".to_string());
                    }
                    let lines: Vec<String> = cfg.bindings.iter()
                        .map(|b| format!("  chat_id={} → user_id={}{}", b.chat_id, b.user_id,
                            b.display.as_ref().map(|d| format!(" ({d})")).unwrap_or_default()))
                        .collect();
                    Ok(format!("Telegram bindings:\n{}", lines.join("\n")))
                }

                "unbind" => {
                    let chat_id = args.get("chat_id")
                        .and_then(Value::as_i64)
                        .ok_or_else(|| anyhow::anyhow!("telegram_pairing: `chat_id` required for unbind"))?;

                    let mut cfg = load_config(cfg_api).await?;
                    let before = cfg.bindings.len();
                    cfg.bindings.retain(|b| b.chat_id != chat_id);
                    if cfg.bindings.len() == before {
                        return Ok(format!("chat_id {chat_id} is not bound."));
                    }
                    save_config(cfg_api, &cfg).await?;
                    Ok(format!("Unbound chat_id {chat_id}."))
                }

                "bind" => {
                    let mut cfg = load_config(cfg_api).await?;

                    // Resolve chat_id + user_id either from a pairing code or
                    // from explicit arguments.
                    let (chat_id, user_id) = if let Some(code) = args.get("code").and_then(Value::as_str) {
                        let entry = cfg.pending_pairings.iter()
                            .find(|e| e.code == code)
                            .ok_or_else(|| anyhow::anyhow!("telegram_pairing: code '{code}' not found (it may have expired or already been used)"))?;
                        let chat_id = entry.chat_id;
                        let user_id = args.get("user_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("telegram_pairing: `user_id` required (the code only identifies the chat)"))?
                            .to_string();
                        // Remove the used pairing entry.
                        cfg.pending_pairings.retain(|e| e.code != code);
                        (chat_id, user_id)
                    } else {
                        let chat_id = args.get("chat_id")
                            .and_then(Value::as_i64)
                            .ok_or_else(|| anyhow::anyhow!("telegram_pairing: either `code` or `chat_id`+`user_id` required"))?;
                        let user_id = args.get("user_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("telegram_pairing: `user_id` required"))?
                            .to_string();
                        (chat_id, user_id)
                    };

                    // Replace any existing binding for this chat_id.
                    cfg.bindings.retain(|b| b.chat_id != chat_id);
                    cfg.bindings.push(Binding {
                        chat_id,
                        user_id: user_id.clone(),
                        display: None,
                    });

                    save_config(cfg_api, &cfg).await?;
                    Ok(format!("Bound Telegram chat_id {chat_id} to user_id {user_id}."))
                }

                other => Err(anyhow::anyhow!("telegram_pairing: unknown action '{other}'")),
            }
        })
    }
}
