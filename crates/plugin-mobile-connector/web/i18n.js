// Frontend translations for the mobile-connector page fragments.
//
// Served at `/api/plugin/mobile-connector/web/i18n.js` and imported by
// `common.js`, which registers it into the host's shared dictionaries via
// `addStrings` (see `web/lib/i18n.js`). Keys are namespaced `plugin.mobile-
// connector.*` so they never collide with core keys. These are the *frontend*
// UI strings; the plugin's backend error strings live in `../i18n/*.json` and
// reach the browser already translated as HTTP response text.
const P = 'plugin.mobile-connector';

export default {
  en: {
    [`${P}.pairing.title`]:       'Pair a device',
    [`${P}.pairing.intro`]:       'Open a pairing window, then scan the QR code with the Skald mobile app. The device is linked to you and works immediately — you can reassign it to another user from the Mobile devices page.',
    [`${P}.pairing.open`]:        'Open pairing window',
    [`${P}.pairing.opening`]:     'Opening…',
    [`${P}.pairing.qr_alt`]:      'Pairing QR',
    [`${P}.pairing.expired`]:     'Window expired',
    [`${P}.pairing.scan_within`]: 'Scan within {n}s',
    [`${P}.pairing.new_code`]:    'New code',
    [`${P}.pairing.close`]:       'Close',

    [`${P}.devices.title`]:            'Mobile devices',
    [`${P}.devices.refresh`]:          'Refresh',
    [`${P}.devices.loading`]:          'Loading…',
    [`${P}.devices.empty`]:            'No paired devices yet.',
    [`${P}.devices.empty_hint`]:       'Use the Pair a device page to add one.',
    [`${P}.devices.col_device`]:       'Device',
    [`${P}.devices.col_state`]:        'State',
    [`${P}.devices.col_bound`]:        'Bound to',
    [`${P}.devices.col_last_seen`]:    'Last seen',
    [`${P}.devices.col_actions`]:      'Actions',
    [`${P}.devices.state_authorized`]: 'authorized',
    [`${P}.devices.state_pending`]:    'pending',
    [`${P}.devices.assign_to`]:        'Assign to…',
    [`${P}.devices.bind`]:             'Bind',
    [`${P}.devices.revoke_confirm`]:   'Revoke this device? It loses access immediately.',
    [`${P}.devices.unknown`]:          'Unknown device',

    [`${P}.time.never`]: '—',
    [`${P}.time.ago_s`]: '{n}s ago',
    [`${P}.time.ago_m`]: '{n}m ago',
    [`${P}.time.ago_h`]: '{n}h ago',
    [`${P}.time.ago_d`]: '{n}d ago',
  },

  it: {
    [`${P}.pairing.title`]:       'Associa un dispositivo',
    [`${P}.pairing.intro`]:       'Apri una finestra di associazione, poi scansiona il codice QR con l’app Skald sul telefono. Il dispositivo viene collegato a te e funziona subito — puoi riassegnarlo a un altro utente dalla pagina Dispositivi mobili.',
    [`${P}.pairing.open`]:        'Apri finestra di associazione',
    [`${P}.pairing.opening`]:     'Apertura…',
    [`${P}.pairing.qr_alt`]:      'QR di associazione',
    [`${P}.pairing.expired`]:     'Finestra scaduta',
    [`${P}.pairing.scan_within`]: 'Scansiona entro {n}s',
    [`${P}.pairing.new_code`]:    'Nuovo codice',
    [`${P}.pairing.close`]:       'Chiudi',

    [`${P}.devices.title`]:            'Dispositivi mobili',
    [`${P}.devices.refresh`]:          'Aggiorna',
    [`${P}.devices.loading`]:          'Caricamento…',
    [`${P}.devices.empty`]:            'Nessun dispositivo associato.',
    [`${P}.devices.empty_hint`]:       'Usa la pagina Associa un dispositivo per aggiungerne uno.',
    [`${P}.devices.col_device`]:       'Dispositivo',
    [`${P}.devices.col_state`]:        'Stato',
    [`${P}.devices.col_bound`]:        'Assegnato a',
    [`${P}.devices.col_last_seen`]:    'Ultimo accesso',
    [`${P}.devices.col_actions`]:      'Azioni',
    [`${P}.devices.state_authorized`]: 'autorizzato',
    [`${P}.devices.state_pending`]:    'in attesa',
    [`${P}.devices.assign_to`]:        'Assegna a…',
    [`${P}.devices.bind`]:             'Associa',
    [`${P}.devices.revoke_confirm`]:   'Revocare questo dispositivo? Perderà l’accesso immediatamente.',
    [`${P}.devices.unknown`]:          'Dispositivo sconosciuto',

    [`${P}.time.never`]: '—',
    [`${P}.time.ago_s`]: '{n}s fa',
    [`${P}.time.ago_m`]: '{n}m fa',
    [`${P}.time.ago_h`]: '{n}h fa',
    [`${P}.time.ago_d`]: '{n}g fa',
  },

  fr: {
    [`${P}.pairing.title`]:       'Associer un appareil',
    [`${P}.pairing.intro`]:       'Ouvrez une fenêtre d’association, puis scannez le QR code avec l’app mobile Skald. L’appareil est lié à vous et fonctionne immédiatement — vous pouvez le réassigner à un autre utilisateur depuis la page Appareils mobiles.',
    [`${P}.pairing.open`]:        'Ouvrir la fenêtre d’association',
    [`${P}.pairing.opening`]:     'Ouverture…',
    [`${P}.pairing.qr_alt`]:      'QR d’association',
    [`${P}.pairing.expired`]:     'Fenêtre expirée',
    [`${P}.pairing.scan_within`]: 'Scannez sous {n}s',
    [`${P}.pairing.new_code`]:    'Nouveau code',
    [`${P}.pairing.close`]:       'Fermer',

    [`${P}.devices.title`]:            'Appareils mobiles',
    [`${P}.devices.refresh`]:          'Actualiser',
    [`${P}.devices.loading`]:          'Chargement…',
    [`${P}.devices.empty`]:            'Aucun appareil associé.',
    [`${P}.devices.empty_hint`]:       'Utilisez la page Associer un appareil pour en ajouter un.',
    [`${P}.devices.col_device`]:       'Appareil',
    [`${P}.devices.col_state`]:        'État',
    [`${P}.devices.col_bound`]:        'Assigné à',
    [`${P}.devices.col_last_seen`]:    'Vu la dernière fois',
    [`${P}.devices.col_actions`]:      'Actions',
    [`${P}.devices.state_authorized`]: 'autorisé',
    [`${P}.devices.state_pending`]:    'en attente',
    [`${P}.devices.assign_to`]:        'Assigner à…',
    [`${P}.devices.bind`]:             'Associer',
    [`${P}.devices.revoke_confirm`]:   'Révoquer cet appareil ? Il perd l’accès immédiatement.',
    [`${P}.devices.unknown`]:          'Appareil inconnu',

    [`${P}.time.never`]: '—',
    [`${P}.time.ago_s`]: 'il y a {n}s',
    [`${P}.time.ago_m`]: 'il y a {n}m',
    [`${P}.time.ago_h`]: 'il y a {n}h',
    [`${P}.time.ago_d`]: 'il y a {n}j',
  },
};
