//! `PrefixCache` — the system prefix, frozen for as long as a provider's prompt
//! cache could still be holding it.
//!
//! Every provider that caches keys on the longest common *prefix*, and the
//! system prompt is the first thing in it — so rebuilding it changes the whole
//! request. That is what used to happen on every round:
//! [`AgentSystemContext`](super::system::AgentSystemContext) reassembles `base`
//! from disk and SQLite each time it is asked, so an agent writing to
//! `user-memory/index.md` in round 3 turned round 4, seconds later and with the
//! cache certainly warm, into a full miss.
//!
//! So the prefix is built once and kept. The refresh rule is the one that costs
//! nothing: **rebuild only once the conversation has been idle long enough that
//! the provider's cache is gone anyway.** Below that window a rebuild buys
//! freshness at the price of a guaranteed miss; above it, it is free. Hence the
//! clock is *idle time of this conversation*, not time since some file changed
//! — and every call to [`PrefixCache::get`] is a request about to go out, which
//! is why reading restarts the window.
//!
//! **Writes are deliberately not reacted to.** When the agent itself edits an
//! injected file the new content is already in the context — the tool call and
//! its result sit two messages downstream — so refreshing the prefix would only
//! repeat what the model just said. A write from *elsewhere* (the same user's
//! Telegram session, a cron job, another member editing `shared-memory/`) is
//! genuinely invisible until the TTL, and that is the trade taken knowingly: it
//! is precisely the case where an immediate rebuild costs the most, since a
//! conversation that would notice is by definition a warm one. The freshness
//! path already exists and is cheaper — the agent can `read_file`, and a tool
//! result *appends*, which never invalidates anything. The injection header in
//! `system.rs` tells it so.
//!
//! Reacting to another user's write would need a `SystemEventBus` variant and a
//! subscriber per user, since the writer lives in a different `UserContext`.
//! That is future work; the seam for it is this type's key.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use agent_loop::ids::ConversationId;

/// How long a prefix survives without its conversation calling a model.
///
/// The asymmetry that sets it: going *below* a provider's cache window pays
/// misses that buy nothing, while going above only costs freshness we have
/// already decided we do not need. Anthropic's `ephemeral` blocks live 5
/// minutes; OpenAI's automatic prefix cache is fuzzier and can last longer.
pub const PREFIX_TTL: Duration = Duration::from_secs(20 * 60);

/// A conversation plus the agent running in it. Both are needed: a sub-agent
/// shares its parent's conversation but has its own prompt, and therefore its
/// own cache prefix.
type Key = (ConversationId, String);

struct Entry {
    base:      String,
    last_used: Instant,
}

/// One user's frozen prefixes. Lives on `UserLoopRuntime`, so it spans every
/// turn of every conversation that user has open.
pub struct PrefixCache {
    ttl:     Duration,
    entries: Mutex<HashMap<Key, Entry>>,
}

impl PrefixCache {
    pub fn new() -> Self {
        Self::with_ttl(PREFIX_TTL)
    }

    /// A cache with a custom idle window — tests, and the knob a config key
    /// would turn if one is ever wanted.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self { ttl, entries: Mutex::new(HashMap::new()) }
    }

    /// The prefix for this turn, if one was built recently enough. Restarts the
    /// idle window on a hit.
    pub fn get(&self, key: &Key) -> Option<String> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(key)?;
        if entry.last_used.elapsed() >= self.ttl {
            entries.remove(key);
            return None;
        }
        entry.last_used = Instant::now();
        Some(entry.base.clone())
    }

    /// Stores a freshly built prefix, dropping whatever has gone idle — which is
    /// what keeps the map bounded without an eviction policy to remember. It is
    /// also what collects the one-shot conversations (system-agent passes,
    /// ephemeral turns) that would otherwise each leave an entry behind.
    ///
    /// Two rounds racing on the same key build twice and the last one wins. That
    /// is why the build happens *outside* this type: holding the lock across it
    /// would serialise every turn of every conversation behind one mutex, to
    /// save a duplicated string.
    pub fn put(&self, key: Key, base: String) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|_, e| e.last_used.elapsed() < self.ttl);
        entries.insert(key, Entry { base, last_used: Instant::now() });
    }

    /// Drops every frozen prefix, so the next round of every conversation
    /// rebuilds one.
    ///
    /// The single exception to "writes are deliberately not reacted to" above,
    /// and it is narrow on purpose. The rule holds for a file the prompt merely
    /// *injects*: the agent that edited it already has the new text two messages
    /// downstream, and a rebuild would repeat what it just said. It does not hold
    /// for the **skills index**, which is not content but a *catalogue*: an admin
    /// who installs a skill and immediately asks for it would be told for twenty
    /// minutes that it does not exist — the prefix is not stale, it is wrong.
    ///
    /// Coarse by design. A per-key flush would need to know which conversations
    /// carry an agent whose prompt includes the index, which is a question about
    /// eleven `AGENT.md` files; installing a skill is rare enough that rebuilding
    /// a handful of prefixes once is cheaper than keeping that answer correct.
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

impl Default for PrefixCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(conv: &str, agent: &str) -> Key {
        (ConversationId::new(conv), agent.to_string())
    }

    #[test]
    fn a_stored_prefix_is_served_back() {
        let cache = PrefixCache::new();
        cache.put(key("session:1", "assistant"), "PROMPT".into());
        assert_eq!(cache.get(&key("session:1", "assistant")).as_deref(), Some("PROMPT"));
    }

    #[test]
    fn an_idle_prefix_is_a_miss() {
        let cache = PrefixCache::with_ttl(Duration::from_millis(20));
        cache.put(key("session:1", "assistant"), "PROMPT".into());
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(cache.get(&key("session:1", "assistant")), None);
    }

    /// The whole point of the idle clock: a conversation that keeps talking
    /// keeps its prefix, however long it runs.
    #[test]
    fn using_a_prefix_restarts_the_idle_window() {
        let cache = PrefixCache::with_ttl(Duration::from_millis(60));
        cache.put(key("session:1", "assistant"), "PROMPT".into());
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(20));
            assert!(cache.get(&key("session:1", "assistant")).is_some());
        }
    }

    /// A sub-agent shares the conversation and must not be served its parent's
    /// prompt.
    #[test]
    fn the_agent_is_part_of_the_key() {
        let cache = PrefixCache::new();
        cache.put(key("session:1", "assistant"), "PARENT".into());
        cache.put(key("session:1", "researcher"), "CHILD".into());
        assert_eq!(cache.get(&key("session:1", "assistant")).as_deref(), Some("PARENT"));
        assert_eq!(cache.get(&key("session:1", "researcher")).as_deref(), Some("CHILD"));
    }

    #[test]
    fn storing_drops_the_entries_that_went_idle() {
        let cache = PrefixCache::with_ttl(Duration::from_millis(20));
        cache.put(key("session:1", "assistant"), "OLD".into());
        std::thread::sleep(Duration::from_millis(40));
        cache.put(key("session:2", "assistant"), "NEW".into());
        assert_eq!(cache.entries.lock().unwrap().len(), 1);
    }
}
