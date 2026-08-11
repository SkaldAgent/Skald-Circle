import hljs      from '../vendor/hljs/core.min.js';
import DOMPurify from 'dompurify';
import python     from '../vendor/hljs/languages/python.min.js';
import javascript from '../vendor/hljs/languages/javascript.min.js';
import typescript from '../vendor/hljs/languages/typescript.min.js';
import json       from '../vendor/hljs/languages/json.min.js';
import yaml       from '../vendor/hljs/languages/yaml.min.js';
import bash       from '../vendor/hljs/languages/bash.min.js';

hljs.registerLanguage('python',     python);
hljs.registerLanguage('javascript', javascript);
hljs.registerLanguage('typescript', typescript);
hljs.registerLanguage('json',       json);
hljs.registerLanguage('yaml',       yaml);
hljs.registerLanguage('bash',       bash);

// File extension → hljs language. Extensions not listed here render as plain
// text (never auto-detected: guessing wrong is worse than no colors).
const LANG_FOR_EXT = {
  py:   'python',
  js: 'javascript', mjs: 'javascript', cjs: 'javascript', jsx: 'javascript',
  ts: 'typescript', tsx: 'typescript',
  json: 'json',
  yml: 'yaml', yaml: 'yaml',
  sh: 'bash', bash: 'bash', zsh: 'bash', fish: 'bash',
};

// Markdown fence info string → hljs language (common aliases included).
const LANG_FOR_FENCE = {
  py: 'python', python: 'python',
  js: 'javascript', jsx: 'javascript', mjs: 'javascript', cjs: 'javascript', javascript: 'javascript',
  ts: 'typescript', tsx: 'typescript', typescript: 'typescript',
  json: 'json',
  yml: 'yaml', yaml: 'yaml',
  sh: 'bash', bash: 'bash', shell: 'bash', zsh: 'bash',
};

export function codeLangForExt(ext) {
  return LANG_FOR_EXT[ext] ?? null;
}

/**
 * Highlight `code` as `lang`, returning sanitized HTML (hljs escapes the input
 * itself; DOMPurify is belt-and-braces). Returns null when the language is not
 * registered, so callers can fall back to plain text.
 */
export function highlightCode(code, lang) {
  if (!lang || !hljs.getLanguage(lang)) return null;
  return DOMPurify.sanitize(hljs.highlight(code, { language: lang }).value);
}

/**
 * Post-process sanitized marked HTML: highlight every `<pre><code
 * class="language-…">` whose fence maps to a registered language, in place.
 * Unknown fences are left as the plain (already escaped) text marked emitted.
 */
export function highlightMarkdownCodeBlocks(htmlStr) {
  if (!htmlStr.includes('language-')) return htmlStr;
  const tpl = document.createElement('template');
  tpl.innerHTML = htmlStr;
  let changed = false;
  for (const code of tpl.content.querySelectorAll('pre > code[class*="language-"]')) {
    const fence = [...code.classList].find(c => c.startsWith('language-'))?.slice(9);
    const lang  = LANG_FOR_FENCE[fence];
    const out   = lang && highlightCode(code.textContent ?? '', lang);
    if (!out) continue;
    code.innerHTML = out;
    code.classList.add('hljs');
    changed = true;
  }
  return changed ? tpl.innerHTML : htmlStr;
}
