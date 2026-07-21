import { html, nothing } from 'lit';
import { t }            from '../../lib/i18n.js';
import { openFile }     from '../../lib/open-file.js';
import { renderDiff }   from '../copilot-render.js';

// Shared engine for the tool-execution detail view — the fetch + the center-panel
// body — so the desktop (`tool-detail-page.js`) and mobile
// (`shared/tool-detail-mobile.js`) surfaces render identically and only supply
// their own chrome (header / back button).

export const STATUS_ICON = {
  done:      { glyph: 'bi-check-circle-fill', cls: 'text-success' },
  error:     { glyph: 'bi-x-circle-fill',     cls: 'text-danger' },
  cancelled: { glyph: 'bi-slash-circle-fill', cls: 'text-secondary' },
  rejected:  { glyph: 'bi-shield-fill-x',     cls: 'text-warning' },
  pending:   { glyph: 'bi-hourglass-split',   cls: 'text-warning' },
};

function prettyJson(v) {
  if (v == null) return '';
  try { return JSON.stringify(v, null, 2); }
  catch { return String(v); }
}

/** Fetches one tool call's full detail from `GET /api/tools/{id}`. Throws on error. */
export async function fetchToolDetail(id) {
  const r = await fetch(`/api/tools/${encodeURIComponent(id)}`, { credentials: 'same-origin' });
  if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
  return r.json();
}

function renderResult(tl) {
  if (tl.status === 'error' || tl.status === 'cancelled' || tl.status === 'rejected') {
    return html`<pre class="tool-detail-pre tool-detail-pre--error">${tl.error ?? tl.result ?? ''}</pre>`;
  }
  if (tl.status === 'pending') {
    return html`<div class="tool-detail-muted">${t('approval.pending')}</div>`;
  }
  let body = tl.result ?? '';
  if (tl.result_type === 'json') {
    try { body = prettyJson(JSON.parse(tl.result ?? 'null')); } catch { /* keep raw */ }
  }
  return html`<pre class="tool-detail-pre">${body}</pre>`;
}

/** The center-panel body: target path, input args, diff (writes), and result. */
export function renderToolBody(tl) {
  if (!tl) return nothing;
  const hasPreview = tl.preview_new != null || tl.preview_old != null;
  return html`
    ${tl.path ? html`
      <div class="tool-detail-section">
        <span class="tool-detail-label">${t('tool_detail.target')}</span>
        <button class="tool-detail-path" @click=${() => openFile(tl.path)}>
          <i class="bi bi-file-earmark-text me-1"></i>${tl.path}
        </button>
      </div>
    ` : nothing}

    <div class="tool-detail-section">
      <span class="tool-detail-label">${t('tool_detail.input')}</span>
      <pre class="tool-detail-pre">${prettyJson(tl.arguments)}</pre>
    </div>

    ${hasPreview ? html`
      <div class="tool-detail-section">
        <span class="tool-detail-label">${t('copilot.changes')}</span>
        <pre class="copilot-diff">${renderDiff(tl.preview_old || '', tl.preview_new || '')}</pre>
      </div>
    ` : nothing}

    <div class="tool-detail-section">
      <span class="tool-detail-label">${t('copilot.result')}</span>
      ${renderResult(tl)}
    </div>
  `;
}
