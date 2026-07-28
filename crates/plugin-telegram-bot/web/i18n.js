// Frontend translations for the Telegram page fragment.
//
// Served at `/api/plugin/telegram/web/i18n.js` and imported by `telegram.js`,
// which registers it into the host's shared dictionaries via `addStrings`
// (see `web/lib/i18n.js`). Keys are namespaced `plugin.telegram.*` so they
// never collide with core keys.
const P = 'plugin.telegram';

export default {
  en: {
    [`${P}.title`]:            'Telegram',
    [`${P}.intro`]:            'Chat with the assistant from Telegram by linking your Telegram chat to your account.',
    [`${P}.status.linked`]:    'Your Telegram chat is linked.',
    [`${P}.status.unlinked`]:  'Your Telegram chat is not linked yet.',
    [`${P}.status.chat_id`]:   'Chat ID',
    [`${P}.howto_title`]:      'How to link it',
    [`${P}.howto_body`]:       'Send any message to the bot — it replies with a 6-character code. Paste the code here.',
    [`${P}.code_label`]:       'Pairing code',
    [`${P}.save`]:             'Link',
    [`${P}.saved`]:            'Linked!',
    [`${P}.relink_hint`]:      'Pasting a new code replaces the current link.',
    [`${P}.loading`]:          'Loading…',
    [`${P}.unavailable`]:      'Telegram is not available to you yet. Ask your administrator to grant access.',
  },

  it: {
    [`${P}.title`]:            'Telegram',
    [`${P}.intro`]:            'Chatta con l’assistente da Telegram collegando la tua chat Telegram al tuo account.',
    [`${P}.status.linked`]:    'La tua chat Telegram è collegata.',
    [`${P}.status.unlinked`]:  'La tua chat Telegram non è ancora collegata.',
    [`${P}.status.chat_id`]:   'ID chat',
    [`${P}.howto_title`]:      'Come collegarla',
    [`${P}.howto_body`]:       'Invia un messaggio qualsiasi al bot — ti risponde con un codice di 6 caratteri. Incolla il codice qui.',
    [`${P}.code_label`]:       'Codice di pairing',
    [`${P}.save`]:             'Collega',
    [`${P}.saved`]:            'Collegata!',
    [`${P}.relink_hint`]:      'Incollare un nuovo codice sostituisce il collegamento attuale.',
    [`${P}.loading`]:          'Caricamento…',
    [`${P}.unavailable`]:      'Telegram non è ancora disponibile per te. Chiedi all’amministratore di darti l’accesso.',
  },

  fr: {
    [`${P}.title`]:            'Telegram',
    [`${P}.intro`]:            'Discutez avec l’assistant depuis Telegram en reliant votre conversation Telegram à votre compte.',
    [`${P}.status.linked`]:    'Votre conversation Telegram est reliée.',
    [`${P}.status.unlinked`]:  'Votre conversation Telegram n’est pas encore reliée.',
    [`${P}.status.chat_id`]:   'ID de conversation',
    [`${P}.howto_title`]:      'Comment la relier',
    [`${P}.howto_body`]:       'Envoyez n’importe quel message au bot — il répond avec un code à 6 caractères. Collez le code ici.',
    [`${P}.code_label`]:       'Code d’appairage',
    [`${P}.save`]:             'Relier',
    [`${P}.saved`]:            'Reliée !',
    [`${P}.relink_hint`]:      'Coller un nouveau code remplace le lien actuel.',
    [`${P}.loading`]:          'Chargement…',
    [`${P}.unavailable`]:      'Telegram n’est pas encore disponible pour vous. Demandez l’accès à votre administrateur.',
  },
};
