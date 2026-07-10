import { LightElement } from './base.js';

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

/**
 * Base class for chat UI components (desktop copilot, mobile chat page).
 *
 * Contains all WebSocket logic, message state, and approval handling.
 * Subclasses implement the render() method and override the DOM hooks:
 *   - _scrollToBottom()
 *   - _getInputContent()  → returns current input value
 *   - _clearInput()       → empties the input
 *   - _onMessagePushed(item) → called after each push (scroll, focus, etc.)
 */
export class ChatSession extends LightElement {
  static properties = {
    _messages:           { state: true },
    _waiting:            { state: true },
    _expanded:           { state: true },
    _providers:          { state: true },
    _selectedClient:     { state: true },
    _rejectingId:           { state: true },
    _rejectNote:            { state: true },
    _clarificationAnswer:   { state: true },
    // Voice recording state (shared by every chat surface).
    _hasTranscribe:      { state: true },
    _recording:          { state: true },
    // Pending attachments for the message being composed (shown as chips above
    // the textarea; uploaded to disk on selection, sent with the next message).
    _attachments:        { state: true },
  };

  // Live events whose arrival implies a turn is in flight (used to restore the
  // STOP button when reconnecting mid-turn).
  static _STREAMING_EVENTS = new Set([
    'thinking', 'tool_start', 'agent_start', 'pending_write', 'approval_required',
  ]);

  constructor() {
    super();
    this._messages          = [];
    this._waiting           = false;
    this._expanded          = new Set();
    this._ws                = null;
    this._providers         = [];
    this._selectedClient    = null;
    this._rejectingId           = null;
    this._rejectNote            = '';
    this._clarificationAnswer   = '';
    // Runtime-selected source. When null, falls back to the static `_wsSource`.
    // Lets a single chat component switch between sessions (e.g. copilot tabs).
    this._activeSource          = null;
    // Voice recording state. Shared so every surface (desktop copilot + mobile
    // chat) can expose the same mic button. The desktop-only Ctrl+Space push-
    // to-talk shortcut is wired in `app-copilot`; `_shortcutRecording` tracks
    // whether a recording session was started by that shortcut.
    this._hasTranscribe     = false;
    this._recording         = false;
    this._shortcutRecording = false;
    this._mediaRecorder     = null;
    this._audioChunks       = [];
    // Each entry: { name, path, mimetype, filesize, uploading? }. While an upload
    // is in flight the entry has `uploading: true` and no `path` yet.
    this._attachments       = [];
  }

  async connectedCallback() {
    super.connectedCallback();
    // Fire-and-forget: availability of a transcription provider determines
    // whether the mic button is rendered at all.
    this._checkTranscribe();
    await Promise.all([this._loadProviders(), this._loadHistory()]);
    this._connectWS();
  }

  // ── Source identity — override in subclass ────────────────────────────────────

  // Static default source for this component. Subclasses override (e.g. 'mobile').
  get _wsSource() { return 'web'; }

  // Effective source: the runtime-selected one, or the static default.
  get _source() { return this._activeSource ?? this._wsSource; }

  /**
   * Switch the live connection to a different source: tear down the current WS,
   * swap source, reload that source's history, and reconnect. Used to move
   * between sessions (e.g. General ↔ a project chat) without remounting.
   */
  async _switchSource(source) {
    if (this._ws) { this._ws.onclose = null; this._ws.close(); this._ws = null; }
    this._activeSource = source;
    this._messages = [];
    this._waiting  = false;
    await this._loadHistory();
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
    }
  }

  async _loadHistory() {
    try {
      const res = await fetch(`/api/${this._source}/messages`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const items = await res.json();
      if (items.length > 0) {
        this._messages = items;
        const expanded = new Set(this._expanded);
        for (const m of items) {
          if (m.kind === 'tool' && m.status === 'pending') expanded.add(m.tool_call_id);
        }
        this._expanded = expanded;
        this._scrollToBottom();
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

  // ── WebSocket ─────────────────────────────────────────────────────────────────

  _connectWS() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/api/ws?source=${this._source}`);
    this._ws = ws;
    ws.onopen = () => {
      if (this._hasPendingTools) {
        ws.send(JSON.stringify({ type: 'resume' }));
        this._hasPendingTools = false;
      }
    };
    ws.onmessage = (ev) => this._handleServerMsg(JSON.parse(ev.data));
    ws.onclose   = ()   => setTimeout(() => this._connectWS(), 2000);
  }

  async _startNewSession() {
    if (this._ws) {
      this._ws.onclose = null;
      this._ws.close();
      this._ws = null;
    }
    this._messages = [];
    this._waiting  = false;
    try {
      const res = await fetch(`/api/sessions?source=${this._source}`, { method: 'POST' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
    } catch (e) {
      this._pushError('Could not clear session: ' + e.message);
    }
    this._connectWS();
  }

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

      case 'thinking':
        this._push({ kind: 'thinking', message_id: msg.message_id, content: msg.content,
                     input_tokens: msg.input_tokens, output_tokens: msg.output_tokens });
        break;

      case 'done':
        this._waiting = false;
        this._push({ kind: 'assistant', content: msg.content,
                     input_tokens: msg.input_tokens, output_tokens: msg.output_tokens });
        break;

      case 'tool_start': {
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
        this._updateTool(msg.tool_call_id, { status: 'done', result: msg.result, result_type: msg.result_type });
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
        this.updateComplete.then(() => this._scrollToBottom());
        break;

      case 'approval_resolved': {
        const { request_id, tool_call_id, approved } = msg;
        this._updatePendingWrite(request_id, { status: approved ? 'approved' : 'rejected' });
        if (tool_call_id != null) {
          if (approved) {
            this._updateTool(tool_call_id, { status: 'running', request_id: null });
          } else {
            this._updateTool(tool_call_id, { status: 'rejected', error: 'Rifiutato.' });
          }
          const expanded = new Set(this._expanded);
          expanded.delete(tool_call_id);
          this._expanded = expanded;
        }
        break;
      }

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
        this.updateComplete.then(() => this._scrollToBottom());
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
        this._pushError(`Risposta troncata dal limite di token (↓${msg.output_tokens?.toLocaleString() ?? '?'} tok).`);
        break;

      case 'error':
        this._waiting = false;
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
        this._messages = [];
        this._waiting  = false;
        break;

      case 'client_selected':
        // Backend is the single source of truth for the pinned model. Updates
        // arrive here regardless of which client (dropdown, /model command,
        // another tab) originated the change — so the dropdown/select stays
        // in sync. We set the field directly; Lit re-renders because
        // `_selectedClient` is `state: true`.
        this._selectedClient = msg.client;
        break;

      case 'llm_failed':
        this._waiting = false;
        this._pushError(`LLM unavailable. Tried: ${msg.tried.join(', ')}. ${msg.last_error}`);
        break;
    }
  }

  _push(item) {
    console.debug('[push]', item.kind, item);
    this._messages = [...this._messages, item];
    this._onMessagePushed(item);
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
    this._clearInput();

    if (content === '/new' || content === '/clear') {
      this._attachments = [];
      await this._startNewSession();
      return;
    }

    // Strip client-only fields; the server persists these as message metadata.
    const attachments = this._attachments.map(({ name, path, mimetype, filesize }) =>
      ({ name, path, mimetype, filesize }));
    this._attachments = [];

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

    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ content, attachments }));
    } else {
      this._pushError('Not connected — reconnecting, please retry.');
      this._waiting = false;
    }
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
      const res = await fetch(`/api/${this._source}/uploads`, { method: 'POST', body: form });
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
    this._updateTool(msg.tool_call_id, { status: 'rejected', error: "Rifiutato dall'utente." });
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

  /** Scrolls the message list to the bottom. */
  _scrollToBottom() {}

  // ── Voice recording (shared by every chat surface) ────────────────────────────

  async _checkTranscribe() {
    try {
      const r = await fetch('/api/transcribe/has');
      this._hasTranscribe = r.status === 204;
    } catch {
      this._hasTranscribe = false;
    }
  }

  async _startRecording(fromShortcut = false) {
    if (this._recording) return;
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
