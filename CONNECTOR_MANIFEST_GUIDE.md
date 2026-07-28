# Skald Connector Authoring Guide

Instructions for generating a **correct connector** for the Skald marketplace
(`https://connectors.skaldagent.net`). Give this file to the agent that produces
new connectors.

A connector is a folder served by the marketplace. Skald installs it, verifies
every file against a SHA-256 pinned in the index, then either runs it on the host
(global connector) or copies it into the user's container and runs it there
(per-user connector, blueprint §6/§7).

---

## 1. The two documents

### 1a. The root index — `connectors.json`

One array of entries, each pointing at a connector folder. **The index is the
signable root: it is the only place that lists a connector's files and their
SHA-256 digests.** Skald refuses any file whose bytes do not match.

```jsonc
{
  "version": 1,
  "connectors": [
    {
      "id": "whatsapp",                 // unique slug = folder name
      "name": "WhatsApp",
      "version": 1,                     // INTEGER build number — the update key (§7)
      "version_string": "2.0.1",        // semver, display only
      "version_release_date": "2026-07-19", // ISO date, display only
      "type": "mcp_local",              // mcp_local | mcp_remote  (see §3)
      "scope": "user",                  // user | global           (see §3)
      "icon_small": "whatsapp/icon_sm.svg",
      "icon_large": "whatsapp/icon_lg.svg",
      "user_description": "Send and read WhatsApp messages from your linked account.",
      "requires": ["NODE"],             // human hint: NODE | PYTHON | OAUTH | API_KEY
      "tags": ["messaging", "mcp", "local", "whatsapp", "qr"],
      "auth": { "type": "qr" },         // may be repeated here and in the manifest
      "folder": "whatsapp",             // defaults to id
      "files": [
        { "path": "index.js",        "sha256": "…", "size": 21258 },
        { "path": "package.json",    "sha256": "…", "size": 302   },
        { "path": "connector.json",  "sha256": "…", "size": 620   },
        { "path": "icon_sm.svg",     "sha256": "…", "size": 306   },
        { "path": "icon_lg.svg",     "sha256": "…", "size": 308   }
      ]
    }
  ]
}
```

**Rules**

- `files[].path` is relative to the connector folder. List **every** file the
  connector ships (server code, `package.json`/`requirements.txt`, icons, and the
  `connector.json` itself). A missing or mismatched digest fails the install.
- Compute `sha256` over the exact bytes served: `sha256sum <file>`.
- Do **not** list `node_modules/` or any generated deps — those are installed on
  the box, not shipped (see §5).
- `size` is optional but recommended.

### 1b. The per-connector manifest — `<folder>/connector.json`

The richer document. Fetched per connector and mapped into Skald's catalog.

```jsonc
{
  "id": "whatsapp",
  "name": "WhatsApp",
  "version": 1,                         // INTEGER build number — the update key (§7)
  "version_string": "2.0.1",            // semver, display only
  "version_release_date": "2026-07-19", // ISO date, display only
  "type": "mcp_local",
  "scope": "user",
  "auth": { "type": "qr" },             // none | api_key | oauth2 | qr  (see §4)
  "mcp_config": {
    "command": "node",                  // interpreter (local) …
    "args": ["index.js"],               // … args[0] MUST name the entry file
    "transport": "stdio"                // stdio (local) | streamable-http (remote)
  },
  "docs": [{
    "lang": "en",
    "description": "Human blurb shown in the UI.",
    "llm_short_description": "One line the model reads to decide whether to use this connector."
  }],
  "env": [],                            // form fields the user fills (see §4b)
  "tools": [                            // OPTIONAL — friendly UI names per tool (§2a)
    { "name": "send_message", "display_name": "Send Message" }
  ],
  "homepage": "https://…",
  "icon_small": "icon_sm.svg",          // relative to the folder here
  "icon_large": "icon_lg.svg",
  "tags": ["messaging", "mcp", "local", "whatsapp", "qr"]
}
```

**`mcp_config.args[0]` is load-bearing for a local connector:** it is how Skald
learns which file to run. At activation Skald rewrites it to the file's path
inside the user's container (`/root/.skald/mcp/<name>/<entry>`), so keep it a
plain relative filename (`index.js`, `server.py`, `pkg/server.py`).

---

## 2. Server contract (MCP over stdio)

A **local** connector is a program speaking JSON-RPC 2.0 over stdin/stdout. It
MUST handle:

- `initialize` → `{ protocolVersion, capabilities: { tools: {} }, serverInfo }`
- `notifications/initialized` → no response
- `tools/list` → `{ tools: [ { name, description, inputSchema } ] }`
- `tools/call` → `{ content: [ { type: "text", text } ], isError? }`

**stdout is reserved for JSON-RPC only.** Send all logs/diagnostics to **stderr**.
Anything a library prints to stdout (a logger, a banner) corrupts the protocol —
silence it (e.g. Baileys/pino → a silent logger; Python → `print(…, file=sys.stderr)`).

A **remote** connector is an HTTP MCP endpoint (`mcp_config.url` +
`transport: "streamable-http"`); no code runs on the box.

### 2a. Friendly tool names (`tools[]`) — optional

Raw MCP tool names are ugly in the chat UI (`search_files`, `send_message`). The
optional top-level `tools[]` block gives each one a human title shown as the tool
card's heading:

```jsonc
"tools": [
  { "name": "send_message",   "display_name": "Send Message" },
  { "name": "list_chats",     "display_name": "List Chats" },
  { "name": "download_media", "display_name": "Download Media" }
]
```

- `name` — the **raw** tool name exactly as your server returns it from `tools/list`.
- `display_name` — the friendly card title (English only; not internationalized).

**Resolution order** for a tool's card title is **`tools[].display_name` → the MCP
`title` field → a prettified raw name**. So you have two ways to set a friendly
name, and can skip `tools[]` entirely:

1. **This block** — the authoritative override, curated in the manifest.
2. **The MCP `title` field** — if your `tools/list` entries already carry a
   `title` (MCP 2025-06-18+), Skald uses it automatically; no manifest change
   needed. `tools[]` wins if both are present.
3. If neither is set, Skald title-cases the raw name (`send_message` → "Send
   Message").

**Icons are per connector, not per tool.** Every tool of a connector shows that
connector's own `icon_small`; there is no per-tool icon field. Only list a tool in
`tools[]` when its prettified name isn't good enough — partial lists are fine
(unlisted tools fall through to steps 2–3).

---

## 3. Placement & risk vocabulary (what the words mean)

| Manifest | Meaning |
| --- | --- |
| `scope: "user"` | runs **once per user**, inside their container. Personal creds. |
| `scope: "global"` | runs **once for the household**, on the host. Shared, stateless. Admin enables it with a key. |
| `type: "mcp_local"` | ships code that will **execute on the box** — installing needs the admin `mcp.register_local_script` capability (RCE-bearing act, §14). |
| `type: "mcp_remote"` | just an HTTP URL; no local code. |

Pick the narrowest: a personal messaging/email/calendar connector is
`scope: "user"`; a shared search API is `scope: "global"`.

---

## 4. Authentication (`auth.type`)

| `auth.type` | Flow | Ships |
| --- | --- | --- |
| `none` | nothing to sign in | — |
| `api_key` | user pastes a key/secret into a form | an `env[]` schema (§4b) |
| `oauth2` | browser consent → paste code back | `auth.provider` + `auth.scopes` + `auth.deliver` (§4c) |
| `qr` | server shows a QR, user scans with a phone | a `login_status` tool (§4d) |

### 4b. `api_key` — the `env[]` schema

Each entry drives one form field **and** is injected as an env var / URL token to
the server:

```jsonc
"env": [{
  "name": "tavilyApiKey",
  "label": "Tavily API key",
  "description": "Create one at https://app.tavily.com.",
  "required": true,
  "secret": true,                       // rendered masked, stored encrypted
  "example": "tvly-xxxxxxxx"
}]
```

The server reads each value from `process.env.<name>` (or `os.environ`). For a
**remote** connector that wants the key in the URL, use a placeholder:
`"url": "https://mcp.example.com/?key={SECRET:tavilyApiKey}"`.

### 4c. `oauth2` — provider consent

```jsonc
"auth": {
  "type": "oauth2",
  "provider": "google",                 // slug into the admin's sign-in providers
  "scopes": ["https://www.googleapis.com/auth/gmail.modify"],
  "deliver": { "as": "env", "format": "google_authorized_user", "env": "GMAIL_CREDS_JSON" }
}
```

The manifest names **only** the provider slug, scopes, and how the obtained token
is delivered — never client secrets or endpoint URLs (those are admin-entered,
kept off the public feed). Skald handles PKCE + code exchange and injects the
credential as the named env var. `format`: `google_authorized_user` (Google) or
`refresh_token`. Today only `as: "env"` is wired.

### 4d. `qr` / interactive device login — the generic contract

For a connector whose credential is produced by **scanning/pairing** (WhatsApp
today), there is no code to paste. The rule:

> **Expose one extra tool, `login_status`, returning a JSON object** (as the
> `text` of a normal text result). Skald calls it directly (never the agent) and a
> login panel polls it.

```jsonc
// login_status result text (a JSON string):
{
  "state": "connecting" | "need_scan" | "ready" | "logged_out",
  "qr": "data:image/png;base64,…",   // present ONLY while state == need_scan
  "message": "human-readable line"
}
```

- `activate` on a `qr` connector inserts a **pending** row and **starts the
  server** (so it can produce the QR), then hands off to the login panel.
- The panel polls `POST /api/mcp/login/status`; when `state == "ready"` the
  connector is marked ready and starts automatically on later logins.
- Also expose a `logout` tool (clears the session, forces a fresh QR) — the panel
  calls it via `POST /api/mcp/login/reset` to re-link a different phone.
- The **credential is the on-disk session**, not a token. Persist it **inside the
  connector's own directory** (e.g. `./auth/` next to the entry file). That folder
  lives under the bind-mounted home, so it survives container recreates and
  connector updates. Never store it under a shared/global path.

Skald resolves `auth.type: "qr"` the same way whether it appears in the index
entry or the manifest.

---

## 5. Dependencies (node & python) — how they get installed

**Do not ship `node_modules/` or vendored wheels.** Declare deps as a standard
manifest **file** and Skald installs them inside the container:

- **node:** ship a `package.json` with a `dependencies` map. Skald runs
  `npm ci --omit=dev` (falling back to `npm install --omit=dev`) in the connector
  dir. `node_modules/` resolves automatically beside the entry file.
- **python:** ship a `requirements.txt`. Skald installs it with
  `pip install --target .pydeps` and puts `.pydeps` on the server's `PYTHONPATH`.

This runs at activation **and** on every startup, guarded by a **content hash** of
the connector's source files:

- first activation / a brand-new container → full install,
- a connector **update** (any shipped file changed) → re-copy + re-install,
- unchanged → skipped in microseconds.

So you never write install steps into the manifest — just ship the dep file, list
it in the index with its SHA-256, and set `requires: ["NODE"]` / `["PYTHON"]` as a
human hint. Pin versions in `package.json` / `requirements.txt` for reproducible
installs. Keep the dep tree lean (containers are slim; avoid native-heavy
packages where a pure alternative exists — e.g. Baileys instead of a browser).

---

## 6. Verify-before-save (optional but recommended)

Ship a `verify.py` / verify snippet and reference it:

```jsonc
"verify": { "command": "python3 verify.py", "timeout_secs": 15 }
```

It runs with the collected env/secret injected and must print **one JSON object**
on stdout: `{"ok": bool, "message": string, "details"?: object}`, exit 0 on
success. Used for `api_key`/`none` connectors to test creds before activating.
(A `qr` connector needs no verify — its `login_status` is the live check.)

---

## 7. Versioning & updates

Three fields, in **both** the index entry and the `connector.json`, kept identical:

| field | type | role |
| --- | --- | --- |
| `version` | **integer** | monotonic build number, **per connector** — the machine comparison key |
| `version_string` | string (semver) | display only |
| `version_release_date` | ISO date `YYYY-MM-DD` | display only |

- `version` is a **number, not a string** (`1`, not `"1"` or `"2.0.1"`). Start at
  `1` for the first release under this scheme; **`+1` on every change** to any
  shipped file **or to any manifest metadata** (description, icons, `version_string`).
  Never reuse or decrement.
- Skald stores the installed `version` and compares it to the feed's: a strictly
  greater feed `version` shows **"update available"** in the marketplace, and the
  Install button becomes **Update**. Clicking it re-downloads the files and rewrites
  the catalog row.
- **The integer is the *only* "is there an update?" signal** — it is compared
  strictly (`feed > installed`). `version_string` (semver), icons and
  `llm_short_description` are **never** compared, so a change to any of them that
  does not also bump the integer is **invisible**: no "update available" badge
  appears. This is the common trap — a "content-only" edit (e.g. a better
  `llm_short_description`) that forgets the integer.
- **Two propagation paths, do not conflate them:**
  - *Per-user code + deps* (the scripts, `package.json`/`requirements.txt`) reconcile
    on a **content-hash** of the source files (§5), so new code lands at each user's
    next login even without a reinstall.
  - *Catalog metadata* (`llm_short_description` → the model's prompt, icons, friendly
    name) is **not** in that hash — it lives in the catalog row and is rewritten only
    by an explicit **reinstall/Update**. On reinstall Skald re-pulls the current feed
    (never the browse cache) and pushes the new description live: enabled global
    servers restart with it, and every logged-in user who activated the connector has
    it restarted with the fresh `llm_short_description` — no re-login needed.
- So: to ship a new `llm_short_description`, **bump the integer** (so the admin sees
  "update available") and the admin clicks **Update**. Nothing auto-propagates a
  description change.
- `version_string` and `version_release_date` are display metadata only — never
  compared. (Migration note: replace any legacy string `"version": "2.0.1"` with
  the integer `version` + `version_string`.)

---

## 8. Checklist for a new connector

1. Folder `myconn/` with: entry file, `connector.json`, deps file
   (`package.json`/`requirements.txt`), `icon_sm.svg`, `icon_lg.svg`,
   optional `verify.*`.
2. Server speaks MCP over stdio (§2); **stdout = JSON-RPC only**.
3. `mcp_config.args[0]` names the entry file.
4. Correct `type` + `scope` (§3) and `auth.type` (§4).
5. For `qr`: implement `login_status` (+ `logout`), persist the session under the
   connector dir (§4d).
6. Deps declared as a file, **not** vendored (§5).
7. Add the entry to `connectors.json` with a correct `sha256` for **every** file.
8. Bump `version`.
```
