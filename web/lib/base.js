import { LitElement } from 'lit';
import { marked }     from 'marked';
import DOMPurify      from 'dompurify';

marked.use({ breaks: true, gfm: true });

/**
 * An http(s) link whose origin differs from the page's is "external" and
 * should open in a new tab. Relative paths, hash anchors (e.g. the app's
 * `#file_viewer?...` routing), and other schemes (mailto:, tel:) are left
 * untouched so in-app navigation and native handlers keep working.
 */
function isExternalLink(href) {
  if (!href) return false;
  try {
    const url = new URL(href, window.location.href);
    if (url.protocol !== 'http:' && url.protocol !== 'https:') return false;
    return url.origin !== window.location.origin;
  } catch {
    return false;
  }
}

// Open external links in a new tab. `rel` is in DOMPurify's default allow-list;
// `target` is whitelisted via ADD_ATTR in renderMarkdown(). Runs once per module load.
DOMPurify.addHook('uponSanitizeElement', (node, data) => {
  if (data.tagName !== 'a' || !node.hasAttribute('href')) return;
  if (isExternalLink(node.getAttribute('href'))) {
    node.setAttribute('target', '_blank');
    node.setAttribute('rel', 'noopener noreferrer');
  }
});

export function renderMarkdown(text) {
  // `target` is not in DOMPurify's default attribute allow-list, so the
  // external-link hook above needs it whitelisted here to survive sanitization.
  return DOMPurify.sanitize(marked.parse(text ?? ''), { ADD_ATTR: ['target'] });
}

// Disable Shadow DOM so Bootstrap CSS flows through naturally.
export class LightElement extends LitElement {
  createRenderRoot() { return this; }
}
