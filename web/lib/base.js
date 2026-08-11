import { LitElement } from 'lit';
import { marked }     from 'marked';
import DOMPurify      from 'dompurify';
import { t }          from './i18n.js';
import { highlightMarkdownCodeBlocks } from './highlight.js';

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
  const html = DOMPurify.sanitize(marked.parse(text ?? ''), { ADD_ATTR: ['target'] });
  // Syntax-highlight fenced code blocks whose language we support (hljs escapes
  // its own output; unknown fences stay plain).
  const highlighted = highlightMarkdownCodeBlocks(html);
  // Wrap fenced code blocks in .md-code-wrap so a copy button can float over
  // them on hover. `<pre>` reaches this point only from a marked code block —
  // a literal one in the source text is escaped by sanitize, so the string
  // replace cannot wrap anything else.
  const btn = `<button type="button" class="md-code-copy" title="${t('chat.copy_code')}"><i class="bi bi-clipboard"></i></button>`;
  return highlighted.replaceAll('<pre>', `<div class="md-code-wrap">${btn}<pre>`)
                    .replaceAll('</pre>', '</pre></div>');
}

// One delegated listener serves every copy button renderMarkdown has ever
// emitted: the buttons live inside `unsafeHTML` fragments, so no per-element
// handler could be attached at render time.
document.addEventListener('click', (ev) => {
  const btn = ev.target.closest?.('.md-code-copy');
  if (!btn) return;
  const pre = btn.closest('.md-code-wrap')?.querySelector('pre');
  if (!pre) return;
  copyToClipboard(pre.textContent ?? '').then((ok) => {
    if (!ok) return;
    const icon = btn.querySelector('i');
    btn.classList.add('copied');
    btn.title = t('chat.copied');
    icon?.classList.replace('bi-clipboard', 'bi-check');
    setTimeout(() => {
      btn.classList.remove('copied');
      btn.title = t('chat.copy_code');
      icon?.classList.replace('bi-check', 'bi-clipboard');
    }, 1500);
  });
});

async function copyToClipboard(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // The Clipboard API requires a secure context; a plain-http LAN box needs
    // the legacy fallback.
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.cssText = 'position:fixed;opacity:0';
    document.body.appendChild(ta);
    ta.select();
    try { return document.execCommand('copy'); }
    catch { return false; }
    finally { ta.remove(); }
  }
}

// Disable Shadow DOM so Bootstrap CSS flows through naturally.
export class LightElement extends LitElement {
  createRenderRoot() { return this; }
}
