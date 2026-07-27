// Shared helpers for the mobile-connector "Mobile App" page fragment.
//
// Served at `/api/plugin/mobile-connector/web/common.js` and imported by the
// page fragment via a relative `./common.js` specifier. Everything the
// fragment needs is self-contained here — the host injects no APIs (see
// `Plugin::web_pages` contract): it talks only to `/api/plugin/<id>/…` and,
// for the user directory used by the admin reassign dropdown plus the caller's
// role, the host `/api/users` and `/api/auth/me` (the fragment runs with the
// logged-in user's full session privileges).
//
// i18n: the plugin ships its own dictionary (`./i18n.js`) and registers it into
// the host's shared strings via `addStrings` (imported from the app root by the
// absolute `/lib/i18n.js` specifier — the same module the host app uses, so
// `t()` and `locale-changed` are shared). `MobileBase` mixes in `I18nMixin` so
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

/// Base for the console fragments: renders into light DOM (so Bootstrap classes
/// and the app's theme CSS variables apply), re-renders on locale change, and
/// exposes the plugin's API root from the host-set `plugin-id` attribute.
export class MobileBase extends I18nMixin(LitElement) {
  createRenderRoot() { return this; }
  get api() { return `/api/plugin/${this.getAttribute('plugin-id') || 'mobile-connector'}`; }
}

/// Human-friendly, localized "time ago" for a Unix-ms timestamp (or "—" when absent).
export function ago(ms) {
  if (!ms) return t('plugin.mobile-connector.time.never');
  const s = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (s < 60) return t('plugin.mobile-connector.time.ago_s', { n: s });
  const m = Math.floor(s / 60);
  if (m < 60) return t('plugin.mobile-connector.time.ago_m', { n: m });
  const h = Math.floor(m / 60);
  if (h < 24) return t('plugin.mobile-connector.time.ago_h', { n: h });
  return t('plugin.mobile-connector.time.ago_d', { n: Math.floor(h / 24) });
}

/// Best-effort device label from the `device_info` JSON a phone sends on hello.
export function deviceLabel(d) {
  const info = d.device_info || {};
  return info.name || info.model || info.device || d.platform || t('plugin.mobile-connector.devices.unknown');
}
