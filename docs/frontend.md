# Frontend

## HTTP Server

Axum router assembled in `src/frontend/server.rs` (`WebServer::build_router_with_plugins`):

- `/api/*` — the app's HTTP handlers (`State<Arc<Skald>>`), plus per-plugin routers nested under `/api/plugin/<id>/`.
- `/data/*` — the on-disk `data/` directory (served via `tower_http::services::ServeDir`, so e.g. `/data/gmail_attachments/...` resolves to a file URL).
- Static fallback — the `web.static_dir` directory (`ServeDir`), i.e. the SPA assets.

**Compression.** A global `tower_http::compression::CompressionLayer` (gzip + brotli, enabled via the `compression-gzip` / `compression-br` features) wraps the whole router. Encoding is negotiated from the client's `Accept-Encoding` (no-op for clients that don't advertise one and for already-compressed media). The main motivation is the **mobile relay path**: the native WebView's HTTP traffic is reverse-proxied byte-for-byte over a relay pipe (`http-local-proxy`), so shrinking text assets (JS/CSS/HTML) ~70-90% means far fewer bytes cross the slow link. Desktop browsers benefit the same way.

**Caching.** Static responses (SPA assets + `/data/*`) carry `Cache-Control: no-cache` (applied via a `tower_http::set_header::SetResponseHeaderLayer` on the two `ServeDir`s). The browser may store the asset but **must revalidate before use** — so after a self-rewrite/restart the client never serves a stale asset (no heuristic caching), and revalidation yields cheap `304`s (`ETag`/`Last-Modified` from `ServeDir`). `/api/*` is deliberately left without the header (dynamic data, not cached). Note: because the mobile loopback proxy listens on an OS-assigned port, WKWebView's URLCache is keyed by a port that changes across app/tab restarts — so cross-session cache hits depend on that port being stable (tracked as a separate, app-side follow-up).

## WebSocket Endpoint

`GET /api/ws?source=<string>`

`source` identifies the conversation: `web` (default, desktop copilot), `mobile` (mobile chat page), or `project-{id}` (a project's interactive chat — see below). The same endpoint serves all; ChatHub maintains one independent, persistent session per source.

One connection per source. The connection is upgraded by Axum's WS handler in `src/frontend/api/ws.rs`. The client sends one `ClientMessage`, receives a stream of `ServerEvent`s, then can send additional messages (cancel, approval) while events are in flight.

History for a source: `GET /api/<source>/messages` (or the legacy alias `/api/web/messages`).

### Project chats

A project's chat is a persistent session bound to source `project-{id}` and driven by the
`project-coordinator` agent. `POST /api/projects/{id}/session` provisions (or resumes) it,
seeding the session's `RunContext` with the project's working directory, fs-write grants, and
context — then returns `{ source, session_id }`. The frontend connects the WS to that source.
Because the session is **not** ephemeral and ChatHub reuses the existing session for a source,
the conversation persists and is resumed on reopen. Resetting it (`POST /api/sessions?source=project-{id}`)
recreates it with the coordinator agent, not `main` (the handler resolves agent + RunContext per
source via `provisioning_for_source`).

In the desktop copilot these appear as browser-style **tabs**: `General` (the `web` source, always
present, not closable) plus one tab per open project chat. The board's **Open Chat** button
dispatches a `project-chat-open` window event (`{source, label}`); the copilot adds/focuses the tab
and switches the live connection via `ChatSession._switchSource(source)`. Closing a project tab is
UI-only — the session persists and can be reopened from the board.

---

## ClientMessage Fields

| Field | Type | Description |
|---|---|---|
| `content` | `String` | The user's prompt text |
| `client` | `Option<String>` | Named LLM model override (or `"auto"`) |
| `attachments` | `Attachment[]` | Files uploaded beforehand via `POST /api/{source}/uploads`; each `{ path, name, mimetype?, filesize? }`. See [Attachments](#attachments) |

---

## ServerEvent Types

All events are JSON objects with a `"type"` tag (snake_case).

| type | Key fields | When emitted |
|---|---|---|
| `tool_start` | `tool_call_id`, `message_id`, `name`, `arguments`, `label_short`, `label_full`, `path?` | Tool call recorded, about to execute. `path` (optional) is the viewable file the call targets — rendered as a clickable link to the file viewer |
| `tool_done` | `tool_call_id`, `result` | Tool executed successfully |
| `tool_error` | `tool_call_id`, `error` | Tool execution failed |
| `agent_start` | `stack_id`, `parent_tool_call_id`, `agent_id`, `depth` | Sub-agent stack frame opened |
| `agent_done` | `stack_id` | Sub-agent stack frame closed |
| `thinking` | `message_id`, `content`, `input_tokens`, `output_tokens` | LLM produced text before tool calls |
| `pending_write` | `request_id`, `tool_call_id`, `path`, `old_content`, `new_content` | Approval required for write/command |
| `agent_question` | `request_id`, `tool_call_id`, `title`, `question`, `suggested_answers` | Sub-agent needs user clarification |
| `file_changed` | `path` | A tool wrote to a file |
| `open_file` | `path` | Agent-driven file open: the file viewer supports Markdown, source code, plain text, raster images (PNG/JPG/GIF/WebP/…), SVG, PDF, LaTeX (`.tex` / `.latex` — compiled to PDF automatically on the server), and HTML (`.html` / `.htm` — rendered live in an origin-isolated `<iframe srcdoc sandbox="allow-scripts">`, toggleable to source). For a LaTeX document, open the `.tex` source rather than a pre-built `.pdf`: only the `.tex` path is compiled, cached dependency-aware, and watched for dependency changes — a raw `.pdf` is served statically and stays stale when its sources change. Emitted by the `show_file_to_user` interface tool (SPA-only, injected in `ws.rs`; `path` normalised relative to the project root) |
| `done` | `message_id`, `stack_id`, `content`, `input_tokens`, `output_tokens` | Turn complete, final response |
| `truncated` | `output_tokens` | LLM hit token limit (`finish_reason=length`) |
| `error` | `message` | Fatal error (session handler failed) |
| `model_fallback` | `from`, `to`, `reason` | Active model swapped to fallback automatically |
| `llm_failed` | `tried`, `last_error` | All LLM fallback attempts exhausted |
| `approval_required` | `request_id`, `tool_call_id`, `tool_name`, `arguments` | Non-file tool call requires user approval |
| `approval_resolved` | `request_id`, `approved` | Approval resolved (any source); all clients update their UI |
| `user_message` | `message_id`, `content`, `attachments?` | A user message persisted to history, echoed to **every** client of the source (the sender included). Emitted at save time — at turn start, or at a round boundary for mid-turn injection — so the bubble lands in its correct position. Carries the typed text + structured attachments (never the `[SYSTEM INFO]` block) |
| `new_session` | `session_id` | Session was cleared (`/new`, `/clear`); clients reset their message list |
| `turn_running` | `running` | Sent to a client on (re)connect: whether a turn is in flight for its session, so a reloaded page restores the SEND→STOP button |
| `client_selected` | `client` | The pinned LLM client for the source changed (`/model` command or dropdown change). Clients update their dropdown/select to match — the backend is the single source of truth |

---

## Attachments

The desktop copilot and the mobile chat page let the user attach files to a message.
Files are added with the paperclip button, **drag & drop** onto the composer, or **paste**
(`Ctrl+V`) — all handled by `ChatSession._addFiles` / `_onDrop` / `_onPaste` in
`web/lib/chat-session.js`. Text is required: a message is never sent with attachments alone.

Flow:

1. On selection, each file is uploaded immediately to `POST /api/{source}/uploads`
   (multipart). The handler (`src/frontend/api/uploads.rs`) **streams** each part straight
   to `data/uploads/{session_id}/` (`field.chunk()` → file, never buffered in RAM) and the
   route disables the default body-size limit. It returns the saved `Attachment`s
   (`{ path, name, mimetype, filesize }`, `path` project-root-relative so `/data/…` serves it).
2. The pending attachments render as **chips above the textarea** (`renderAttachmentChips` in
   `copilot-render.js`, `removable: true`, with a spinner while the upload is in flight).
3. On send, the client posts `{ content, attachments }` over the WebSocket. `content` is the
   clean typed text; `attachments` are the uploaded objects.
4. Server-side, the message is persisted as a user `chat_history` row, and a `user_message`
   event (carrying its `message_id` + `attachments`) is broadcast **at save time** — at turn
   start, or at a round boundary for messages injected mid-turn (see *Telnet-style echo* below).
   The attachments are stored in the generic `metadata` JSON column. **The `[SYSTEM INFO]` block
   the LLM sees is generated on the fly** by the message builder from `metadata.attachments`
   (path-only — the agent reads the files with its own tools), so `content` and the UI stay
   clean. On reload, `build_items`
   surfaces `attachments` again so the chips reappear (clickable → file viewer).

The Telegram plugin reuses the same `MessageMetadata`/`Attachment` types for Document/Photo
uploads, so those render as chips too when viewing the `telegram` source — see
[plugins/telegram.md](plugins/telegram.md).

## Sending messages: telnet-style echo + mid-turn injection

The client does **not** render the user's message optimistically. `_send()` clears the
composer and posts `{ content, attachments }` over the WebSocket; the bubble appears only when
the backend persists the message and echoes it back as a `user_message` event (with its real
`message_id`). This "telnet" model makes the backend the single source of truth — no
client-generated id, no content-based dedup, and every client (the sender included) renders the
same echo. The `user_message` handler in `chat-session.js` therefore just pushes a bubble; the
old dedup against a local optimistic push is gone.

- **Sending while a turn is running is allowed.** The composer is no longer disabled on
  `_waiting`, and the send button is shown **alongside** the STOP button during a turn (Enter
  still sends on desktop). The message is queued and [injected into the running turn](session.md#mid-turn-injection)
  at its next round boundary; the bubble appears (via echo) at that moment — i.e. after the
  current round's tools, where the agent actually sees it. With a long-running tool the echo is
  delayed until the round ends.
- **Slash commands are the exception:** they are handled server-side and never persisted/echoed,
  so `_send()` renders them optimistically (`content.startsWith('/')`).
- A `/stop` before the next round boundary drops the queued message: no echo, no bubble.

## Slash Commands (Web Copilot)

The web copilot supports the following slash commands, intercepted server-side in
`src/frontend/api/ws.rs` before reaching the LLM:

| Command | Effect |
|---|---|---|
| `/new` | Create a new chat session (handled client-side, clears context) |
| `/help` | Show available commands |
| `/models` | List available LLM models ordered by priority (numbered `0..N`, index 0 is `auto`) |
| `/model <N\|name\|auto>` | Pin the model for this chat by index, name (substring allowed), or reset to `auto`. The web dropdown and the `/model` command share the same backend state (`ChatHub.selected_clients[source]`); changes from either mutate the SOT and broadcast `ClientSelected`, so all open tabs/mobile update in sync. Cleared on server restart |
| `/context` | Show last turn's token usage (`↑X tok · ↓Y tok`) |
| `/cost` | Show total spend for this session in USD (sync sub-agents included; async tasks excluded). `None` → "no cost recorded" when the provider does not report pricing |
| `/compact` | Force context compaction (bypasses the token threshold) |
| `/resettools` | Remove all activated tool groups (MCP servers + `config`) from the session |
| `/sethome` | Set web as the home source for background notifications |

Any other message starting with `/` is treated as an unknown command: the server
replies with an "Unknown command" notice followed by the help list, and never
forwards it to the LLM.

---

## Tool Call Status Lifecycle

Tool calls in `chat_llm_tools` progress through these states:

| DB status  | Meaning | Frontend `build_items` |
|------------|---------|------------------------|
| `running`  | Tool executing — no user action required | `status: 'error', error: 'Interrupted.'` (shown after page refresh/restart) |
| `pending`  | Blocked on explicit user input (approval gate `Require`, or `ask_user_clarification`) | `status: 'pending'` → shows approval/clarification form |
| `done`     | Completed successfully | `status: 'done'` |
| `failed`   | Terminated with error | `status: 'error'` |

On **page refresh** or **app restart**, the frontend detects pending/interrupted tools in history (`_hasPendingTools` flag set in `_loadHistory`). On `ws.onopen` it sends `{"type":"resume"}`, which triggers `resume_turn()` → `resume_pending_tools()`:
- `running` tools → re-executed through the approval gate
- `pending` tools (approval) → approval channel re-registered, `approval_required` re-emitted with new `request_id`
- `pending` tools (`ask_user_clarification`) → question re-asked via `dispatch_ask_user_clarification`
- `call_agent` tools → skipped here; child stack is resumed by `resume_turn()` cascade (see below)

`resume_turn()` also cascades upward when a sub-agent stack completes: it terminates the child, marks the parent's `call_agent` tool as `done`, then continues running the parent stack until the root emits `Done`.

---

## Approval Flow

1. Server emits `pending_write` with `request_id`, `path`, `old_content`, `new_content`.
2. Frontend shows a diff and prompts the user.
3. User approves → client sends: `{"type":"approve_write","request_id":<N>}`
4. User rejects → client sends: `{"type":"reject_write","request_id":<N>,"note":"<optional reason>"}`
5. Server receives the message via `handle_approval_msg()`, calls `handler.resolve_approval(request_id, decision)`.
6. The `oneshot` channel unblocks in `run_agent_turn`, execution proceeds or is skipped.

Before blocking on the approval channel, the server sets `status='pending'` in `chat_llm_tools` via `set_approval_pending()`. This is what distinguishes "waiting for user" from "tool was executing when the session was interrupted" (`running`).

## Clarification Flow

### Interactive sessions (web / Telegram)

1. An agent (root or sub-agent) calls `ask_user_clarification(title, question, suggested_answers?)`.
2. Server sets `status='pending'` for the tool call, registers it with `ClarificationManager` (so it also appears in the Agent Inbox), then sends `agent_question` with `request_id`, `title`, `question`, and optional `suggested_answers`.
3. Frontend shows the question and collects a free-text answer (suggested answers shown as clickable chips).
   The `question` body is rendered as **sanitised Markdown** via the shared `renderMarkdown()` (`web/lib/base.js`, marked + DOMPurify), so the agent may use `**bold**`, lists, `code`, etc. The `title` and the suggested-answer chips remain plain text.
4. Client sends: `{"type":"answer_question","request_id":<N>,"answer":"<user text>"}`
5. Server calls `handler.resolve_question(request_id, answer)`.
6. The answer is returned as the tool result and the agent continues.

On WS disconnect while waiting, `cancel_pending_questions()` drops all channels, causing the awaiting tool call to fail with an error. On reconnect, auto-resume re-asks the question.

### Background sessions (cron / tic)

1. The agent (root or sub-agent) calls `ask_user_clarification(title, question, suggested_answers?)`.
2. `dispatch_ask_user_clarification` sets `status='pending'` then registers with `ClarificationManager` (in-memory, in-process).
3. The entry appears in `GET /api/inbox` under `clarifications`.
4. User answers via the Agent Inbox page → `POST /api/inbox/clarifications/:request_id/resolve`.
5. The `oneshot` channel unblocks, answer is returned as tool result, agent continues.

Cancel message (abort current turn): `{"type":"cancel"}`

---

## Elicitation Flow (MCP server-initiated input)

An MCP server can request input *during* a tool call (e.g. a sudo password). Unlike
clarification, this is driven by the **server**, not an agent tool call, and supports
`accept`/`decline`/`cancel` with a masked input. See `docs/mcp.md` for the protocol.

1. The MCP server sends `elicitation/create`; the `mcp-client` read-loop bridges it to
   `ElicitationManager::register` (in-memory, no session binding).
2. The entry appears in `GET /api/inbox` under `elicitations` and as a global
   `elicitation_requested` event (so the Inbox badge / mobile push update).
3. The Agent Inbox renders a **"Secrets"** card (`_renderElicitationCard` in
   `web/lib/inbox-mixin.js`): a masked `<input type="password">` when `sensitive`, a
   plain input otherwise, or just Confirm/Reject when `is_confirmation`.
4. User confirms or rejects → `POST /api/inbox/elicitations/:request_id/resolve` with
   `{action, content?}`. On `accept` the value is packed as `{ [field_name]: value }`.
5. `ElicitationManager::resolve` unblocks the bridge, which writes the JSON-RPC reply to
   the server's stdin. The value is **never** logged, broadcast, or persisted.

---

## Lit Component Inventory

| File | Element | Responsibility |
|---|---|---|---|
| `web/lib/chat-session.js` | `ChatSession` (base) | Shared WS logic, message state, all approval/LLM event handling, **voice recording + transcription** (`_checkTranscribe`, `_startRecording`, `_stopRecording`, `_toggleRecording`, `_submitAudio`; renders a mic button when `/api/transcribe/has` returns 204), and textarea helpers (`_inputEl` hook, `_autoResize`). Subclasses override `_wsSource`, `_inputEl`, `_getInputContent`/`_clearInput` (defaults now driven by `_inputEl`), `_scrollToBottom`, `_onMessagePushed`. Effective source is `_source` (`_activeSource ?? _wsSource`); `_switchSource(source)` tears down the WS, reloads history, and reconnects to switch sessions live. **Attachments** (`_attachments` state, `_addFiles`/`_removeAttachment`/`_onDrop`/`_onPaste`): upload on selection, send with the next message — see [Attachments](#attachments) |
| `web/components/copilot.js` | `<app-copilot>` | Desktop copilot panel (`_wsSource='web'`); resize, composer input with model pill and auto-resize textarea. **Voice recording is inherited from `ChatSession`**; only the desktop-only Ctrl+Space push-to-talk shortcut (`_onKeydown`/`_onKeyup`) lives here. Browser-style tabs (`General` + project chats); listens for the `project-chat-open` window event to add/focus a project tab |
| `web/components/shared/chat-page.js` | `<chat-page>` | Mobile chat page; extends `ChatSession` with a mobile-specific layout. Composer mirrors the desktop copilot: a single unified box (`.chat-page-composer`) wrapping an auto-resizing textarea with a toolbar below — toolbar-left holds a native `<select>` model pill (`auto` + providers, opens the OS picker), toolbar-right holds the mic button (inherited recording) and the send/stop button. **Enter inserts a newline** (no Shift+Enter on mobile) — only the send button submits. The `source` prop (default `mobile`) re-points the chat: when it changes the component calls `_switchSource` to bind to a project's `project-{id}` session; it also honours `source` on the first connect (cold deep link from the native shell); inside a project the header shows the project `label` + a back button that emits `project-exit` |
| `web/components/shared/projects-page.js` | `<projects-page>` | Mobile project list. Loads `GET /api/projects`; tapping a project `POST`s `/api/projects/{id}/session` and emits a `project-open` event (`{source, label}`) so `<mobile-app>` re-points the chat |
| `web/components/copilot-render.js` | (helpers) | Renders messages, tool call blocks, diffs — shared by copilot and chat-page. Tool labels and diff headers render the call's `path` (when present) as a clickable link via `renderLabel`/`renderPath` → `openFile(path)` |
| `web/components/sidebar.js` | `<app-sidebar>` | Navigation sidebar; polls `/api/inbox` every 10 s for badge count |
| `web/components/topbar.js` | `<app-topbar>` | Top navigation bar |
| `web/components/editor.js` | (removed) | The legacy `<app-main>` editor panel was removed. Use `<file-viewer>` (see [File Viewer](#file-viewer)) instead |
| `web/components/shared/file-viewer-base.js` | `FileViewerBase` (base) | Shared file-viewer engine: fetch, kind detection (image/pdf/svg/latex/html/text/binary), markdown asset rewriting, LaTeX compile + error formatting, HTML preview⇄source toggle, live watcher, and `_renderBody`. Navigation-agnostic — driven by `_show(path)` / `_hide()`; subclasses supply the chrome |
| `web/components/file-viewer-page.js` | `<file-viewer-page>` | Desktop subclass of `FileViewerBase`. Self-routes off the hash (`#file_viewer?path=...`) via the `llm-page-change` + `hashchange` events; renders in the main workspace. Opened by `window.openFile(path)`. Preview only — editor + watcher tabs planned |
| `web/components/shared/file-viewer-mobile.js` | `<mobile-file-viewer-page>` | Mobile subclass of `FileViewerBase`. Prop-driven (`visible` / `path`, set by `<mobile-app>`'s hash router); full-screen with a mobile header + back button |
| `web/components/cron-jobs.js` | `<cron-jobs-page>` | Cron job management UI — columns: Title (+ one-shot badge), Cron, Agent, Last run, Next run, Enabled, Actions |
| `web/components/agent-inbox.js` | `<agent-inbox-page>` | Unified inbox for pending approvals, clarifications, and MCP elicitations ("Secrets"); polls `/api/inbox` every 8 s when open |
| `web/components/models-hub.js` | `<models-hub-page>` | Models hub — 3-card landing page (LLM / Transcription / Image Generation) with live model counts; internal navigation to sub-sections |
| `web/components/models-llm.js` | `<models-llm-section>` | LLM model management: drag-and-drop priority, catalog picker (OpenRouter/Ollama/…), add/edit/delete |
| `web/components/models-transcribe.js` | `<models-transcribe-section>` | Transcription model CRUD; filters providers by `supported_types.includes('transcribe')` |
| `web/components/models-image.js` | `<models-image-section>` | Image generation model CRUD; filters providers by `supported_types.includes('image_generate')` |
| `web/components/llm-providers.js` | `<llm-providers-page>` | LLM provider management |
| `web/components/agents.js` | `<agents-page>` | Agent discovery and configuration |
| `web/components/approval-groups.js` | `<approval-groups-page>` | Groups list: create, rename, duplicate, delete permission groups; navigates to rules view via `approval-navigate` event |
| `web/components/approval-rules.js` | `<approval-rules-page>` | Per-group rules view: rule matrix, override/low-priority panels, default action bar; shows when `approval-navigate` fires with a non-null group |
| `web/components/llm-requests.js` | `<llm-requests-page>` | LLM request log viewer with filterable table, pagination, clickable rows that drill into detail view (`#llm-requests/<id>`) |
| `web/components/llm-request-detail.js` | `<llm-request-detail>` | LLM request detail: stat bar, system prompt, conversation messages, tool definitions, response — with collapsible sections |
| `web/components/session-detail.js` | `<session-detail-page>` | Read-only debug view of any session. Navigate to `#session/{id}` to load. Shows full message tree with tool calls, sub-agent frames, synthetic user messages, and collapsible reasoning blocks. Not linked from the sidebar — accessed by typing the hash directly. |

All components extend `LightElement` from `web/lib/base.js` (Lit-based).

### Markdown rendering & link behavior

`renderMarkdown(text)` (in `web/lib/base.js`) is the single entry point for rendering assistant/file markdown: it runs `marked.parse` then `DOMPurify.sanitize`. **External links** (http/https whose origin differs from the page) get `target="_blank" rel="noopener noreferrer"` via a DOMPurify `uponSanitizeElement` hook, so they open in a new tab instead of navigating away from the app. Relative paths, hash anchors (e.g. the app's `#file_viewer?...` routing), and non-http schemes (`mailto:`, `tel:`) are left untouched, preserving in-app navigation and native handlers.

### Approval Rules navigation protocol

`<approval-groups-page>` and `<approval-rules-page>` communicate via a custom DOM event instead of shared state:

| Event | Detail | Who fires | Who handles |
| --- | --- | --- | --- |
| `approval-navigate` | `{ group: ToolPermissionGroup \| null }` | groups page (navigate to rules) | rules page (show with group) |
| `approval-navigate` | `{ group: null }` | rules page (`← Back` button) | groups page (show again) |

Hash persistence: `window.location.hash` is set to `#approval/{group_id}` when navigating to a rules view. On page load, the groups page reads the hash and re-fires the event so deep-links and page reloads restore the correct sub-view.

### Agent Inbox page

Approval cards have a yellow left border; clarification cards have a blue left border. Clarification cards show suggested-answer chips (click pre-populates the input) and a free-text input — submit with Enter or the Send button.

Approval cards have Approve / Reject buttons and a timed bypass menu (15 min / 1 hour / Session) scoped to the tool's category or MCP server. The bypass scope auto-detects from the pending approval's metadata: `tool_category` for category-scoped, `mcp_server` for MCP server-scoped, otherwise `all`. The REST API also supports `bypass_secs` and `bypass_scope` fields in the resolve body.

---

## Mobile App & Native Shell

The mobile UI (`web/mobile.html` → `<mobile-app>`) is the same SPA the desktop uses, laid out for touch. It is also what the **native iOS shell** renders in a WKWebView over the relay (see [relay/pipe.md](relay/pipe.md)).

### Hash routing

`<mobile-app>` drives its active section from the URL hash — the same model as the desktop sidebar (`web/components/sidebar.js`), so the URL is always the source of truth. `_readHash()` / `_applyHash()` react to `hashchange` and `popstate`; `_nav(section)` sets `location.hash`. This gives deep links, working back/refresh, and — for the native shell — a single observable signal for menu sync.

| Hash | Section | Notes |
|---|---|---|
| `#inbox` | Inbox | Pending approvals + clarifications |
| `#projects` | Projects | Project list |
| `#chat` | Chat | Main mobile session (source `mobile`) |
| `#chat/project-<id>` | Chat | Bound to a project's session (source `project-<id>`). The header label resolves from `/api/projects` (cached in `<mobile-app>`), so a cold deep link still shows the name. Back/refresh keep the user inside the project |
| `#file_viewer?path=<enc>` | File viewer | Full-screen file preview, reached from content via `openFile(path)` (e.g. a clickable tool path) — **not** a bottom-nav tab. The back button returns to the previous section via history |
| `#notifications`, `#settings` | (coming soon) | Placeholder |

`<chat-page>` (`web/components/shared/chat-page.js`) honours its `source` prop on the **first** connect (via `_activeSource`), so a cold `#chat/project-<id>` deep link connects straight to that session instead of opening the `mobile` session and switching a tick later.

`_applyHash()` **skips** the `skaldNav` notify for the `file_viewer` section: the viewer is a transient overlay reached from content (not a tab), so opening a file from the chat must not deselect the native "chat" tab.

### Native shell mode (`?native=true`)

When loaded with `?native=true`, `<mobile-app>` sets a `data-native` attribute and **hides its HTML bottom nav** — the native tab bar is the chrome. `web/css/mobile.css` drops the web-side safe-area insets under `mobile-app[data-native]` (the native chrome owns the status-bar + home-indicator insets). Everything else is identical to the mobile-browser path.

### Native ↔ Web contract

The native tab bar and the web router stay in sync over one mechanism each direction:

| Direction | Mechanism | Payload / call |
|---|---|---|
| Native → Web | set the hash | `webView.evaluateJavaScript("location.hash='#projects'")` — the web `hashchange` handler switches section. A project deep link works too (`#chat/project-<id>`). Same code path the browser uses. |
| Web → Native | `WKScriptMessageHandler` named **`skaldNav`** | on every section change `<mobile-app>` calls `window.webkit.messageHandlers.skaldNav.postMessage({ section, project })`, where `project` is `null` or `'project-<id>'`. The shell updates its tab highlight. No-op when the handler is absent (mobile browser). |

The `skaldNav` bridge is the reliable sync: relying solely on observing the WKWebView URL is fragile, because same-document (hash-only) navigations don't reliably fire `WKNavigationDelegate` callbacks across iOS versions.

---

## File-Change Watcher (live reload)

`<file-viewer-page>` automatically reloads when the file it is showing changes
on disk — whether the change comes from a Skald tool, an external editor
(VSCode, vim, …), or any other process. The mechanism is a dedicated
WebSocket.

### Endpoint

`GET /api/file/watch` — upgrades to a long-lived WebSocket. Client → server
commands:

| Command | Effect |
|---|---|
| `{"op":"subscribe","path":"docs/index.md"}` | Start watching `path` (relative or absolute — same path model as `GET /api/file`) |
| `{"op":"unsubscribe","path":"docs/index.md"}` | Stop watching `path` |

Server → client messages:

| Message | Meaning |
|---|---|
| `{"type":"subscribed","path":"..."}` | Ack — watch installed successfully |
| `{"type":"unsubscribed","path":"..."}` | Ack — watch removed |
| `{"type":"changed","path":"..."}` | The file at `path` changed on disk (any event kind: create/modify/remove) |
| `{"type":"error","path":"...","error":"..."}` | Watch install failed (e.g. path does not exist, permission denied) |
| `{"type":"error","error":"..."}` | Malformed client message or unknown op (no `path`) |

The `path` field always round-trips the original user-supplied string, so the
client can match it against the path it asked to watch.

### Implementation notes

- **Backend** (`src/frontend/api/file_watch.rs`): one `notify::RecommendedWatcher`
  per watched file per connection (one watcher per file — for a LaTeX source
  that means one per dependency, see below). On disconnect every watcher is
  dropped and OS resources are released automatically. Path resolution uses
  `fs_tools::resolve` (same as `GET /api/file`), so absolute paths are used
  as-is and relative paths resolve against Skald's process CWD.
- **LaTeX dependency watching**: when subscribing to a `.tex` / `.latex`
  source, the server expands the single path into the full dependency set
  discovered via `LatexCompiler::watch_paths_for()` (which reads the cached
  `.fls` recorder file — see [LaTeX](#latex) below). One OS watcher is
  installed per dependency; any change to any of them is forwarded to the
  client as a `changed` event for the original `.tex` path. The dependency
  set is re-synced on every change event (watchers dropped and re-installed
  with the fresh `.fls`), so newly-added `\input`s are picked up
  automatically. On the very first subscribe, when no compile has happened
  yet, only the main `.tex` itself is watched; once the viewer's first
  compile writes the `.fls`, the next change event triggers the re-sync.
- **Frontend** (`web/lib/file-watcher.js`): singleton `fileWatcher` with a
  single persistent connection, ref-counting per path (multiple consumers of
  the same path share one OS watcher and one subscribe message),
  auto-reconnect on close with 2 s backoff, and automatic re-subscribe of all
  active paths on reconnect. Consumers call `fileWatcher.watch(path, cb)` and
  get back an `unsub()` function.
- **`<file-viewer-page>`** subscribes when it opens (or when the path changes
  via hash navigation) and unsubscribes when it closes or navigates away.
  Change notifications are debounced (300 ms) and trigger a **silent reload**
  (no spinner, no flicker — image previews swap the object URL only after the
  new blob is ready, text previews replace the content atomically).
- **Cross-platform:** uses `notify`'s recommended backend (FSEvents on macOS,
  inotify on Linux, ReadDirectoryChangesW on Windows).
- **Dirty-buffer conflict handling** is not implemented yet — there is no
  editor tab yet. When the CodeMirror editor lands (roadmap), a changed event
  arriving while the buffer has unsaved edits will show an "Overwrite / Discard
  / Ignore" banner instead of auto-reloading.

---

## File Viewer

`<file-viewer-page>` is a top-level page that previews files from disk in the
main workspace, so users (and agents, in future phases) can read
markdown/code/images without leaving the UI. It is registered in
`web/app.js` and lives once in the DOM at `index.html` (default
`display:none`, shown via the standard page router — see below).

The fetch / kind-detection / markdown-asset / LaTeX-compile / live-watch logic
and `_renderBody` live in `FileViewerBase` (`web/components/shared/file-viewer-base.js`),
shared with the mobile viewer. The base is navigation-agnostic — driven only by
`_show(path)` / `_hide()`; each subclass supplies its own chrome and decides when
to call them: the desktop `<file-viewer-page>` from the hash, the mobile
`<mobile-file-viewer-page>` from props (see [Mobile](#mobile) below).

### Opening files

The global helper `openFile(path)` is the single entry point — defined in
`web/lib/open-file.js`:

```js
openFile('data/memory/index.md');
// or
window.openFile('docs/frontend.md');
```

`openFile(path)` sets `location.hash` to `#file_viewer?path=<enc>`, which the
sidebar hash router (`web/components/sidebar.js:_pageFromHash`) resolves to
the `file_viewer` page. This means:

- **Back/forward browser navigation** works naturally.
- **Deep-linkable** — the URL can be shared or bookmarked.
- **The chat and everything else stay usable** while the file is open
  (clicking any other sidebar entry just changes the hash and the page
  switches out).

Both surfaces (`openFile(...)` and setting the hash directly) are equivalent.
Components that want to open a file should call `openFile(...)` so the URL
format lives in one place.

**Callers of `openFile`:**

- **Tool-call cards & write diffs** in the copilot/chat transcript. When a tool
  call targets a single viewable file, the backend reports the path in
  `ServerEvent::ToolStart.path` (via `Tool::target_path` — see
  [tools.md](tools.md#clickable-target-path)); `copilot-render.js` renders it as
  a clickable link. Falls back gracefully: tools without a `path` render the
  plain label, unsupported file types show the viewer's "preview not available"
  state, and unreadable paths show its error state.
- **The `show_file_to_user` tool**, via the `OpenFile` WebSocket event
  (`open_file`, handled in `chat-session.js`). It is an `InterfaceTool` injected
  only for SPA clients (`web` + `mobile`) in `src/frontend/api/ws.rs`, so the
  assistant can proactively open a file — see
  [tools.md](tools.md#registration-pattern). Every kind — HTML included — routes
  through `openFile` and renders inside the viewer.

### Routing

`file_viewer` is registered in the sidebar's segment whitelist
(`sidebar.js:_pageFromHash`) but has **no sidebar menu entry** — like
`#session/{id}`, it's an "accessory" page reachable only via link or
`openFile`. The page follows the standard pattern: it listens for
`llm-page-change` (shows/hides itself) and `hashchange` (re-reads the path
from the URL when navigating between files while staying on the page).

Path resolution is delegated to the backend via `GET /api/file?path=<enc>`
(`src/frontend/api/files.rs`), which calls `fs_tools::resolve`:

- **Relative paths** resolve against Skald's process CWD (the data root).
- **Absolute paths** are used as-is — required when opening files that live
  outside the data root, e.g. inside a project's custom working directory.

`get_file` serves **raw bytes** with a `Content-Type` derived from the extension
(`content_type_for`), not `read_to_string` — so binary formats (images, PDFs)
work. The viewer reads text kinds via `res.text()` and binary kinds via
`res.blob()` → object URL.

A query parameter `?force_download=true` makes the handler add
`Content-Disposition: attachment` so the browser **saves** the file instead of
rendering it inline. The attachment filename is the path's basename — or, when
combined with `?compile-latex=true` on a `.tex` source, `<stem>.pdf`. The
filename is sanitised to visible ASCII (header-value constraint). This backs the
header's **download button** (see below).

A query parameter `?compile-latex=true` switches the handler into LaTeX mode
when `path` is a `.tex` / `.latex` file: instead of returning the raw source,
the server runs `latexmk -xelatex` (via `LatexCompiler` in
`src/core/latex/`) and returns the resulting PDF (`application/pdf`). Compiled
PDFs are cached under `<tmp>/skald-latex/` in a **dependency-aware** way:

- `<path-hash>.fls` — the `.fls` recorder file from the last compile of that
  source (keyed by SHA-256 of the source's absolute path). Lists every file TeX
  actually read.
- `<deps-hash>.pdf` — the compiled PDF (keyed by SHA-256 of every
  user-controlled input's contents, sorted by path).

On each request the compiler first re-derives `deps-hash` from the cached
`.fls` and serves the matching PDF without invoking `latexmk` if it exists.
This means a change to any `\input`'ed fragment, custom `.sty` / `.cls`
package, `.bib`, or `\includegraphics` target invalidates the cache correctly
even when the main `.tex` file is unchanged. Inputs under system TeX
distribution paths (`/usr/local/texlive`, `/Library/TeX`, …) and TeX
auxiliary outputs (`.aux`, `.log`, `.fls`, `.synctex.gz`, …) are filtered out
of the dependency set — they only change on a distro upgrade, which is rare
and easy to handle by clearing the cache. Failures produce a non-2xx response
with the textual `latexmk` log in the body:

| Outcome | Status | Body |
|---|---|---|
| Compiled (or cache hit) | `200` | PDF bytes (`application/pdf`) |
| Compilation error | `422` | `latexmk` log (`text/plain`) |
| `latexmk` not installed | `501` | Explanatory message |
| Compile timeout (> 30s) | `504` | Explanatory message |

### Header chrome

Both chromes render a header with the file name and a **download button**
(`bi-download`, right-aligned). The button calls `_download()` in
`FileViewerBase`, which builds `/api/file?path=…&force_download=true` (adding
`compile-latex=true` for LaTeX kinds, so a `.tex` always downloads its compiled
PDF) and clicks a transient `<a download>` — the server's `Content-Disposition`
supplies the saved filename.

The file name uses **tail-truncation**: when a path is too long the ellipsis is
placed at the *start* (`…/dir/report.tex`) so the filename stays visible, via
`direction: rtl; text-align: left` with the path wrapped in `<bdi>` (to keep its
characters left-to-right). The full path remains on the `title` attribute. The
desktop chrome shows the full path; the mobile chrome shows only the basename.

### Supported kinds

| Kind | Extensions | Rendering |
|---|---|---|
| **Markdown** | `.md`, `.markdown` | `renderMarkdown()` (marked + DOMPurify) via `unsafeHTML`. Relative `<img>` sources are rewritten (`rewriteMarkdownAssets`) to `/api/file?path=<dir-of-md + src>` so images referenced relative to the file load from disk; external/`data:`/root-relative URLs are left untouched |
| **Image** | `.png .jpg .jpeg .gif .webp .bmp .ico .avif` | `<img>` loaded as a Blob from `/api/file` (object URL) |
| **SVG** | `.svg` | `<iframe sandbox="allow-same-origin">` to a Blob object URL. Rendered in a sandboxed iframe (not `<img>`): the SVG root fills the iframe viewport so viewBox-only files scale correctly, and `allow-scripts` is withheld so any embedded `<script>` cannot execute. `allow-same-origin` is required for the iframe to read the blob: URL |
| **PDF** | `.pdf` | `<iframe>` to a Blob object URL — the browser's native PDF viewer |
| **HTML** | `.html`, `.htm` | Fetched as text, then rendered live in `<iframe srcdoc sandbox="allow-scripts allow-forms allow-modals allow-popups">`. `srcdoc` (not a blob `src`) gives the frame a **unique opaque origin**, so its JS runs but is fully isolated from the app origin — it cannot read cookies/localStorage or reach `/api/*`. `allow-same-origin` is deliberately withheld: combined with `allow-scripts` it would let the frame strip its own sandbox. A header toggle (`_renderModeToggle`) flips between the live preview and the raw source (`<pre><code>`). Limitation: relative asset paths inside the HTML don't resolve (opaque origin has no disk base) — self-contained HTML and absolute/CDN URLs work |
| **LaTeX** | `.tex`, `.latex` | On open, the viewer first requests `?compile-latex=true`; on `200` it renders the resulting PDF in an `<iframe>` (same path as a native `.pdf`). On any non-2xx response (compilation error, missing `latexmk`, timeout) it falls back to showing the raw source as a `<pre><code>` block, with the extracted compilation error in a collapsible banner — `formatLatexError` distils the full latexmk log down to the actionable `path:line: …` / `! …` lines plus context (the leading banner + package preamble are dropped), so the shown text is what a user pastes into an agent. The file watcher installs one OS watcher per dependency discovered via the `.fls` recorder file (`\input`'ed fragments, custom `.sty` / `.cls`, `.bib`, images, etc.) — so saving any of them triggers an automatic recompile. Requires `latexmk` with `xelatex` on the server's `PATH` (e.g. MacTeX / TeX Live). See the [LaTeX compile & cache](#latex-compile--cache) section below for the full dependency-aware algorithm. |
| **Text/code** | `.txt .rs .js .ts .py .json .yml .toml .sh .sql .go .css .vue ...` (see `TEXT_EXTS` in the source) | `<pre><code>` block, monospace, horizontal scroll |
| **Binary/unknown** | anything else | Placeholder: "Preview not available for this file type." |

### LaTeX compile & cache

The `.tex` kind is special: it is the only kind where the server produces a
derived artefact (PDF) on demand rather than serving the raw file. The
`LatexCompiler` in `src/core/latex/` orchestrates `latexmk -xelatex` and
maintains a dependency-aware cache so:

- saving any `\input`'ed fragment, custom `.sty` / `.cls`, `.bib`, or
  `\includegraphics` target invalidates the cache correctly (even when the
  main `.tex` is unchanged);
- unchanged inputs are served without recompiling.

Two artefacts live under `<tmp>/skald-latex/`:

| Artefact          | Key                                            | Purpose                                          |
|-------------------|------------------------------------------------|--------------------------------------------------|
| `<path-hash>.fls` | SHA-256 of the `.tex` absolute path            | Last-known input list for that source            |
| `<deps-hash>.pdf` | SHA-256 of every user-controlled input's bytes | The compiled PDF for that exact content state    |

Per request:

1. Read `<path-hash>.fls` (the recorder file produced by the last compile).
   Missing → fresh compile.
2. Filter out system TeX paths (`/usr/local/texlive`, `/Library/TeX`, …) and
   auxiliary artefacts (`.aux`, `.log`, `.fls`, `.synctex.gz`, …).
3. Hash every remaining input's contents, derive `<deps-hash>`.
4. If `<deps-hash>.pdf` exists → cache hit, serve it.
5. Otherwise → run `latexmk` in a per-compile scratch directory with
   `-output-directory=<tmp>/skald-latex/<path-key>-<pid>-<ns>/`, capture the
   new `.fls`, overwrite the `<path-hash>.fls` sidecar, save the PDF as
   `<deps-hash>.pdf`, serve.

The file watcher (`/api/file/watch`) re-syncs its OS watchers on every change
event for a `.tex` source — dropping the per-dependency watchers and
re-installing them with the fresh `.fls`, so newly-added `\input`s are picked
up automatically. On the very first subscribe (no `.fls` yet), only the main
`.tex` is watched; once the first compile writes the sidecar, the next change
event triggers the re-sync.

Limitations: system TeX distribution files are excluded from the dependency
hash (they only change on a distro upgrade — clear the cache directory to
force a rebuild); shell-escape inputs (`\input{|"command"}`) are not tracked.

### Mobile

The mobile app renders its own `<mobile-file-viewer-page>` (`web/components/shared/file-viewer-mobile.js`), a thin subclass of `FileViewerBase` that shares all of the desktop viewer's fetch/render/watch logic and only swaps the chrome (full-screen page, mobile header + back button). `<mobile-app>`'s hash router (see [Mobile App & Native Shell](#mobile-app--native-shell)) routes `#file_viewer?path=...` to a non-tab `file_viewer` section and binds the component's `visible` / `path` props; the same `openFile(path)` used in the desktop transcript (a clickable tool path) therefore works unchanged on mobile. The back button returns to the previous section via history, and `_applyHash` skips the `skaldNav` notify so the native tab highlight stays put.

### Roadmap

The page is the foundation for several follow-up phases (tracked separately):

1. **Tab Editor (CodeMirror 6)** — second tab in the page with syntax-highlighted
   editable buffer; saves via `PUT /api/file`. Bypasses the approval gate (user is
   editing manually, not via an agent tool). Will introduce the "Overwrite /
   Discard / Ignore" banner for dirty-buffer conflicts.
2. **Agent-driven opening** — new `ServerEvent::OpenFile { path }` emitted by a
   `show_file_to_user(path)` interface tool; `chat-session.js` sets the hash from
   the WS payload, so both manual and agent-driven paths funnel into the same
   `<file-viewer-page>`.
3. **More media** — video (`<video>`), audio (`<audio>`), PDF (`<iframe>`).

### Files

| File | Purpose |
|---|---|
| `web/lib/open-file.js` | Defines and registers `window.openFile`; sets `location.hash` |
| `web/components/shared/file-viewer-base.js` | `FileViewerBase` — shared engine: fetch, kind detection, markdown assets, LaTeX compile, watcher, `_renderBody`. Driven by `_show`/`_hide` |
| `web/components/file-viewer-page.js` | `<file-viewer-page>` desktop subclass — hash routing (`llm-page-change` + `hashchange`) + desktop chrome |
| `web/components/shared/file-viewer-mobile.js` | `<mobile-file-viewer-page>` mobile subclass — prop-driven (`visible`/`path`) + mobile chrome (full-screen, back button) |
| `web/lib/file-watcher.js` | Singleton client for `/api/file/watch` — ref-counting, auto-reconnect, re-subscribe |
| `web/css/file-viewer.css` | Page + content styling (markdown, code, image, LaTeX compile-error banner, state). Loaded by both `index.html` and `mobile.html` |
| `src/frontend/api/file_watch.rs` | `/api/file/watch` WS handler — `notify::RecommendedWatcher` per watched file (one per LaTeX dependency for `.tex` sources) |
| `src/core/latex/mod.rs`, `src/core/latex/compiler.rs` | `LatexCompiler` — `latexmk -xelatex` invocation, SHA-256 content cache, error mapping. Called by `get_file` when `?compile-latex=true` |

---

## Adding a New ServerEvent

1. Add the variant to `ServerEvent` enum in `src/core/events.rs`.
2. Add the `type_name()` match arm in `src/core/events.rs`.
3. Emit it at the appropriate point (session handler, ChatHub, or ws.rs).
4. Handle it in `web/lib/chat-session.js` `_handleServerMsg()` — all clients inherit the handler automatically.
5. Update the ServerEvent Types table above.

---

## Debug Mode

A persistent flag stored in the `config` DB table under key `DEBUG_MODE` (`"true"` / `"false"`). The API is in `src/frontend/api/dev.rs`.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/api/dev/debug_mode` | — | `{ "enabled": bool }` |
| `POST` / `PUT` | `/api/dev/debug_mode` | `{ "enabled": bool }` | `{ "enabled": bool }` |
| `GET` | `/api/dev/llm-requests` | query: `?page=1&per_page=20&agent_id=&source=&from=&to=` | `{ items: LlmRequest[], total: int }` |
| `GET` | `/api/dev/llm-requests/{id}` | — | Full request/response payload with system prompt, messages, tool definitions, and response |

The frontend reads this flag at startup and uses it to show or hide sections in the sidebar menu that are otherwise invisible in production.

---

## When to Update This File

- A `ServerEvent` variant is added, removed, or its fields change
- `ClientMessage` gains or loses a field
- A new Lit component is added
- The approval message format changes
- The debug-mode endpoint changes
- The file viewer gains a new phase (editor tab, agent-driven opening) or a new supported kind
- The `/api/file/watch` protocol (commands, messages) changes
