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

/**
 * Whether this page has ever held a session.
 *
 * The dialog answers "your session died **while you were using the app**", and
 * that premise is not free: on a cold load with no session at all, the shells
 * mount every component before their boot auth check resolves, so a dozen `/api`
 * calls 401 in parallel and used to raise the dialog *over* the login screen the
 * boot check was about to show (`.relogin-backdrop` is z-10000, `.login-page`
 * z-9999). Logging in through that modal only closes the modal — the login page
 * underneath stayed up with the app hidden, so the user was asked for their
 * password a second time and a manual reload was the only way through.
 *
 * So a 401 is only an *expiry* once something proved we had a session; before
 * that it is the ordinary "not logged in yet", which the boot check owns.
 */
let established = false;

// The native mobile shell authenticates in the background and must never be
// gated by a web login form (the rule `mobile.html`'s bootstrap already states).
// Guarding the report rather than each producer means no future caller can
// reintroduce the dialog there.
const NATIVE_SHELL = new URLSearchParams(location.search).get('native') === 'true';

/**
 * True in the native mobile shell, which authenticates on its own and gets no
 * dialog. Exported because "no dialog is coming" is not the same fact as "the
 * session is fine": a caller deciding whether to keep retrying needs to tell the
 * two apart (see `chat-session.js::_scheduleReconnect`).
 */
export function isNativeShell() {
  return NATIVE_SHELL;
}

/** True once the server has told us this browser has no session anymore. */
export function isSessionExpired() {
  return expired;
}

/**
 * Report a lost session. Idempotent and one-way: the first call fires the
 * `auth-expired` window event, every later one is a no-op — several components
 * discover the same 401 at once, and the dialog must be raised once.
 *
 * A no-op while no session was ever established (see [`established`]): there is
 * nothing to renew, and the shell's own boot check is already showing the login
 * screen.
 */
export function notifySessionExpired() {
  if (expired || !established || NATIVE_SHELL) return;
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
 *
 * The same wrapper is where a session is recognised as **established**, and it
 * is deliberately passive rather than a call the two shells each make after
 * their boot check: `mobile.html` probes `/api/auth/me` from a classic inline
 * script that runs *before* this module exists, so an explicit marker would
 * never fire there and the dialog would be dead on mobile. Any success from a
 * gated endpoint proves a live session (the gate is deny-by-default), which is
 * why only the routes `guard.rs::is_public` lets through unauthenticated are
 * excluded — `auth/me` and `auth/login` answering 200 *do* prove one.
 */
export function installSessionExpiryWatch() {
  const native = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const res = await native(input, init);
    const url = typeof input === 'string' ? input : (input?.url ?? '');
    const path = url.startsWith('http') ? new URL(url).pathname : url;
    if (res.status === 401) {
      if (path.startsWith('/api/') && !path.startsWith('/api/auth/') && !path.startsWith('/api/setup/')) {
        notifySessionExpired();
      }
    } else if (res.ok && provesSession(path)) {
      established = true;
    }
    return res;
  };
}

/** Whether a 2xx on this path can only have come from an authenticated call. */
function provesSession(path) {
  return path.startsWith('/api/')
    && !path.startsWith('/api/setup/')
    && path !== '/api/auth/logout';
}
