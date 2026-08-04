//! The `list_items(type="mcp")` report: everything an agent needs to know about
//! this caller's connectors, in one call.
//!
//! Three sources answer three different questions and none of them is redundant:
//! the **registry** says what exists and who may have it, the caller's **owner
//! database** says what they activated, and the **live runtimes** say what is
//! actually connected right now (a row can read `ready` while its process is
//! dead, and a per-user server only appears once its container started it).
//!
//! The report is deliberately verbose. It is a tool *result*, so it appends to
//! the context rather than rewriting the system prefix — unlike `__MCP_LIST__`,
//! which is frozen per conversation for prompt-cache stability (see
//! `loop_adapters::prefix_cache`) and therefore stays a bare table. Cheap and
//! detailed here, stable and minimal there.
//!
//! **Read-only, and that is structural.** Everything below is a `SELECT`.
//! Enabling, activating or configuring a connector is not agent-reachable
//! (blueprint §14 — the reason the old `register_mcp` tool was deleted), so the
//! report's job when something is unusable is to name the human step, never to
//! offer a tool that performs it.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use core_api::tool::McpDirectory;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::db::{
    mcp_catalog, mcp_catalog_access, mcp_global_access, mcp_global_servers, mcp_user_servers,
    role_capabilities, users,
};

/// Static orientation, identical for every caller. Says the three things that
/// are actually mis-modelled by LLMs: connectors are called something else by
/// users, their tools are **not** in the tool set until loaded, and no tool
/// enables them.
const HOW_THIS_WORKS: &str = "\
MCP servers are shown to users as \"Connectors\". They are curated by the administrator and \
activated per user from the Connectors page in the web UI. There is no tool that enables, \
disables, activates or configures a connector — when one is not usable, tell the user what to \
do in the UI instead of looking for a tool.

A connector's tools are NOT in your tool set until you load them: call activate_tools([\"<id>\"]) \
with ids from `ready_to_load`, then call its tools as mcp__<id>__<tool>. The activation lasts the \
whole session and survives a restart, so never call activate_tools twice for the same id. \
Connectors in `loaded_now` are already loaded — call their tools directly.

For how connectors work in user-facing terms, read docs/index.md.";

const NOTE_PER_USER: &str = "Per-user connector: it runs in your own container and is bound to \
    your own account only, never another user's.";
const NOTE_GLOBAL: &str = "Shared connector: it runs on the server under credentials owned by the \
    administrator, and is not tied to your account.";

/// One connector's row in the report, in whichever bucket it lands.
struct Entry {
    id:          String,
    name:        String,
    description: Option<String>,
    scope:       &'static str,
    state:       &'static str,
    tools:       Vec<String>,
    note:        String,
    next_step:   Option<String>,
}

impl Entry {
    fn to_json(&self) -> Value {
        json!({
            "id":          self.id,
            "name":        self.name,
            "description": self.description,
            "scope":       self.scope,
            "state":       self.state,
            "tools":       self.tools,
            "note":        self.note,
            "next_step":   self.next_step,
        })
    }
}

/// Build the report. Every lookup degrades rather than fails: a caller whose
/// role cannot be read is reported as a non-manager, which is the narrow
/// reading, and a missing live view leaves the durable picture intact.
pub async fn build(
    registry:   &SqlitePool,
    owner:      &SqlitePool,
    user_id:    &str,
    session_id: i64,
    live:       Option<&dyn McpDirectory>,
) -> Result<Value> {
    let role_id = users::get(registry, user_id).await?.map(|u| u.role_id);
    let can_manage = match &role_id {
        Some(r) => role_capabilities::has(registry, r, role_capabilities::MANAGE_CATALOG).await?,
        None    => false,
    };

    // What the live runtimes report, by runtime name.
    let connected: HashMap<String, Vec<String>> = live
        .map(|l| l.connected().into_iter().map(|s| (s.name, s.tools)).collect())
        .unwrap_or_default();

    // Session-scoped activations. `activated_tools` also holds the reserved
    // `config` group, which simply never matches a server name — so intersecting
    // with the connector set is enough and no kind filter is needed. Sub-agent
    // frame activations are not visible here (a `ToolContext` carries no stack
    // id); under-reporting is the safe direction, since a redundant
    // `activate_tools` is idempotent while a missed one is an unknown-tool error.
    let loaded: HashSet<String> =
        crate::db::activated_tools::list_refs_session(owner, session_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

    let catalog_by_name: HashMap<String, mcp_catalog::McpCatalogRow> =
        mcp_catalog::list(registry).await?
            .into_iter()
            .map(|r| (r.name.clone(), r))
            .collect();

    let mut loaded_now  = Vec::new();
    let mut ready       = Vec::new();
    let mut needs_setup = Vec::new();
    let mut installable = Vec::new();

    // ── Per-user activations (the owner's own rows) ──────────────────────────
    let user_rows = mcp_user_servers::all(owner).await?;
    let activated_names: HashSet<String> =
        user_rows.iter().map(|r| r.name.clone()).collect();
    let activated_catalog: HashSet<String> =
        user_rows.iter().filter_map(|r| r.catalog_name.clone()).collect();

    for row in &user_rows {
        let cat = row.catalog_name.as_deref().and_then(|n| catalog_by_name.get(n));
        let mut e = Entry {
            id:          row.name.clone(),
            name:        cat.and_then(|c| c.friendly_name.clone()).unwrap_or_else(|| row.name.clone()),
            description: cat.and_then(|c| c.description.clone()),
            scope:       "per_user",
            state:       "",
            tools:       Vec::new(),
            note:        NOTE_PER_USER.into(),
            next_step:   None,
        };

        if !row.enabled {
            e.state = "disabled";
            e.note = format!("{NOTE_PER_USER} It is currently deactivated.");
            e.next_step = Some(ui_step(&e.name, "re-activate it"));
            needs_setup.push(e);
        } else if row.auth_state == "pending" {
            // The kind of pending is the difference between "paste a code" and
            // "scan a QR" (blueprint §15) — both human, but not the same human step.
            let kind = cat.map(|c| c.auth_kind.as_str()).unwrap_or("none");
            let (state, what) = match kind {
                "oauth" => ("pending_oauth", "finish signing in (the sign-in was never completed, so no token is stored)"),
                "qr"    => ("pending_login", "finish the device login by scanning the QR code"),
                _       => ("pending_setup", "finish setting it up"),
            };
            e.state = state;
            e.next_step = Some(ui_step(&e.name, what));
            needs_setup.push(e);
        } else if let Some(tools) = connected.get(&row.name) {
            e.tools = tools.clone();
            if loaded.contains(&row.name) {
                e.state = "loaded";
                loaded_now.push(e);
            } else {
                e.state = "ready";
                e.next_step = Some(activate_step(&row.name));
                ready.push(e);
            }
        } else {
            e.state = "not_running";
            e.note = format!(
                "{NOTE_PER_USER} It is activated and configured, but its process is not running \
                 right now, so its tools cannot be loaded."
            );
            e.next_step = Some(ui_step(&e.name, "check it — signing out and back in usually restarts it"));
            needs_setup.push(e);
        }
    }

    // ── Global connectors ────────────────────────────────────────────────────
    let granted_globals: HashSet<String> =
        mcp_global_access::server_names_for_user(registry, user_id).await?
            .into_iter()
            .collect();

    for row in mcp_global_servers::all(registry).await? {
        let granted = granted_globals.contains(&row.name);
        let mut e = Entry {
            id:          row.name.clone(),
            name:        row.friendly_name.clone().unwrap_or_else(|| row.name.clone()),
            description: row.description.clone(),
            scope:       "global",
            state:       "",
            tools:       Vec::new(),
            note:        NOTE_GLOBAL.into(),
            next_step:   None,
        };

        if !granted {
            // A catalog manager needs to see a connector they have not granted
            // themselves, or it is invisible and they cannot reason about it.
            // Everyone else must not learn it exists — that is the grant.
            if can_manage {
                e.state = "not_granted";
                e.note = format!("{NOTE_GLOBAL} It is enabled on this instance but not granted to you.");
                e.next_step = Some(
                    "You manage the catalog: grant it to yourself from the Connectors page in the web UI.".into(),
                );
                installable.push(e);
            }
            continue;
        }

        if !row.enabled {
            e.state = "disabled";
            e.note = format!("{NOTE_GLOBAL} It is currently disabled on this instance.");
            e.next_step = Some(admin_step(&e.name, "re-enable it"));
            needs_setup.push(e);
        } else if let Some(tools) = connected.get(&row.name) {
            e.tools = tools.clone();
            if loaded.contains(&row.name) {
                e.state = "loaded";
                loaded_now.push(e);
            } else {
                e.state = "ready";
                e.next_step = Some(activate_step(&row.name));
                ready.push(e);
            }
        } else {
            e.state = "not_running";
            e.note = format!(
                "{NOTE_GLOBAL} It is enabled but not connected right now, so its tools cannot be loaded."
            );
            e.next_step = Some(admin_step(&e.name, "check why it is not connected"));
            needs_setup.push(e);
        }
    }

    // ── Catalog entries the caller could still activate ──────────────────────
    let granted_catalog: HashSet<String> =
        mcp_catalog_access::catalog_names_for_user(registry, user_id).await?
            .into_iter()
            .collect();

    for row in mcp_catalog::list_for_scope(registry, "per_user").await? {
        if !(can_manage || granted_catalog.contains(&row.name)) {
            continue;
        }
        if activated_catalog.contains(&row.name) || activated_names.contains(&row.name) {
            continue;
        }
        let what = match row.auth_kind.as_str() {
            "oauth" => "activate it and sign in",
            "qr"    => "activate it and complete the device login",
            _       => "activate it",
        };
        installable.push(Entry {
            id:          row.name.clone(),
            name:        row.friendly_name.clone().unwrap_or_else(|| row.name.clone()),
            description: row.description.clone(),
            scope:       "per_user",
            state:       "not_activated",
            tools:       Vec::new(),
            note:        format!("{NOTE_PER_USER} It is available to you but has never been activated."),
            next_step:   Some(ui_step(&row.friendly_name.unwrap_or(row.name), what)),
        });
    }

    let guidance = if can_manage {
        "You manage the connector catalog: you can add, remove and grant connectors yourself, \
         from the Connectors page in the web UI. You still cannot do it from a tool."
    } else {
        "You cannot add or configure connectors. If you need one that is not listed here, tell \
         the user to ask an administrator to grant it."
    };

    Ok(json!({
        "how_this_works": HOW_THIS_WORKS,
        "your_role": {
            "role_id":            role_id,
            "can_manage_catalog": can_manage,
            "guidance":           guidance,
        },
        "loaded_now":  loaded_now.iter().map(Entry::to_json).collect::<Vec<_>>(),
        "ready_to_load": ready.iter().map(Entry::to_json).collect::<Vec<_>>(),
        "needs_setup": needs_setup.iter().map(Entry::to_json).collect::<Vec<_>>(),
        "installable": installable.iter().map(Entry::to_json).collect::<Vec<_>>(),
    }))
}

fn activate_step(id: &str) -> String {
    format!("Call activate_tools([\"{id}\"]) to load its tools, then call them as mcp__{id}__<tool>.")
}

/// A step only the user can take, named as such — the model must relay it, not
/// attempt it.
fn ui_step(name: &str, what: &str) -> String {
    format!("Tell the user to open the Connectors page in the web UI, select \"{name}\" and {what}. \
             You cannot do this for them.")
}

fn admin_step(name: &str, what: &str) -> String {
    format!("Tell the user that an administrator must open the Connectors page and {what} for \
             \"{name}\". You cannot do this for them.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_api::tool::McpServerView;

    /// Stands in for the live runtimes: whatever is listed here is "connected".
    struct FakeLive(Vec<&'static str>);

    impl McpDirectory for FakeLive {
        fn connected(&self) -> Vec<McpServerView> {
            self.0.iter()
                .map(|n| McpServerView {
                    name:        (*n).into(),
                    description: None,
                    tools:       vec![format!("{n}_do")],
                })
                .collect()
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("skald-mcpreport-{tag}-{}-{nanos}", std::process::id()));
        p
    }

    /// Only `admin` is seeded with the schema; the ordinary roles come from a
    /// setup profile, so a test that wants a non-manager creates one.
    async fn seed_member(registry: &SqlitePool) {
        sqlx::query("INSERT INTO roles (id, label, permission_group) VALUES ('member', 'Member', 'default')")
            .execute(registry).await.unwrap();
    }

    fn ids(bucket: &Value) -> Vec<String> {
        bucket.as_array().unwrap().iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn state_of(bucket: &Value, id: &str) -> String {
        bucket.as_array().unwrap().iter()
            .find(|e| e["id"] == id)
            .unwrap_or_else(|| panic!("`{id}` not in bucket"))["state"]
            .as_str().unwrap().to_string()
    }

    /// The four buckets are the whole contract: a connector must land in exactly
    /// one, and the one it lands in is what tells the model whether to call
    /// `activate_tools`, to call the tool directly, or to send the user to the UI.
    #[tokio::test]
    async fn buckets_split_by_activation_and_liveness() {
        let dir = temp_dir("buckets");
        std::fs::create_dir_all(&dir).unwrap();
        let registry = crate::db::init_system_pool(dir.join("system.db").to_str().unwrap())
            .await.unwrap();
        let owner = crate::db::create_user_pool(&dir.join("u1.db"), None).await.unwrap();

        let r = |q: &'static str| sqlx::query(q).execute(&registry);
        let o = |q: &'static str| sqlx::query(q).execute(&owner);

        seed_member(&registry).await;
        r("INSERT INTO users (id, username, role_id, encrypted) VALUES ('u1', 'u1', 'member', 0)")
            .await.unwrap();

        // Catalogue: three per-user entries granted, one deliberately not.
        for (name, auth) in [("gmail", "oauth"), ("whatsapp", "qr"), ("gcal", "oauth"), ("hidden", "none")] {
            sqlx::query("INSERT INTO mcp_catalog (name, scope, source, auth_kind) VALUES (?, 'per_user', 'local_script', ?)")
                .bind(name).bind(auth).execute(&registry).await.unwrap();
        }
        for name in ["gmail", "whatsapp", "gcal"] {
            sqlx::query("INSERT INTO mcp_catalog_access (catalog_name, user_id) VALUES (?, 'u1')")
                .bind(name).execute(&registry).await.unwrap();
        }

        // One global, enabled and granted.
        r("INSERT INTO mcp_global_servers (id, name, enabled) VALUES (1, 'tavily', 1)").await.unwrap();
        r("INSERT INTO mcp_global_access (server_id, user_id) VALUES (1, 'u1')").await.unwrap();

        // Owner side: gmail activated and signed in, whatsapp still pairing.
        o("INSERT INTO mcp_user_servers (name, catalog_name, source, auth_state) \
           VALUES ('gmail', 'gmail', 'local_script', 'ready')").await.unwrap();
        o("INSERT INTO mcp_user_servers (name, catalog_name, source, auth_state) \
           VALUES ('whatsapp', 'whatsapp', 'local_script', 'pending')").await.unwrap();

        // gmail was already loaded into this session; tavily was not.
        o("INSERT INTO chat_sessions (id, title) VALUES (1, 't')").await.unwrap();
        o("INSERT INTO chat_sessions_stack (id, session_id) VALUES (1, 1)").await.unwrap();
        o("INSERT INTO chat_history (id, session_stack_id, role, content) VALUES (1, 1, 'user', 'hi')")
            .await.unwrap();
        o("INSERT INTO activated_tools (session_id, stack_id, message_id, kind, ref) \
           VALUES (1, NULL, 1, 'mcp', 'gmail')").await.unwrap();

        let live = FakeLive(vec!["gmail", "tavily"]);
        let out = build(&registry, &owner, "u1", 1, Some(&live)).await.unwrap();

        assert_eq!(ids(&out["loaded_now"]), ["gmail"]);
        assert_eq!(out["loaded_now"][0]["tools"][0], "gmail_do");
        // Already loaded ⇒ no next step, or the model activates it a second time.
        assert!(out["loaded_now"][0]["next_step"].is_null());

        assert_eq!(ids(&out["ready_to_load"]), ["tavily"]);
        assert!(out["ready_to_load"][0]["next_step"].as_str().unwrap().contains("activate_tools"));

        // The pending kind is the difference between pasting a code and scanning
        // a QR — both human steps, but not the same one.
        assert_eq!(state_of(&out["needs_setup"], "whatsapp"), "pending_login");

        assert_eq!(ids(&out["installable"]), ["gcal"]);
        assert_eq!(out["your_role"]["can_manage_catalog"], false);
    }

    /// Deny-by-default survives the report: an ungranted catalogue entry must not
    /// even be named, or the listing becomes a directory of what to ask for.
    #[tokio::test]
    async fn ungranted_catalog_entries_stay_invisible() {
        let dir = temp_dir("deny");
        std::fs::create_dir_all(&dir).unwrap();
        let registry = crate::db::init_system_pool(dir.join("system.db").to_str().unwrap())
            .await.unwrap();
        let owner = crate::db::create_user_pool(&dir.join("u1.db"), None).await.unwrap();

        seed_member(&registry).await;
        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('u1', 'u1', 'member', 0)")
            .execute(&registry).await.unwrap();
        sqlx::query("INSERT INTO mcp_catalog (name, scope, source, auth_kind) VALUES ('hidden', 'per_user', 'local_script', 'none')")
            .execute(&registry).await.unwrap();
        sqlx::query("INSERT INTO mcp_global_servers (id, name, enabled) VALUES (1, 'secret', 1)")
            .execute(&registry).await.unwrap();

        let out = build(&registry, &owner, "u1", 1, None).await.unwrap();

        let rendered = out.to_string();
        assert!(!rendered.contains("hidden"), "ungranted catalog entry leaked: {rendered}");
        assert!(!rendered.contains("secret"), "ungranted global leaked: {rendered}");
    }

    /// A catalogue manager must see what they have not granted themselves, or
    /// they cannot reason about the instance they administer.
    #[tokio::test]
    async fn a_catalog_manager_sees_ungranted_globals() {
        let dir = temp_dir("admin");
        std::fs::create_dir_all(&dir).unwrap();
        let registry = crate::db::init_system_pool(dir.join("system.db").to_str().unwrap())
            .await.unwrap();
        let owner = crate::db::create_user_pool(&dir.join("a1.db"), None).await.unwrap();

        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('a1', 'a1', 'admin', 0)")
            .execute(&registry).await.unwrap();
        sqlx::query("INSERT INTO mcp_global_servers (id, name, enabled) VALUES (1, 'tavily', 1)")
            .execute(&registry).await.unwrap();

        let out = build(&registry, &owner, "a1", 1, None).await.unwrap();

        assert_eq!(out["your_role"]["can_manage_catalog"], true);
        assert_eq!(ids(&out["installable"]), ["tavily"]);
        assert_eq!(state_of(&out["installable"], "tavily"), "not_granted");
    }

    /// A live view is an optimisation for freshness, never a precondition: with
    /// the runtimes unreachable the durable picture must still be reported.
    #[tokio::test]
    async fn a_ready_connector_without_a_live_view_is_not_running() {
        let dir = temp_dir("nolive");
        std::fs::create_dir_all(&dir).unwrap();
        let registry = crate::db::init_system_pool(dir.join("system.db").to_str().unwrap())
            .await.unwrap();
        let owner = crate::db::create_user_pool(&dir.join("u1.db"), None).await.unwrap();

        seed_member(&registry).await;
        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('u1', 'u1', 'member', 0)")
            .execute(&registry).await.unwrap();
        sqlx::query("INSERT INTO mcp_user_servers (name, source, auth_state) VALUES ('gmail', 'local_script', 'ready')")
            .execute(&owner).await.unwrap();

        let out = build(&registry, &owner, "u1", 1, None).await.unwrap();

        assert_eq!(state_of(&out["needs_setup"], "gmail"), "not_running");
        assert!(ids(&out["ready_to_load"]).is_empty());
    }
}
