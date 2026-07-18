import { html } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }             from '../lib/i18n.js';

const CARDS = [
  {
    id:   'llm',
    icon: 'bi-cpu',
    titleKey: 'models.hub.card.llm.title',
    descKey:  'models.hub.card.llm.desc',
  },
  {
    id:   'transcribe',
    icon: 'bi-mic',
    titleKey: 'models.hub.card.transcribe.title',
    descKey:  'models.hub.card.transcribe.desc',
  },
  {
    id:   'image',
    icon: 'bi-image',
    titleKey: 'models.hub.card.image.title',
    descKey:  'models.hub.card.image.desc',
  },
  {
    id:   'tts',
    icon: 'bi-volume-up',
    titleKey: 'models.hub.card.tts.title',
    descKey:  'models.hub.card.tts.desc',
  },
];

export class ModelsHubPage extends LightElement {
  static properties = {
    _section: { state: true },
    _counts:  { state: true },
    _loading: { state: true },
  };

  constructor() {
    super();
    this._section = null;
    this._counts  = { llm: 0, transcribe: 0, image: 0, tts: 0 };
    this._loading = false;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      const open = e.detail.page === 'models';
      this.style.display = open ? 'flex' : 'none';
      if (open) {
        this._section = this._sectionFromHash();
        if (!this._section) this._loadCounts();
      }
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  _sectionFromHash() {
    const parts = location.hash.slice(1).split('/');
    if (parts[0] === 'models' && parts[1]) {
      return ['llm', 'transcribe', 'image', 'tts'].includes(parts[1]) ? parts[1] : null;
    }
    return null;
  }

  async _loadCounts() {
    this._loading = true;
    try {
      const [llmRes, tRes, igRes, ttsRes] = await Promise.all([
        fetch('/api/llm/models'),
        fetch('/api/transcribe/models'),
        fetch('/api/image-generate/models'),
        fetch('/api/tts/models'),
      ]);
      const [llm, transcribe, image, tts] = await Promise.all([
        llmRes.ok  ? llmRes.json()  : [],
        tRes.ok    ? tRes.json()    : [],
        igRes.ok   ? igRes.json()   : [],
        ttsRes.ok  ? ttsRes.json()  : [],
      ]);
      this._counts = {
        llm:        llm.length,
        transcribe: transcribe.length,
        image:      image.length,
        tts:        tts.length,
      };
    } catch {
      // counts stay at 0 on error — non-critical
    } finally {
      this._loading = false;
    }
  }

  _openSection(id) {
    this._section = id;
    history.pushState({ page: 'models', section: id }, '', `#models/${id}`);
  }

  _goBack() {
    this._section = null;
    this._loadCounts();
    history.replaceState({ page: 'models' }, '', '#models');
  }

  _countLabel(id) {
    const n = this._counts[id] ?? 0;
    return n === 0 ? t('models.hub.count.none') : n === 1 ? t('models.hub.count.one') : t('models.hub.count.many', { n });
  }

  render() {
    if (this._section) {
      return html`
        ${this._section === 'llm'        ? html`<models-llm-section .onback=${() => this._goBack()}></models-llm-section>`        : ''}
        ${this._section === 'transcribe' ? html`<models-transcribe-section .onback=${() => this._goBack()}></models-transcribe-section>` : ''}
        ${this._section === 'image'      ? html`<models-image-section .onback=${() => this._goBack()}></models-image-section>`    : ''}
        ${this._section === 'tts'        ? html`<models-tts-section .onback=${() => this._goBack()}></models-tts-section>`          : ''}
      `;
    }

    return html`
      <div class="models-hub">
        <h2 class="llm-page-title">${t('models.hub.title')}</h2>
        <p class="text-muted" style="font-size:0.88rem;margin-top:0.25rem">
          ${t('models.hub.subtitle')}
        </p>
        <div class="models-hub-grid">
          ${CARDS.map(card => html`
            <button class="models-type-card" @click=${() => this._openSection(card.id)}>
              <div class="models-type-card-icon">
                <i class="bi ${card.icon}"></i>
              </div>
              <div class="models-type-card-title">${t(card.titleKey)}</div>
              <div class="models-type-card-desc">${t(card.descKey)}</div>
              <div class="models-type-card-count ${this._counts[card.id] > 0 ? 'has-models' : ''}">
                ${this._loading ? '…' : this._countLabel(card.id)}
              </div>
            </button>
          `)}
        </div>
      </div>
    `;
  }
}
