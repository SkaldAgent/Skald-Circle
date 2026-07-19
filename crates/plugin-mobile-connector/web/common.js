// Shared helpers for the mobile-connector console fragments.
//
// Served at `/api/plugin/mobile-connector/web/common.js` and imported by the
// two page fragments via a relative `./common.js` specifier. Everything the
// fragments need is self-contained here — the host injects no APIs (see
// `Plugin::web_pages` contract): they talk only to `/api/plugin/<id>/…` and,
// for the user directory used by the reassign dropdown, the host `/api/users`
// (the fragment runs with the logged-in admin's full session privileges).
import { LitElement } from 'lit';

/// JSON fetch that throws the server's error text on non-2xx and tolerates an
/// empty (204) body.
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
/// and the app's theme CSS variables apply) and exposes the plugin's API root
/// from the host-set `plugin-id` attribute.
export class MobileBase extends LitElement {
  createRenderRoot() { return this; }
  get api() { return `/api/plugin/${this.getAttribute('plugin-id') || 'mobile-connector'}`; }
}

/// Human-friendly "time ago" for a Unix-ms timestamp (or "—" when absent).
export function ago(ms) {
  if (!ms) return '—';
  const s = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

/// Best-effort device label from the `device_info` JSON a phone sends on hello.
export function deviceLabel(d) {
  const info = d.device_info || {};
  return info.name || info.model || info.device || d.platform || 'Unknown device';
}
