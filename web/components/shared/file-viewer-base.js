import { html, nothing } from 'lit';
import { unsafeHTML }     from 'lit/directives/unsafe-html.js';
import { keyed }          from 'lit/directives/keyed.js';
import { LightElement, renderMarkdown } from '../../lib/base.js';
import { codeLangForExt, highlightCode } from '../../lib/highlight.js';
import { fileWatcher }    from '../../lib/file-watcher.js';
import { t }              from '../../lib/i18n.js';
import './pdf-view.js';   // registers <pdf-view>; pdf.js itself is imported lazily

/**
 * Shared file-viewer engine. Holds all of the fetch / kind-detection /
 * markdown-asset-rewriting / LaTeX-compile / live-watch logic plus `_renderBody`,
 * driven purely by two methods: `_show(path)` and `_hide()`. It carries no
 * navigation or page chrome of its own — subclasses (desktop `<file-viewer-page>`
 * and mobile `<mobile-file-viewer-page>`) wire visibility/path to those methods
 * and provide their own `render()` header.
 */

const IMG_EXTS  = ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'ico', 'avif'];
const LATEX_EXTS = ['tex', 'latex'];
const TEXT_EXTS = [
  'txt', 'md', 'markdown', 'rs', 'js', 'mjs', 'cjs', 'ts', 'tsx', 'jsx',
  'py', 'json', 'yml', 'yaml', 'toml', 'sh', 'bash', 'zsh', 'fish',
  'css', 'scss', 'less',
  'sql', 'go', 'java', 'c', 'h', 'cpp', 'hpp', 'cc', 'kt', 'scala',
  'lua', 'pl', 'php', 'rb', 'swift', 'dart',
  'xml', 'csv', 'tsv', 'log', 'env', 'ini', 'cfg', 'conf',
  'gitignore', 'dockerignore', 'editorconfig',
  'vue', 'svelte', 'astro',
  // LaTeX is also kept here as the fallback when compilation fails — kindFor
  // still routes it to 'latex' so the viewer knows to attempt a compile first.
  'tex', 'latex',
];

export function extOf(path) {
  if (!path) return '';
  const dot = path.lastIndexOf('.');
  if (dot < 0) return '';
  // Reject dots that are inside a directory segment, not the file extension.
  if (path.indexOf('/', dot + 1) >= 0) return '';
  return path.slice(dot + 1).toLowerCase();
}

export function kindFor(path) {
  const ext = extOf(path);
  // SVG is excluded from IMG_EXTS on purpose: rendered in a sandboxed iframe
  // (not <img>), which both scales viewBox-only SVGs to fill the viewport and
  // isolates any embedded <script> from the host page.
  if (ext === 'svg')             return 'svg';
  if (IMG_EXTS.includes(ext))    return 'image';
  if (ext === 'pdf')             return 'pdf';
  // HTML is rendered live in a script-enabled but origin-isolated iframe
  // (srcdoc + sandbox="allow-scripts", no allow-same-origin) — see _renderBody.
  if (ext === 'html' || ext === 'htm') return 'html';
  if (LATEX_EXTS.includes(ext))  return 'latex';
  if (TEXT_EXTS.includes(ext))   return 'text';
  return 'binary';
}

/** Directory portion of a path: `docs/guide.md` → `docs`, `guide.md` → ``. */
function dirOf(path) {
  const i = path.lastIndexOf('/');
  return i < 0 ? '' : path.slice(0, i);
}

/** Lexically resolve `.`/`..` segments, preserving a leading slash for absolute paths. */
function normalizePath(p) {
  const abs = p.startsWith('/');
  const out = [];
  for (const seg of p.split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') { out.pop(); continue; }
    out.push(seg);
  }
  return (abs ? '/' : '') + out.join('/');
}

/**
 * Resolve an asset reference found inside a markdown file. External URLs, data
 * URIs, protocol-relative and root-relative paths are left untouched; a path
 * relative to the markdown file's directory is routed through `/api/file` so it
 * loads from disk instead of resolving against the SPA origin. In history mode
 * (`rev` set) the asset is served from the same extracted tree as the markdown
 * itself, so the image is contemporaneous with the text referencing it.
 */
function resolveAssetSrc(src, baseDir, rev) {
  if (!src || /^([a-z][a-z0-9+.-]*:|\/\/|#|\/)/i.test(src)) return src;
  const joined = baseDir ? `${baseDir}/${src}` : src;
  let url = `/api/file?path=${encodeURIComponent(normalizePath(joined))}`;
  if (rev) url += `&rev=${encodeURIComponent(rev)}`;
  return url;
}

/**
 * Rewrite relative `<img>` sources in rendered markdown HTML so they resolve
 * against the markdown file's location on disk (via `/api/file`). Parsed in an
 * inert <template> so the original (broken) URLs never trigger a fetch.
 */
function rewriteMarkdownAssets(htmlStr, baseDir, rev) {
  const tpl = document.createElement('template');
  tpl.innerHTML = htmlStr;
  let changed = false;
  for (const img of tpl.content.querySelectorAll('img[src]')) {
    const src = img.getAttribute('src');
    const resolved = resolveAssetSrc(src, baseDir, rev);
    if (resolved !== src) { img.setAttribute('src', resolved); changed = true; }
  }
  return changed ? tpl.innerHTML : htmlStr;
}

/**
 * Distil a raw latexmk / xelatex log into its actionable error block.
 *
 * The 422 body carries the *full* log. Under `-file-line-error` the meaningful
 * `path:line: message` errors (and `! TeX error` lines) sit deep in the log —
 * the opening lines are only the engine banner and package preamble. Slicing
 * the first N characters therefore hid the real error; instead we extract the
 * error lines plus a few trailing context lines (LaTeX echoes the offending
 * source line right after) so the user can read it — or paste it straight into
 * an agent. Falls back to the log tail when no error line is recognised.
 */
function formatLatexError(log) {
  if (!log) return '';
  const lines = log.split('\n');
  const blocks = [];
  for (let i = 0; i < lines.length; i++) {
    if (/:\d+: /.test(lines[i]) || lines[i].startsWith('! ')) {
      blocks.push(lines.slice(i, i + 4).join('\n').trimEnd());
    }
  }
  const excerpt = blocks.join('\n\n').trim();
  if (excerpt) return excerpt;
  return (log.length > 4000 ? log.slice(-4000) : log).trim();
}

export class FileViewerBase extends LightElement {
  static properties = {
    _path:         { state: true },
    _kind:         { state: true },
    _content:      { state: true },
    _codeHtml:     { state: true }, // highlighted HTML for code files (null = plain text)
    _blobUrl:      { state: true },
    _loading:      { state: true },
    _error:        { state: true },
    _compileError: { state: true },
    _htmlMode:     { state: true },
    // ── Markdown source editor (View | Edit) ─────────────────────────────────
    _mdMode:       { state: true }, // 'view' | 'edit'
    _editBuffer:   { state: true }, // text being edited (diverges from _content when dirty)
    _editDirty:    { state: true }, // _editBuffer !== _content
    _etag:         { state: true }, // server version token (GET ETag) for optimistic locking
    _canWrite:     { state: true }, // caller may edit this path (X-Writable)
    _conflict:     { state: true }, // remote changed while editing — show the banner
    _saving:       { state: true },
    // ── History mode (git-versioned files) ───────────────────────────────────
    _versions:     { state: true }, // null = not versioned/unknown; array = commits, newest first
    _currentRev:   { state: true }, // HEAD sha at load time
    _rev:          { state: true }, // revision being viewed (null = current working tree)
    _revInfo:      { state: true }, // the versions entry of _rev (drives the banner)
    _historyOpen:  { state: true }, // the versions popover
  };

  constructor() {
    super();
    this._path        = null;
    this._kind        = null;
    this._content     = '';
    this._codeHtml    = null;
    this._blobUrl      = null;
    this._loading     = false;
    this._error       = null;
    this._compileError = null;
    this._htmlMode    = 'preview'; // HTML view: 'preview' (live iframe) | 'source'
    this._mdMode      = 'view';    // MD: 'view' (rendered) | 'edit' (source textarea)
    this._editBuffer  = '';
    this._editDirty   = false;
    this._etag        = null;
    this._canWrite    = false;
    this._conflict    = false;
    this._saving      = false;
    this._versions    = null;
    this._currentRev  = null;
    this._rev         = null;
    this._revInfo     = null;
    this._historyOpen = false;
    this._watchPath   = null;     // path currently being watched (async-verified)
    this._watchUnsub  = null;     // unsubscribe function returned by fileWatcher
    this._reloadTimer = null;     // debounce timer for change-triggered reloads
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this._teardownWatch();
    if (this._reloadTimer) clearTimeout(this._reloadTimer);
    this._revokeBlobUrl();
  }

  // ── Drivers used by subclasses ──────────────────────────────────────────────

  /** Show `path`: (re)subscribe the watcher and load it. No-op if unchanged. */
  _show(path) {
    if (!path) return;
    if (path === this._path && !this._error) return; // already loaded
    // Guard unsaved edits when navigating to a different file: dropping them
    // silently is the worse failure mode. (Accepted wrinkle: the hash has
    // already moved; we don't fight the router here.)
    if (this._editDirty && !confirm(t('fv.dirty_warn'))) return;
    // Navigating to another file leaves any history mode behind.
    if (path !== this._path) {
      this._rev = null;
      this._revInfo = null;
      this._historyOpen = false;
      this._versions = null;
    }
    this._setupWatch(path);
    this._load(path);
  }

  /** Hide: drop the content and release the watcher. */
  _hide() {
    this._reset();
    this._teardownWatch();
  }

  /**
   * Download the current file. LaTeX sources always download the compiled PDF
   * (`compile-latex=true`); every kind is served with `force_download=true` so
   * the server sets `Content-Disposition: attachment` and the browser saves it
   * (with the server-supplied name) instead of rendering inline.
   */
  _download() {
    const path = this._path;
    if (!path) return;
    // In history mode the download is the version being viewed.
    const extra = { force_download: 'true' };
    if (this._kind === 'latex') extra['compile-latex'] = 'true';
    const a = document.createElement('a');
    a.href = this._fileUrl(path, extra);
    a.download = '';                 // server Content-Disposition supplies the name
    document.body.appendChild(a);
    a.click();
    a.remove();
  }

  /**
   * The one place `/api/file` URLs are built — in history mode every fetch
   * (content, compiled LaTeX, markdown assets, downloads) carries the same
   * `rev`, so the whole view comes from the tree at that revision.
   */
  _fileUrl(path, extra = {}) {
    const params = new URLSearchParams({ path, ...extra });
    if (this._rev) params.set('rev', this._rev);
    return `/api/file?${params.toString()}`;
  }

  _revokeBlobUrl() {
    if (this._blobUrl) {
      URL.revokeObjectURL(this._blobUrl);
      this._blobUrl = null;
    }
  }

  _reset() {
    this._path        = null;
    this._kind        = null;
    this._content     = '';
    this._codeHtml    = null;
    this._error       = null;
    this._compileError = null;
    this._htmlMode    = 'preview';
    this._mdMode      = 'view';
    this._editBuffer  = '';
    this._editDirty   = false;
    this._etag        = null;
    this._canWrite    = false;
    this._conflict    = false;
    this._rev         = null;
    this._revInfo     = null;
    this._historyOpen = false;
    this._versions    = null;
    this._currentRev  = null;
    this._revokeBlobUrl();
  }

  async _load(path, silent = false) {
    if (!silent) {
      this._path    = path;
      this._kind    = kindFor(path);
      this._content = '';
      this._codeHtml = null;
      this._error   = null;
      this._compileError = null;
      // Fresh load: drop any editor state from the previous file.
      this._mdMode    = 'view';
      this._editDirty = false;
      this._editBuffer = '';
      this._conflict  = false;
      this._revokeBlobUrl();
      this._loading = true;
      this._versions = null; // no stale history button while the new file loads
      this._loadVersions(path);
    } else {
      // Silent reload (file changed externally): keep showing the old content
      // until the new fetch lands; only update visible state on success.
      this._error = null;
    }
    try {
      const url = this._fileUrl(path);
      if (this._kind === 'image' || this._kind === 'pdf' || this._kind === 'svg') {
        const res = await fetch(url);
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const blob = await res.blob();
        // Swap URLs only after the new blob is ready so the preview never flickers.
        const oldUrl = this._blobUrl;
        this._blobUrl = URL.createObjectURL(blob);
        if (oldUrl) URL.revokeObjectURL(oldUrl);
      } else if (this._kind === 'latex') {
        await this._loadLatex(path);
      } else if (this._kind === 'text' || this._kind === 'html') {
        const res = await fetch(url);
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        this._content = await res.text();
        // Syntax highlighting is computed once per load (not per render) and is
        // best-effort: a highlight failure must never lose the file's content.
        this._codeHtml = null;
        if (this._kind === 'text') {
          try { this._codeHtml = highlightCode(this._content, codeLangForExt(extOf(path))); }
          catch { this._codeHtml = null; }
        }
        // Optimistic-locking version token + write flag (editable surface).
        this._etag     = res.headers.get('ETag');
        this._canWrite = res.headers.get('X-Writable') === '1';
      }
      // binary: nothing to fetch
      // Keep the editor buffer glued to the content on any non-dirty load
      // (initial load and silent external reloads while not editing). When the
      // user is mid-edit with unsaved changes, the buffer is deliberately left
      // alone — the watcher's _probeRemote path governs that case instead.
      if (!this._editDirty) this._editBuffer = this._content;
    } catch (e) {
      this._error = e.message || String(e);
    } finally {
      if (!silent) this._loading = false;
    }
  }

  /**
   * Load a `.tex` / `.latex` file. Tries to compile to PDF server-side first;
   * on any non-OK response (422 compilation error, 501 no latexmk, etc.) it
   * falls back to showing the raw source as plain text, preserving the error
   * message so the user can see why the compile failed.
   */
  async _loadLatex(path) {
    const compileUrl = this._fileUrl(path, { 'compile-latex': 'true' });
    try {
      const res = await fetch(compileUrl);
      if (res.ok) {
        const blob = await res.blob();
        const oldUrl = this._blobUrl;
        this._blobUrl = URL.createObjectURL(blob);
        if (oldUrl) URL.revokeObjectURL(oldUrl);
        this._content = '';
        this._compileError = null;
        return;
      }
      // The 422 body is the full latexmk log. Extract the actionable error
      // block (see formatLatexError) instead of slicing from the top, which
      // under -file-line-error only shows the preamble and hides the real error.
      let detail = '';
      try { detail = formatLatexError(await res.text()); } catch { /* ignore */ }
      this._compileError = detail || `HTTP ${res.status}`;
    } catch (e) {
      this._compileError = e.message || String(e);
    }
    // Fallback: fetch the raw .tex source.
    this._revokeBlobUrl();
    const res = await fetch(this._fileUrl(path));
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    this._content = await res.text();
  }

  // ── History mode (git-versioned files) ─────────────────────────────────────

  /**
   * Load the version list for `path`. Anything that cannot have a history
   * answers `versioned: false` and the clock button simply never appears —
   * this fetch failing is therefore silent by design.
   */
  async _loadVersions(path) {
    try {
      const res = await fetch(`/api/file/versions?path=${encodeURIComponent(path)}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      // Race: the viewer may have navigated away while the fetch was in flight.
      if (path !== this._path) return;
      this._versions   = data.versioned ? (data.versions || []) : null;
      this._currentRev = data.current_rev || null;
    } catch {
      this._versions = null;
      this._currentRev = null;
    }
  }

  /** Open one version from the popover. Selecting HEAD is "back to current". */
  _selectVersion(entry) {
    this._historyOpen = false;
    if (!entry || entry.rev === this._currentRev) {
      this._backToCurrent();
      return;
    }
    this._rev = entry.rev;
    this._revInfo = entry;
    this._load(this._path);
  }

  _backToCurrent() {
    this._historyOpen = false;
    if (!this._rev) return;
    this._rev = null;
    this._revInfo = null;
    this._load(this._path);
  }

  _fmtVersionDate(iso) {
    const d = new Date(iso);
    if (isNaN(d)) return iso;
    return d.toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric' }) +
      ' ' + d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  }

  // ── File watcher ────────────────────────────────────────────────────────────

  _setupWatch(path) {
    this._teardownWatch();
    if (!path) return;
    this._watchPath = path;
    fileWatcher.watch(path, () => this._onFileChanged())
      .then(unsub => {
        // Race: if the path changed or the page closed while awaiting, release
        // the subscription immediately so the OS watcher is torn down.
        if (this._watchPath !== path) {
          try { unsub(); } catch { /* ignore */ }
          return;
        }
        this._watchUnsub = unsub;
      })
      .catch(() => { /* WS error; client auto-reconnects and re-subscribes */ });
  }

  _teardownWatch() {
    if (this._reloadTimer) {
      clearTimeout(this._reloadTimer);
      this._reloadTimer = null;
    }
    if (this._watchUnsub) {
      try { this._watchUnsub(); } catch { /* ignore */ }
      this._watchUnsub = null;
    }
    this._watchPath = null;
  }

  _onFileChanged() {
    // History mode shows an immutable revision: working-tree changes must not
    // yank the user back to the present. The watcher re-engages on "back to
    // current".
    if (this._rev) return;
    // Debounce: collapse bursts of FS events into a single reload. `_watchPath`
    // is cleared by `_teardownWatch` (called on hide/path-change), so a queued
    // change never reloads a file the viewer has already navigated away from.
    if (this._reloadTimer) return;
    this._reloadTimer = setTimeout(() => {
      this._reloadTimer = null;
      const path = this._watchPath;
      if (!path) return;
      // While editing with unsaved changes, never clobber the buffer. Instead
      // probe the server for the current version: if it moved on from what we
      // last knew, raise a conflict; if it matches (most often our own save
      // echoing back), stay quiet.
      if (this._mdMode === 'edit' && this._editDirty) {
        this._probeRemote(path);
        return;
      }
      this._load(path, true);
    }, 300);
  }

  async _probeRemote(path) {
    try {
      const res = await fetch(`/api/file?path=${encodeURIComponent(path)}`);
      const remote = res.headers.get('ETag');
      if (remote && remote !== this._etag) this._conflict = true;
    } catch { /* transient — the next change event retries */ }
  }

  // ── HTML preview/source toggle ──────────────────────────────────────────────

  _toggleHtmlMode() {
    this._htmlMode = this._htmlMode === 'preview' ? 'source' : 'preview';
  }

  // ── Markdown View | Edit ────────────────────────────────────────────────────

  /** Switch the Markdown surface between rendered view and source editor. */
  _setMdMode(mode) {
    if (mode === this._mdMode) return;
    if (mode === 'edit') {
      // Entering edit: seed the buffer from the on-disk content (unless the
      // user still has unsaved edits from a previous foray into Edit on this
      // same file, which we preserve).
      if (!this._editDirty) this._editBuffer = this._content;
    }
    this._mdMode = mode;
  }

  _onEditInput(e) {
    this._editBuffer = e.target.value;
    this._editDirty  = this._editBuffer !== this._content;
  }

  /** Discard edits and return to the rendered view. */
  _cancelEdit() {
    if (this._editDirty && !confirm(t('fv.dirty_warn'))) return;
    this._editBuffer = this._content;
    this._editDirty  = false;
    this._conflict   = false;
    this._mdMode     = 'view';
  }

  /**
   * Persist the buffer. By default it sends `if_match` (optimistic locking): a
   * `409` means the file changed remotely and we surface a conflict instead of
   * overwriting. With `force=true` (the "Overwrite" button) it omits the token
   * and clobbers whatever is on disk.
   */
  async _save({ force = false } = {}) {
    if (!this._path || this._saving) return;
    this._saving = true;
    try {
      const payload = { path: this._path, content: this._editBuffer };
      if (!force) payload.if_match = this._etag;
      const res = await fetch('/api/file', {
        method:  'PUT',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify(payload),
      });
      if (res.status === 409) {
        // Remote moved on — keep the buffer, let the user decide via the banner.
        this._conflict = true;
        return;
      }
      if (!res.ok) throw new Error(await res.text());
      // Success: adopt the new version token + the written content as the new
      // baseline. The buffer stays equal to _content ⇒ no longer dirty.
      const etag = res.headers.get('ETag');
      if (etag) this._etag = etag;
      this._content   = this._editBuffer;
      this._editDirty = false;
      this._conflict  = false;
    } catch (e) {
      this._error = e.message || String(e);
    } finally {
      this._saving = false;
    }
  }

  /** Conflict resolution: reload the remote version, discarding local edits. */
  async _reloadRemote() {
    if (!this._path) return;
    this._conflict = false;
    this._editDirty = false;
    await this._load(this._path, true);
    this._editBuffer = this._content;
  }

  /** Conflict resolution: overwrite the remote with our buffer (no lock check). */
  _overwrite() {
    this._conflict = false;
    this._save({ force: true });
  }

  /** Conflict resolution: copy our edits to the clipboard, then reload remote. */
  async _copyMyChanges() {
    try {
      await navigator.clipboard.writeText(this._editBuffer);
    } catch { /* clipboard may be blocked; the reload still proceeds */ }
    await this._reloadRemote();
  }

  /**
   * Header button that flips an HTML file between the live preview and its raw
   * source. Returns `nothing` for every other kind, so subclasses can drop it
   * unconditionally into their header. `btnClass` carries the chrome-specific
   * button styling (desktop vs mobile use different classes).
   */
  _renderModeToggle(btnClass) {
    if (this._kind !== 'html') return nothing;
    const showingSource = this._htmlMode === 'source';
    return html`<button
      class=${btnClass}
      title=${showingSource ? t('fv.mode_preview') : t('fv.mode_source')}
      @click=${() => this._toggleHtmlMode()}>
      <i class="bi ${showingSource ? 'bi-eye' : 'bi-code-slash'}"></i>
    </button>`;
  }

  /** View | Edit tab bar for Markdown. Only rendered when the caller can write. */
  _renderMdTabs() {
    const view = this._mdMode === 'view';
    return html`<div class="fv-md-tabs" role="tablist">
      <button class="fv-md-tab ${view ? 'active' : ''}" role="tab" aria-selected=${view}
        @click=${() => this._setMdMode('view')}>${t('fv.tab_view')}</button>
      <button class="fv-md-tab ${!view ? 'active' : ''}" role="tab" aria-selected=${!view}
        @click=${() => this._setMdMode('edit')}>${t('fv.tab_edit')}</button>
      ${this._editDirty
        ? html`<span class="fv-md-dirty-dot" title=${t('fv.dirty_badge')}></span>`
        : nothing}
    </div>`;
  }

  /**
   * The history button + versions popover, rendered in the header of git-
   * versioned files. Returns `nothing` when the file has no versions, so
   * subclasses can drop it unconditionally into their header. `btnClass`
   * carries the chrome-specific button styling (desktop vs mobile).
   */
  _renderHistoryButton(btnClass) {
    if (!this._versions?.length) return nothing;
    return html`<span class="fv-history">
      <button class=${btnClass} title=${t('fv.history')}
        @click=${() => { this._historyOpen = !this._historyOpen; }}>
        <i class="bi bi-clock-history"></i>
      </button>
      ${this._historyOpen ? html`
        <div class="fv-history-overlay" @click=${() => { this._historyOpen = false; }}></div>
        <div class="fv-history-pop" role="menu">
          ${this._versions.map(v => html`
            <button class="fv-history-row ${v.rev === this._rev ? 'active' : ''}" role="menuitem"
              @click=${() => this._selectVersion(v)}>
              <span class="fv-history-date">
                ${this._fmtVersionDate(v.date)}
                ${v.rev === this._currentRev
                  ? html`<span class="fv-history-current">${t('fv.current')}</span>`
                  : nothing}
              </span>
              <span class="fv-history-subject"><bdi>${v.subject}</bdi></span>
            </button>`)}
        </div>` : nothing}
    </span>`;
  }

  /**
   * The history banner: shown while viewing a past revision — what it is, and
   * the way back. Rendered by the subclasses between header and body.
   */
  _renderVersionBanner() {
    if (!this._rev) return nothing;
    const v = this._revInfo;
    return html`<div class="fv-version-banner" role="status">
      <i class="bi bi-clock-history"></i>
      <span class="fv-version-text">
        ${t('fv.version_banner', { date: v ? this._fmtVersionDate(v.date) : this._rev.slice(0, 7) })}${v?.subject
          ? html` — <bdi>${v.subject}</bdi>`
          : nothing}
      </span>
      <button class="btn btn-sm btn-outline-secondary" @click=${() => this._backToCurrent()}>
        <i class="bi bi-arrow-counterclockwise"></i>&nbsp;${t('fv.back_to_current')}
      </button>
    </div>`;
  }

  /**
   * The conflict banner: shown while editing when the file was modified
   * remotely (another user / tab / agent) after our buffer diverged. Three
   * escapes — reload remote (discard mine), overwrite (force my version), or
   * copy mine to the clipboard before reloading.
   */
  _renderConflictBanner() {
    if (!this._conflict) return nothing;
    return html`<div class="fv-conflict-banner" role="alert">
      <i class="bi bi-exclamation-triangle-fill"></i>
      <span class="fv-conflict-text">${t('fv.conflict_title')}</span>
      <div class="fv-conflict-actions">
        <button class="btn btn-sm btn-outline-secondary" @click=${() => this._reloadRemote()}>
          ${t('fv.conflict_reload')}
        </button>
        <button class="btn btn-sm btn-outline-secondary" @click=${() => this._copyMyChanges()}>
          ${t('fv.conflict_copy')}
        </button>
        <button class="btn btn-sm btn-warning" @click=${() => this._overwrite()}>
          ${t('fv.conflict_overwrite')}
        </button>
      </div>
    </div>`;
  }

  // ── Body rendering (shared by both chromes) ─────────────────────────────────

  _renderBody() {
    // Spinner while loading, and also in the pre-load window: the mobile viewer
    // is prop-driven, so Lit runs render() (visible just flipped true) before
    // `updated()` kicks off `_show()` — at that point no kind/content exists yet.
    if (this._loading || (!this._kind && !this._error)) {
      return html`<div class="fv-state"><span class="spinner-border"></span></div>`;
    }
    if (this._error) {
      return html`<div class="fv-state text-danger">
        <i class="bi bi-exclamation-triangle fs-3 d-block mb-2"></i>${this._error}
      </div>`;
    }
    if (this._kind === 'image' && this._blobUrl) {
      return html`<div class="fv-image-wrap"><img src=${this._blobUrl} alt=${this._path} class="fv-image" /></div>`;
    }
    if (this._kind === 'pdf' && this._blobUrl) {
      // Drawn by <pdf-view> (pdf.js on canvas), never by the browser's built-in
      // viewer in an <iframe>: WebKit renders a framed PDF as a static first-page
      // thumbnail, so on iOS — Safari and every WKWebView, the native shell
      // included — the document had one page and no scroll. It also removes the
      // per-browser viewer chrome (Chrome's toolbar, Safari's page sidebar), so
      // a PDF now looks the same everywhere. `keyed` is gone with the iframe:
      // updating a property pushes no session-history entry, which is what the
      // watch-reload loop used to bury the back button under blob: entries.
      return html`<pdf-view class="fv-pdf" .src=${this._blobUrl}></pdf-view>`;
    }
    if (this._kind === 'latex' && this._blobUrl) {
      // Successfully compiled server-side — render the resulting PDF exactly as
      // a native .pdf is rendered (see the note above).
      return html`<pdf-view class="fv-pdf" .src=${this._blobUrl}></pdf-view>`;
    }
    if (this._kind === 'svg' && this._blobUrl) {
      // `allow-same-origin` (and nothing else) is required so the iframe can load
      // the blob: URL — those are only readable from their creating origin. With
      // `allow-scripts` absent, any <script> inside the SVG still cannot execute,
      // so this stays an isolated, script-free render.
      return html`<div class="fv-image-wrap">
        ${keyed(this._blobUrl, html`<iframe class="fv-svg" sandbox="allow-same-origin" src=${this._blobUrl} title=${this._path}></iframe>`)}
      </div>`;
    }
    if (this._kind === 'binary') {
      return html`<div class="fv-state text-muted">
        <i class="bi bi-file-earmark-binary fs-3 d-block mb-2"></i>
        ${t('fv.binary_unavailable')}
      </div>`;
    }
    if (this._kind === 'html') {
      if (this._htmlMode === 'source') {
        return html`<pre class="fv-code"><code>${this._content}</code></pre>`;
      }
      // Live render. `srcdoc` (not a blob: src) gives the frame a unique opaque
      // origin, so `allow-scripts` can run the page's JS while it stays fully
      // isolated from the app origin — no `allow-same-origin`, so it cannot read
      // cookies/localStorage or reach `/api/*`. Never add allow-same-origin here:
      // combined with allow-scripts it lets the frame remove its own sandbox.
      return html`<iframe
        class="fv-html"
        sandbox="allow-scripts allow-forms allow-modals allow-popups"
        srcdoc=${this._content}
        title=${this._path}></iframe>`;
    }
    const ext = extOf(this._path);
    if (ext === 'md' || ext === 'markdown') {
      const editing = this._mdMode === 'edit' && this._canWrite;
      // In View we render the edits in flight too (when dirty), so toggling
      // View/Edit is a live preview of what you're writing — not a flashback to
      // the on-disk content.
      const mdSrc = editing || this._editDirty ? this._editBuffer : this._content;
      const rendered = rewriteMarkdownAssets(renderMarkdown(mdSrc), dirOf(this._path || ''), this._rev);
      return html`<div class="fv-md-wrap">
        ${this._canWrite ? this._renderMdTabs() : nothing}
        ${editing
          ? html`<div class="fv-edit">
              ${this._renderConflictBanner()}
              <textarea class="fv-edit-textarea"
                .value=${this._editBuffer}
                spellcheck="false"
                autocomplete="off"
                autocapitalize="off"
                placeholder=${t('fv.edit_placeholder')}
                @input=${this._onEditInput}></textarea>
              <div class="fv-edit-toolbar">
                <button class="btn btn-sm btn-primary fv-edit-save"
                  ?disabled=${!this._editDirty || this._saving}
                  @click=${() => this._save()}>${this._saving ? t('fv.saving') : t('fv.save')}</button>
                <button class="btn btn-sm btn-outline-secondary"
                  @click=${() => this._cancelEdit()}>${t('fv.cancel')}</button>
                <span class="fv-edit-hint">${t('fv.edit_hint')}</span>
              </div>
            </div>`
          : html`<div class="fv-md">${unsafeHTML(rendered)}</div>`}
      </div>`;
    }
    if (this._kind === 'latex') {
      // Compile failed — show why, then fall back to the source.
      return html`
        ${this._compileError
          ? html`<details class="fv-compile-error">
              <summary><i class="bi bi-exclamation-triangle text-warning"></i>&nbsp;${t('fv.latex_failed')}</summary>
              <pre>${this._compileError}</pre>
            </details>`
          : nothing}
        <pre class="fv-code"><code>${this._content}</code></pre>
      `;
    }
    if (this._codeHtml != null) {
      return html`<pre class="fv-code"><code class="hljs">${unsafeHTML(this._codeHtml)}</code></pre>`;
    }
    return html`<pre class="fv-code"><code>${this._content}</code></pre>`;
  }
}
