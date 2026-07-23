import { html, nothing }  from 'lit';
import { unsafeHTML }      from 'lit/directives/unsafe-html.js';
import { renderMarkdown }  from '../lib/base.js';
import { openFile }        from '../lib/open-file.js';
import { openToolDetail }  from '../lib/open-tool.js';
import { t }               from '../lib/i18n.js';
import { connectorIconUrl } from './shared/connector-common.js';

// ── Tool icons ─────────────────────────────────────────────────────────────────

/**
 * Maps a tool's semantic `icon` key (from the backend `Tool::icon`) to a Bootstrap
 * glyph + a CSS accent class. The key commits to meaning; the look lives here and in
 * `copilot-messages.css` (theme-aware, no hardcoded colors). Unknown keys fall back
 * to the generic wrench.
 */
const TOOL_ICON = {
  edit:          { glyph: 'bi-pencil-square',      cls: 'tool-ico--edit' },
  read:          { glyph: 'bi-file-earmark-text',  cls: 'tool-ico--read' },
  list:          { glyph: 'bi-folder2-open',       cls: 'tool-ico--list' },
  search:        { glyph: 'bi-search',             cls: 'tool-ico--search' },
  shell:         { glyph: 'bi-terminal',           cls: 'tool-ico--shell' },
  subagent:      { glyph: 'bi-diagram-3',          cls: 'tool-ico--subagent' },
  image:         { glyph: 'bi-image',              cls: 'tool-ico--image' },
  config:        { glyph: 'bi-sliders',            cls: 'tool-ico--config' },
  introspection: { glyph: 'bi-info-circle',        cls: 'tool-ico--introspection' },
  file:          { glyph: 'bi-file-earmark',       cls: 'tool-ico--file' },
  mcp:           { glyph: 'bi-plug',               cls: 'tool-ico--mcp' },
  tool:          { glyph: 'bi-wrench',             cls: 'tool-ico--tool' },
};

/** Whether a tool call carries a file-write diff snapshot to render. */
function hasPreview(msg) {
  return msg.preview_new != null || msg.preview_old != null;
}

/**
 * Whether to render the diff inline on the tool card. Suppressed while a sibling
 * `pending_write` card for the same call is present (it already shows the diff — an
 * approval-gated write). After a reload there is no such card, so the tool card
 * becomes the single place the diff lives. `host` may be absent in bare renders.
 */
function showInlineDiff(host, msg) {
  if (!hasPreview(msg)) return false;
  const siblings = host && host._messages;
  if (Array.isArray(siblings)
      && siblings.some(m => m.kind === 'pending_write' && m.tool_call_id === msg.tool_call_id)) {
    return false;
  }
  return true;
}

/** The MCP server name embedded in an `mcp__<server>__<tool>` id, or null. */
function mcpServerOf(name) {
  if (typeof name !== 'string' || !name.startsWith('mcp__')) return null;
  const rest = name.slice(5);
  const i = rest.indexOf('__');
  return i === -1 ? rest : rest.slice(0, i);
}

/**
 * The leading tool icon for a card. MCP tools show their connector's own icon
 * (parsed from the `mcp__server__tool` id), falling back to a plug glyph if the
 * connector shipped none; every other tool shows its semantic glyph + accent.
 */
function renderToolIcon(msg) {
  const server = mcpServerOf(msg.name);
  if (server) {
    return html`<span class="copilot-tool-ico-wrap">
      <img class="copilot-tool-ico-img" src=${connectorIconUrl(server, 'sm')} alt=""
        @error=${(e) => { const w = e.target.closest('.copilot-tool-ico-wrap'); if (w) w.classList.add('img-failed'); }}>
      <i class="bi bi-plug copilot-tool-ico tool-ico--mcp"></i>
    </span>`;
  }
  const ic = TOOL_ICON[msg.icon] || TOOL_ICON.tool;
  return html`<i class="bi ${ic.glyph} copilot-tool-ico ${ic.cls}"></i>`;
}

/**
 * The muted secondary detail beside a tool's friendly title: the target path
 * (clickable) or the primary argument. Derived from `label_full` by stripping the
 * leading raw tool-name token — which the friendly `display_name` now replaces — so
 * an MCP tool (whose label is just its raw id) shows no redundant secondary.
 */
function toolSecondary(msg) {
  let rest = msg.label_full || '';
  if (msg.name && rest.startsWith(msg.name)) rest = rest.slice(msg.name.length);
  rest = rest.trim();
  if (!rest) return nothing;
  return html`<span class="copilot-tool-detail">${renderLabel(rest, msg.path)}</span>`;
}

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
    title=${t('copilot.open_in_viewer')}
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
      result.push(html`<span class="diff-ellipsis">${t('copilot.unchanged_lines', { n: eqBuf.length - 6 })}</span>`);
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
          title=${t('copilot.open_in_viewer')}
          @click=${() => openFile(msg.path)}
          @keydown=${(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openFile(msg.path); } }}
        >${msg.path}</span>
        ${msg.status === 'pending'
          ? html`<span class="badge bg-warning text-dark ms-auto">${t('approval.pending')}</span>`
          : msg.status === 'approved'
            ? html`<span class="badge bg-success ms-auto">${t('approval.approved')}</span>`
            : html`<span class="badge bg-danger ms-auto">${t('approval.rejected')}</span>`}
      </div>

      <pre class="copilot-diff">${renderDiff(msg.old_content, msg.new_content)}</pre>

      ${msg.status === 'pending' ? html`
        <div class="copilot-approval-actions">
          ${isRejecting ? html`
            <textarea
              class="form-control form-control-sm copilot-reject-note"
              rows="2"
              placeholder=${t('approval.reject_hint')}
              .value=${host._rejectNote}
              @input=${(e) => { host._rejectNote = e.target.value; }}
            ></textarea>
            <div class="copilot-approval-btns">
              <button class="btn btn-sm btn-danger" @click=${() => host._confirmReject(msg)}>
                <i class="bi bi-x-circle me-1"></i>${t('approval.confirm_reject')}
              </button>
              <button class="btn btn-sm btn-outline-secondary" @click=${() => { host._rejectingId = null; }}>
                ${t('copilot.cancel')}
              </button>
            </div>
          ` : html`
            <div class="copilot-approval-btns">
              <button class="btn btn-sm btn-success" @click=${() => host._approve(msg)}>
                <i class="bi bi-check-circle me-1"></i>${t('approval.approve')}
              </button>
              <button class="btn btn-sm btn-outline-danger" @click=${() => host._startReject(msg)}>
                <i class="bi bi-x-circle me-1"></i>${t('approval.reject')}
              </button>
              <button class="btn btn-sm btn-outline-secondary" title=${t('approval.bypass_15')}
                @click=${() => host._approveWriteBypass(msg, 900)}>
                <i class="bi bi-clock me-1"></i>${t('copilot.bypass_15min')}
              </button>
              <button class="btn btn-sm btn-outline-secondary" title=${t('approval.bypass_all')}
                @click=${() => host._approveWriteBypass(msg, 0)}>
                <i class="bi bi-arrow-repeat me-1"></i>${t('copilot.bypass_session')}
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
      ? html`<span class="spinner-border spinner-border-sm text-warning" role="status" title=${t('copilot.status_awaiting')}></span>`
    : msg.status === 'done'
      ? html`<i class="bi bi-check-circle-fill text-success"></i>`
    : msg.status === 'cancelled'
      ? html`<i class="bi bi-slash-circle-fill text-secondary" title=${t('copilot.status_cancelled')}></i>`
    : msg.status === 'rejected'
      ? html`<i class="bi bi-shield-fill-x text-warning" title=${t('copilot.status_denied')}></i>`
      : html`<i class="bi bi-x-circle-fill text-danger"></i>`;

  return html`
    <div class="copilot-tool ${isPending ? 'copilot-tool--pending' : ''}">
      <button class="copilot-tool-header" @click=${() => host._toggleExpand(msg.tool_call_id)}>
        <span class="copilot-tool-status">${statusIcon}</span>
        ${renderToolIcon(msg)}
        <span class="copilot-tool-name">
          <span class="copilot-tool-title">${msg.display_name || msg.label_full || msg.name}</span>
          ${toolSecondary(msg)}
        </span>
        ${isPending ? html`<span class="badge bg-warning text-dark ms-2">${t('approval.pending')}</span>` : nothing}
        ${msg.status !== 'running' ? html`
          <span class="copilot-tool-eye ms-auto" role="button" tabindex="0"
            title=${t('copilot.view_details')}
            @click=${(e) => { e.stopPropagation(); openToolDetail(msg.tool_call_id); }}
            @keydown=${(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); e.stopPropagation(); openToolDetail(msg.tool_call_id); } }}>
            <i class="bi bi-eye"></i>
          </span>
          <i class="bi bi-chevron-${isOpen ? 'up' : 'down'}"></i>
        ` : html`<i class="bi bi-chevron-${isOpen ? 'up' : 'down'} ms-auto"></i>`}
      </button>
      ${isOpen ? html`
        <div class="copilot-tool-body">
          ${!(isPending && msg.name === 'ask_user_clarification') ? html`
            <div class="copilot-tool-section">
              <span class="copilot-tool-label">args</span>
              <pre class="copilot-tool-pre">${argsStr}</pre>
            </div>
          ` : nothing}
          ${showInlineDiff(host, msg) ? html`
            <div class="copilot-tool-section">
              <span class="copilot-tool-label">${t('copilot.changes')}</span>
              <pre class="copilot-diff">${renderDiff(msg.preview_old || '', msg.preview_new || '')}</pre>
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
                  placeholder=${t('copilot.clarification_ph')}
                  .value=${host._clarificationAnswer}
                  @input=${(e) => { host._clarificationAnswer = e.target.value; }}
                  @keydown=${(e) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); host._answerQuestion(msg); } }}
                ></textarea>
                <button class="btn btn-sm btn-primary ms-2"
                  @click=${() => host._answerQuestion(msg)}
                  ?disabled=${!host._clarificationAnswer.trim()}>
                  <i class="bi bi-send me-1"></i>${t('copilot.send')}
                </button>
              </div>
            </div>
          ` : html`
            <div class="copilot-approval-actions">
              ${isRejecting ? html`
                <textarea
                  class="form-control form-control-sm copilot-reject-note"
                  rows="2"
                  placeholder=${t('approval.reject_hint')}
                  .value=${host._rejectNote}
                  @input=${(e) => { host._rejectNote = e.target.value; }}
                ></textarea>
                <div class="copilot-approval-btns">
                  <button class="btn btn-sm btn-danger"
                    @click=${() => msg.request_id != null ? host._rejectWsTool(msg) : host._rejectTool(msg)}>
                    <i class="bi bi-x-circle me-1"></i>${t('approval.confirm_reject')}
                  </button>
                  <button class="btn btn-sm btn-outline-secondary"
                    @click=${() => { host._rejectingId = null; }}>
                    ${t('copilot.cancel')}
                  </button>
                </div>
              ` : html`
                <div class="copilot-approval-btns">
                  <button class="btn btn-sm btn-success"
                    @click=${(e) => { e.stopPropagation(); msg.request_id != null ? host._approveWsTool(msg) : host._approveTool(msg); }}>
                    <i class="bi bi-check-circle me-1"></i>${t('approval.approve')}
                  </button>
                  <button class="btn btn-sm btn-outline-danger"
                    @click=${(e) => { e.stopPropagation(); host._rejectingId = msg.tool_call_id; host._rejectNote = ''; }}>
                    <i class="bi bi-x-circle me-1"></i>${t('approval.reject')}
                  </button>
                  ${msg.request_id != null ? html`
                    <button class="btn btn-sm btn-outline-secondary" title=${t('approval.bypass_15')}
                      @click=${(e) => { e.stopPropagation(); host._approveWsToolBypass(msg, 900); }}>
                      <i class="bi bi-clock me-1"></i>${t('copilot.bypass_15min')}
                    </button>
                    <button class="btn btn-sm btn-outline-secondary" title=${t('approval.bypass_all')}
                      @click=${(e) => { e.stopPropagation(); host._approveWsToolBypass(msg, 0); }}>
                      <i class="bi bi-arrow-repeat me-1"></i>${t('copilot.bypass_session')}
                    </button>
                  ` : nothing}
                </div>
              `}
            </div>
          `) : msg.status !== 'running' ? (
            msg.status === 'done' && msg.result_type === 'json' ? html`
            <div class="copilot-tool-section">
              <span class="copilot-tool-label copilot-tool-label--done">${t('copilot.result_json')}</span>
              <pre class="copilot-tool-pre copilot-tool-pre--done copilot-tool-pre--json">${
                truncate(prettyJson(msg.result))
              }</pre>
            </div>
          ` : html`
            <div class="copilot-tool-section">
              <span class="copilot-tool-label copilot-tool-label--${msg.status}">
                ${msg.status === 'done' ? t('copilot.result') : t('copilot.error_label')}
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
          <strong>${msg.parent_agent_id ?? 'assistant'}</strong>
          <i class="bi bi-arrow-right mx-1" style="font-size:0.7rem"></i>
          <strong>${msg.agent_id}</strong>
        </span>
        ${msg.done ? html`<span class="copilot-agent-badge done">${t('copilot.agent_done')}</span>` : html`<span class="copilot-agent-badge running">${t('copilot.agent_running')}</span>`}
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
          <strong>${msg.parent_agent_id ?? 'assistant'}</strong>
        </span>
        <span class="copilot-agent-badge done">${t('copilot.agent_finished')}</span>
      </div>
      ${msg.result_preview ? html`
        <pre class="copilot-agent-preview copilot-agent-preview--result">${msg.result_preview}</pre>
      ` : nothing}
    </div>
  `;
}

function failedBadge() {
  return html`<span class="copilot-failed-badge" title=${t('copilot.not_sent_to_llm')}>
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
            <button class="attach-chip-remove" title=${t('copilot.remove')}
                    @click=${(e) => { e.stopPropagation(); host._removeAttachment(i); }}>
              <i class="bi bi-x"></i>
            </button>` : nothing}
        </div>
      `)}
    </div>`;
}

/**
 * Collapsible chain-of-thought block: small, muted, collapsed by default so it
 * never weighs on the UI. A native <details> — Lit keeps the element stable
 * across re-renders, so a user-expanded block stays open while tokens stream
 * into it (live) and in past history items alike.
 */
function renderReasoning(host, msg) {
  if (!msg.reasoning) return nothing;
  if (host?._me?.ui_mode === 'simple') return nothing;
  return html`
    <details class="reasoning-block ${msg.streaming ? 'reasoning-block--live' : ''}">
      <summary>${t('chat.reasoning')}</summary>
      <div class="reasoning-content">${msg.reasoning}</div>
    </details>`;
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
            ${renderReasoning(host, msg)}
            ${unsafeHTML(renderMarkdown(msg.content))}
            ${msg.input_tokens != null ? html`<div class="copilot-token-count">↑${msg.input_tokens.toLocaleString()} tok &nbsp;↓${msg.output_tokens?.toLocaleString()} tok</div>` : nothing}
          </div>`;
      case 'assistant':
        return html`
          <div class="copilot-msg assistant copilot-markdown ${msg.failed ? 'copilot-msg--failed' : ''}">
            ${msg.failed ? failedBadge() : nothing}
            ${renderReasoning(host, msg)}
            ${unsafeHTML(renderMarkdown(msg.content))}
            ${msg.streaming ? html`<span class="stream-caret"></span>` : nothing}
            ${msg.input_tokens != null && !msg.streaming ? html`<div class="copilot-token-count">↑${msg.input_tokens.toLocaleString()} tok &nbsp;↓${msg.output_tokens?.toLocaleString()} tok</div>` : nothing}
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
