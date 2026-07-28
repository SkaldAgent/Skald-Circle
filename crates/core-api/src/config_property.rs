use serde::{Deserialize, Serialize};

/// How a config property is rendered and edited in the Config UI.
///
/// Beyond the plain scalars (`String`/`Int`/`Bool`, rendered as text/number/
/// switch), a variant can stand for a **custom, higher-level control** whose
/// allowed values are computed by the backend rather than typed by hand —
/// `SecurityGroup` and `Locale` are both of this kind: they turn into a
/// dropdown fed by a server-supplied `options` list.
///
/// **Adding your own is cheap and encouraged.** If a new config section would
/// otherwise expose a free-text field where only a fixed/derived set of values
/// is valid, prefer adding a variant here instead. The wiring is three small,
/// symmetric edits:
///   1. add the variant below;
///   2. in `frontend/api/config.rs`, map it to a type string and (if it's a
///      dropdown) build its `options: Vec<SelectOption>`;
///   3. in `web/components/config-page.js`, add a render branch for that type.
/// Anything carrying `options` renders as a `<select>` — see `_renderInput`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PropertyType {
    String,
    Int,
    Bool,
    /// Dropdown of the instance's security groups (run-context groups).
    SecurityGroup,
    /// Dropdown of the interface languages the instance supports.
    Locale,
    /// Dropdown of the LLM models configured on the instance (by model name,
    /// the resolution key). Nullable: empty means "auto-select".
    LlmModel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigProperty {
    pub key:           String,
    pub name:          String,
    pub description:   String,
    pub property_type: PropertyType,
    /// Value used when the key is absent from the DB config table.
    pub default_value: Option<String>,
}

/// A named group of related [`ConfigProperty`] items, shown as a distinct
/// section of whichever page owns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSet {
    pub name:        String,
    pub description: String,
    pub properties:  Vec<ConfigProperty>,
    /// Who this set belongs to, and therefore **where it is edited**.
    ///
    /// `None` is the general Config page. `Some(id)` hands the set to the
    /// surface that owns `id` — today the System agents page, which shows an
    /// agent's settings next to that same agent's run history, because "why did
    /// it not run" is half a config question and half a log question.
    ///
    /// Placement is deliberately **data on the set** rather than a filter that
    /// knows set names: a page selects by owner, so a new owned set lands in the
    /// right place without touching either page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner:       Option<String>,
}

impl ConfigSet {
    /// Hand this set to the surface that owns `owner` (see [`ConfigSet::owner`]).
    pub fn owned_by(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }
}
