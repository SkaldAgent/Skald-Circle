use std::sync::Arc;

use serde_json::json;
use teloxide::prelude::*;
use teloxide::types::InputFile;

use core_api::interface_tool::InterfaceTool;
use core_api::tts::{TextToSpeech, TtsProvider};

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
) -> Vec<InterfaceTool> {
    let mut tools = vec![send_attachment_tool(bot.clone(), chat_id)];

    if let Some(synth) = tts.get().await {
        tools.push(send_voice_tool(bot, chat_id, synth));
    }

    tools
}

// ── send_attachment ───────────────────────────────────────────────────────────

fn send_attachment_tool(bot: Bot, chat_id: ChatId) -> InterfaceTool {
    InterfaceTool {
        definition: json!({
            "type": "function",
            "function": {
                "name": "send_attachment",
                "description": "Send a file from the local filesystem to the user on Telegram. Images (jpg/png/webp) and videos (mp4/mov/webm) are sent inline by default; any other type is sent as a document. Set as_document=true to force sending as a downloadable file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type":        "string",
                            "description": "Absolute or relative path to the file to send."
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
            Box::pin(async move {
                let file_path = args["file_path"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("send_attachment: missing `file_path`"))?;
                let caption     = args["caption"].as_str().map(str::to_string);
                let as_document = args["as_document"].as_bool().unwrap_or(false);

                let path = std::path::Path::new(file_path);
                if !path.exists() {
                    anyhow::bail!("send_attachment: file not found: {file_path}");
                }

                // Present images/videos inline by default; everything else (and
                // anything when as_document=true) as a downloadable document.
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let kind = if as_document {
                    "document"
                } else {
                    match ext.as_str() {
                        "jpg" | "jpeg" | "png" | "webp" => "photo",
                        "mp4" | "mov" | "webm"          => "video",
                        _                               => "document",
                    }
                };

                let file = InputFile::file(path);
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
