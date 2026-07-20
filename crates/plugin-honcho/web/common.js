// Shared helpers for the Honcho page fragments.
//
// Served at `/api/plugin/honcho/web/common.js` and imported by the two page
// fragments via a relative `./common.js` specifier. Everything the fragments
// need is self-contained here — the host injects no APIs (see the
// `Plugin::web_pages` contract): they talk only to `/api/plugin/honcho/…` and,
// for save/opt-in, the host's core plugin endpoints `/api/plugins/…` (the
// fragment runs with the logged-in user's full session privileges).
//
// i18n: the plugin ships its own dictionary (`./i18n.js`) and registers it into
// the host's shared strings via `addStrings` (imported from the app root by the
// absolute `/lib/i18n.js` specifier — the same module the host app uses, so
// `t()` and `locale-changed` are shared). `HonchoBase` mixes in `I18nMixin` so
// every fragment re-renders on a language switch. Register once, at module load.
import { LitElement } from 'lit';
import { t, addStrings, I18nMixin } from '/lib/i18n.js';
import STRINGS from './i18n.js';

addStrings(STRINGS);

export { t };

/// JSON fetch that throws the server's error text on non-2xx and tolerates an
/// empty (204) body. The server's error text is already localized (the backend
/// resolves the caller's locale), so it is safe to surface directly.
export async function jf(url, opts = {}) {
  const res = await fetch(url, {
    headers: { 'Content-Type': 'application/json', ...(opts.headers || {}) },
    ...opts,
  });
  if (!res.ok) {
    const txt = await res.text().catch(() => '');
    throw new Error(txt || `HTTP ${res.status}`);
  }
  if (res.status === 204) return null;
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : res.text();
}

/// Base for the Honcho fragments: renders into light DOM (so Bootstrap classes
/// and the app's theme CSS variables apply), re-renders on locale change, and
/// exposes the plugin's API root from the host-set `plugin-id` attribute.
export class HonchoBase extends I18nMixin(LitElement) {
  createRenderRoot() { return this; }
  get api() { return `/api/plugin/${this.getAttribute('plugin-id') || 'honcho'}`; }
}
