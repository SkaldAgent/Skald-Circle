import { t } from '../../lib/i18n.js';

// Shared vocabulary for the Connectors list and a connector's own page.
//
// Both surfaces have to answer "what state is this connector in?" and both draw the
// same env/secret form. Deriving that twice is how the two drift, so the derivation
// lives here and each page only decides layout.

/// The icon of an **installed** connector, off the box's own `connectors/` folder —
/// not the marketplace proxy, which is admin-only and dies with the feed.
export function connectorIconUrl(name, size = 'sm') {
  return `/api/mcp/catalog/${encodeURIComponent(name)}/icon?size=${size}`;
}

/// How each status reads on a chip. `tone` maps to the `connector-chip--*` accents
/// in `web/css/connectors.css`.
export const STATUS_LABEL = {
  active:      { tone: 'ok' },
  pending:     { tone: 'script' },
  needs_login: { tone: 'script' },
  enabled:     { tone: 'scope' },
  off:         { tone: '' },
  available:   { tone: '' },
};

export function statusText(status) {
  return {
    active:      t('connectors.status.active'),
    pending:     t('connectors.status.needs_fix'),
    needs_login: t('connectors.status.needs_signin'),
    enabled:     t('connectors.status.enabled'),
    off:         t('connectors.status.off'),
    available:   t('connectors.status.available'),
  }[status] ?? status;
}

/// The one place that decides what a connector's state *is*, from whichever runtime
/// rows exist for it.
///
/// A per-user activation whose credentials failed verification is `pending`, not
/// `active`: the row exists but is deliberately held out of `all_startable`, and
/// calling that "active" would be a lie the user acts on.
///
/// `enabled` vs `active` for a global is the §7 distinction between *running* and
/// *reachable by me*: an admin can enable a connector for someone else and never
/// grant it to themselves, and their own list must not claim they have it.
export function statusOf(row) {
  if (row._act) {
    if (row._act.auth_state !== 'pending') return 'active';
    // An OAuth connector sitting at `pending` is waiting for its interactive
    // sign-in, not for a failed credential to be fixed — a different ask.
    return row._act.oauth_provider ? 'needs_login' : 'pending';
  }
  if (row._glob) {
    if (!row._glob.enabled) return 'off';
    return row._glob.can_use ? 'active' : 'enabled';
  }
  return 'available';
}

/// Normalizes a catalog entry's `config_schema_json` into form-field descriptors,
/// whether the feed shipped the object-array form or the legacy bare-name list.
export function normalizeSchema(raw) {
  if (!Array.isArray(raw)) return [];
  return raw.map(e => {
    if (typeof e === 'string') {
      return { name: e, label: e, description: '', required: false, secret: false, example: '', default: '' };
    }
    return {
      name:        e.name || '',
      label:       e.label || e.name || '',
      description: e.description || '',
      required:    !!e.required,
      secret:      !!e.secret,
      example:     e.example || '',
      default:     e.default || '',
    };
  });
}

export function parseJson(s, fallback) {
  if (!s) return fallback;
  try { return JSON.parse(s); } catch { return fallback; }
}

/// Every schema field seeded with its `default` (empty string when none).
export function seedEnv(schema) {
  return Object.fromEntries(schema.map(e => [e.name, e.default || '']));
}

export async function jf(url, opts) {
  const res = await fetch(url, opts);
  if (!res.ok) throw new Error(await res.text() || `HTTP ${res.status}`);
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : null;
}

/// Tells the Connectors list its cached state is stale.
export function announceChange() {
  window.dispatchEvent(new CustomEvent('connectors-changed'));
}
