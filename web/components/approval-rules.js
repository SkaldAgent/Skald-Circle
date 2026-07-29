import { html, nothing } from 'lit';
import { unsafeHTML }     from 'lit/directives/unsafe-html.js';
import { LightElement }  from '../lib/base.js';
import { t }             from '../lib/i18n.js';

const DEFAULT_PRIORITY = 999999;

const ACTIONS = ['require', 'allow', 'deny'];

const ACTION_STYLE = {
  require: { icon: 'bi-person-check',  bg: 'rgba(234,179,8,0.12)',  color: '#a16207' },
  allow:   { icon: 'bi-check-circle',  bg: 'rgba(34,197,94,0.12)',  color: '#16a34a' },
  deny:    { icon: 'bi-slash-circle',  bg: 'rgba(239,68,68,0.12)',  color: '#dc2626' },
};

const CATEGORY_ORDER = ['filesystem', 'shell', 'subagent', 'introspection', 'config', 'dynamic'];

// File System permission model. Each path row maps to exactly one approval rule via a
// synthetic `@fs_*` tool_pattern token (understood by the backend matcher). A single
// selector collapses the (access-class × action) axes into the mental model from the
// mockup: Allow read / Allow write / Deny / Require.
const FS_ACCESS = {
  allow_read:  { tool_pattern: '@fs_read', action: 'allow'   },
  allow_write: { tool_pattern: '@fs_any',  action: 'allow'   },
  deny:        { tool_pattern: '@fs_any',  action: 'deny'    },
  require:     { tool_pattern: '@fs_any',  action: 'require' },
};
// Priority band for the settable "Default" row (below specific fs path rules, above the
// global `*` catch-all at 999999).
const FS_DEFAULT_PRIORITY = 900;

export class ApprovalRulesPage extends LightElement {
  static properties = {
    _open:          { state: true },
    _rules:         { state: true },
    _tools:         { state: true },
    _error:         { state: true },
    _selectedGroup: { state: true },
    _editingId:     { state: true },
    _formMode:      { state: true },   // 'override' | 'lowprio' | null
    _form:          { state: true },
    _toolFilter:    { state: true },
    _saving:        { state: true },
    _openSections:  { state: true },   // Set<string>
    _overrideOpen:  { state: true },
    _lowPrioOpen:   { state: true },
    _toolSaving:    { state: true },   // Set<string>
    _fsOpen:        { state: true },
    _fsNewPath:     { state: true },
    _fsNewAccess:   { state: true },
    _fsSaving:      { state: true },   // Set<number|'new'>
  };

  constructor() {
    super();
    this._open          = false;
    this._rules         = [];
    this._tools         = null;
    this._error         = null;
    this._selectedGroup = null;
    this._editingId     = null;
    this._formMode      = null;
    this._form          = this._emptyForm(null);
    this._toolFilter    = '';
    this._saving        = false;
    this._openSections  = new Set();
    this._overrideOpen  = false;
    this._lowPrioOpen   = false;
    this._toolSaving    = new Set();
    this._fsOpen        = true;
    this._fsNewPath     = '';
    this._fsNewAccess   = 'allow_read';
    this._fsSaving      = new Set();
  }

  _emptyForm(mode) {
    const priority = mode === 'override' ? -10 : 100;
    return { tool_pattern: '', path_pattern: '', action: 'require', priority, agent_id: '', source: '', note: '' };
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      if (e.detail.page !== 'approval') {
        this._open = false;
        this.style.display = 'none';
      }
    });
    window.addEventListener('approval-navigate', async (e) => {
      if (e.detail.group === null) {
        this._open = false;
        this.style.display = 'none';
        return;
      }
      this._open          = true;
      this.style.display  = 'flex';
      this._selectedGroup = e.detail.group;
      this._editingId     = null;
      this._formMode      = null;
      this._openSections  = new Set();
      this._overrideOpen  = false;
      this._lowPrioOpen   = false;
      this._fsOpen        = true;
      this._fsNewPath     = '';
      this._fsSaving      = new Set();
      await this._load();
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _load() {
    this._error = null;
    try {
      const [rulesRes, toolsRes] = await Promise.all([
        fetch('/api/approval/rules'),
        fetch('/api/approval/tools'),
      ]);
      if (!rulesRes.ok) throw new Error(`HTTP ${rulesRes.status}`);
      if (!toolsRes.ok) throw new Error(`HTTP ${toolsRes.status}`);
      this._rules = await rulesRes.json();
      this._tools = await toolsRes.json();
    } catch (e) {
      this._error = e.message;
    }
  }

  _rulesForGroup(groupId) {
    return this._rules.filter(r => (r.group_id ?? 'default') === groupId);
  }

  // ── Rule classification ────────────────────────────────────────────────────────

  _isSimpleRule(r) {
    return !r.tool_pattern.includes('*')
        && (r.path_pattern == null || r.path_pattern === '')
        && (r.agent_id    == null || r.agent_id    === '')
        && (r.source      == null || r.source      === '')
        && Number(r.priority) === 0;
  }

  _isDefaultRule(r) {
    return r.tool_pattern === '*'
        && (r.path_pattern == null || r.path_pattern === '')
        && (r.agent_id    == null || r.agent_id    === '')
        && (r.source      == null || r.source      === '')
        && Number(r.priority) === DEFAULT_PRIORITY;
  }

  // File System rules (`@fs_*` tool_pattern) are managed by their own panel, so keep
  // them out of the override/low-priority/default buckets and the per-tool matrix.
  _isFsRule(r) {
    return typeof r.tool_pattern === 'string' && r.tool_pattern.startsWith('@fs');
  }

  _buckets(groupId) {
    const all = this._rules.filter(r => (r.group_id ?? 'default') === groupId && !this._isFsRule(r));
    return {
      overrides: all.filter(r => Number(r.priority) < 0),
      lowPrio:   all.filter(r => !this._isDefaultRule(r) && !this._isSimpleRule(r) && Number(r.priority) >= 0),
      defRule:   all.find(r => this._isDefaultRule(r)) ?? null,
    };
  }

  _getSimpleRule(toolName, groupId) {
    return this._rules.find(r =>
      this._isSimpleRule(r) &&
      r.tool_pattern === toolName &&
      (r.group_id ?? 'default') === groupId
    ) ?? null;
  }

  _getToolAction(toolName) {
    return this._getSimpleRule(toolName, this._selectedGroup.id)?.action ?? null;
  }

  // ── Tool action CRUD ─────────────────────────────────────────────────────────

  async _setToolAction(toolName, action) {
    const existing = this._getSimpleRule(toolName, this._selectedGroup.id);
    this._toolSaving = new Set([...this._toolSaving, toolName]);
    this._error = null;
    try {
      if (action === null) {
        if (existing) {
          const res = await fetch(`/api/approval/rules/${existing.id}`, { method: 'DELETE' });
          if (!res.ok) throw new Error(await res.text());
        }
      } else if (existing) {
        const res = await fetch(`/api/approval/rules/${existing.id}`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            tool_pattern: existing.tool_pattern,
            path_pattern: existing.path_pattern ?? null,
            action,
            priority: 0,
            agent_id: existing.agent_id ?? null,
            source:   existing.source   ?? null,
            note:     existing.note     ?? null,
            group_id: existing.group_id ?? 'default',
          }),
        });
        if (!res.ok) throw new Error(await res.text());
      } else {
        const res = await fetch('/api/approval/rules', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            tool_pattern: toolName,
            action,
            priority: 0,
            group_id: this._selectedGroup.id,
          }),
        });
        if (!res.ok) throw new Error(await res.text());
      }
      await this._load();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._toolSaving = new Set([...this._toolSaving].filter(n => n !== toolName));
    }
  }

  // ── Default action CRUD ──────────────────────────────────────────────────────

  _getDefaultAction() {
    return this._buckets(this._selectedGroup?.id ?? 'default').defRule?.action ?? null;
  }

  async _setDefaultAction(action) {
    const existing = this._buckets(this._selectedGroup.id).defRule;
    this._error = null;
    try {
      if (action === null) {
        if (existing) {
          const res = await fetch(`/api/approval/rules/${existing.id}`, { method: 'DELETE' });
          if (!res.ok) throw new Error(await res.text());
        }
      } else if (existing) {
        const res = await fetch(`/api/approval/rules/${existing.id}`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            tool_pattern: '*',
            action,
            priority: DEFAULT_PRIORITY,
            group_id: this._selectedGroup.id,
          }),
        });
        if (!res.ok) throw new Error(await res.text());
      } else {
        const res = await fetch('/api/approval/rules', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            tool_pattern: '*',
            action,
            priority: DEFAULT_PRIORITY,
            group_id: this._selectedGroup.id,
          }),
        });
        if (!res.ok) throw new Error(await res.text());
      }
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── File System CRUD ──────────────────────────────────────────────────────────

  // Path rules: @fs_* rules that carry a path_pattern, ordered by evaluation priority.
  _fsPathRules(groupId) {
    return this._rules
      .filter(r =>
        (r.group_id ?? 'default') === groupId &&
        this._isFsRule(r) &&
        r.path_pattern != null && r.path_pattern !== '')
      .sort((a, b) => Number(a.priority) - Number(b.priority) || a.id - b.id);
  }

  // The optional settable fallback: an @fs_* rule with no path_pattern.
  _fsDefaultRule(groupId) {
    return this._rules.find(r =>
      (r.group_id ?? 'default') === groupId &&
      this._isFsRule(r) &&
      (r.path_pattern == null || r.path_pattern === '')
    ) ?? null;
  }

  // Round-trip a stored rule back to a FS_ACCESS selector key.
  _fsAccessValue(r) {
    if (r.action === 'deny')    return 'deny';
    if (r.action === 'require') return 'require';
    if (r.tool_pattern === '@fs_read') return 'allow_read';
    return 'allow_write';
  }

  // Strip `./`, leading slashes and any trailing `/` or `/*` — the caller appends `/*`.
  _normalizeFsPath(raw) {
    return (raw ?? '')
      .trim()
      .replace(/^\.\//, '')
      .replace(/^\/+/, '')
      .replace(/\/\*$/, '')
      .replace(/\/+$/, '');
  }

  // Display form of a rule's path (`memory/*` → `memory/`).
  _fsDisplayPath(r) {
    const pp = r.path_pattern ?? '';
    return pp.endsWith('/*') ? `${pp.slice(0, -2)}/` : pp;
  }

  // Deeper paths evaluate first; depth-1 lands on 5 to match the seeded defaults.
  _fsPriorityForPath(clean) {
    const depth = clean.split('/').filter(Boolean).length;
    return Math.max(1, 6 - depth);
  }

  async _addFsRule() {
    const clean = this._normalizeFsPath(this._fsNewPath);
    if (!clean) { this._error = t('approval.error.enter_path'); return; }
    const access = FS_ACCESS[this._fsNewAccess] ?? FS_ACCESS.allow_read;
    this._fsSaving = new Set([...this._fsSaving, 'new']);
    this._error = null;
    try {
      const res = await fetch('/api/approval/rules', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          tool_pattern: access.tool_pattern,
          path_pattern: `${clean}/*`,
          action:       access.action,
          priority:     this._fsPriorityForPath(clean),
          group_id:     this._selectedGroup.id,
          note:         'file system',
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._fsNewPath = '';
      await this._load();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._fsSaving = new Set([...this._fsSaving].filter(x => x !== 'new'));
    }
  }

  async _setFsAccess(rule, accessValue) {
    const access = FS_ACCESS[accessValue];
    if (!access) return;
    this._fsSaving = new Set([...this._fsSaving, rule.id]);
    this._error = null;
    try {
      const res = await fetch(`/api/approval/rules/${rule.id}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          tool_pattern: access.tool_pattern,
          path_pattern: rule.path_pattern,
          action:       access.action,
          priority:     rule.priority,
          group_id:     rule.group_id ?? 'default',
          note:         rule.note ?? 'file system',
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._fsSaving = new Set([...this._fsSaving].filter(x => x !== rule.id));
    }
  }

  async _deleteFsRule(rule) {
    if (!confirm(t('approval.confirm.delete_fs', { path: this._fsDisplayPath(rule) }))) return;
    this._fsSaving = new Set([...this._fsSaving, rule.id]);
    this._error = null;
    try {
      const res = await fetch(`/api/approval/rules/${rule.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._fsSaving = new Set([...this._fsSaving].filter(x => x !== rule.id));
    }
  }

  async _setFsDefault(accessValue) {
    const existing = this._fsDefaultRule(this._selectedGroup.id);
    this._error = null;
    try {
      if (accessValue === null) {
        if (existing) {
          const res = await fetch(`/api/approval/rules/${existing.id}`, { method: 'DELETE' });
          if (!res.ok) throw new Error(await res.text());
        }
      } else {
        const access = FS_ACCESS[accessValue];
        const body = {
          tool_pattern: access.tool_pattern,
          path_pattern: null,
          action:       access.action,
          priority:     FS_DEFAULT_PRIORITY,
          group_id:     this._selectedGroup.id,
          note:         'file system default',
        };
        const url = existing ? `/api/approval/rules/${existing.id}` : '/api/approval/rules';
        const res = await fetch(url, {
          method:  existing ? 'PUT' : 'POST',
          headers: { 'Content-Type': 'application/json' },
          body:    JSON.stringify(body),
        });
        if (!res.ok) throw new Error(await res.text());
      }
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Section toggling ─────────────────────────────────────────────────────────

  _toggleSection(id) {
    const s = new Set(this._openSections);
    if (s.has(id)) s.delete(id); else s.add(id);
    this._openSections = s;
  }

  // ── Tool grouping ─────────────────────────────────────────────────────────────

  _catLabel(key) {
    return {
      filesystem:    t('approval.category.filesystem'),
      shell:         t('approval.category.shell'),
      subagent:      t('approval.category.subagent'),
      introspection: t('approval.category.introspection'),
      config:        t('approval.category.config'),
      dynamic:       t('approval.category.dynamic'),
    }[key] ?? key;
  }

  _groupedTools() {
    if (!this._tools) return [];
    const map     = new Map();
    const metaMap = new Map();

    for (const t of this._tools.built_in) {
      if (t.category === 'filesystem') continue;
      const cat = t.category || 'other';
      if (!map.has(cat)) map.set(cat, []);
      map.get(cat).push(t);
    }
    const servers = this._tools.mcp_servers ?? {};
    for (const t of this._tools.mcp) {
      const serverId = t.server ?? t.name;
      const meta     = servers[serverId] ?? {};
      const key      = `mcp:${serverId}`;
      if (!map.has(key)) {
        map.set(key, []);
        if (meta.description) metaMap.set(key, meta.description);
      }
      map.get(key).push(t);
    }

    const result = [];
    for (const cat of CATEGORY_ORDER) {
      if (map.has(cat)) result.push([cat, map.get(cat), null]);
    }
    for (const [key, tools] of map.entries()) {
      if (!CATEGORY_ORDER.includes(key) && key !== 'other') result.push([key, tools, metaMap.get(key) ?? null]);
    }
    if (map.has('other')) result.push(['other', map.get('other'), null]);
    return result;
  }

  // ── Override / LowPrio rule management ────────────────────────────────────────

  _startNew(mode) {
    this._editingId  = 'new';
    this._formMode   = mode;
    this._form       = this._emptyForm(mode);
    this._toolFilter = '';
    if (mode === 'override') this._overrideOpen = true;
    if (mode === 'lowprio')  this._lowPrioOpen  = true;
  }

  _startEdit(rule) {
    this._editingId  = rule.id;
    this._formMode   = Number(rule.priority) < 0 ? 'override' : 'lowprio';
    this._toolFilter = '';
    this._form = {
      tool_pattern: rule.tool_pattern,
      path_pattern: rule.path_pattern ?? '',
      action:       rule.action,
      priority:     rule.priority,
      agent_id:     rule.agent_id  ?? '',
      source:       rule.source    ?? '',
      note:         rule.note      ?? '',
    };
    if (this._formMode === 'override') this._overrideOpen = true;
    if (this._formMode === 'lowprio')  this._lowPrioOpen  = true;
  }

  _cancelEdit() { this._editingId = null; this._formMode = null; this._toolFilter = ''; }

  _patch(field, value) { this._form = { ...this._form, [field]: value }; }

  _selectTool(name) { this._form = { ...this._form, tool_pattern: name }; }

  async _save() {
    if (!this._form.tool_pattern.trim()) { this._error = t('approval.error.tool_required'); return; }

    const p = Number(this._form.priority);
    if (this._formMode === 'override' && p >= 0) {
      this._error = t('approval.error.override_prio'); return;
    }
    if (this._formMode === 'lowprio' && (p <= 0 || p >= DEFAULT_PRIORITY)) {
      this._error = t('approval.error.lowprio_range', { max: DEFAULT_PRIORITY - 1 }); return;
    }

    this._saving = true;
    this._error  = null;
    try {
      const body = {
        tool_pattern: this._form.tool_pattern.trim(),
        path_pattern: this._form.path_pattern.trim() || null,
        action:       this._form.action,
        priority:     p,
        agent_id:     this._form.agent_id.trim() || null,
        source:       this._form.source.trim()   || null,
        note:         this._form.note.trim()     || null,
        group_id:     this._selectedGroup?.id ?? 'default',
      };
      const isNew = this._editingId === 'new';
      const url   = isNew ? '/api/approval/rules' : `/api/approval/rules/${this._editingId}`;
      const res   = await fetch(url, {
        method:  isNew ? 'POST' : 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify(body),
      });
      if (!res.ok) throw new Error(await res.text());
      this._editingId = null;
      this._formMode  = null;
      await this._load();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._saving = false;
    }
  }

  async _delete(rule) {
    if (!confirm(t('approval.confirm.delete_rule', { pattern: rule.tool_pattern }))) return;
    try {
      const res = await fetch(`/api/approval/rules/${rule.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Back to groups ────────────────────────────────────────────────────────────

  _goBack() {
    this._open          = false;
    this.style.display  = 'none';
    window.location.hash = 'approval';
    window.dispatchEvent(new CustomEvent('approval-navigate', { detail: { group: null } }));
  }

  // ── Tool picker ───────────────────────────────────────────────────────────────

  _renderToolPicker() {
    if (!this._tools) return nothing;
    const q       = this._toolFilter.toLowerCase();
    const current = this._form.tool_pattern;

    const allTools = [
      { name: '*',      description: t('approval.tool.any'),     source: 'glob', server: null },
      { name: 'mcp__*', description: t('approval.tool.any_mcp'), source: 'glob', server: null },
      ...this._tools.built_in,
      ...this._tools.mcp,
    ];

    const filtered = allTools.filter(t =>
      !q ||
      t.name.toLowerCase().includes(q) ||
      t.description.toLowerCase().includes(q) ||
      (t.server && t.server.toLowerCase().includes(q))
    );

    const groups = {};
    for (const tool of filtered) {
      const key = tool.source === 'mcp'
        ? t('approval.tool.group_mcp', { server: tool.server })
        : tool.source === 'built-in'
          ? t('approval.tool.group_builtin')
          : t('approval.tool.group_glob');
      if (!groups[key]) groups[key] = [];
      groups[key].push(tool);
    }

    return html`
      <div class="apr-tool-picker">
        <input
          class="form-control form-control-sm mb-2"
          placeholder=${t('approval.tool.search')}
          .value=${this._toolFilter}
          @input=${(e) => { this._toolFilter = e.target.value; }}
        />
        <div class="apr-tool-list">
          ${Object.entries(groups).map(([group, tools]) => html`
            <div class="apr-tool-group-label">${group}</div>
            ${tools.map(t => html`
              <button
                class="apr-tool-item ${current === t.name ? 'selected' : ''}"
                @click=${() => this._selectTool(t.name)}
                title=${t.description}
              >
                <code class="apr-tool-name">${t.name}</code>
                <span class="apr-tool-desc">${t.description}</span>
              </button>
            `)}
          `)}
          ${filtered.length === 0 ? html`<div class="text-muted p-2">${t('approval.tool.no_results')}</div>` : nothing}
        </div>
      </div>
    `;
  }

  // ── Override / LowPrio rule form ──────────────────────────────────────────────

  _renderForm() {
    const f          = this._form;
    const isOverride = this._formMode === 'override';
    return html`
      <div class="apr-form">
        <div class="apr-form-header">
          <i class="bi ${isOverride ? 'bi-exclamation-triangle' : 'bi-arrow-down-circle'}"></i>
          <span>${this._editingId === 'new'
            ? (isOverride ? t('approval.form.new_override') : t('approval.form.new_lowprio'))
            : t('approval.form.edit')}</span>
          <button class="apr-form-close" @click=${() => this._cancelEdit()}>
            <i class="bi bi-x"></i>
          </button>
        </div>
        <div class="apr-form-body">
          <div class="row g-3">
            <div class="col-12">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('approval.form.tool_pattern')} <span class="text-danger">*</span></label>
              <input
                class="form-control form-control-sm font-monospace"
                placeholder=${t('approval.form.tool_pattern_ph')}
                .value=${f.tool_pattern}
                @input=${(e) => this._patch('tool_pattern', e.target.value)}
              />
              <div class="form-text" style="font-size:0.75rem">${unsafeHTML(t('approval.form.tool_pattern_hint'))}</div>
            </div>
            <div class="col-12">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('approval.form.select_tool')}</label>
              ${this._renderToolPicker()}
            </div>
            <div class="col-12">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('approval.form.path_pattern')} <span class="text-muted fw-normal">${t('approval.label.optional')}</span></label>
              <input
                class="form-control form-control-sm font-monospace"
                placeholder=${t('approval.form.path_pattern_ph')}
                .value=${f.path_pattern}
                @input=${(e) => this._patch('path_pattern', e.target.value)}
              />
              <div class="form-text" style="font-size:0.75rem">${unsafeHTML(t('approval.form.path_pattern_hint'))}</div>
            </div>
            <div class="col-sm-4">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('approval.form.action')}</label>
              <select
                class="form-select form-select-sm"
                .value=${f.action}
                @change=${(e) => this._patch('action', e.target.value)}
              >
                ${ACTIONS.map(a => html`<option value=${a} ?selected=${f.action === a}>${a}</option>`)}
              </select>
            </div>
            <div class="col-sm-4">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('approval.form.priority')}</label>
              <input
                type="number"
                class="form-control form-control-sm"
                .value=${String(f.priority)}
                @input=${(e) => this._patch('priority', e.target.value)}
              />
              <div class="form-text" style="font-size:0.75rem">
                ${isOverride
                  ? unsafeHTML(t('approval.form.priority_override_hint'))
                  : unsafeHTML(t('approval.form.priority_lowprio_hint', { max: DEFAULT_PRIORITY - 1 }))}
              </div>
            </div>
            <div class="col-sm-4">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('approval.form.source')} <span class="text-muted fw-normal">${t('approval.label.optional')}</span></label>
              <select
                class="form-select form-select-sm"
                @change=${(e) => this._patch('source', e.target.value)}
              >
                <option value="" ?selected=${!f.source}>${t('approval.form.source_any')}</option>
                ${['web', 'telegram', 'cron'].map(s => html`
                  <option value=${s} ?selected=${f.source === s}>${s}</option>
                `)}
              </select>
            </div>
            <div class="col-sm-6">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('approval.form.agent_id')} <span class="text-muted fw-normal">${t('approval.label.optional')}</span></label>
              <input
                class="form-control form-control-sm font-monospace"
                placeholder=${t('approval.form.agent_id_ph')}
                .value=${f.agent_id}
                @input=${(e) => this._patch('agent_id', e.target.value)}
              />
            </div>
            <div class="col-sm-6">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('approval.form.note')} <span class="text-muted fw-normal">${t('approval.label.optional')}</span></label>
              <input
                class="form-control form-control-sm"
                placeholder=${t('approval.form.note_ph')}
                .value=${f.note}
                @input=${(e) => this._patch('note', e.target.value)}
              />
            </div>
          </div>
          <div class="apr-form-actions">
            <button type="button" class="btn btn-sm btn-outline-secondary" @click=${() => this._cancelEdit()}>${t('approval.form.cancel')}</button>
            <button class="btn btn-sm btn-primary" @click=${() => this._save()} ?disabled=${this._saving}>
              ${this._saving
                ? html`<span class="spinner-border spinner-border-sm me-1"></span>${t('approval.form.saving')}`
                : html`<i class="bi bi-check-lg me-1"></i>${t('approval.form.save')}`}
            </button>
          </div>
        </div>
      </div>
    `;
  }

  // ── Rule card ─────────────────────────────────────────────────────────────────

  _renderCard(rule) {
    const s   = ACTION_STYLE[rule.action] ?? ACTION_STYLE.require;
    const has = (v) => v != null && v !== '';
    return html`
      <div class="apr-card" style="--apr-action-bg: ${s.bg}; --apr-action-color: ${s.color}">
        <div class="apr-card-row1">
          <span class="apr-action-badge">
            <i class="bi ${s.icon}"></i>
            ${{ require: t('approval.action.require'), allow: t('approval.action.allow'), deny: t('approval.action.deny') }[rule.action] ?? rule.action}
          </span>
          <code class="apr-pattern">${rule.tool_pattern}</code>
          <span class="apr-priority-badge" title=${t('approval.card.priority')}>
            <i class="bi bi-list-ol"></i>
            ${rule.priority}
          </span>
          <div class="apr-card-actions">
            <button class="apr-btn-icon apr-btn-edit" title=${t('approval.card.edit')} @click=${() => this._startEdit(rule)}>
              <i class="bi bi-pencil"></i>
            </button>
            <button class="apr-btn-icon apr-btn-delete" title=${t('approval.card.delete')} @click=${() => this._delete(rule)}>
              <i class="bi bi-trash"></i>
            </button>
          </div>
        </div>
        ${has(rule.path_pattern) ? html`
          <div class="apr-card-row2">
            <span class="apr-tag"><i class="bi bi-folder2"></i><code>${rule.path_pattern}</code></span>
          </div>
        ` : ''}
        <div class="apr-card-row3">
          ${has(rule.source)   ? html`<span class="apr-tag"><i class="bi bi-box-arrow-in-right"></i>${rule.source}</span>` : ''}
          ${has(rule.agent_id) ? html`<span class="apr-tag"><i class="bi bi-robot"></i>${rule.agent_id}</span>` : ''}
          ${has(rule.note)     ? html`<span class="apr-tag apr-tag-note"><i class="bi bi-chat-text"></i>${rule.note}</span>` : ''}
        </div>
      </div>
    `;
  }

  // ── 4-state chip group ────────────────────────────────────────────────────────

  _renderChipGroup(currentAction, onChange) {
    const chips = [
      { action: null,      label: t('approval.chip.unset') },
      { action: 'allow',   label: t('approval.action.allow') },
      { action: 'require', label: t('approval.chip.req') },
      { action: 'deny',    label: t('approval.action.deny') },
    ];
    return html`
      <div class="apr-chip-group">
        ${chips.map(({ action, label }) => {
          const isActive = currentAction === action;
          return html`
            <button
              class="apr-chip ${isActive ? 'active' : ''}"
              data-action=${action ?? 'unset'}
              @click=${() => onChange(isActive && action !== null ? null : action)}
            >${label}</button>
          `;
        })}
      </div>
    `;
  }

  // ── Tool row ─────────────────────────────────────────────────────────────────

  _renderToolRow(tool) {
    const action = this._getToolAction(tool.name);
    const saving = this._toolSaving.has(tool.name);
    return html`
      <div class="apr-tool-row">
        <code class="apr-matrix-tool-name" title=${tool.description}>${tool.name}</code>
        ${saving
          ? html`<span class="spinner-border spinner-border-sm ms-auto" style="flex-shrink:0;color:var(--bs-secondary-color)"></span>`
          : this._renderChipGroup(action, (a) => this._setToolAction(tool.name, a))
        }
      </div>
    `;
  }

  // ── Category section ─────────────────────────────────────────────────────────

  _renderCategorySection(key, tools, description) {
    const open       = this._openSections.has(key);
    const groupId    = this._selectedGroup.id;
    const configured = tools.filter(t => this._getSimpleRule(t.name, groupId) !== null).length;
    const label      = key.startsWith('mcp:')
      ? t('approval.tool.group_mcp', { server: key.slice(4) })
      : this._catLabel(key);
    return html`
      <div class="apr-cat-section ${open ? 'apr-cat-section--open' : ''}">
        <div class="apr-cat-header" @click=${() => this._toggleSection(key)}>
          <i class="bi bi-chevron-${open ? 'down' : 'right'} apr-cat-chevron"></i>
          <span class="apr-cat-name">${label}</span>
          ${description ? html`<span class="apr-cat-desc">${description}</span>` : nothing}
          <span class="apr-cat-count ${configured === 0 ? 'apr-cat-count--muted' : ''}">
            ${configured > 0 ? `${configured}/` : ''}${tools.length}
          </span>
        </div>
        <div class="apr-cat-body">
          ${tools.map(t => this._renderToolRow(t))}
        </div>
      </div>
    `;
  }

  // ── Tool matrix ───────────────────────────────────────────────────────────────

  _renderToolMatrix() {
    const groups = this._groupedTools();
    return html`
      <div class="apr-matrix">
        <div class="apr-matrix-header">
          <span class="apr-matrix-title">${t('approval.matrix.title')}</span>
          <span class="apr-matrix-subtitle">${t('approval.matrix.subtitle')}</span>
        </div>
        <div class="apr-matrix-body">
          ${groups.length === 0
            ? html`<div class="text-muted p-4 text-center" style="font-size:0.85rem">${t('approval.matrix.loading')}</div>`
            : groups.map(([key, tools, desc]) => this._renderCategorySection(key, tools, desc))}
        </div>
      </div>
    `;
  }

  // ── File System panel ─────────────────────────────────────────────────────────

  _fsAccessLabel(key) {
    return {
      allow_read:  t('approval.fs.allow_read'),
      allow_write: t('approval.fs.allow_write'),
      deny:        t('approval.fs.deny'),
      require:     t('approval.fs.require'),
    }[key] ?? key;
  }

  _renderFsAccessSelect(value, onChange, allowUnset) {
    return html`
      <select
        class="form-select form-select-sm apr-fs-select"
        @change=${(e) => onChange(e.target.value || null)}
      >
        ${allowUnset
          ? html`<option value="" ?selected=${!value}>${t('approval.fs.default')}</option>`
          : nothing}
        ${Object.entries(FS_ACCESS).map(([k]) => html`
          <option value=${k} ?selected=${value === k}>${this._fsAccessLabel(k)}</option>
        `)}
      </select>
    `;
  }

  _renderFsRow(rule) {
    const saving = this._fsSaving.has(rule.id);
    const value  = this._fsAccessValue(rule);
    return html`
      <div class="apr-fs-row">
        <i class="bi bi-folder2 apr-fs-row-icon"></i>
        <code class="apr-fs-path" title=${rule.path_pattern ?? ''}>${this._fsDisplayPath(rule)}</code>
        ${saving
          ? html`<span class="spinner-border spinner-border-sm ms-auto" style="flex-shrink:0"></span>`
          : html`
            ${this._renderFsAccessSelect(value, (v) => v && this._setFsAccess(rule, v), false)}
            <button class="apr-btn-icon apr-btn-delete" title=${t('approval.card.remove')} @click=${() => this._deleteFsRule(rule)}>
              <i class="bi bi-trash"></i>
            </button>
          `}
      </div>
    `;
  }

  _renderFsAddRow() {
    const saving = this._fsSaving.has('new');
    return html`
      <div class="apr-fs-row apr-fs-add">
        <i class="bi bi-plus-circle apr-fs-row-icon"></i>
        <input
          class="form-control form-control-sm font-monospace apr-fs-path-input"
          placeholder=${t('approval.fs.add_ph')}
          .value=${this._fsNewPath}
          @input=${(e) => { this._fsNewPath = e.target.value; }}
          @keydown=${(e) => { if (e.key === 'Enter') this._addFsRule(); }}
        />
        <select
          class="form-select form-select-sm apr-fs-select"
          @change=${(e) => { this._fsNewAccess = e.target.value; }}
        >
          ${Object.entries(FS_ACCESS).map(([k]) => html`
            <option value=${k} ?selected=${this._fsNewAccess === k}>${this._fsAccessLabel(k)}</option>
          `)}
        </select>
        <button class="btn btn-sm btn-primary apr-fs-add-btn" @click=${() => this._addFsRule()} ?disabled=${saving}>
          ${saving
            ? html`<span class="spinner-border spinner-border-sm"></span>`
            : html`<i class="bi bi-plus-lg"></i>`}
        </button>
      </div>
    `;
  }

  _renderFsPanel() {
    const groupId  = this._selectedGroup.id;
    const rules    = this._fsPathRules(groupId);
    const isOpen   = this._fsOpen;
    const defRule  = this._fsDefaultRule(groupId);
    const defValue = defRule ? this._fsAccessValue(defRule) : null;
    return html`
      <div class="apr-side-panel apr-fs-panel ${isOpen ? 'apr-side-panel--open' : ''}">
        <div class="apr-side-panel-header" @click=${() => { this._fsOpen = !this._fsOpen; }}>
          <i class="bi bi-chevron-${isOpen ? 'down' : 'right'} apr-cat-chevron"></i>
          <i class="bi bi-hdd-stack apr-panel-icon"></i>
          <span class="apr-panel-title">${t('approval.fs.title')}</span>
          <span class="apr-panel-subtitle">${t('approval.fs.subtitle')}</span>
          ${rules.length > 0 ? html`<span class="apr-count-badge">${rules.length}</span>` : nothing}
        </div>
        ${isOpen ? html`
          <div class="apr-side-panel-body">
            ${rules.length === 0
              ? html`<div class="apr-panel-empty">${t('approval.fs.empty')}</div>`
              : rules.map(r => this._renderFsRow(r))}
            ${this._renderFsAddRow()}
            <div class="apr-fs-row apr-fs-default">
              <i class="bi bi-skip-end-fill apr-fs-row-icon"></i>
              <span class="apr-fs-path apr-fs-default-label">${t('approval.fs.default_label')} <span class="apr-default-hint">${t('approval.fs.default_hint')}</span></span>
              ${this._renderFsAccessSelect(defValue, (v) => this._setFsDefault(v), true)}
            </div>
          </div>
        ` : nothing}
      </div>
    `;
  }

  // ── Side panel (Override / LowPrio) ──────────────────────────────────────────

  _renderSidePanel(panelKey, title, icon, subtitle, rules, isOpen, onToggle, onAdd) {
    const formActive = this._editingId !== null && this._formMode === panelKey;
    return html`
      <div class="apr-side-panel ${isOpen ? 'apr-side-panel--open' : ''}">
        <div class="apr-side-panel-header" @click=${onToggle}>
          <i class="bi bi-chevron-${isOpen ? 'down' : 'right'} apr-cat-chevron"></i>
          <i class="bi ${icon} apr-panel-icon"></i>
          <span class="apr-panel-title">${title}</span>
          <span class="apr-panel-subtitle">${subtitle}</span>
          ${rules.length > 0 ? html`<span class="apr-count-badge">${rules.length}</span>` : nothing}
          <button
            class="btn btn-sm btn-outline-secondary apr-panel-add-btn"
            @click=${(e) => { e.stopPropagation(); onAdd(); }}
          ><i class="bi bi-plus-lg me-1"></i>${t('approval.sidebar.add')}</button>
        </div>
        ${isOpen ? html`
          <div class="apr-side-panel-body">
            ${formActive ? this._renderForm() : nothing}
            ${rules.length === 0 && !formActive
              ? html`<div class="apr-panel-empty">${t('approval.sidebar.empty')}</div>`
              : rules.map(r => this._renderCard(r))}
          </div>
        ` : nothing}
      </div>
    `;
  }

  // ── Default action bar ────────────────────────────────────────────────────────

  _renderDefaultActionBar() {
    const action = this._getDefaultAction();
    return html`
      <div class="apr-default-bar">
        <div class="apr-default-label">
          <i class="bi bi-skip-end-fill me-1"></i>
          <strong>${t('approval.default_bar.title')}</strong>
          <span class="apr-default-hint">${t('approval.default_bar.hint')}</span>
        </div>
        ${this._renderChipGroup(action, (a) => this._setDefaultAction(a))}
        ${action === null
          ? html`<span class="apr-default-unset">${t('approval.default_bar.unset')}</span>`
          : nothing}
      </div>
    `;
  }

  // ── Rules view ────────────────────────────────────────────────────────────────

  render() {
    if (!this._selectedGroup) return nothing;
    const group      = this._selectedGroup;
    const isDefault  = group.id === 'default';
    const { overrides, lowPrio } = this._buckets(group.id);
    const totalRules = this._rulesForGroup(group.id).length;

    return html`
      <div class="apr-page">
        <div class="page-header">
          <div class="page-header-left">
            <button
              class="btn btn-sm btn-outline-secondary page-header-back"
              @click=${() => this._goBack()}
            >
              <i class="bi bi-arrow-left"></i>
            </button>
            <h2 class="page-header-title">
              ${isDefault ? html`<span class="apr-group-default-badge" style="vertical-align:middle">${t('approval.header.default_badge')}</span>` : nothing}
              ${group.name}
            </h2>
          </div>
          <div class="page-header-actions">
            <span class="page-header-count">${totalRules === 1 ? t('approval.header.rule_count', { n: totalRules }) : t('approval.header.rule_count_plural', { n: totalRules })}</span>
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mt-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : nothing}

        <div class="apr-rules-body">
          ${this._renderSidePanel(
            'override',
            t('approval.sidebar.overrides'),
            'bi-exclamation-triangle-fill',
            t('approval.sidebar.overrides_sub'),
            overrides,
            this._overrideOpen,
            () => { this._overrideOpen = !this._overrideOpen; },
            () => this._startNew('override')
          )}

          ${this._renderFsPanel()}

          ${this._renderToolMatrix()}

          ${this._renderSidePanel(
            'lowprio',
            t('approval.sidebar.lowprio'),
            'bi-arrow-down-circle-fill',
            t('approval.sidebar.lowprio_sub'),
            lowPrio,
            this._lowPrioOpen,
            () => { this._lowPrioOpen = !this._lowPrioOpen; },
            () => this._startNew('lowprio')
          )}

          ${this._renderDefaultActionBar()}
        </div>
      </div>
    `;
  }
}
