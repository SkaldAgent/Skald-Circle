//! Opaque id newtypes. The store contract requires `MessageId` and `ToolCallId`
//! to be **monotonically increasing per frame**: a concurrent fan-out allocates
//! ids in call order BEFORE execution, and the model reconstructs results by id.

use std::fmt;

/// Identifies a conversation (Skald: `"session:42"`; InMemory: any string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConversationId(pub String);

impl ConversationId {
    pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

impl From<&str> for ConversationId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}
impl From<String> for ConversationId {
    fn from(s: String) -> Self { Self(s) }
}

macro_rules! int_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub i64);

        impl $name {
            pub fn get(self) -> i64 { self.0 }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
        }

        impl From<i64> for $name {
            fn from(v: i64) -> Self { Self(v) }
        }
    };
}

int_id!(FrameId, "A conversation frame (root frame = the conversation; children = sub-agents).");
int_id!(MessageId, "A stored message. Monotonically increasing per frame.");
int_id!(ToolCallId, "A stored tool call. Monotonically increasing per frame.");
int_id!(TaskId, "An async delegated task.");
int_id!(SummaryId, "A compaction summary.");

/// Key of a model inside a `ModelSelector` ("kimi-k3", "claude-sonnet-4", …).
pub type ModelId = String;
