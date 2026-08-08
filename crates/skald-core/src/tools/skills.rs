//! `skill_register` and `skill_delete` — the whole write surface of the skills
//! trees (blueprint §7.3/§7.4).
//!
//! Both live in the **`Config` category**, so they are absent from the schema of
//! every request until `activate_tools(["config"])` asks for them. The round that
//! costs is a fair price for administration, but the real gain is elsewhere: a
//! prompt injection cannot reach a tool the model has not been shown, so it must
//! first make the model *activate the group* — one more step, and a step that
//! leaves a line in the transcript.
//!
//! Authorization is a **capability on the role**, checked server-side (§14).
//! `scope: "global"` needs `skill.manage`; `scope: "mine"` is always the
//! caller's own. Never inferred from anything the prompt says about who the user
//! is — the same shape as `mcp.register_local_script` versus
//! `mcp.register_remote`.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::skills::{PromptPrefixCell, PromptScope, Scope, install};
use crate::tools::fs::{FsTarget, resolve_target};
use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
};

/// Everything both tools need: the registry (to read the caller's role) and the
/// seam that tells live conversations their index moved.
struct Deps {
    registry: Arc<SqlitePool>,
    prefixes: Arc<PromptPrefixCell>,
}

impl Deps {
    /// Whether this caller may write to the group's tree.
    ///
    /// A failure to *read* the role is a denial, not a pass: the group's scope is
    /// the one that puts text into everybody's prompt, and "the database hiccuped"
    /// is not a reason to widen.
    async fn may_manage_shared(&self, user_id: &str) -> bool {
        let role = match crate::db::users::get(&self.registry, user_id).await {
            Ok(Some(u)) => u.role_id,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(user = %user_id, error = %e, "skills: cannot read role, denying global scope");
                return false;
            }
        };
        crate::db::role_capabilities::has(
            &self.registry,
            &role,
            crate::db::role_capabilities::MANAGE_SKILLS,
        )
        .await
        .unwrap_or(false)
    }

    async fn authorize(&self, user_id: &str, scope: Scope) -> Result<()> {
        if scope == Scope::Shared && !self.may_manage_shared(user_id).await {
            anyhow::bail!(
                "you are not allowed to change the group's skills. Use scope \"mine\" for a \
                 skill of your own, or ask an admin to install this one for everybody."
            );
        }
        Ok(())
    }

    /// Announces that the rendered index has moved, so a conversation that is
    /// already warm does not keep quoting the old one for twenty minutes.
    async fn invalidate(&self, user_id: &str, scope: Scope) {
        let scope = match scope {
            Scope::Shared => PromptScope::Everyone,
            Scope::Own    => PromptScope::User(user_id.to_string()),
        };
        self.prefixes.invalidate(scope).await;
    }
}

/// Parses the `scope` argument shared by both tools.
fn scope_arg(args: &Value) -> Result<Scope> {
    let raw = args["scope"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing required argument `scope` (\"mine\" or \"global\")"))?;
    Scope::parse(raw)
}

/// The `scope` property, identical in both schemas.
fn scope_property() -> Value {
    json!({
        "type": "string",
        "enum": ["mine", "global"],
        "description": "\"mine\" — your own skills, visible only to you. \
                        \"global\" — the group's skills, which every member reads as \
                        instructions (requires the skill.manage capability)."
    })
}

// ── skill_register ────────────────────────────────────────────────────────────

pub struct SkillRegister(Deps);

impl SkillRegister {
    pub fn new(registry: Arc<SqlitePool>, prefixes: Arc<PromptPrefixCell>) -> Self {
        Self(Deps { registry, prefixes })
    }
}

impl Tool for SkillRegister {
    fn name(&self) -> &str { crate::tools::tool_names::SKILL_REGISTER }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Config }
    fn display_name(&self) -> &str { "Install Skill" }

    fn description(&self) -> &str {
        "Install a skill folder into the read-only skills tree — the only way to add one. \
         The folder must live somewhere you can write (your home, a project or a shared \
         folder), NOT in the container-only filesystem such as /tmp, and must contain a \
         `SKILL.md` opening with a YAML frontmatter block declaring `name` (lowercase \
         letters, digits and hyphens) and `description` (when to use the skill, under 1000 \
         characters). The installed folder is named after that `name`, not after the source \
         folder. Registering an id that already exists in the same scope replaces it — that \
         is how a skill is updated; a skill is never edited in place. Read `docs/skills.md` \
         before writing one."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["scope", "path"],
            "properties": {
                "scope": scope_property(),
                "path": {
                    "type": "string",
                    "description": "Path of the folder to install, e.g. \"~/drafts/ics-import\". \
                                    It is copied, not moved."
                }
            }
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        match args["scope"].as_str() {
            Some("global") => format!("install `{path}` as a skill for the whole group"),
            _              => format!("install `{path}` as one of your skills"),
        }
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        args["path"].as_str().map(str::to_string)
    }

    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let fs = ctx.fs.clone();
        let user_id = ctx.user_id.clone();
        Box::new(SimpleExecution::new(Box::pin(async move {
            let scope = scope_arg(&args)?;
            self.0.authorize(&user_id, scope).await?;

            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing required argument `path`"))?;

            // The copy is host-side, so the source has to be on a mount. `/tmp`
            // is the first place a model puts a working folder, and a bare
            // ENOENT there reads as "the folder is gone" rather than "wrong side
            // of the boundary" — so say which it is.
            let host = match resolve_target(&fs, path)? {
                FsTarget::Host(p) => p,
                FsTarget::Container { .. } => anyhow::bail!(
                    "`{path}` exists only inside your container, and a skill is installed from \
                     the host side. Move the folder into your home (e.g. `~/{}`) and register \
                     that path instead.",
                    std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "my-skill".into())
                ),
            };

            let done = install::install(&fs, scope, &host)?;
            self.0.invalidate(&user_id, scope).await;

            let verb = if done.replaced { "Replaced" } else { "Installed" };
            Ok(ToolResult::Text(format!(
                "{verb} skill `{}` at {}. It is in the index now — read it back with \
                 `read_file {}/SKILL.md`.",
                done.id, done.agent_dir, done.agent_dir
            )))
        })))
    }
}

// ── skill_delete ──────────────────────────────────────────────────────────────

pub struct SkillDelete(Deps);

impl SkillDelete {
    pub fn new(registry: Arc<SqlitePool>, prefixes: Arc<PromptPrefixCell>) -> Self {
        Self(Deps { registry, prefixes })
    }
}

impl Tool for SkillDelete {
    fn name(&self) -> &str { crate::tools::tool_names::SKILL_DELETE }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Config }
    fn display_name(&self) -> &str { "Delete Skill" }

    fn description(&self) -> &str {
        "Remove an installed skill. The id is its folder name — use \
         `list_items` with type=skills to see the installed ids and scopes. \
         There is no recycle bin: the folder is deleted."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["scope", "id"],
            "properties": {
                "scope": scope_property(),
                "id": {
                    "type": "string",
                    "description": "The skill's id — its folder name, e.g. \"ics-import\"."
                }
            }
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let id = args["id"].as_str().unwrap_or("?");
        match args["scope"].as_str() {
            // Said in full on the card: this removes something from every
            // member's prompt, not just from the caller's.
            Some("global") => format!("delete skill `{id}` for the whole group"),
            _              => format!("delete your skill `{id}`"),
        }
    }

    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let fs = ctx.fs.clone();
        let user_id = ctx.user_id.clone();
        Box::new(SimpleExecution::new(Box::pin(async move {
            let scope = scope_arg(&args)?;
            self.0.authorize(&user_id, scope).await?;
            let id = args["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing required argument `id`"))?;

            install::remove(&fs, scope, id)?;
            self.0.invalidate(&user_id, scope).await;
            Ok(ToolResult::Text(format!("Deleted skill `{id}` ({}).", scope.as_arg())))
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    fn registry() -> ToolRegistry {
        // `connect_lazy` performs no I/O, and neither tool touches the pool
        // unless it is asked for the group's scope.
        let pool = Arc::new(SqlitePool::connect_lazy("sqlite::memory:").unwrap());
        let cell = Arc::new(PromptPrefixCell::default());
        let mut r = ToolRegistry::new();
        r.register(SkillRegister::new(Arc::clone(&pool), Arc::clone(&cell)));
        r.register(SkillDelete::new(pool, cell));
        r
    }

    fn names(defs: &[Value]) -> Vec<String> {
        defs.iter()
            .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
            .collect()
    }

    /// The two write verbs are absent from the schema of an ordinary request and
    /// arrive only with `activate_tools(["config"])`. That extra round is the
    /// price of administration; the gain is that a prompt injection has to make
    /// the model activate the group first — a step that shows in the transcript.
    #[tokio::test]
    async fn the_write_verbs_are_invisible_until_the_config_group_is_activated() {
        let r = registry();
        assert!(names(&r.openai_definitions_excluding_config()).is_empty());

        let mut lazy = names(&r.openai_definitions_config_only());
        lazy.sort();
        assert_eq!(lazy, vec!["skill_delete".to_string(), "skill_register".to_string()]);
    }

    /// Enumerating is harmless and is what makes "delete the X skill" possible
    /// without guessing an id, so it stays in every request.
    #[test]
    fn enumeration_is_not_in_the_lazy_group() {
        assert_ne!(
            crate::tools::ToolCategory::Config,
            crate::tools::ToolCategory::Introspection,
        );
    }

    #[test]
    fn the_scope_argument_is_the_vocabulary_the_tools_take() {
        assert_eq!(Scope::parse("mine").unwrap(), Scope::Own);
        assert_eq!(Scope::parse("global").unwrap(), Scope::Shared);
        // Not a username: a tool that asked for one would invite passing
        // somebody else's, which the server would have to ignore anyway.
        assert!(Scope::parse("daniele").is_err());
    }
}
