/**
 * Session-expiry detection for an already-open tab.
 *
 * A session lives in the server's RAM (blueprint §9), so a restart invalidates
 * every token while the browser keeps happily sending its cookie. From that
 * moment every gated `/api` call answers 401 and the chat socket is refused at
 * the upgrade — and a refused upgrade is indistinguishable, in `onclose`, from a
 * dropped wifi. That is why the chat used to sit forever behind "Not connected —
 * reconnecting": it was reconnecting, correctly, to a server that will never
 * accept it again.
 *
 * This module is the single place that turns "the server says I am nobody" into
 * a fact the app can act on: the `auth-expired` event, answered by the re-login
 * dialog (`components/session-relogin.js`), and `auth-restored` once a new
 * session is in hand. Nothing here touches the DOM — reporting and reacting stay
 * apart, so a second reactor (a banner, a mobile-specific screen) costs nothing.
 */

let expired = false;

// The native mobile shell authenticates in the background and must never be
// gated by a web login form (the rule `mobile.html`'s bootstrap already states).
// Guarding the report rather than each producer means no future caller can
// reintroduce the dialog there.
const NATIVE_SHELL = new URLSearchParams(location.search).get('native') === 'true';

/** True once the server has told us this browser has no session anymore. */
export function isSessionExpired() {
  return expired;
}

/**
 * Report a lost session. Idempotent and one-way: the first call fires the
 * `auth-expired` window event, every later one is a no-op — several components
 * discover the same 401 at once, and the dialog must be raised once.
 */
export function notifySessionExpired() {
  if (expired || NATIVE_SHELL) return;
  expired = true;
  window.dispatchEvent(new CustomEvent('auth-expired'));
}

/**
 * Report that a fresh session has been obtained (the re-login dialog succeeded).
 * Re-arms the detector and fires `auth-restored`, on which the live connections
 * that gave up — the chat socket above all — pick themselves back up.
 */
export function notifySessionRestored() {
  if (!expired) return;
  expired = false;
  window.dispatchEvent(new CustomEvent('auth-restored'));
}

/**
 * Ask the server whether this browser still has a session.
 *
 * Returns `'ok'`, `'expired'`, or `'unknown'` when the server could not be
 * reached — the caller must treat that third case as "keep retrying", never as a
 * logout: a box that is merely down comes back, and throwing the user at a login
 * form they cannot submit would be strictly worse than waiting.
 */
export async function probeSession() {
  try {
    const res = await fetch('/api/auth/me');
    if (res.status === 401) return 'expired';
    return res.ok ? 'ok' : 'unknown';
  } catch {
    return 'unknown';
  }
}

/**
 * Wrap `window.fetch` so that a 401 from any gated `/api` endpoint reports an
 * expired session, wherever in the app it happens.
 *
 * A wrapper rather than a helper every call site opts into: the components call
 * `fetch` directly in dozens of places, and a seam that has to be remembered is
 * one that will be forgotten by the next page. The auth endpoints are excluded
 * because 401 is a *normal* answer there — `auth/me` is the "am I logged in?"
 * probe and `auth/login` answers it to a wrong password; treating either as an
 * expiry would raise the login screen from the login screen.
 */
export function installSessionExpiryWatch() {
  const native = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const res = await native(input, init);
    if (res.status === 401) {
      const url = typeof input === 'string' ? input : (input?.url ?? '');
      const path = url.startsWith('http') ? new URL(url).pathname : url;
      if (path.startsWith('/api/') && !path.startsWith('/api/auth/') && !path.startsWith('/api/setup/')) {
        notifySessionExpired();
      }
    }
    return res;
  };
}
