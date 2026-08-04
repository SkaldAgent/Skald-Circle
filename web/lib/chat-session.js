import { html, nothing } from 'lit';
import { LightElement } from './base.js';
import { InboxCardsMixin } from './inbox-cards.js';
import { t } from './i18n.js';
import { isSessionExpired, notifySessionExpired, probeSession } from './session-expiry.js';

// Slash commands handled entirely server-side: they reply with a `Done` and never
// echo back as a `user_message`, so they are the only commands rendered
// optimistically on send. Everything else — custom commands and unknown ones — is
// telnet-style: the bubble appears only when the backend persists it and re-emits a
// `user_message` (custom commands carry their typed form as the echoed content).
// (`/new` and `/clear` are intercepted earlier and never reach the echo.)
const SYSTEM_SLASH_COMMANDS = new Set([
  '/help', '/models', '/model', '/context', '/cost',
  '/compact', '/resettools', '/sethome',
]);

// Pixels from the bottom within which auto-scroll still "sticks". Scrolling
// further up during streaming makes auto-scroll yield so the reader is left in
// peace; scrolling back within this band re-arms it.
const SCROLL_STICKY_PX = 80;

/**
 * Base class for chat UI components (desktop copilot, mobile chat page).
 *
 * Contains all WebSocket logic, message state, and approval handling.
 * Subclasses implement the render() method and override the DOM hooks:
 *   - _messagesContainer() → returns the scrollable message-list element
 *   - _getInputContent()   → returns current input value
 *   - _clearInput()        → empties the input
 *   - _onMessagePushed(item) → called after each push (scroll, focus, etc.)
 *
 * Auto-scroll is stick-to-bottom: it follows the stream only while the reader is
 * within SCROLL_STICKY_PX of the bottom. Scrolling up to read pauses it, and a
 * "jump to latest" affordance (driven by the `_showJump` state) is shown then.
 */
export class ChatSession extends InboxCardsMixin(LightElement) {
  static properties = {
    _messages:           { state: true },
    _waiting:            { state: true },
    _expanded:           { state: true },
    _providers:          { state: true },
    _selectedClient:     { state: true },
    _providersLoaded:    { state: true },
    // Session security-group (permission group) picker — the twin of the model
    // pill. `_securityGroups` is the caller's selectable set; `_selectedGroup` is
    // the session's current group (backend is the source of truth).
    _securityGroups:     { state: true },
    _selectedGroup:      { state: true },
    _rejectingId:           { state: true },
    _rejectNote:            { state: true },
    _clarificationAnswer:   { state: true },
    // Voice recording state (shared by every chat surface).
    _hasTranscribe:      { state: true },
    _recording:          { state: true },
    // Whether to show the "jump to latest" affordance: true only while the reader
    // is scrolled away from the bottom (i.e. auto-scroll has yielded). Driven by
    // the stickiness sensor in `_scrollToBottom`.
    _showJump:           { state: true },
    // Pending attachments for the message being composed (shown as chips above
    // the textarea; uploaded to disk on selection, sent with the next message).
    _attachments:        { state: true },
    // Background tasks (`execute_task mode=async`) this conversation started.
    // Each entry: { job_id, title, agent_id, session_id, state, error, started_at }.
    _tasks:              { state: true },
    // Bumped once a second while a task is running, so the elapsed counters move.
    _taskTick:           { state: true },
    // Approvals and questions raised by those tasks: `{ approvals, clarifications }`,
    // each item carrying the `job_id` / `job_title` that asked.
    _taskInbox:          { state: true },
  };

  // Live events whose arrival implies a turn is in flight (used to restore the
  // STOP button when reconnecting mid-turn).
  static _STREAMING_EVENTS = new Set([
    'thinking', 'tool_start', 'agent_start', 'pending_write', 'approval_required',
    'token_delta',
  ]);

  constructor() {
    super();
    this._messages          = [];
    this._waiting           = false;
    this._expanded          = new Set();
    this._ws                = null;
    // True only for an auto-reconnect after an unexpected socket close (set in
    // `onclose`), so the next `onopen` reconciles tool state that may have advanced
    // while we were disconnected. A deliberate teardown (source switch / new session)
    // nulls `onclose` first, so it never sets this.
    this._reconnecting      = false;
    this._providers         = [];
    this._selectedClient    = null;
    this._providersLoaded   = false;
    this._securityGroups    = [];
    this._selectedGroup     = 'default';
    this._rejectingId           = null;
    this._rejectNote            = '';
    this._clarificationAnswer   = '';
    // Runtime-selected source. When null, falls back to the static `_wsSource`.
    // Lets a single chat component switch between sessions (e.g. copilot tabs).
    this._activeSource          = null;
    // When set, this chat is bound to one specific conversation rather than to
    // whatever its source currently points at. See `_apiBase`.
    this._activeSessionId       = null;
    // Voice recording state. Shared so every surface (desktop copilot + mobile
    // chat) can expose the same mic button. The desktop-only Ctrl+Space push-
    // to-talk shortcut is wired in `app-copilot`; `_shortcutRecording` tracks
    // whether a recording session was started by that shortcut.
    this._hasTranscribe     = false;
    this._recording         = false;
    this._shortcutRecording = false;
    this._mediaRecorder     = null;
    this._audioChunks       = [];
    // Stick-to-bottom auto-scroll: `_stickToBottom` is `true` while the reader is
    // within SCROLL_STICKY_PX of the bottom. A manual scroll-up flips it false so
    // live streaming stops fighting the reader; scrolling back within the band
    // re-arms it. `_scrollEl` tracks the bound container so the scroll sensor is
    // attached exactly once (and re-attached if the element is recreated).
    this._stickToBottom = true;
    this._scrollEl      = null;
    this._showJump      = false;
    // Each entry: { name, path, mimetype, filesize, uploading? }. While an upload
    // is in flight the entry has `uploading: true` and no `path` yet.
    this._attachments       = [];
    this._tasks             = [];
    this._taskInbox         = { approvals: [], clarifications: [] };
    this._taskTick          = 0;
    this._taskTimer         = null;
    // Timers that drop a finished task from the strip after a grace period.
    this._taskDropTimers    = new Map();
    this._onAuthRestored    = this._onAuthRestored.bind(this);
  }

  async connectedCallback() {
    super.connectedCallback();
    window.addEventListener('auth-restored', this._onAuthRestored);
    // Fire-and-forget: availability of a transcription provider determines
    // whether the mic button is rendered at all.
    this._checkTranscribe();
    await Promise.all([
      this._loadProviders(), this._loadHistory(), this._loadTasks(), this._loadTaskInbox(),
    ]);
    this._connectWS();
  }

  disconnectedCallback() {
    super.disconnectedCallback?.();
    window.removeEventListener('auth-restored', this._onAuthRestored);
    this._stopTaskClock();
    for (const timer of this._taskDropTimers.values()) clearTimeout(timer);
    this._taskDropTimers.clear();
  }

  // ── Source identity — override in subclass ────────────────────────────────────

  // Static default source for this component. Subclasses override (e.g. 'mobile').
  get _wsSource() { return 'web'; }

  // The socket is addressed the same two ways as `_apiBase`. The source rides
  // along even for a session-addressed connection: the server still needs it for
  // `/sethome` and for tagging what it broadcasts.
  _wsQuery() {
    const q = `source=${encodeURIComponent(this._source)}`;
    return this._activeSessionId ? `${q}&session=${this._activeSessionId}` : q;
  }

  // Effective source: the runtime-selected one, or the static default.
  get _source() { return this._activeSource ?? this._wsSource; }

  /**
   * The REST prefix every per-chat call hangs off. Two ways to name a
   * conversation: through its source ("whatever `web` points at right now",
   * which is what background delivery reaches) or directly by id. An extra tab
   * is the second kind — its source points at a different conversation, so it
   * is unreachable by name.
   */
  get _apiBase() {
    return this._activeSessionId
      ? `/api/sessions/${this._activeSessionId}`
      : `/api/${this._source}`;
  }

  /**
   * Switch the live connection to another conversation: tear down the current
   * WS, rebind, reload that conversation's history, and reconnect. Used to move
   * between tabs without remounting. `sessionId` null ⇒ address it by source.
   */
  async _switchTo(source, sessionId = null) {
    if (this._ws) { this._ws.onclose = null; this._ws.close(); this._ws = null; }
    this._activeSource    = source;
    this._activeSessionId = sessionId;
    this._messages = [];
    this._waiting  = false;
    this._tasks    = [];
    this._taskInbox = { approvals: [], clarifications: [] };
    await Promise.all([this._loadHistory(), this._loadTasks(), this._loadTaskInbox()]);
    this._connectWS();
  }

  // ── Data loading ──────────────────────────────────────────────────────────────

  async _loadProviders() {
    try {
      const res = await fetch('/api/llm/models/selector');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const { models, default: def } = await res.json();
      this._providers      = models;
      this._selectedClient = def;
    } catch (e) {
      console.error('Failed to load LLM models:', e);
    } finally {
      this._providersLoaded = true;
    }
  }

  get _noModels() {
    return this._providersLoaded
      && this._providers.filter(p => p !== 'auto').length === 0;
  }

  _renderNoModelsBanner() {
    if (!this._noModels) return nothing;
    return html`
      <div class="home-banner home-banner--error chat-no-models">
        <div class="home-banner-icon"><i class="bi bi-cpu-fill"></i></div>
        <div class="home-banner-body">
          <strong>${t('dashboard.banner.no_models.title')}</strong>
          ${t('dashboard.banner.no_models.desc')}
          <a href="#llm-providers">${t('dashboard.banner.no_models.action')}</a>
        </div>
      </div>
    `;
  }

  async _loadHistory() {
    try {
      const res = await fetch(`${this._apiBase}/messages`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const items = await res.json();
      if (items.length > 0) {
        this._messages = items;
        const expanded = new Set(this._expanded);
        for (const m of items) {
          if (m.kind === 'tool' && m.status === 'pending') expanded.add(m.tool_call_id);
        }
        this._expanded = expanded;
        this._forceScrollToBottom();
        // Set flag so ws.onopen sends a resume if there are pending tools
        // (approval/clarification waiting) or interrupted tools (status=error+Interrupted).
        this._hasPendingTools = items.some(
          m => m.kind === 'tool' && (m.status === 'pending' || (m.status === 'error' && m.error === 'Interrupted.'))
        );
      }
    } catch (e) {
      console.warn('Could not load history:', e.message);
    }
  }

  // ── Background tasks ──────────────────────────────────────────────────────────
  //
  // The agent can hand work to a background task (`execute_task mode="async"`)
  // and keep talking. Until now the only trace of one was the receipt in the
  // transcript and a row on the Tasks page, so "is it still going?" had no
  // answer in the chat itself. `_tasks` is that answer: a small live list of
  // the tasks *this* conversation started.
  //
  // Live updates arrive as `task_update` over the WebSocket — a broadcast with
  // no replay, which is why `_loadTasks` runs on every load and reconnect: it
  // is what makes the strip survive a browser refresh.

  // A finished task lingers this long before disappearing on its own. Its result
  // is already in the conversation by then; the chip is just the hand-off.
  static TASK_DROP_MS = 20000;

  /**
   * Whether this surface can open a task's own session page (`#session/{id}`).
   * The mobile shell routes a fixed set of sections and silently falls back to
   * the chat for anything else, so there the row is shown without a link rather
   * than with one that quietly navigates somewhere wrong.
   */
  get _canOpenTaskSession() { return false; }

  // Failures the user has dismissed with the ✕. Kept across reloads (the server
  // keeps reporting a recent failure, and dismissing it should mean dismissed).
  static _DISMISSED_KEY = 'skald.dismissedTasks';

  _dismissedTasks() {
    try {
      return new Set(JSON.parse(localStorage.getItem(ChatSession._DISMISSED_KEY) ?? '[]'));
    } catch { return new Set(); }
  }

  _rememberDismissed(jobId) {
    const ids = [...this._dismissedTasks(), jobId].slice(-50);
    try { localStorage.setItem(ChatSession._DISMISSED_KEY, JSON.stringify(ids)); } catch { /* private mode */ }
  }

  async _loadTasks() {
    try {
      const res = await fetch(`${this._apiBase}/tasks`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const dismissed = this._dismissedTasks();
      this._tasks = (await res.json()).filter(t => !dismissed.has(t.job_id));
      this._syncTaskClock();
    } catch (e) {
      console.warn('Could not load background tasks:', e.message);
    }
  }

  /** Insert or advance one task from a `task_update` event. */
  _upsertTask(task) {
    if (this._dismissedTasks().has(task.job_id)) return;
    const idx  = this._tasks.findIndex(t => t.job_id === task.job_id);
    const prev = idx >= 0 ? this._tasks[idx] : null;
    // Keep the fields the event does not carry (a terminal update has no
    // `started_at`, and the elapsed counter should not reset at the finish line).
    const next = { ...prev, ...task, started_at: task.started_at ?? prev?.started_at ?? null };
    this._tasks = idx >= 0
      ? this._tasks.map((t, i) => (i === idx ? next : t))
      : [...this._tasks, next];

    // A failure stays until dismissed — it is the only place the reason is
    // readable at a glance. Everything else clears itself.
    if (next.state === 'completed' || next.state === 'cancelled') {
      this._scheduleTaskDrop(next.job_id);
    }
    this._syncTaskClock();
  }

  _scheduleTaskDrop(jobId) {
    clearTimeout(this._taskDropTimers.get(jobId));
    this._taskDropTimers.set(jobId, setTimeout(() => {
      this._taskDropTimers.delete(jobId);
      this._tasks = this._tasks.filter(t => t.job_id !== jobId);
      this._syncTaskClock();
    }, ChatSession.TASK_DROP_MS));
  }

  _dismissTask(jobId) {
    this._rememberDismissed(jobId);
    clearTimeout(this._taskDropTimers.get(jobId));
    this._taskDropTimers.delete(jobId);
    this._tasks = this._tasks.filter(t => t.job_id !== jobId);
    this._syncTaskClock();
  }

  /** Stop a running task. The kill lands as a `task_update` like any other end. */
  async _stopTask(jobId) {
    try {
      const res = await fetch(`/api/cron/jobs/${jobId}/kill`, { method: 'POST' });
      if (!res.ok) throw new Error(await res.text() || `HTTP ${res.status}`);
    } catch (e) {
      console.warn('Could not stop task:', e.message);
    }
  }

  /** The 1 s clock runs only while something is actually running. */
  _syncTaskClock() {
    const running = this._tasks.some(t => t.state === 'running');
    if (running && !this._taskTimer) {
      this._taskTimer = setInterval(() => { this._taskTick++; }, 1000);
    } else if (!running) {
      this._stopTaskClock();
    }
  }

  _stopTaskClock() {
    clearInterval(this._taskTimer);
    this._taskTimer = null;
  }

  // ── What those tasks are waiting on ───────────────────────────────────────────
  //
  // An async sub-agent runs in a session of its own, so the rich per-session
  // events that draw the inline approval card (`approval_required`,
  // `agent_question`) never reach this socket — only the id-only inbox lifecycle
  // events do, which is why this is a fetch and not an event payload. The chat
  // renders the first pending item above the task strip: outside the transcript,
  // in a fixed position, so the conversation stays readable underneath it.
  //
  // The queue needs no state of its own. Resolving an item broadcasts
  // `approval_resolved` / `clarification_resolved`, this list is re-read, and the
  // next item becomes the first.

  async _loadTaskInbox() {
    try {
      const res = await fetch(`${this._apiBase}/inbox`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      this._taskInbox = {
        approvals:      data.approvals      ?? [],
        clarifications: data.clarifications ?? [],
      };
    } catch (e) {
      console.warn('Could not load background-task inbox:', e.message);
    }
  }

  // Cards the user has waved away with the ✕. Dismissing is *not* answering:
  // the item stays pending, the task stays blocked, and the Inbox stays the
  // place to deal with it — this only says "not in front of my chat". Persisted
  // for the same reason the strip persists dismissed tasks: the endpoint keeps
  // reporting the item until it is resolved, so an in-memory set would put the
  // card back on the next reload.
  static _DISMISSED_ASKS_KEY = 'skald.dismissedAsks';

  /** Identity for dismissal. Post-restart items carry `request_id: 0`, so they
   *  key on the durable `tool_call_id` instead — otherwise every one of them
   *  would share a single key and dismissing one would hide them all. */
  static askKey(item) {
    return item.request_id
      ? `${item.kind}-${item.request_id}`
      : `${item.kind}-tc${item.tool_call_id}`;
  }

  _dismissedAsks() {
    try {
      return new Set(JSON.parse(localStorage.getItem(ChatSession._DISMISSED_ASKS_KEY) ?? '[]'));
    } catch { return new Set(); }
  }

  _dismissAsk(item) {
    const keys = [...this._dismissedAsks(), ChatSession.askKey(item)].slice(-50);
    try { localStorage.setItem(ChatSession._DISMISSED_ASKS_KEY, JSON.stringify(keys)); } catch { /* private mode */ }
    this.requestUpdate();
  }

  /** Approvals first: a blocked tool call is more urgent than an open question. */
  get _taskPendingAll() {
    return [
      ...this._taskInbox.approvals.map(x      => ({ ...x, kind: 'approval' })),
      ...this._taskInbox.clarifications.map(x => ({ ...x, kind: 'clarification' })),
    ];
  }

  /** What the chat still offers to show. */
  get _taskPending() {
    const dismissed = this._dismissedAsks();
    return this._taskPendingAll.filter(x => !dismissed.has(ChatSession.askKey(x)));
  }

  /** Waved away but still waiting — the strip keeps a pointer to the Inbox for these. */
  get _taskPendingHidden() {
    return this._taskPendingAll.length - this._taskPending.length;
  }

  /** `InboxCardsMixin` hook: a resolved item leaves this list, and may reveal the next. */
  async _afterInboxResolve() {
    await this._loadTaskInbox();
  }

  // ── WebSocket ─────────────────────────────────────────────────────────────────

  _connectWS() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/api/ws?${this._wsQuery()}`);
    this._ws = ws;
    ws.onopen = () => {
      // After an auto-reconnect, reconcile tool state: a terminal event
      // (tool_done / tool_error) delivered while the socket was down is lost —
      // the server bus is a broadcast with no replay — so a card could otherwise
      // stay 'running' forever until a manual reload. Re-fetch history and advance
      // any locally-unfinished card that has since reached a terminal state.
      if (this._reconnecting) {
        this._reconnecting = false;
        this._resyncOnReconnect();
        // A task that ended while the socket was down emitted its `task_update`
        // into the void: re-read the authoritative list — and with it whatever
        // those tasks started asking for in the meantime.
        this._loadTasks();
        this._loadTaskInbox();
      }
      if (this._hasPendingTools) {
        ws.send(JSON.stringify({ type: 'resume' }));
        this._hasPendingTools = false;
      }
    };
    ws.onmessage = (ev) => this._handleServerMsg(JSON.parse(ev.data));
    ws.onclose   = ()   => { this._reconnecting = true; this._scheduleReconnect(); };
  }

  /**
   * Reconnect after an unexpected close — unless we were dropped because the
   * server no longer knows this browser. Sessions live in the server's RAM
   * (blueprint §9), so a restart refuses the WS upgrade, and a refused upgrade
   * reaches `onclose` looking exactly like a flaky network: the loop used to
   * retry every 2 s forever behind "Not connected", against a server that would
   * never accept it again. So ask first, and let the re-login dialog take it
   * from there — the socket comes back on `auth-restored`.
   */
  async _scheduleReconnect() {
    if (isSessionExpired()) return;               // the dialog is already up
    if ((await probeSession()) === 'expired') {
      notifySessionExpired();
      // A shell that handles auth itself (native mobile) ignores the report;
      // there is no dialog coming, so keep retrying as before.
      if (isSessionExpired()) return;
    }
    setTimeout(() => this._connectWS(), 2000);
  }

  /**
   * A new session was obtained without leaving the page: reconnect and reconcile
   * like any other unexpected disconnection (the `_reconnecting` flag is what
   * makes `onopen` re-sync tool state that advanced while we were away).
   */
  _onAuthRestored() {
    if (this._ws && this._ws.readyState !== WebSocket.CLOSED) return;
    this._reconnecting = true;
    this._connectWS();
  }

  /**
   * Reconcile tool cards after an unexpected reconnect. Re-fetches the server's
   * message history and advances any locally-unfinished tool card (`running` or
   * `pending`) whose server row has reached a **terminal** state while the socket
   * was down. Only ever moves a card *forward* to a terminal state: a tool still
   * executing reads as `pending`/Interrupted in history and is deliberately left
   * untouched, so this never re-shows a spinner or an approval form for live work.
   */
  async _resyncOnReconnect() {
    let items;
    try {
      const res = await fetch(`${this._apiBase}/messages`);
      if (!res.ok) return;
      items = await res.json();
    } catch { return; }
    // Terminal from the history projection: 'done', or a genuine 'error' (a tool
    // that was merely interrupted mid-run surfaces as error 'Interrupted.' and is
    // NOT terminal — it may still be executing).
    const isTerminal = (it) =>
      it.kind === 'tool' &&
      (it.status === 'done' || (it.status === 'error' && it.error !== 'Interrupted.'));
    for (const it of items) {
      if (!isTerminal(it)) continue;
      const local = this._messages.find(
        m => m.kind === 'tool' && m.tool_call_id === it.tool_call_id
      );
      if (!local || (local.status !== 'running' && local.status !== 'pending')) continue;
      this._updateTool(it.tool_call_id, {
        status:      it.status,
        result:      it.result,
        result_type: it.result_type,
        error:       it.error,
        request_id:  null,
      });
      // Collapse a resolved approval form.
      const expanded = new Set(this._expanded);
      expanded.delete(it.tool_call_id);
      this._expanded = expanded;
    }
  }

  async _startNewSession() {
    if (this._ws) {
      this._ws.onclose = null;
      this._ws.close();
      this._ws = null;
    }
    this._cancelStreamFlush();
    this._messages = [];
    this._waiting  = false;
    // Tasks belong to the conversation that started them; this is a new one.
    this._tasks = [];
    this._stopTaskClock();
    try {
      // Two different meanings of "start over". A source-addressed chat resets its
      // source: the pointer moves to a fresh conversation, so background delivery
      // follows along. An extra tab has no pointer to move — resetting the source
      // would silently restart a *different* chat — so it opens one more
      // conversation and rebinds to it, leaving its source alone.
      const url = this._activeSessionId
        ? `/api/sessions/new?source=${encodeURIComponent(this._source)}`
        : `/api/sessions?source=${encodeURIComponent(this._source)}`;
      const res = await fetch(url, { method: 'POST' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const { session_id } = await res.json();
      if (session_id) {
        const previous = this._activeSessionId;
        if (previous) this._activeSessionId = session_id;
        this._onSessionReplaced(this._source, session_id, previous);
      }
    } catch (e) {
      this._pushError('Could not clear session: ' + e.message);
    }
    this._connectWS();
  }

  /**
   * This chat is now a different conversation — either its source was reset, or an
   * extra tab started over. Anything anchored to the session id (the copilot's tab
   * bar) has to follow it across. `previous` is the id being left behind, null when
   * the chat was addressed by source. No-op here.
   */
  _onSessionReplaced(_source, _sessionId, _previous) {}

  // ── Message handling ──────────────────────────────────────────────────────────

  _handleServerMsg(msg) {
    console.debug('[WS ←]', msg.type, msg);
    // Receiving a live streaming event means a turn is active — restore the STOP
    // button even if we reconnected mid-turn and missed the start. `done`/`error`
    // reset it below.
    if (!this._waiting && ChatSession._STREAMING_EVENTS.has(msg.type)) {
      this._waiting = true;
    }
    switch (msg.type) {
      // Sent on (re)connect: authoritative running state for this session.
      case 'turn_running':
        this._waiting = msg.running;
        break;

      case 'pending_write':
        this._push({
          kind:         'pending_write',
          request_id:   msg.request_id,
          tool_call_id: msg.tool_call_id,
          path:         msg.path,
          old_content:  msg.old_content ?? '',
          new_content:  msg.new_content,
          status:       'pending',
        });
        break;

      case 'thinking': {
        // A tool-call round's text. When the round streamed, its pending bubble
        // becomes the thinking item in place. Reasoning comes from the event
        // (buffered providers) or the streamed accumulation.
        const last = this._messages[this._messages.length - 1];
        const streaming = (last?.kind === 'assistant' && last.streaming) ? last : null;
        const item = { kind: 'thinking', message_id: msg.message_id, content: msg.content,
                       reasoning: msg.reasoning_content ?? streaming?.reasoning ?? null,
                       input_tokens: msg.input_tokens, output_tokens: msg.output_tokens };
        if (streaming) this._replaceLast(item); else this._push(item);
        break;
      }

      case 'token_delta': {
        // Best-effort live tokens. Accumulate into a pending assistant bubble;
        // the final `done` (or `thinking`) event replaces it with authoritative
        // content. Mutate in place + throttled flush: deltas can arrive at a
        // high rate and a full Lit update per token would be wasteful.
        let last = this._messages[this._messages.length - 1];
        if (last?.kind !== 'assistant' || !last.streaming) {
          last = { kind: 'assistant', content: '', reasoning: '', streaming: true };
          this._messages = [...this._messages, last];
          this._onMessagePushed(last);
        }
        if (msg.kind === 'reasoning') last.reasoning += msg.delta;
        else last.content += msg.delta;
        this._scheduleStreamFlush();
        break;
      }

      case 'done': {
        this._waiting = false;
        const last = this._messages[this._messages.length - 1];
        if (last?.kind === 'assistant' && last.streaming) {
          // Finalize the streamed bubble with the authoritative content.
          this._replaceLast({ kind: 'assistant', content: msg.content,
                              reasoning: msg.reasoning_content ?? last.reasoning ?? null,
                              input_tokens: msg.input_tokens, output_tokens: msg.output_tokens });
        } else {
          this._push({ kind: 'assistant', content: msg.content,
                       reasoning: msg.reasoning_content ?? null,
                       input_tokens: msg.input_tokens, output_tokens: msg.output_tokens });
        }
        break;
      }

      case 'tool_start': {
        // A round with tool calls but no Thinking event (no usage/text) leaves a
        // reasoning-only streaming bubble behind: finalize it in place so its
        // content isn't swallowed by the next round's deltas.
        const last = this._messages[this._messages.length - 1];
        if (last?.kind === 'assistant' && last.streaming) {
          this._replaceLast({ kind: 'thinking', content: last.content,
                              reasoning: last.reasoning || null,
                              input_tokens: null, output_tokens: null });
        }
        // On resume, the server re-emits ToolStart for tools already in history.
        // Update in place rather than pushing a duplicate card.
        const existingIdx = this._messages.findIndex(
          m => (m.kind === 'tool') && m.tool_call_id === msg.tool_call_id
        );
        if (existingIdx >= 0) {
          this._updateTool(msg.tool_call_id, { status: 'running', result: null, error: null });
        } else {
          this._push({
            kind:         'tool',
            tool_call_id: msg.tool_call_id,
            name:         msg.name,
            display_name: msg.display_name,
            icon:         msg.icon,
            label_short:  msg.label_short,
            label_full:   msg.label_full,
            path:         msg.path,
            arguments:    msg.arguments,
            status:       'running',
            result:       null,
            error:        null,
          });
        }
        break;
      }

      case 'tool_done':
        // `preview_old`/`preview_new` are present only for a file-write; they let the
        // card render the diff inline even for an auto-allowed write (no PendingWrite).
        this._updateTool(msg.tool_call_id, {
          status: 'done', result: msg.result, result_type: msg.result_type,
          preview_old: msg.preview_old, preview_new: msg.preview_new,
        });
        break;

      case 'tool_error':
        this._updateTool(msg.tool_call_id, { status: 'error', error: msg.error });
        break;

      case 'tool_cancelled':
        // Stopped by the user via /stop — distinct from an error.
        this._updateTool(msg.tool_call_id, { status: 'cancelled' });
        break;

      case 'tool_rejected':
        // Denied by an approval policy or a human — distinct from an error.
        this._updateTool(msg.tool_call_id, { status: 'rejected', error: msg.reason });
        break;

      case 'approval_required':
        this._updateTool(msg.tool_call_id, { status: 'pending', request_id: msg.request_id });
        this._expanded = new Set([...this._expanded, msg.tool_call_id]);
        this._forceScrollToBottom();
        break;

      case 'approval_resolved': {
        const { request_id, tool_call_id, approved } = msg;
        window.dispatchEvent(new CustomEvent('inbox-changed'));
        // Resolved elsewhere (the Inbox page, the phone) or by us — either way
        // the card above the strip has to go, and the next one to appear.
        this._loadTaskInbox();
        this._updatePendingWrite(request_id, { status: approved ? 'approved' : 'rejected' });
        if (tool_call_id != null) {
          if (approved) {
            this._updateTool(tool_call_id, { status: 'running', request_id: null });
          } else {
            this._updateTool(tool_call_id, { status: 'rejected', error: t('chat.rejected') });
          }
          const expanded = new Set(this._expanded);
          expanded.delete(tool_call_id);
          this._expanded = expanded;
        }
        break;
      }

      case 'approval_requested':
      case 'clarification_requested':
      case 'clarification_resolved':
        // Inbox lifecycle from any of this user's sessions (chat, cron,
        // background): nudge listeners (sidebar badge, inbox page) to refresh
        // immediately instead of waiting for the next poll.
        window.dispatchEvent(new CustomEvent('inbox-changed'));
        // These carry ids only, so they cannot say whether the item belongs to
        // one of *our* background tasks — the endpoint answers that. An item
        // raised by this chat's own turn is filtered out server-side and stays
        // where it belongs, inline in the transcript.
        this._loadTaskInbox();
        break;

      case 'elicitation_requested':
      case 'elicitation_resolved':
        // Not attributable to a task: `PendingElicitationInfo` carries no
        // session_id, so the strip cannot claim one. Inbox only, for now.
        window.dispatchEvent(new CustomEvent('inbox-changed'));
        break;

      case 'agent_question':
        // Link the question form to the tool card by updating status + storing request_id.
        this._updateTool(msg.tool_call_id, {
          status:            'pending',
          request_id:        msg.request_id,
          question:          msg.question,
          question_title:    msg.title,
          suggested_answers: msg.suggested_answers ?? [],
        });
        this._expanded = new Set([...this._expanded, msg.tool_call_id]);
        this._forceScrollToBottom();
        break;

      case 'agent_start':
        this._push({
          kind:            'agent',
          stack_id:        msg.stack_id,
          agent_id:        msg.agent_id,
          parent_agent_id: msg.parent_agent_id,
          prompt_preview:  msg.prompt_preview,
          depth:           msg.depth,
          done:            false,
        });
        break;

      case 'agent_done': {
        // A sub-agent's final round emits no Done: its streamed bubble would
        // stay pending forever — finalize it with the accumulated content.
        const last = this._messages[this._messages.length - 1];
        if (last?.kind === 'assistant' && last.streaming) {
          this._replaceLast({ kind: 'assistant', content: last.content,
                              reasoning: last.reasoning || null,
                              input_tokens: null, output_tokens: null });
        }
        this._updateAgent(msg.stack_id, { done: true });
        const agentMsg = this._messages.find(m => m.kind === 'agent' && m.stack_id === msg.stack_id);
        if (agentMsg) {
          this._push({
            kind:            'agent_end',
            agent_id:        msg.agent_id,
            parent_agent_id: msg.parent_agent_id,
            result_preview:  msg.result_preview,
            depth:           agentMsg.depth,
          });
        }
        break;
      }

      case 'truncated':
        this._pushError(`${t('chat.truncated', { tokens: msg.output_tokens?.toLocaleString() ?? '?' })}`);
        break;

      case 'error':
        this._waiting = false;
        this._dropStreaming();
        this._pushError(msg.message);
        break;

      case 'file_changed':
        window.dispatchEvent(new CustomEvent('file-changed', { detail: { path: msg.path } }));
        break;

      case 'open_file': {
        // Agent-driven file open. Every kind — HTML included — routes through the
        // file-viewer page, which renders HTML live in an origin-isolated iframe.
        const p = msg.path ?? '';
        if (p && typeof window.openFile === 'function') window.openFile(p);
        break;
      }

      case 'model_fallback':
        // A fallback mid-stream means the previous attempt's deltas are orphaned:
        // drop the pending bubble — the replacement model streams a fresh one.
        this._dropStreaming();
        this._push({ kind: 'info', content: `⚡ Model fallback: ${msg.from} → ${msg.to}` });
        break;

      case 'user_message':
        // Telnet-style echo: the backend emits this when the message is persisted
        // to history, so we render the bubble here — for the sending client and
        // every other client alike. No dedup needed: regular messages are never
        // rendered optimistically (only slash commands are, and those are never
        // echoed). `message_id` is the real chat_history row id.
        this._push({
          kind:        'user',
          content:     msg.content,
          attachments: msg.attachments ?? [],
          message_id:  msg.message_id,
        });
        break;

      case 'new_session':
        this._cancelStreamFlush();
        this._messages = [];
        this._waiting  = false;
        // A reset from another client of this source: follow the new session id.
        // Source-addressed only — an extra tab never sees this event.
        if (msg.session_id) this._onSessionReplaced(this._source, msg.session_id, null);
        break;

      case 'client_selected':
        // Backend is the single source of truth for the pinned model. Updates
        // arrive here regardless of which client (dropdown, /model command,
        // another tab) originated the change — so the dropdown/select stays
        // in sync. We set the field directly; Lit re-renders because
        // `_selectedClient` is `state: true`.
        this._selectedClient = msg.client;
        break;

      case 'security_group_selected':
        // Twin of `client_selected`: the backend is the source of truth for the
        // session's security-group. Arrives on connect (initial state) and on
        // every change (this tab, another tab, or a role-default), so the picker
        // stays in sync. Direct set — Lit re-renders (`_selectedGroup` is state).
        this._selectedGroup = msg.group;
        break;

      case 'task_update':
        // A background task this conversation started changed state.
        this._upsertTask({
          job_id:     msg.job_id,
          title:      msg.title,
          agent_id:   msg.agent_id,
          session_id: msg.session_id ?? null,
          state:      msg.state,
          error:      msg.error ?? null,
          started_at: msg.state === 'running' ? new Date().toISOString() : null,
        });
        break;

      case 'llm_failed':
        this._waiting = false;
        this._dropStreaming();
        this._pushError(`LLM unavailable. Tried: ${msg.tried.join(', ')}. ${msg.last_error}`);
        break;
    }
  }

  _push(item) {
    console.debug('[push]', item.kind, item);
    this._messages = [...this._messages, item];
    this._onMessagePushed(item);
  }

  // ── Live token streaming ────────────────────────────────────────────────────
  // A pending assistant bubble (`streaming: true`) is mutated in place by
  // `token_delta` events and flushed to Lit at most ~15×/s; turn-ending events
  // (`done`/`thinking`) finalize it via `_replaceLast`, failures drop it.

  _scheduleStreamFlush() {
    if (this._streamFlushTimer) return;
    this._streamFlushTimer = setTimeout(() => {
      this._streamFlushTimer = null;
      this._messages = [...this._messages];
      this._scrollToBottom();
    }, 66);
  }

  _cancelStreamFlush() {
    if (!this._streamFlushTimer) return;
    clearTimeout(this._streamFlushTimer);
    this._streamFlushTimer = null;
  }

  _replaceLast(item) {
    this._cancelStreamFlush();
    const updated = [...this._messages];
    updated[updated.length - 1] = item;
    this._messages = updated;
    this._scrollToBottom();
  }

  _dropStreaming() {
    this._cancelStreamFlush();
    const last = this._messages[this._messages.length - 1];
    if (last?.kind === 'assistant' && last.streaming) {
      this._messages = this._messages.slice(0, -1);
    }
  }

  _pushError(text) {
    this._push({ kind: 'error', content: text });
  }

  _updateAgent(stack_id, patch) {
    const idx = this._messages.findIndex(m => m.kind === 'agent' && m.stack_id === stack_id);
    if (idx < 0) return;
    const updated = [...this._messages];
    updated[idx] = { ...updated[idx], ...patch };
    this._messages = updated;
  }

  _updateTool(tool_call_id, patch) {
    const idx = this._messages.findIndex(
      m => (m.kind === 'tool' || m.kind === 'pending_write') && m.tool_call_id === tool_call_id
    );
    if (idx < 0) return;
    const updated = [...this._messages];
    updated[idx] = { ...updated[idx], ...patch };
    this._messages = updated;
  }

  _updatePendingWrite(request_id, patch) {
    const idx = this._messages.findIndex(
      m => m.kind === 'pending_write' && m.request_id === request_id
    );
    if (idx < 0) return;
    // Once resolved (approved or rejected) remove the block entirely —
    // the tool card already shows the outcome.
    if (patch.status === 'approved' || patch.status === 'rejected') {
      this._messages = this._messages.filter((_, i) => i !== idx);
      return;
    }
    const updated = [...this._messages];
    updated[idx] = { ...updated[idx], ...patch };
    this._messages = updated;
  }

  // ── User input ────────────────────────────────────────────────────────────────

  async _send() {
    const content = this._getInputContent();
    // Text is required; attachments are a complement, never sent on their own.
    // Sending is allowed while a turn is in flight: the message is queued and
    // injected into the running turn at its next round boundary.
    if (!content) return;
    // Don't send while an attachment is still streaming to disk, or its path
    // would be missing from the message.
    if (this._attachments.some(a => a.uploading)) return;
    // Handled entirely over HTTP (and it reconnects the socket itself), so it must
    // stay available precisely when the socket is down.
    if (content === '/new' || content === '/clear') {
      this._clearInput();
      this._attachments = [];
      await this._startNewSession();
      return;
    }

    // Nothing below this point may run while the socket is down: everything from
    // here on is destructive to what the user typed (the input is cleared, the
    // attachment chips are dropped). Bailing first is what keeps a long message
    // recoverable — the composer still holds it, so retrying after the automatic
    // reconnect is one Enter, not a retype.
    if (this._ws?.readyState !== WebSocket.OPEN) {
      this._pushError(t('chat.not_connected'));
      return;
    }
    this._clearInput();

    // Strip client-only fields; the server persists these as message metadata.
    const attachments = this._attachments.map(({ name, path, mimetype, filesize }) =>
      ({ name, path, mimetype, filesize }));
    this._attachments = [];

    // Sending implies the reader wants to follow the conversation: always land at
    // the latest, even if they had scrolled up to read before sending.
    this._forceScrollToBottom();

    // System slash commands reply with a `Done` and never echo back as a
    // `user_message`, so render them optimistically. Regular messages and custom
    // slash commands use telnet-style echo: no local push — the bubble appears only
    // when the backend persists the message and sends it back as a `user_message`
    // event (for a custom command, carrying the typed form as its content), placing
    // it correctly (e.g. after the current round's tools when injected mid-turn).
    if (SYSTEM_SLASH_COMMANDS.has(content.split(/\s+/)[0])) {
      this._push({ kind: 'user', content, attachments });
    }
    this._waiting = true;
    this._ws.send(JSON.stringify({ content, attachments }));
  }

  // ── Attachments ────────────────────────────────────────────────────────────

  /**
   * Upload the given files to `data/uploads/{session}/` and add them as chips.
   * Each file is streamed to disk server-side; while in flight its chip shows a
   * spinner. Accepts a FileList or array of File.
   */
  async _addFiles(files) {
    const list = Array.from(files || []).filter(Boolean);
    if (list.length === 0) return;

    // Optimistic placeholders so the chips appear immediately.
    const pending = list.map(f => ({ name: f.name, filesize: f.size, mimetype: f.type, uploading: true }));
    this._attachments = [...this._attachments, ...pending];

    const form = new FormData();
    for (const f of list) form.append('files', f, f.name);

    try {
      const res = await fetch(`${this._apiBase}/uploads`, { method: 'POST', body: form });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const saved = await res.json(); // [{ name, path, mimetype, filesize }]
      // Replace the placeholders with the saved entries (preserve other chips).
      this._attachments = this._attachments.filter(a => !pending.includes(a)).concat(saved);
    } catch (e) {
      console.error('upload failed:', e);
      // Drop the failed placeholders and surface the error.
      this._attachments = this._attachments.filter(a => !pending.includes(a));
      this._pushError('Upload failed: ' + e.message);
    }
  }

  _removeAttachment(i) {
    this._attachments = this._attachments.filter((_, idx) => idx !== i);
  }

  /** Handler for a paste event: uploads any files on the clipboard. */
  _onPaste(e) {
    const files = e.clipboardData?.files;
    if (files && files.length) {
      e.preventDefault();
      this._addFiles(files);
    }
  }

  /** Handler for a drop event on the composer: uploads the dropped files. */
  _onDrop(e) {
    const files = e.dataTransfer?.files;
    if (files && files.length) {
      e.preventDefault();
      this._addFiles(files);
    }
  }

  /**
   * Pin a client (model) for the current source. Mirrors the state locally for
   * instant feedback, then notifies the backend, which is the single source of
   * truth — it broadcasts `client_selected` back to every client of the source
   * (this tab included), so the dropdown/select re-syncs from authoritative
   * state. Pass `'auto'` to clear the pin.
   */
  _selectClient(client) {
    this._selectedClient = client;
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'select_client', client }));
    }
  }

  /**
   * Load the caller's selectable security-groups (the role's effective set). One
   * fetch; the current selection arrives over the WS (`security_group_selected`),
   * so this only feeds the dropdown's options.
   */
  async _loadSecurityGroups() {
    try {
      const res = await fetch('/api/my/security-groups');
      if (!res.ok) return;
      const list = await res.json();
      if (Array.isArray(list)) this._securityGroups = list;
    } catch { /* the picker just stays hidden if this fails */ }
  }

  /**
   * Pick a security-group for the current session. Mirrors [`_selectClient`]: set
   * locally for instant feedback, then notify the backend, which validates against
   * the role, persists on the session, and broadcasts `security_group_selected`
   * back to every client (this tab included) so the picker re-syncs from truth.
   */
  _selectGroup(group) {
    this._selectedGroup = group;
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'select_security_group', group }));
    }
  }

  _cancel() {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'cancel' }));
    }
    this._waiting = false;
  }

  // ── Approval — pending_write (WS) ─────────────────────────────────────────────

  _approve(msg) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'approve_write', request_id: msg.request_id }));
    }
    this._updatePendingWrite(msg.request_id, { status: 'approved' });
  }

  _approveWriteBypass(msg, bypassSecs) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'approve_write', request_id: msg.request_id, bypass_secs: bypassSecs }));
    }
    this._updatePendingWrite(msg.request_id, { status: 'approved' });
  }

  _startReject(msg) {
    this._rejectingId = msg.request_id;
    this._rejectNote  = '';
  }

  _confirmReject(msg) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'reject_write', request_id: msg.request_id, note: this._rejectNote }));
    }
    this._updatePendingWrite(msg.request_id, { status: 'rejected' });
    this._rejectingId = null;
  }

  // ── Approval — tool (WS, live) ────────────────────────────────────────────────

  _approveWsTool(msg) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'approve_tool', request_id: msg.request_id }));
    }
    this._updateTool(msg.tool_call_id, { status: 'running', request_id: null });
    this._rejectingId = null;
  }

  _approveWsToolBypass(msg, bypassSecs) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'approve_tool', request_id: msg.request_id, bypass_secs: bypassSecs }));
    }
    this._updateTool(msg.tool_call_id, { status: 'running', request_id: null });
    this._rejectingId = null;
  }

  _rejectWsTool(msg) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'reject_tool', request_id: msg.request_id, note: this._rejectNote }));
    }
    this._updateTool(msg.tool_call_id, { status: 'rejected', error: t('chat.rejected_by_user') });
    this._rejectingId = null;
  }

  // ── Approval — tool (REST, from history) ─────────────────────────────────────

  async _approveTool(msg) {
    this._updateTool(msg.tool_call_id, { status: 'running' });
    try {
      const res = await fetch(`/api/tools/${msg.tool_call_id}/resolve`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ action: 'approve' }),
      });
      if (!res.ok) {
        this._updateTool(msg.tool_call_id, { status: 'error', error: `Approval failed: ${await res.text()}` });
        return;
      }
      const data = await res.json();
      this._updateTool(msg.tool_call_id, { status: data.status, result: data.result, result_type: data.result_type });
      if (this._ws?.readyState === WebSocket.OPEN) this._ws.send(JSON.stringify({ type: 'resume' }));
    } catch (e) {
      this._updateTool(msg.tool_call_id, { status: 'error', error: String(e) });
    }
  }

  async _rejectTool(msg) {
    this._updateTool(msg.tool_call_id, { status: 'running' });
    try {
      const res = await fetch(`/api/tools/${msg.tool_call_id}/resolve`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ action: 'reject', note: this._rejectNote }),
      });
      if (!res.ok) {
        this._updateTool(msg.tool_call_id, { status: 'error', error: `Rejection failed: ${await res.text()}` });
        return;
      }
      this._updateTool(msg.tool_call_id, { status: 'error', error: 'Rejected by user.' });
      this._rejectingId = null;
      if (this._ws?.readyState === WebSocket.OPEN) this._ws.send(JSON.stringify({ type: 'resume' }));
    } catch (e) {
      this._updateTool(msg.tool_call_id, { status: 'error', error: String(e) });
    }
  }

  // ── Clarification ─────────────────────────────────────────────────────────────

  _answerQuestion(msg) {
    const answer = this._clarificationAnswer.trim();
    if (!answer) return;
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'answer_question', request_id: msg.request_id, answer }));
    }
    this._updateTool(msg.tool_call_id, { status: 'running', request_id: null });
    this._clarificationAnswer = '';
  }

  // ── DOM hooks (override in subclass) ─────────────────────────────────────────

  /** Called after every _push(). Override to handle scrolling, focus, etc. */
  _onMessagePushed(_item) {}

  /** Returns the chat input textarea element. Subclasses must override. */
  _inputEl() { return null; }

  /** Returns the current value of the chat input. */
  _getInputContent() { return this._inputEl()?.value.trim() ?? ''; }

  /** Clears the chat input and resets its auto-resize height. */
  _clearInput() {
    const el = this._inputEl();
    if (!el) return;
    el.value = '';
    el.style.height = 'auto';
  }

  /** Auto-resizes a textarea to fit its content (capped by CSS max-height). */
  _autoResize(el) {
    el.style.height = 'auto';
    el.style.height = el.scrollHeight + 'px';
  }

  /** Returns the scrollable message-list element. Subclasses must override. */
  _messagesContainer() { return null; }

  /**
   * Scrolls the message list to the bottom — unless the reader has scrolled away
   * from the bottom, in which case streaming is left to flow without yanking them
   * back. Pass `force=true` for cases that must always land at the latest content
   * (history load, the chat becoming visible, interactive approval/clarification
   * prompts).
   *
   * Stickiness is tracked as a flag updated by a passive `scroll` listener on the
   * container (attached lazily), NOT by re-measuring the distance on every call:
   * a single fast flush can add more than the threshold of new content, which
   * would otherwise make a stuck reader look "far from the bottom" and kill the
   * auto-scroll. The listener recomputes the flag from the live scroll position,
   * so scrolling back within the band re-arms it.
   */
  _scrollToBottom(force = false) {
    this.updateComplete.then(() => {
      const el = this._messagesContainer();
      if (!el) return;
      // Lazily bind the stickiness sensor to the (possibly recreated) container.
      if (el !== this._scrollEl) {
        this._scrollEl = el;
        el.addEventListener('scroll', () => {
          if (this._scrollEl !== el) return;
          this._stickToBottom =
            (el.scrollHeight - el.scrollTop - el.clientHeight) <= SCROLL_STICKY_PX;
          this._updateJumpButton();
        }, { passive: true });
      }
      if (force || this._stickToBottom) {
        el.scrollTop = el.scrollHeight;
        this._stickToBottom = true;
        this._updateJumpButton();
      }
    });
  }

  /** Always scrolls to the bottom regardless of the reader's position. */
  _forceScrollToBottom() { this._scrollToBottom(true); }

  /** "Jump to latest" handler: force-scroll and re-stick. */
  _jumpToBottom() { this._forceScrollToBottom(); }

  /** Toggles the `_showJump` state from the current stickiness (only on change). */
  _updateJumpButton() {
    const show = !this._stickToBottom && this._messages.length > 0;
    if (show !== this._showJump) this._showJump = show;
  }

  // ── Voice recording (shared by every chat surface) ────────────────────────────

  async _checkTranscribe() {
    try {
      const r = await fetch('/api/transcribe/has');
      this._hasTranscribe = r.status === 204;
    } catch {
      this._hasTranscribe = false;
    }
  }

  /**
   * Why the microphone can be missing even when the server has a transcribe
   * model: `navigator.mediaDevices` is only exposed in a **secure context**
   * (HTTPS, or localhost/127.0.0.1) — over http on a LAN address the property
   * is `undefined`, not a denied permission. That used to die in the `catch`
   * below as a bare console line, leaving the button frozen with no
   * explanation, so the unavailable cases are named here instead of guessed at.
   */
  _micUnavailableReason() {
    if (!navigator.mediaDevices?.getUserMedia) {
      return window.isSecureContext === false
        ? t('chat.mic.insecure')
        : t('chat.mic.unsupported');
    }
    if (typeof MediaRecorder === 'undefined') return t('chat.mic.unsupported');
    return null;
  }

  async _startRecording(fromShortcut = false) {
    if (this._recording) return;
    const unavailable = this._micUnavailableReason();
    if (unavailable) {
      this._pushError(unavailable);
      return;
    }
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      this._audioChunks      = [];
      this._shortcutRecording = fromShortcut;

      const mimeType = MediaRecorder.isTypeSupported('audio/webm;codecs=opus')
        ? 'audio/webm;codecs=opus'
        : MediaRecorder.isTypeSupported('audio/webm')
          ? 'audio/webm'
          : '';

      this._mediaRecorder = mimeType
        ? new MediaRecorder(stream, { mimeType })
        : new MediaRecorder(stream);

      this._mediaRecorder.addEventListener('dataavailable', e => {
        if (e.data.size > 0) this._audioChunks.push(e.data);
      });

      this._mediaRecorder.addEventListener('stop', () => {
        stream.getTracks().forEach(t => t.stop());
        this._submitAudio();
      });

      this._mediaRecorder.start();
      this._recording = true;
    } catch (err) {
      console.error('mic error:', err);
      this._pushError(
        err?.name === 'NotAllowedError' || err?.name === 'SecurityError'
          ? t('chat.mic.denied')
          : t('chat.mic.failed', { error: err?.message || err?.name || 'unknown' }),
      );
    }
  }

  _stopRecording() {
    if (!this._recording || !this._mediaRecorder) return;
    this._mediaRecorder.stop();
    this._recording = false;
  }

  /** Toggle button handler: start or stop a recording (button-initiated). */
  _toggleRecording() {
    if (this._recording) {
      this._shortcutRecording = false;
      this._stopRecording();
    } else {
      this._startRecording(false);
    }
  }

  async _submitAudio() {
    if (this._audioChunks.length === 0) return;

    const mimeType = this._mediaRecorder?.mimeType ?? 'audio/webm';
    const blob = new Blob(this._audioChunks, { type: mimeType });
    // Derive file extension from mimeType, e.g. "audio/webm;codecs=opus" → "webm"
    const ext = mimeType.split('/')[1]?.split(';')[0] ?? 'webm';

    const form = new FormData();
    form.append('audio', blob, `recording.${ext}`);

    try {
      const resp = await fetch('/api/transcribe/audio', { method: 'POST', body: form });
      if (!resp.ok) throw new Error(await resp.text());
      const { text } = await resp.json();
      if (text) {
        const ta = this._inputEl();
        if (ta) {
          ta.value = (ta.value ? ta.value + ' ' : '') + text;
          this._autoResize(ta);
          ta.focus();
        }
      }
    } catch (err) {
      console.error('transcription error:', err);
    }
  }
}
