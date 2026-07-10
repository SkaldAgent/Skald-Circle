import { html, nothing }  from 'lit';
import { unsafeHTML }      from 'lit/directives/unsafe-html.js';
import { renderMarkdown }  from '../lib/base.js';
import { openFile }        from '../lib/open-file.js';

// ── Utilities ────────────────────────────────────────────────────────────────

/**
 * Render a tool label (with backtick-wrapped arguments) as Lit nodes. Each
 * backtick segment becomes a `<code>`; the segment that exactly matches `path`
 * — the file a file-targeting tool acts on, supplied by the backend
 * `target_path` — instead becomes a link that opens it in the file viewer.
 * Lit auto-escapes text, so no manual HTML escaping is needed.
 */
function renderLabel(label, path) {
  const out = [];
  let rest = label || '';
  while (rest.length) {
    const open = rest.indexOf('`');
    if (open === -1) { out.push(rest); break; }
    if (open > 0) out.push(rest.slice(0, open));
    rest = rest.slice(open + 1);
    const close = rest.indexOf('`');
    if (close === -1) { out.push('`' + rest); break; }
    out.push(renderPath(rest.slice(0, close), path));
    rest = rest.slice(close + 1);
  }
  return out;
}

/** A backtick segment: a clickable file link when it is the call's target path, else plain `<code>`. */
function renderPath(seg, path) {
  if (!path || seg !== path) return html`<code>${seg}</code>`;
  const open = (e) => { e.stopPropagation(); openFile(seg); };
  return html`<span class="copilot-tool-path" role="button" tabindex="0"
    title="Open in viewer"
    @click=${open}
    @keydown=${(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); open(e); } }}
  >${seg}</span>`;
}

export function truncate(s, max = 400) {
  if (!s) return '';
  const str = typeof s === 'string' ? s : JSON.stringify(s, null, 2);
  return str.length > max ? str.slice(0, max) + '\n…' : str;
}

/**
 * Pretty-prints a structured (`result_type === 'json'`) tool result for display.
 * The backend stores `structuredContent` as a compact JSON string; re-indent it
 * for readability, falling back to the raw string if it isn't valid JSON.
 */
function prettyJson(s) {
  try { return JSON.stringify(JSON.parse(s), null, 2); }
  catch { return s; }
}

// ── Diff ─────────────────────────────────────────────────────────────────────

export function renderDiff(oldText, newText) {
  const oldLines = (oldText || '').split('\n');
  const newLines = (newText || '').split('\n');

  const m = oldLines.length, n = newLines.length;
  const dp = Array.from({ length: m + 1 }, () => new Array(n + 1).fill(0));
  for (let i = 1; i <= m; i++)
    for (let j = 1; j <= n; j++)
      dp[i][j] = oldLines[i-1] === newLines[j-1]
        ? dp[i-1][j-1] + 1
        : Math.max(dp[i-1][j], dp[i][j-1]);

  const ops = [];
  let i = m, j = n;
  while (i > 0 || j > 0) {
    if (i > 0 && j > 0 && oldLines[i-1] === newLines[j-1]) {
      ops.push({ type: 'eq',  text: oldLines[i-1] }); i--; j--;
    } else if (j > 0 && (i === 0 || dp[i][j-1] >= dp[i-1][j])) {
      ops.push({ type: 'add', text: newLines[j-1] }); j--;
    } else {
      ops.push({ type: 'del', text: oldLines[i-1] }); i--;
    }
  }
  ops.reverse();

  const result = [];
  let eqBuf = [];
  const flushEq = () => {
    if (eqBuf.length === 0) return;
    if (eqBuf.length <= 6) {
      result.push(html`<span class="diff-unchanged">${eqBuf.join('\n')}\n</span>`);
    } else {
      result.push(html`<span class="diff-unchanged">${eqBuf.slice(0, 3).join('\n')}\n</span>`);
      result.push(html`<span class="diff-ellipsis">⋯ ${eqBuf.length - 6} unchanged lines ⋯</span>`);
      result.push(html`<span class="diff-unchanged">\n${eqBuf.slice(-3).join('\n')}\n</span>`);
    }
    eqBuf = [];
  };
  for (const op of ops) {
    if (op.type === 'eq') {
      eqBuf.push(op.text);
    } else {
      flushEq();
      const cls = op.type === 'add' ? 'diff-added' : 'diff-removed';
      result.push(html`<span class="${cls}">${op.text}\n</span>`);
    }
  }
  flushEq();
  return result;
}

// ── Message renderers ────────────────────────────────────────────────────────

export function renderPendingWrite(host, msg) {
  console.debug('[renderPendingWrite]', msg.path, 'old_len=' + (msg.old_content?.length ?? 0), 'new_len=' + (msg.new_content?.length ?? 0));
  const isRejecting = host._rejectingId === msg.request_id;
  return html`
    <div class="copilot-approval copilot-approval--${msg.status}">
      <div class="copilot-approval-header">
        <i class="bi bi-pencil-square"></i>
        <span class="copilot-approval-path copilot-tool-path" role="button" tabindex="0"
          title="Open in viewer"
          @click=${() => openFile(msg.path)}
          @keydown=${(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openFile(msg.path); } }}
        >${msg.path}</span>
        ${msg.status === 'pending'
          ? html`<span class="badge bg-warning text-dark ms-auto">Pending approval</span>`
          : msg.status === 'approved'
            ? html`<span class="badge bg-success ms-auto">Approved</span>`
            : html`<span class="badge bg-danger ms-auto">Rejected</span>`}
      </div>

      <pre class="copilot-diff">${renderDiff(msg.old_content, msg.new_content)}</pre>

      ${msg.status === 'pending' ? html`
        <div class="copilot-approval-actions">
          ${isRejecting ? html`
            <textarea
              class="form-control form-control-sm copilot-reject-note"
              rows="2"
              placeholder="Optional: explain why you rejected this (sent to the LLM)"
              .value=${host._rejectNote}
              @input=${(e) => { host._rejectNote = e.target.value; }}
            ></textarea>
            <div class="copilot-approval-btns">
              <button class="btn btn-sm btn-danger" @click=${() => host._confirmReject(msg)}>
                <i class="bi bi-x-circle me-1"></i>Confirm reject
              </button>
              <button class="btn btn-sm btn-outline-secondary" @click=${() => { host._rejectingId = null; }}>
                Cancel
              </button>
            </div>
          ` : html`
            <div class="copilot-approval-btns">
              <button class="btn btn-sm btn-success" @click=${() => host._approve(msg)}>
                <i class="bi bi-check-circle me-1"></i>Approve
              </button>
              <button class="btn btn-sm btn-outline-danger" @click=${() => host._startReject(msg)}>
                <i class="bi bi-x-circle me-1"></i>Reject
              </button>
              <button class="btn btn-sm btn-outline-secondary" title="Approve and skip similar approvals for 15 minutes"
                @click=${() => host._approveWriteBypass(msg, 900)}>
                <i class="bi bi-clock me-1"></i>15 min
              </button>
              <button class="btn btn-sm btn-outline-secondary" title="Approve and skip all approvals for this session"
                @click=${() => host._approveWriteBypass(msg, 0)}>
                <i class="bi bi-arrow-repeat me-1"></i>Session
              </button>
            </div>
          `}
        </div>
      ` : nothing}
    </div>
  `;
}

export function renderTool(host, msg) {
  const isOpen  = host._expanded.has(msg.tool_call_id);
  const argsStr = truncate(msg.arguments);
  const isPending   = msg.status === 'pending';
  const isRejecting = isPending && host._rejectingId === msg.tool_call_id;

  const statusIcon =
    msg.status === 'running'
      ? html`<span class="spinner-border spinner-border-sm" role="status"></span>`
    : isPending
      ? html`<span class="spinner-border spinner-border-sm text-warning" role="status" title="Awaiting approval"></span>`
    : msg.status === 'done'
      ? html`<i class="bi bi-check-circle-fill text-success"></i>`
    : msg.status === 'cancelled'
      ? html`<i class="bi bi-slash-circle-fill text-secondary" title="Cancelled by user"></i>`
    : msg.status === 'rejected'
      ? html`<i class="bi bi-shield-fill-x text-warning" title="Denied by policy"></i>`
      : html`<i class="bi bi-x-circle-fill text-danger"></i>`;

  return html`
    <div class="copilot-tool ${isPending ? 'copilot-tool--pending' : ''}">
      <button class="copilot-tool-header" @click=${() => host._toggleExpand(msg.tool_call_id)}>
        <span class="copilot-tool-status">${statusIcon}</span>
        <span class="copilot-tool-name">${renderLabel(msg.label_full || msg.name, msg.path)}</span>
        ${isPending ? html`<span class="badge bg-warning text-dark ms-2">Pending approval</span>` : nothing}
        <i class="bi bi-chevron-${isOpen ? 'up' : 'down'} ms-auto"></i>
      </button>
      ${isOpen ? html`
        <div class="copilot-tool-body">
          ${!(isPending && msg.name === 'ask_user_clarification') ? html`
            <div class="copilot-tool-section">
              <span class="copilot-tool-label">args</span>
              <pre class="copilot-tool-pre">${argsStr}</pre>
            </div>
          ` : nothing}
          ${isPending ? (msg.name === 'ask_user_clarification' ? html`
            <div class="copilot-approval-actions">
              ${msg.question_title ? html`<div class="copilot-clarification-title">${msg.question_title}</div>` : nothing}
              <div class="copilot-clarification-question copilot-markdown">${unsafeHTML(renderMarkdown(msg.question ?? msg.arguments?.question ?? ''))}</div>
              ${(msg.suggested_answers ?? []).length > 0 ? html`
                <div class="copilot-clarification-chips">
                  ${(msg.suggested_answers ?? []).map(s => html`
                    <button class="btn btn-sm btn-outline-secondary copilot-chip"
                      @click=${() => { host._clarificationAnswer = s; }}>
                      ${s}
                    </button>
                  `)}
                </div>
              ` : nothing}
              <div class="copilot-clarification-input-row">
                <textarea
                  class="form-control form-control-sm copilot-reject-note"
                  rows="2"
                  placeholder="Type your answer…"
                  .value=${host._clarificationAnswer}
                  @input=${(e) => { host._clarificationAnswer = e.target.value; }}
                  @keydown=${(e) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); host._answerQuestion(msg); } }}
                ></textarea>
                <button class="btn btn-sm btn-primary ms-2"
                  @click=${() => host._answerQuestion(msg)}
                  ?disabled=${!host._clarificationAnswer.trim()}>
                  <i class="bi bi-send me-1"></i>Send
                </button>
              </div>
            </div>
          ` : html`
            <div class="copilot-approval-actions">
              ${isRejecting ? html`
                <textarea
                  class="form-control form-control-sm copilot-reject-note"
                  rows="2"
                  placeholder="Reason for rejection (optional, sent to the LLM)"
                  .value=${host._rejectNote}
                  @input=${(e) => { host._rejectNote = e.target.value; }}
                ></textarea>
                <div class="copilot-approval-btns">
                  <button class="btn btn-sm btn-danger"
                    @click=${() => msg.request_id != null ? host._rejectWsTool(msg) : host._rejectTool(msg)}>
                    <i class="bi bi-x-circle me-1"></i>Confirm reject
                  </button>
                  <button class="btn btn-sm btn-outline-secondary"
                    @click=${() => { host._rejectingId = null; }}>
                    Cancel
                  </button>
                </div>
              ` : html`
                <div class="copilot-approval-btns">
                  <button class="btn btn-sm btn-success"
                    @click=${(e) => { e.stopPropagation(); msg.request_id != null ? host._approveWsTool(msg) : host._approveTool(msg); }}>
                    <i class="bi bi-check-circle me-1"></i>Approve
                  </button>
                  <button class="btn btn-sm btn-outline-danger"
                    @click=${(e) => { e.stopPropagation(); host._rejectingId = msg.tool_call_id; host._rejectNote = ''; }}>
                    <i class="bi bi-x-circle me-1"></i>Reject
                  </button>
                  ${msg.request_id != null ? html`
                    <button class="btn btn-sm btn-outline-secondary" title="Approve and skip similar approvals for 15 minutes"
                      @click=${(e) => { e.stopPropagation(); host._approveWsToolBypass(msg, 900); }}>
                      <i class="bi bi-clock me-1"></i>15 min
                    </button>
                    <button class="btn btn-sm btn-outline-secondary" title="Approve and skip all approvals for this session"
                      @click=${(e) => { e.stopPropagation(); host._approveWsToolBypass(msg, 0); }}>
                      <i class="bi bi-arrow-repeat me-1"></i>Session
                    </button>
                  ` : nothing}
                </div>
              `}
            </div>
          `) : msg.status !== 'running' ? (
            msg.status === 'done' && msg.result_type === 'json' ? html`
            <div class="copilot-tool-section">
              <span class="copilot-tool-label copilot-tool-label--done">result · json</span>
              <pre class="copilot-tool-pre copilot-tool-pre--done copilot-tool-pre--json">${
                truncate(prettyJson(msg.result))
              }</pre>
            </div>
          ` : html`
            <div class="copilot-tool-section">
              <span class="copilot-tool-label copilot-tool-label--${msg.status}">
                ${msg.status === 'done' ? 'result' : 'error'}
              </span>
              <pre class="copilot-tool-pre copilot-tool-pre--${msg.status}">${
                truncate(msg.status === 'done' ? msg.result : msg.error)
              }</pre>
            </div>
          `) : nothing}
        </div>
      ` : nothing}
    </div>
  `;
}

export function renderAgent(msg) {
  const icon = msg.done ? 'check2-all' : 'arrow-right-circle';
  return html`
    <div class="copilot-agent" style="--agent-depth:${Math.min(msg.depth, 4)}">
      <div class="copilot-agent-header">
        <i class="bi bi-${icon}"></i>
        <span>
          <strong>${msg.parent_agent_id ?? 'main'}</strong>
          <i class="bi bi-arrow-right mx-1" style="font-size:0.7rem"></i>
          <strong>${msg.agent_id}</strong>
        </span>
        ${msg.done ? html`<span class="copilot-agent-badge done">done</span>` : html`<span class="copilot-agent-badge running">running…</span>`}
      </div>
      ${msg.prompt_preview ? html`
        <pre class="copilot-agent-preview">${msg.prompt_preview}</pre>
      ` : nothing}
    </div>
  `;
}

export function renderAgentEnd(msg) {
  return html`
    <div class="copilot-agent-end" style="--agent-depth:${Math.min(msg.depth, 4)}">
      <div class="copilot-agent-header">
        <i class="bi bi-arrow-return-left"></i>
        <span>
          <strong>${msg.agent_id}</strong>
          <i class="bi bi-arrow-right mx-1" style="font-size:0.7rem"></i>
          <strong>${msg.parent_agent_id ?? 'main'}</strong>
        </span>
        <span class="copilot-agent-badge done">finished</span>
      </div>
      ${msg.result_preview ? html`
        <pre class="copilot-agent-preview copilot-agent-preview--result">${msg.result_preview}</pre>
      ` : nothing}
    </div>
  `;
}

function failedBadge() {
  return html`<span class="copilot-failed-badge" title="This message is not sent to the LLM">
    <i class="bi bi-exclamation-triangle-fill"></i>
  </span>`;
}

// ── Attachment chips ───────────────────────────────────────────────────────────

/** Bootstrap icon class for a file based on its MIME type / extension. */
function attachmentIcon(att) {
  const m = (att.mimetype || '').toLowerCase();
  const n = (att.name || '').toLowerCase();
  if (m.startsWith('image/'))                          return 'bi-file-earmark-image';
  if (m === 'application/pdf' || n.endsWith('.pdf'))   return 'bi-file-earmark-pdf';
  if (m.startsWith('audio/'))                          return 'bi-file-earmark-music';
  if (m.startsWith('video/'))                          return 'bi-file-earmark-play';
  if (m.startsWith('text/') || /\.(md|txt|csv|json|ya?ml|rs|js|ts|py)$/.test(n)) return 'bi-file-earmark-text';
  return 'bi-file-earmark';
}

/** Human-readable file size, e.g. "1.2 MB". */
function fmtSize(bytes) {
  if (bytes == null) return '';
  const u = ['B', 'KB', 'MB', 'GB'];
  let i = 0, n = bytes;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n < 10 && i > 0 ? n.toFixed(1) : Math.round(n)} ${u[i]}`;
}

/**
 * Render a list of attachment chips. Used both above the composer (pending
 * uploads — `removable`, with an × button and a spinner while uploading) and
 * inside a sent user bubble (clickable to open the file). `host` must provide
 * `_removeAttachment(i)` when `removable` is true.
 */
export function renderAttachmentChips(host, attachments, { removable = false } = {}) {
  if (!attachments?.length) return nothing;
  return html`
    <div class="attach-chips">
      ${attachments.map((att, i) => html`
        <div class="attach-chip ${att.uploading ? 'attach-chip--uploading' : ''} ${!removable && att.path ? 'attach-chip--clickable' : ''}"
             title=${att.name}
             @click=${() => { if (!removable && att.path) openFile(att.path); }}>
          ${att.uploading
            ? html`<span class="spinner-border spinner-border-sm"></span>`
            : html`<i class="bi ${attachmentIcon(att)}"></i>`}
          <span class="attach-chip-name">${att.name}</span>
          ${att.filesize != null ? html`<span class="attach-chip-size">${fmtSize(att.filesize)}</span>` : nothing}
          ${removable ? html`
            <button class="attach-chip-remove" title="Remove"
                    @click=${(e) => { e.stopPropagation(); host._removeAttachment(i); }}>
              <i class="bi bi-x"></i>
            </button>` : nothing}
        </div>
      `)}
    </div>`;
}

export function renderMsg(host, msg) {
  try {
    switch (msg.kind) {
      case 'user':
        return html`<div class="copilot-msg user ${msg.failed ? 'copilot-msg--failed' : ''}" style="white-space:pre-wrap">${msg.failed ? failedBadge() : nothing}${msg.content}${renderAttachmentChips(host, msg.attachments)}</div>`;
      case 'thinking':
        return html`
          <div class="copilot-msg assistant copilot-markdown ${msg.failed ? 'copilot-msg--failed' : ''}">
            ${msg.failed ? failedBadge() : nothing}
            ${unsafeHTML(renderMarkdown(msg.content))}
            ${msg.input_tokens != null ? html`<div class="copilot-token-count">↑${msg.input_tokens.toLocaleString()} tok &nbsp;↓${msg.output_tokens?.toLocaleString()} tok</div>` : nothing}
          </div>`;
      case 'assistant':
        return html`
          <div class="copilot-msg assistant copilot-markdown ${msg.failed ? 'copilot-msg--failed' : ''}">
            ${msg.failed ? failedBadge() : nothing}
            ${unsafeHTML(renderMarkdown(msg.content))}
            ${msg.input_tokens != null ? html`<div class="copilot-token-count">↑${msg.input_tokens.toLocaleString()} tok &nbsp;↓${msg.output_tokens?.toLocaleString()} tok</div>` : nothing}
          </div>`;
      case 'error':
        return html`
          <div class="copilot-msg error">
            <i class="bi bi-exclamation-triangle-fill me-1"></i>${msg.content}
          </div>`;
      case 'info':
        return html`
          <div class="copilot-msg info">
            ${msg.content}
          </div>`;
      case 'pending_write':
        return renderPendingWrite(host, msg);
      case 'tool':
        return renderTool(host, msg);
      case 'agent':
        return renderAgent(msg);
      case 'agent_end':
        return renderAgentEnd(msg);
      default:
        return nothing;
    }
  } catch (err) {
    console.error('[renderMsg] kind=' + msg.kind, err);
    return html`<div class="copilot-msg error">Render error [${msg.kind}]: ${err.message}</div>`;
  }
}
