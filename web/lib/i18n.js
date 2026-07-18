import en from '../i18n/en.js';
import it from '../i18n/it.js';
import fr from '../i18n/fr.js';

const DICTS = { en, it, fr };

export const LOCALES = [
  { id: 'en', label: 'English' },
  { id: 'it', label: 'Italiano' },
  { id: 'fr', label: 'Français' },
];

// Pre-auth the last choice is cached in localStorage (the login page can be
// localized before any session exists); after login the server is the source
// of truth: the user's own `locale` wins over the instance default.
let _locale = localStorage.getItem('locale') || 'en';

export function t(key, params) {
  let s = DICTS[_locale]?.[key] ?? DICTS.en[key] ?? key;
  if (params) {
    for (const [k, v] of Object.entries(params)) s = s.replaceAll(`{${k}}`, String(v));
  }
  return s;
}

export function getLocale() { return _locale; }

export function setLocale(locale, { persist = false } = {}) {
  if (!DICTS[locale]) locale = 'en';
  const changed = locale !== _locale;
  _locale = locale;
  localStorage.setItem('locale', locale);
  document.documentElement.lang = locale;
  if (changed) window.dispatchEvent(new CustomEvent('locale-changed', { detail: { locale } }));
  if (persist) {
    fetch('/api/auth/profile', {
      method:  'PUT',
      headers: { 'Content-Type': 'application/json' },
      body:    JSON.stringify({ locale }),
    }).catch(() => {});
  }
}

/**
 * Resolves the effective locale once the session is known:
 * user preference (users.locale) → instance default (config `ui_locale`) →
 * cached/browser default. Pre-auth (login/setup) keeps the cached locale.
 */
export async function initI18n() {
  try {
    const res = await fetch('/api/auth/me');
    if (!res.ok) return;
    const me = await res.json();
    const eff = me.locale || me.default_locale;
    if (eff) setLocale(eff);
  } catch { /* keep cached locale */ }
}

/** Re-renders the host component whenever the locale changes. */
export const I18nMixin = (Base) => class extends Base {
  connectedCallback() {
    super.connectedCallback?.();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
  }
  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback?.();
  }
};
