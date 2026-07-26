//! Skald's projection configuration — the only place the app states what its
//! models need on the wire. The projection engine itself is the library's
//! (`agent_loop::projection`); this is the set of knobs, in one place, so a
//! provider quirk is a value change and not a code change.

use std::sync::Arc;

use agent_loop::activation::ActivationSource;
use agent_loop::context::LinearAssembler;
use agent_loop::projection::{MediaBudget, Projection, ReasoningEcho, ResultLimit};
use core_api::user_fs::UserFs;

use crate::compactor::SUMMARY_PREFIX;
use crate::loop_adapters::media_source::SkaldMediaSource;
use crate::loop_adapters::tool_digest::SkaldDigest;
use crate::tools::tool_names as tn;

/// Where the summary block ends and full history resumes.
const SUMMARY_SUFFIX: &str =
    "[End of context summary — the following messages are the most recent exchanges in full.]";

/// A call still `running`/`pending` at projection time died mid-flight: the
/// wording tells the model it may retry, which a bare "interrupted" would not.
const INTERRUPTED: &str = "Error: tool call was interrupted (connection lost before user approval). \
                           Please retry the operation.";

/// The knobs Skald's model fleet needs.
///
/// - `max_history_messages` applies **only without compaction**: with the
///   compactor on, the summary is what bounds the context, and a window on top
///   of it would silently drop messages the summary does not cover.
/// - tool results are shrunk for previous turns only, so the in-flight turn
///   always sees its own output in full.
pub fn skald_projection(
    max_history_messages:  usize,
    compaction_enabled:    bool,
    max_tool_result_chars: Option<usize>,
) -> Projection {
    Projection {
        summary_prefix:  SUMMARY_PREFIX.to_string(),
        summary_suffix:  Some(SUMMARY_SUFFIX.to_string()),
        max_messages:    (!compaction_enabled).then_some(max_history_messages),
        max_tool_result: max_tool_result_chars.map(|max_chars| ResultLimit {
            max_chars,
            previous_turns_only: true,
        }),
        interrupted_text:  INTERRUPTED.to_string(),
        rejected_default:  "User rejected this tool call.".to_string(),
        cancelled_default: "Tool call was cancelled by the user.".to_string(),
        // DeepSeek's thinking mode rejects a replayed tool-calling turn whose
        // reasoning_content is empty.
        reasoning_placeholder: Some("(no reasoning recorded for this step)".to_string()),
        // Some endpoints read `reasoning_content`, others `reasoning`; neither
        // rejects the extra key, so Skald sends both.
        reasoning_echo: ReasoningEcho::Both,
        tail_separator: "\n\n---\n".to_string(),
        media:          MediaBudget::default(),
        // The DTL marker belongs on the activation's own result, not on
        // whichever tool result happens to come first in the round.
        activation_anchor_tool: Some(tn::ACTIVATE_TOOLS.to_string()),
    }
}

/// The assembler every Skald turn runs on: the configuration above plus the two
/// content hooks. `fs` is the caller's filesystem view — without it media is
/// never inlined (nothing can be authorized), which is the right default for a
/// context with no user workspace.
pub fn skald_assembler(
    activation:            Arc<dyn ActivationSource>,
    fs:                    Option<Arc<UserFs>>,
    max_history_messages:  usize,
    compaction_enabled:    bool,
    max_tool_result_chars: Option<usize>,
) -> LinearAssembler {
    let mut assembler = LinearAssembler::new()
        .with_projection(skald_projection(
            max_history_messages,
            compaction_enabled,
            max_tool_result_chars,
        ))
        .with_activation(activation)
        .with_digest(Arc::new(SkaldDigest));
    if let Some(fs) = fs {
        assembler = assembler.with_media(Arc::new(SkaldMediaSource::new(fs)));
    }
    assembler
}
