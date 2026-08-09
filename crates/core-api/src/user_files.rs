//! Reading a user's files from a channel plugin (blueprint §6).
//!
//! A channel adapter that hands a file back to the user — Telegram's
//! `send_attachment` is the first — is given a path in the **agent's** vocabulary
//! (`~/report.pdf`, `uploads/{session}/photo.jpg`, `shared/{X}/…`, or a
//! container-absolute `/tmp/out.png`), because that is the only vocabulary the
//! model has ever seen. None of those spellings is a host path: resolving them
//! means the same two-backing routing the fs-tools do — a bind-mounted path read
//! host-side, anything else read through the user's container.
//!
//! That routing lives in the core, so this is the seam that lets a plugin borrow
//! it instead of touching the process working directory (which is what a plain
//! `std::fs::read` of an agent path does — it either fails or, worse, reads a
//! same-named file next to the binary).

use async_trait::async_trait;

/// A file read out of a user's workspace.
pub struct UserFile {
    /// The canonical agent-vocabulary path — what the user and the model see.
    pub display: String,
    /// Basename of [`display`](Self::display), for surfaces that need a file name.
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Reads files from one user's workspace, with the agent's own path routing.
///
/// Obtained from [`UserChannelHandle::files`](crate::user_channel::UserChannelHandle::files),
/// so it is already scoped to that user: containment is the core's
/// (canonicalize + prefix-check on the mounts, the container otherwise) and a
/// path outside the caller's view is refused, never silently resolved elsewhere.
#[async_trait]
pub trait UserFilesApi: Send + Sync {
    /// Reads `path`, refusing anything larger than `max_bytes` **before** loading
    /// it — the cap is the caller's own limit (Telegram's upload ceiling, say),
    /// and a size check that ran after the read would protect nothing.
    ///
    /// Virtual memory notes (`user-memory/…`, `shared-memory/…`) are not files and
    /// are rejected with a clear error.
    async fn read(&self, path: &str, max_bytes: u64) -> anyhow::Result<UserFile>;
}
