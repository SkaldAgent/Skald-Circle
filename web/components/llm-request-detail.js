import { html, nothing } from 'lit';
import { unsafeHTML } from 'lit/directives/unsafe-html.js';
import { LightElement, renderMarkdown } from '../lib/base.js';

// ── Helpers ───────────────────────────────────────────────────────────────────

function formatDate(iso) {
  if (!iso) return '—';
  return new Date(iso).toLocaleString(undefined, {
    day: '2-digit', month: '2-digit', year: '2-digit',
    hour: '2-digit', minute: '2-digit',
  });
}

function fmtTokens(n) {
  if (n == null) return '—';
  if (n >= 1000) return (n / 1000).toFixed(1) + 'k';
  return String(n);
}

function cacheHitPct(item) {
  if (item.cache_read_tokens == null || !item.input_tokens) return '—';
  return (item.cache_read_tokens / item.input_tokens * 100).toFixed(0) + '%';
}

function cacheTooltip(item) {
  const parts = [];
  if (item.cache_read_tokens != null) parts.push(`read: ${item.cache_read_tokens.toLocaleString()} tk`);
  if (item.cache_creation_tokens != null) parts.push(`write: ${item.cache_creation_tokens.toLocaleString()} tk`);
  return parts.length ? parts.join(' | ') : '';
}

function parseJson(str) {
  if (!str) return null;
  try { return JSON.parse(str); } catch { return null; }
}

function extractSystem(req) {
  if (!req) return null;
  // Anthropic: top-level system field
  if (req.system != null) {
    if (typeof req.system === 'string') return req.system;
    if (Array.isArray(req.system)) {
      return req.system.map(b => (typeof b === 'string' ? b : b.text ?? '')).join('\n\n');
    }
  }
  // OpenAI: only the very first message if it is role=system
  if (Array.isArray(req.messages) && req.messages[0]?.role === 'system') {
    return typeof req.messages[0].content === 'string' ? req.messages[0].content : '';
  }
  return null;
}

function extractMessages(req) {
  if (!req || !Array.isArray(req.messages)) return [];
  let msgs = req.messages;
  // For OpenAI format: skip the first message if it was already shown as the System Prompt
  if (!req.system && msgs[0]?.role === 'system') msgs = msgs.slice(1);
  // All remaining messages are kept, including mid-conversation role=system inserts
  return msgs;
}

function extractParams(req) {
  if (!req) return [];
  const skip = new Set(['model', 'messages', 'system', 'tools', 'tool_choice', 'stream']);
  return Object.entries(req)
    .filter(([k]) => !skip.has(k))
    .map(([k, v]) => [k, typeof v === 'object' ? JSON.stringify(v) : String(v)]);
}

function extractTools(req) {
  return req?.tools ?? [];
}

function extractRespBlocks(resp) {
  if (!resp) return [];
  if (Array.isArray(resp.content)) return resp.content; // Anthropic
  const msg = resp?.choices?.[0]?.message;
  if (msg) return contentBlocks(msg); // OpenAI — reuse helper (handles reasoning_content, tool_calls)
  return [];
}

function extractRespMeta(resp) {
  if (!resp) return [];
  const pairs = [];
  const skip = new Set(['content', 'choices', 'type', 'role', 'object', 'usage']);
  for (const [k, v] of Object.entries(resp)) {
    if (skip.has(k)) continue;
    pairs.push([k, typeof v === 'object' ? JSON.stringify(v) : String(v)]);
  }
  // OpenAI: finish_reason lives inside choices
  const choice = resp?.choices?.[0];
  if (choice?.finish_reason && !resp.stop_reason) {
    pairs.push(['finish_reason', choice.finish_reason]);
  }
  // usage — flatten sub-keys
  if (resp.usage && typeof resp.usage === 'object') {
    for (const [k, v] of Object.entries(resp.usage)) {
      if (v != null) pairs.push([`usage.${k}`, typeof v === 'object' ? JSON.stringify(v) : String(v)]);
    }
  }
  return pairs;
}

function paramsPreview(input) {
  if (!input || Object.keys(input).length === 0) return '';
  const str = JSON.stringify(input);
  return str.length > 80 ? str.slice(0, 77) + '…' : str;
}

function normalizeToolResultContent(block) {
  if (Array.isArray(block.content))
    return block.content.map(b => b.text ?? JSON.stringify(b)).join('\n');
  if (typeof block.content === 'string') return block.content;
  return JSON.stringify(block.content ?? '');
}

function buildToolResultMap(msgs) {
  const map = new Map(); // tool_use_id → { content, is_error }
  for (const msg of msgs) {
    // Anthropic format: tool_result blocks inside user message content
    for (const block of contentBlocks(msg)) {
      if (block.type === 'tool_result') {
        map.set(block.tool_use_id, {
          content:  normalizeToolResultContent(block),
          is_error: !!block.is_error,
        });
      }
    }
    // OpenAI format: role='tool' messages carry the result directly
    if (msg.role === 'tool' && msg.tool_call_id) {
      const content = typeof msg.content === 'string'
        ? msg.content
        : (Array.isArray(msg.content) ? msg.content.map(b => b.text ?? JSON.stringify(b)).join('\n') : JSON.stringify(msg.content ?? ''));
      map.set(msg.tool_call_id, { content, is_error: false });
    }
  }
  return map;
}

function contentBlocks(msg) {
  const blocks = [];
  if (typeof msg.reasoning_content === 'string' && msg.reasoning_content) {
    blocks.push({ type: 'reasoning', text: msg.reasoning_content });
  }
  if (msg.content) {
    if (typeof msg.content === 'string') blocks.push({ type: 'text', text: msg.content });
    else if (Array.isArray(msg.content)) blocks.push(...msg.content);
  }
  // OpenAI format: tool calls live in tool_calls[], not in content
  if (Array.isArray(msg.tool_calls)) {
    for (const tc of msg.tool_calls) {
      let input = {};
      try { input = JSON.parse(tc.function?.arguments ?? '{}'); } catch { /* ignore */ }
      blocks.push({ type: 'tool_use', id: tc.id, name: tc.function?.name ?? '?', input });
    }
  }
  return blocks;
}

// ── Component ─────────────────────────────────────────────────────────────────

export class LlmRequestDetail extends LightElement {
  static properties = {
    detailId:       { type: Number },
    _detail:        { state: true },
    _loading:       { state: true },
    _error:         { state: true },
    _openSections:  { state: true },
    _expandedTools: { state: true },
  };

  constructor() {
    super();
    this.detailId       = null;
    this._detail        = null;
    this._loading       = false;
    this._error         = null;
    this._openSections  = new Set(['response']);
    this._expandedTools = new Set();
  }

  updated(changed) {
    if (changed.has('detailId') && this.detailId != null) {
      this._detail        = null;
      this._error         = null;
      this._openSections  = new Set(['response']);
      this._expandedTools = new Set();
      this._fetch(this.detailId);
    }
  }

  async _fetch(id) {
    this._loading = true;
    this._error   = null;
    try {
      const res = await fetch(`/api/dev/llm-requests/${id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._detail = await res.json();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  _back() {
    this.dispatchEvent(new CustomEvent('detail-back', { bubbles: true }));
  }

  _toggleSection(name) {
    const next = new Set(this._openSections);
    next.has(name) ? next.delete(name) : next.add(name);
    this._openSections = next;
  }

  _toggleToolExpand(key) {
    const next = new Set(this._expandedTools);
    next.has(key) ? next.delete(key) : next.add(key);
    this._expandedTools = next;
  }

  // ── Section wrapper ─────────────────────────────────────────────────────────

  _renderSection(id, title, content, badge = null) {
    const open = this._openSections.has(id);
    return html`
      <div class="llmr-section ${open ? 'llmr-section--open' : ''}">
        <div class="llmr-section-header" @click=${() => this._toggleSection(id)}>
          <span class="llmr-section-chevron">
            <i class="bi bi-chevron-${open ? 'down' : 'right'}"></i>
          </span>
          <span class="llmr-section-title">${title}</span>
          ${badge != null ? html`<span class="llmr-section-badge">${badge}</span>` : nothing}
        </div>
        <div class="llmr-section-body">
          ${content}
        </div>
      </div>
    `;
  }

  // ── Key-value table ──────────────────────────────────────────────────────────

  _renderKvTable(pairs) {
    if (!pairs || pairs.length === 0) return html`<p class="text-secondary small mb-0">—</p>`;
    return html`
      <table class="llmr-kv-table">
        <tbody>
          ${pairs.map(([k, v]) => html`
            <tr>
              <td class="llmr-kv-key">${k}</td>
              <td class="llmr-kv-val">${v}</td>
            </tr>
          `)}
        </tbody>
      </table>
    `;
  }

  // ── Stat bar ─────────────────────────────────────────────────────────────────

  _renderStatBar(d) {
    return html`
      <div class="llmr-detail-statbar">
        <span class="llmr-badge-agent">${d.agent_id ?? 'no agent'}</span>
        <span class="llmr-badge-source">${d.source ?? '—'}</span>
        <span class="llmr-detail-model">${d.model_name}</span>
        ${d.stack_id != null ? html`<span class="llmr-detail-pill llmr-detail-pill--stack">stack #${d.stack_id}</span>` : nothing}
        <span class="llmr-detail-sep"></span>
        <span class="llmr-detail-stat" title="Input tokens">
          <i class="bi bi-arrow-up-circle"></i> ${fmtTokens(d.input_tokens)}
        </span>
        <span class="llmr-detail-stat" title="Output tokens">
          <i class="bi bi-arrow-down-circle"></i> ${fmtTokens(d.output_tokens)}
        </span>
        ${d.cache_read_tokens > 0 ? html`
          <span class="llmr-detail-stat llmr-detail-stat--cache" title=${cacheTooltip(d)}>
            <i class="bi bi-lightning-charge"></i> cache ${cacheHitPct(d)}
          </span>
        ` : nothing}
        <span class="llmr-detail-stat">
          <i class="bi bi-clock"></i> ${d.duration_ms} ms
        </span>
        <span class="llmr-detail-date">${formatDate(d.created_at)}</span>
        ${d.error_text ? html`
          <span class="llmr-detail-error-badge" title=${d.error_text}>
            <i class="bi bi-exclamation-triangle-fill"></i> error
          </span>
        ` : nothing}
      </div>
      ${d.error_text ? html`
        <div class="llmr-detail-error-box">
          <i class="bi bi-exclamation-triangle-fill"></i> ${d.error_text}
        </div>
      ` : nothing}
    `;
  }

  // ── Content blocks ───────────────────────────────────────────────────────────

  // toolResultMap: Map<tool_use_id, {content, is_error}> — null when not available
  _renderContentBlock(block, keyPrefix, toolResultMap = null) {
    if (!block) return nothing;
    const type = block.type;

    if (type === 'reasoning') {
      const key  = `${keyPrefix}-reasoning`;
      const open = this._expandedTools.has(key);
      const text = block.text ?? '';
      return html`
        <div class="llmr-reasoning-block">
          <div class="llmr-reasoning-header" @click=${() => this._toggleToolExpand(key)}>
            <i class="bi bi-lightbulb"></i>
            <span>reasoning</span>
            <span class="llmr-tool-toggle ms-auto">
              <i class="bi bi-${open ? 'dash' : 'plus'}-circle"></i>
            </span>
          </div>
          ${open ? html`<pre class="llmr-text-block llmr-reasoning-body">${text}</pre>` : nothing}
        </div>
      `;
    }

    if (type === 'text') {
      const text = block.text ?? '';
      if (!text) return nothing;
      return html`<pre class="llmr-text-block">${text}</pre>`;
    }

    if (type === 'tool_use') {
      const key     = `${keyPrefix}-use-${block.id ?? block.name}`;
      const open    = this._expandedTools.has(key);
      const args    = block.input != null ? JSON.stringify(block.input, null, 2) : '{}';
      const preview = paramsPreview(block.input);
      const result  = toolResultMap?.get(block.id);
      return html`
        <div class="llmr-tool-block llmr-tool-block--use ${result?.is_error ? 'llmr-tool-block--error' : ''}">
          <div class="llmr-tool-block-header" @click=${() => this._toggleToolExpand(key)}>
            <i class="bi bi-wrench"></i>
            <span class="llmr-tool-name">${block.name}</span>
            ${preview ? html`<span class="llmr-tool-preview">${preview}</span>` : nothing}
            <span class="llmr-tool-toggle ms-auto">
              <i class="bi bi-${open ? 'dash' : 'plus'}-circle"></i>
            </span>
          </div>
          ${open ? html`
            <div class="llmr-tool-expanded">
              <div class="llmr-tool-section-label">Parameters</div>
              <pre class="llmr-tool-pre">${args}</pre>
              ${result != null ? html`
                <div class="llmr-tool-section-label llmr-tool-section-label--result">
                  Result ${result.is_error ? html`<span class="badge bg-danger ms-1">error</span>` : nothing}
                </div>
                <pre class="llmr-tool-pre">${result.content}</pre>
              ` : nothing}
            </div>
          ` : nothing}
        </div>
      `;
    }

    // tool_result: only rendered when there is no toolResultMap (i.e. not in conversation context)
    if (type === 'tool_result') {
      if (toolResultMap != null) return nothing; // shown inline inside the tool_use block
      const key     = `${keyPrefix}-result-${block.tool_use_id}`;
      const open    = this._expandedTools.has(key);
      const content = normalizeToolResultContent(block);
      return html`
        <div class="llmr-tool-block llmr-tool-block--result ${block.is_error ? 'llmr-tool-block--error' : ''}">
          <div class="llmr-tool-block-header" @click=${() => this._toggleToolExpand(key)}>
            <i class="bi bi-arrow-return-left"></i>
            <span class="llmr-tool-name">result</span>
            <span class="llmr-tool-id">${block.tool_use_id ?? ''}</span>
            ${block.is_error ? html`<span class="badge bg-danger ms-1">error</span>` : nothing}
            <span class="llmr-tool-toggle ms-auto">
              <i class="bi bi-${open ? 'dash' : 'plus'}-circle"></i>
            </span>
          </div>
          ${open ? html`<pre class="llmr-tool-pre">${content}</pre>` : nothing}
        </div>
      `;
    }

    return html`<pre class="llmr-tool-pre">${JSON.stringify(block, null, 2)}</pre>`;
  }

  _renderMessage(msg, idx, toolResultMap) {
    const role   = msg.role ?? 'unknown';
    const blocks = contentBlocks(msg);

    // skip messages that are entirely tool results (shown inline inside the tool_use block)
    // Anthropic: role=user messages whose content is all tool_result blocks
    // OpenAI:    role=tool messages
    if (role === 'tool') return nothing;
    if (role === 'user' && blocks.length > 0 && blocks.every(b => b.type === 'tool_result')) {
      return nothing;
    }

    // mid-conversation system prompt: render with markdown and a distinct style
    if (role === 'system') {
      const text = typeof msg.content === 'string' ? msg.content : '';
      if (!text) return nothing;
      return html`
        <div class="llmr-msg llmr-msg--system">
          <div class="llmr-msg-role"><i class="bi bi-shield-lock-fill"></i> system</div>
          <div class="llmr-msg-body">
            <div class="llmr-system-md copilot-markdown">${unsafeHTML(renderMarkdown(text))}</div>
          </div>
        </div>
      `;
    }

    return html`
      <div class="llmr-msg llmr-msg--${role}">
        <div class="llmr-msg-role">${role}</div>
        <div class="llmr-msg-body">
          ${blocks.map((b, bi) => this._renderContentBlock(b, `msg-${idx}-${bi}`, toolResultMap))}
        </div>
      </div>
    `;
  }

  _renderResponseBlock(block, idx) {
    if (!block) return nothing;
    // Delegate to _renderContentBlock for shared block types (reasoning, text, tool_use, tool_result)
    return this._renderContentBlock(block, `resp-${idx}`);
  }

  // ── Main render ──────────────────────────────────────────────────────────────

  render() {
    if (this._loading) return html`
      <div class="llmr-page">
        <div class="llmr-detail-back">
          <button class="btn btn-sm btn-outline-secondary" @click=${() => this._back()}>
            <i class="bi bi-arrow-left"></i> Back
          </button>
        </div>
        <div class="llmr-state">
          <div class="spinner-border spinner-border-sm text-secondary" role="status"></div>
          <span>Loading…</span>
        </div>
      </div>
    `;

    if (this._error) return html`
      <div class="llmr-page">
        <div class="llmr-detail-back">
          <button class="btn btn-sm btn-outline-secondary" @click=${() => this._back()}>
            <i class="bi bi-arrow-left"></i> Back
          </button>
        </div>
        <div class="llmr-state llmr-state--error">
          <i class="bi bi-exclamation-circle"></i>
          <span>${this._error}</span>
        </div>
      </div>
    `;

    if (!this._detail) return nothing;

    const d      = this._detail;
    const req    = parseJson(d.request_json);
    const resp   = parseJson(d.response_json);
    const hdrs     = parseJson(d.request_headers);
    const respHdrs = parseJson(d.response_headers);
    const system = extractSystem(req);
    const msgs   = extractMessages(req);
    const params = extractParams(req);
    const tools  = extractTools(req);

    const payloadMissing = !req && !resp;
    const respBlocks     = extractRespBlocks(resp);
    const respMeta       = extractRespMeta(resp);
    const toolResultMap  = buildToolResultMap(msgs);

    return html`
      <div class="llmr-page">
        <div class="llmr-detail-back">
          <button class="btn btn-sm btn-outline-secondary" @click=${() => this._back()}>
            <i class="bi bi-arrow-left"></i> Back
          </button>
          <span class="llmr-detail-title">
            <i class="bi bi-journal-code"></i> Request <span class="llmr-detail-id">#${d.id}</span>
          </span>
        </div>

        ${this._renderStatBar(d)}

        ${payloadMissing ? html`
          <div class="llmr-purged-banner">
            <i class="bi bi-hourglass-split"></i>
            Payload not available — this request has been purged by the retention policy.
          </div>
        ` : nothing}

        ${hdrs ? this._renderSection('req-headers', 'Request Headers',
            this._renderKvTable(Object.entries(hdrs))
          ) : nothing}

        ${respHdrs ? this._renderSection('resp-headers', 'Response Headers',
            this._renderKvTable(Object.entries(respHdrs))
          ) : nothing}

        ${params.length ? this._renderSection('params', 'Parameters',
            this._renderKvTable(params)
          ) : nothing}

        ${system ? this._renderSection('system', 'System Prompt',
            html`<div class="llmr-system-md copilot-markdown">${unsafeHTML(renderMarkdown(system))}</div>`
          ) : nothing}

        ${msgs.length ? this._renderSection('conversation', 'Conversation',
            html`<div class="llmr-msg-list">
              ${msgs.map((m, i) => this._renderMessage(m, i, toolResultMap))}
            </div>`,
            msgs.length
          ) : nothing}

        ${tools.length ? this._renderSection('tools', 'Tools Defined',
            html`<div class="llmr-tool-def-list">
              ${tools.map((t, i) => {
                // Anthropic: { name, description, input_schema }
                // OpenAI:    { type: 'function', function: { name, description, parameters } }
                const name   = t.name ?? t.function?.name ?? '(unknown)';
                const desc   = t.description ?? t.function?.description ?? '';
                const schema = t.input_schema ?? t.function?.parameters ?? null;
                const key    = `tooldef-${i}-${name}`;
                const open   = this._expandedTools.has(key);
                return html`
                  <div class="llmr-tool-def ${open ? 'llmr-tool-def--open' : ''}">
                    <div class="llmr-tool-def-header" @click=${() => this._toggleToolExpand(key)}>
                      <i class="bi bi-wrench" style="color:#0891b2;font-size:.75rem"></i>
                      <span class="llmr-tool-name">${name}</span>
                      <span class="llmr-tool-def-desc">${desc}</span>
                      ${schema ? html`
                        <span class="llmr-tool-toggle ms-auto">
                          <i class="bi bi-${open ? 'dash' : 'plus'}-circle"></i>
                        </span>
                      ` : nothing}
                    </div>
                    ${open && schema ? html`
                      <pre class="llmr-tool-pre">${JSON.stringify(schema, null, 2)}</pre>
                    ` : nothing}
                  </div>
                `;
              })}
            </div>`,
            tools.length
          ) : nothing}

        ${resp ? this._renderSection('response', 'Response',
            html`
              ${respMeta.length ? this._renderKvTable(respMeta) : nothing}
              <div class="llmr-msg-list llmr-msg-list--resp">
                ${respBlocks.map((b, i) => this._renderResponseBlock(b, i))}
              </div>
            `
          ) : nothing}
      </div>
    `;
  }
}
