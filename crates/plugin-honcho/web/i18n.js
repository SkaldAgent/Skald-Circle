// Frontend translations for the Honcho page fragments.
//
// Served at `/api/plugin/honcho/web/i18n.js` and imported by `common.js`, which
// registers it into the host's shared dictionaries via `addStrings` (see
// `web/lib/i18n.js`). Keys are namespaced `plugin.honcho.*` so they never
// collide with core keys. These are the *frontend* UI strings; the plugin's
// backend error strings live in `../i18n/*.json` and reach the browser already
// translated as HTTP response text.
const P = 'plugin.honcho';

export default {
  en: {
    // Admin config page
    [`${P}.config.title`]:         'Honcho — Long-term memory',
    [`${P}.config.intro`]:         'Connect the Honcho memory server. When enabled, each user can opt in from their own Long-term memory page; nothing leaves the box until they do.',
    [`${P}.config.enabled`]:       'Plugin enabled',
    [`${P}.config.base_url`]:      'Server URL',
    [`${P}.config.base_url_hint`]: 'e.g. http://localhost:8000',
    [`${P}.config.api_key`]:       'API key',
    [`${P}.config.api_key_hint`]:  'Leave empty for a local, unauthenticated instance.',
    [`${P}.config.workspace`]:     'Workspace ID',
    [`${P}.config.workspace_hint`]:'One shared workspace for the whole instance; each user is a separate peer inside it.',
    [`${P}.config.save`]:          'Save',
    [`${P}.config.saved`]:         'Saved.',
    [`${P}.config.test`]:          'Test connection',
    [`${P}.config.testing`]:       'Testing…',
    [`${P}.config.test_ok`]:       'Connected — {n} workspace(s) reachable.',
    [`${P}.config.required`]:      'The server URL is required.',
    [`${P}.config.loading`]:       'Loading…',
    [`${P}.config.not_found`]:     'Honcho plugin not found.',

    // User opt-in page
    [`${P}.memory.title`]:         'Long-term memory',
    [`${P}.memory.intro`]:         'Let the assistant remember you across conversations, so it gets more helpful over time.',
    [`${P}.memory.privacy_title`]: 'Before you turn this on',
    [`${P}.memory.privacy_body`]:  'Your messages are stored in cleartext on the Honcho memory server, outside your encrypted database. Turn this on only if you are comfortable with that. It is off unless you enable it, and you can turn it off at any time.',
    [`${P}.memory.toggle`]:        'Remember me across conversations',
    [`${P}.memory.save`]:          'Save',
    [`${P}.memory.saved`]:         'Saved.',
    [`${P}.memory.loading`]:       'Loading…',
    [`${P}.memory.unavailable`]:   'Long-term memory is not available to you yet. Ask your administrator to grant access.',
    [`${P}.memory.soon_title`]:    'Coming soon',
    [`${P}.memory.soon_body`]:     'Soon you will be able to ask Honcho what it remembers about you, and manage it, right from this page.',
  },

  it: {
    [`${P}.config.title`]:         'Honcho — Memoria a lungo termine',
    [`${P}.config.intro`]:         'Collega il server di memoria Honcho. Quando è attivo, ogni utente può dare il consenso dalla propria pagina Memoria a lungo termine; finché non lo fa, nulla lascia il box.',
    [`${P}.config.enabled`]:       'Plugin attivo',
    [`${P}.config.base_url`]:      'URL del server',
    [`${P}.config.base_url_hint`]: 'es. http://localhost:8000',
    [`${P}.config.api_key`]:       'Chiave API',
    [`${P}.config.api_key_hint`]:  'Lascia vuoto per un’istanza locale senza autenticazione.',
    [`${P}.config.workspace`]:     'ID workspace',
    [`${P}.config.workspace_hint`]:'Un solo workspace condiviso per l’intera istanza; ogni utente è un peer separato al suo interno.',
    [`${P}.config.save`]:          'Salva',
    [`${P}.config.saved`]:         'Salvato.',
    [`${P}.config.test`]:          'Prova connessione',
    [`${P}.config.testing`]:       'Verifica…',
    [`${P}.config.test_ok`]:       'Connesso — {n} workspace raggiungibili.',
    [`${P}.config.required`]:      'L’URL del server è obbligatorio.',
    [`${P}.config.loading`]:       'Caricamento…',
    [`${P}.config.not_found`]:     'Plugin Honcho non trovato.',

    [`${P}.memory.title`]:         'Memoria a lungo termine',
    [`${P}.memory.intro`]:         'Permetti all’assistente di ricordarti tra una conversazione e l’altra, così diventa più utile nel tempo.',
    [`${P}.memory.privacy_title`]: 'Prima di attivarla',
    [`${P}.memory.privacy_body`]:  'I tuoi messaggi vengono memorizzati in chiaro sul server di memoria Honcho, fuori dal tuo database cifrato. Attivala solo se ti sta bene. È disattivata finché non la abiliti, e puoi disattivarla in qualsiasi momento.',
    [`${P}.memory.toggle`]:        'Ricordami tra le conversazioni',
    [`${P}.memory.save`]:          'Salva',
    [`${P}.memory.saved`]:         'Salvato.',
    [`${P}.memory.loading`]:       'Caricamento…',
    [`${P}.memory.unavailable`]:   'La memoria a lungo termine non è ancora disponibile per te. Chiedi all’amministratore di darti l’accesso.',
    [`${P}.memory.soon_title`]:    'In arrivo',
    [`${P}.memory.soon_body`]:     'Presto potrai chiedere a Honcho cosa ricorda di te e gestirlo, direttamente da questa pagina.',
  },

  fr: {
    [`${P}.config.title`]:         'Honcho — Mémoire à long terme',
    [`${P}.config.intro`]:         'Connectez le serveur de mémoire Honcho. Une fois activé, chaque utilisateur peut consentir depuis sa page Mémoire à long terme ; rien ne quitte la machine tant qu’il ne l’a pas fait.',
    [`${P}.config.enabled`]:       'Plugin activé',
    [`${P}.config.base_url`]:      'URL du serveur',
    [`${P}.config.base_url_hint`]: 'ex. http://localhost:8000',
    [`${P}.config.api_key`]:       'Clé API',
    [`${P}.config.api_key_hint`]:  'Laissez vide pour une instance locale sans authentification.',
    [`${P}.config.workspace`]:     'ID de l’espace',
    [`${P}.config.workspace_hint`]:'Un seul espace partagé pour toute l’instance ; chaque utilisateur y est un peer distinct.',
    [`${P}.config.save`]:          'Enregistrer',
    [`${P}.config.saved`]:         'Enregistré.',
    [`${P}.config.test`]:          'Tester la connexion',
    [`${P}.config.testing`]:       'Test…',
    [`${P}.config.test_ok`]:       'Connecté — {n} espace(s) accessibles.',
    [`${P}.config.required`]:      'L’URL du serveur est obligatoire.',
    [`${P}.config.loading`]:       'Chargement…',
    [`${P}.config.not_found`]:     'Plugin Honcho introuvable.',

    [`${P}.memory.title`]:         'Mémoire à long terme',
    [`${P}.memory.intro`]:         'Laissez l’assistant se souvenir de vous d’une conversation à l’autre, pour qu’il devienne plus utile avec le temps.',
    [`${P}.memory.privacy_title`]: 'Avant d’activer',
    [`${P}.memory.privacy_body`]:  'Vos messages sont stockés en clair sur le serveur de mémoire Honcho, en dehors de votre base chiffrée. N’activez que si cela vous convient. C’est désactivé tant que vous ne l’activez pas, et vous pouvez le désactiver à tout moment.',
    [`${P}.memory.toggle`]:        'Se souvenir de moi entre les conversations',
    [`${P}.memory.save`]:          'Enregistrer',
    [`${P}.memory.saved`]:         'Enregistré.',
    [`${P}.memory.loading`]:       'Chargement…',
    [`${P}.memory.unavailable`]:   'La mémoire à long terme ne vous est pas encore accessible. Demandez l’accès à votre administrateur.',
    [`${P}.memory.soon_title`]:    'Bientôt disponible',
    [`${P}.memory.soon_body`]:     'Bientôt, vous pourrez demander à Honcho ce qu’il retient de vous et le gérer, directement depuis cette page.',
  },
};
