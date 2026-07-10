#!/usr/bin/env node
'use strict';

/**
 * WhatsApp MCP Server (JSON-RPC 2.0 over stdio)
 *
 * Uses whatsapp-web.js + puppeteer to provide read/write access to WhatsApp.
 * Session is persisted in ./secrets/whatsapp_session/ (LocalAuth).
 * QR code for first-time auth is saved as ASCII art to ./secrets/whatsapp_qr.txt
 *
 * Run `npm install` inside scripts/whatsapp_mcp/ before first use.
 *
 * Register with the agent:
 *   register_mcp(name="whatsapp", transport="stdio",
 *                command="node", args=["scripts/whatsapp_mcp/index.js"])
 */

const fs = require('fs');
const path = require('path');
const readline = require('readline');

// ── Paths ──────────────────────────────────────────────────────────────────

// __dirname = <project>/scripts/whatsapp_mcp  →  root = ../..
const PROJECT_ROOT = path.resolve(__dirname, '..', '..');
const SECRETS_DIR  = path.join(PROJECT_ROOT, 'secrets');
const DATA_DIR     = path.join(PROJECT_ROOT, 'data');
const SESSION_DIR  = path.join(SECRETS_DIR, 'whatsapp_session');
const QR_FILE      = path.join(SECRETS_DIR, 'whatsapp_qr.txt');
const MEDIA_DIR    = path.join(DATA_DIR, 'whatsapp_media');

// ── Logging ────────────────────────────────────────────────────────────────

function log(msg) {
  process.stderr.write(`[whatsapp_mcp] ${msg}\n`);
}

// ── QR artifact cleanup ──────────────────────────────────────────────────────
// Remove every QR file we may have written (PNG, HTML, ASCII fallback) so a
// stale code is never served after auth succeeds or the session is reset.

function cleanupQrFiles() {
  const QR_HTML = path.join(SECRETS_DIR, 'whatsapp_qr.html');
  const QR_PNG  = path.join(DATA_DIR, 'whatsapp_qr.png');
  for (const f of [QR_FILE, QR_HTML, QR_PNG]) {
    try { if (fs.existsSync(f)) fs.unlinkSync(f); } catch (_) {}
  }
}

// ── JSON-RPC notification helper ───────────────────────────────────────────
// Emits a server-initiated notification (no "id") to stdout.
// The Rust McpServer reader loop captures these and writes them to mcp_events.

function notify(method, params) {
  const msg = JSON.stringify({ jsonrpc: '2.0', method, params });
  process.stdout.write(msg + '\n');
}

// ── State ──────────────────────────────────────────────────────────────────

/**
 * Connection states:
 *   INITIALIZING  – client is starting up (loading session or launching browser)
 *   QR_READY      – need QR scan; QR saved to secrets/whatsapp_qr.html
 *   AUTHENTICATED – QR scanned, session being established
 *   READY         – fully connected, tools operational
 *   DISCONNECTED  – lost connection (will attempt reconnect)
 */
let state      = 'INITIALIZING';
let client     = null;
let lastQrStr  = null;   // dedup: only regenerate files when QR string changes

// ── Stale-browser cleanup (self-healing) ─────────────────────────────────────
// The host (skald) kills this MCP process with SIGKILL on shutdown/restart
// (`kill_on_drop` in the Rust MCP client — see crates/mcp-client/src/server.rs).
// SIGKILL is untrappable, so neither our shutdown handler nor puppeteer's own
// cleanup ever runs: the headless Chrome that whatsapp-web.js spawned is
// reparented to init and keeps an exclusive lock (SingletonLock) on the profile
// directory. The next launch then dies with "The browser is already running for
// <profile>. Use a different userDataDir or stop the running browser first." and
// every WhatsApp tool is dead until the orphan is killed by hand.
//
// skald only ever runs one WhatsApp MCP at a time and SIGKILLs the old node
// before spawning the new one, so ANY Chrome still bound to our profile at
// startup is guaranteed to be such an orphan. On startup we therefore kill every
// process whose command line references our profile dir and clear the stale
// Singleton* lock files — making the server self-healing across hard restarts.

// LocalAuth stores the Chromium profile at <dataPath>/session[-<clientId>].
const SESSION_PROFILE = path.join(SESSION_DIR, 'session');

function cleanupStaleBrowser() {
  // 1. Kill any orphaned browser process bound to our profile directory.
  try {
    const { spawnSync } = require('child_process');
    const res  = spawnSync('pgrep', ['-f', SESSION_PROFILE], { encoding: 'utf8' });
    const pids = (res.stdout || '')
      .split('\n')
      .map((s) => parseInt(s.trim(), 10))
      .filter((pid) => Number.isInteger(pid) && pid !== process.pid);
    for (const pid of pids) {
      try { process.kill(pid, 'SIGKILL'); } catch (_) {}
    }
    if (pids.length) {
      log(`Cleaned up ${pids.length} orphaned browser process(es) holding ${SESSION_PROFILE}`);
    }
  } catch (e) {
    // pgrep missing/unusable — fall through to lock removal, which alone fixes
    // the common case (Chrome treats a lock owned by a dead pid as stale).
    log(`Stale-browser scan skipped: ${e.message}`);
  }

  // 2. Remove leftover single-instance lock files so a fresh launch is unblocked
  //    even if a just-killed process had not finished releasing them.
  for (const f of ['SingletonLock', 'SingletonCookie', 'SingletonSocket']) {
    try { fs.rmSync(path.join(SESSION_PROFILE, f), { force: true }); } catch (_) {}
  }
}

// ── WhatsApp Client ────────────────────────────────────────────────────────

function initClient() {
  // Release any profile lock held by an orphaned browser from a prior (hard-
  // killed) run before puppeteer tries to open the profile again.
  cleanupStaleBrowser();

  let Client, LocalAuth;
  try {
    ({ Client, LocalAuth } = require('whatsapp-web.js'));
  } catch (e) {
    log(`ERROR: whatsapp-web.js not found. Run: cd scripts/whatsapp_mcp && npm install`);
    state = 'DISCONNECTED';
    return;
  }

  client = new Client({
    authStrategy: new LocalAuth({ dataPath: SESSION_DIR }),
    puppeteer: {
      headless: true,
      args: ['--no-sandbox', '--disable-setuid-sandbox', '--disable-dev-shm-usage'],
    },
  });

  client.on('qr', async (qr) => {
    state = 'QR_READY';

    // Dedup: skip file writes if the QR string hasn't changed.
    if (qr === lastQrStr) return;
    lastQrStr = qr;

    const QR_HTML = path.join(SECRETS_DIR, 'whatsapp_qr.html');

    // Generate a scannable HTML page with embedded PNG data-URL.
    try {
      const qrcode = require('qrcode');
      const dataUrl = await qrcode.toDataURL(qr, { width: 300, margin: 2 });
      const html = `<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>WhatsApp QR</title>
<style>body{display:flex;flex-direction:column;align-items:center;justify-content:center;min-height:100vh;margin:0;background:#111;color:#eee;font-family:sans-serif;}
img{border:12px solid white;border-radius:8px;}</style></head>
<body>
  <h2>Scan with WhatsApp → Linked Devices → Link a Device</h2>
  <img src="${dataUrl}" alt="WhatsApp QR Code" />
  <p style="margin-top:1rem;opacity:.6">This QR expires in ~20 s — reload the page if it has changed</p>
</body>
</html>`;
      fs.writeFileSync(QR_HTML, html, 'utf8');
      log(`QR code saved → open in browser: ${QR_HTML}`);

      // Also save a standalone PNG for direct access via HTTP / local file.
      const pngPath = path.join(DATA_DIR, 'whatsapp_qr.png');
      await qrcode.toFile(pngPath, qr, { width: 300, margin: 2 });
      log(`QR PNG saved → ${pngPath}`);
    } catch (e) {
      // Fallback: ASCII art text file
      try {
        const qrTerm = require('qrcode-terminal');
        let qrText = '';
        qrTerm.generate(qr, { small: true }, (str) => { qrText = str; });
        fs.writeFileSync(QR_FILE, qrText, 'utf8');
        log(`QR code (ASCII) saved to ${QR_FILE} — scan it with WhatsApp`);
      } catch (_) {
        fs.writeFileSync(QR_FILE, `RAW_QR_STRING:\n${qr}`, 'utf8');
        log(`QR raw string saved to ${QR_FILE}`);
      }
    }
  });

  client.on('authenticated', () => {
    state = 'AUTHENTICATED';
    lastQrStr = null;
    cleanupQrFiles();
    log('Authenticated successfully');
  });

  client.on('ready', () => {
    state = 'READY';
    log('WhatsApp client ready');
  });

  // ── Push notifications: new incoming messages ──────────────────────────
  client.on('message', async (msg) => {
    // Ignore messages sent by us.
    if (msg.fromMe) return;

    // Try to resolve the human-readable chat name.
    let chatName = msg.from;
    try {
      const chat = await msg.getChat();
      chatName = chat.name || msg.from;
    } catch (_) {}

    notify('event/whatsapp_message', {
      chat_id:   msg.from,
      chat_name: chatName,
      from:      msg.author || msg.from,
      body:      (msg.body || '').slice(0, 1000),
      timestamp: msg.timestamp,
      is_group:  msg.from.endsWith('@g.us'),
    });
    log(`Notification emitted: new message from ${chatName}`);
  });

  client.on('auth_failure', (msg) => {
    state = 'DISCONNECTED';
    log(`Auth failure: ${msg}`);
  });

  client.on('disconnected', (reason) => {
    state = 'DISCONNECTED';
    log(`Disconnected: ${reason}`);
  });

  client.initialize().catch((e) => {
    log(`Failed to initialize client: ${e.message}`);
    state = 'DISCONNECTED';
  });
}

// ── Helpers ────────────────────────────────────────────────────────────────

function requireReady() {
  if (state !== 'READY') {
    return `WhatsApp not ready (status: ${state}).${
      state === 'QR_READY'
        ? ' Use get_qr to retrieve the QR code and scan it with your phone.'
        : state === 'INITIALIZING'
        ? ' Please wait a moment and try again.'
        : ''
    }`;
  }
  return null;
}

function formatTimestamp(unixSec) {
  return new Date(unixSec * 1000).toISOString().replace('T', ' ').slice(0, 19);
}

// Resolve the target chat from either an explicit chat_id or a raw phone
// number. Accepting a number removes the list_chats/search_contacts round-trip
// the agent otherwise needs just to message someone. Returns { id } on success
// or { error } with a ready-to-return message. Numbers only address individual
// contacts — groups must be passed as a chat_id.
async function resolveChatId(args) {
  if (args.chat_id) return { id: String(args.chat_id) };
  if (args.number) {
    const digits = String(args.number).replace(/\D/g, '');
    if (!digits) return { error: 'Error: `number` has no digits.' };
    const wid = await client.getNumberId(digits);
    if (!wid) return { error: `Error: ${args.number} is not a WhatsApp number.` };
    return { id: wid._serialized };
  }
  return { error: 'Error: provide either chat_id or number.' };
}

// Map a MIME type to a file extension for saved media (fallback: 'bin').
function extFromMime(mime) {
  if (!mime) return 'bin';
  const map = {
    'image/jpeg': 'jpg', 'image/png': 'png', 'image/gif': 'gif', 'image/webp': 'webp',
    'video/mp4': 'mp4', 'audio/ogg': 'ogg', 'audio/mpeg': 'mp3', 'audio/mp4': 'm4a',
    'application/pdf': 'pdf',
  };
  return map[mime] || (mime.split('/')[1] || 'bin').split(';')[0];
}

// ── Tool implementations ───────────────────────────────────────────────────

// Build a clear, LLM-actionable status report: the state, a plain-language
// description of what it means, and concrete next steps whenever it is not
// fully operational. Our lifecycle `state` is event-driven and can lag behind a
// silent socket drop, so when we believe we are connected we confirm against
// the live socket state (`client.getState()` → WAState) and surface any mismatch.
async function toolStatus() {
  let liveState = null;   // WAState string, or null
  let liveErr   = false;  // getState() threw (page/browser gone)
  if (client && state === 'READY') {
    try {
      liveState = await client.getState();
    } catch (_) {
      liveErr = true;
    }
  }

  const report = (icon, label, kind, description, steps) => {
    const lines = [`Status: ${label} ${icon} (${kind})`, description];
    if (steps && steps.length) {
      lines.push('', 'What to do:');
      steps.forEach((s, i) => lines.push(`${i + 1}. ${s}`));
    }
    return lines.join('\n');
  };

  // ── Fully operational ──
  if (state === 'READY' && (liveState === 'CONNECTED' || (liveState === null && !liveErr))) {
    return report('✅', 'READY', 'ok',
      'WhatsApp is connected and all tools (read, search, send, media) are operational.');
  }

  // ── We think we are READY, but the live socket disagrees: silent drop ──
  if (state === 'READY') {
    if (liveState === 'UNPAIRED' || liveState === 'UNPAIRED_IDLE') {
      return report('❌', `READY→${liveState}`, 'action needed',
        'This device was unlinked from the phone — the session is no longer valid.',
        ['Call logout to clear the dead session.',
         'Then call get_qr and have the user scan the new QR code.',
         'Poll status until it returns READY.']);
    }
    if (liveState === 'CONFLICT') {
      return report('⚠️', 'READY→CONFLICT', 'action needed',
        'The session was taken over by WhatsApp Web open elsewhere (another browser/device).',
        ['Ask the user to close WhatsApp Web in the other browser, OR',
         'Call logout and re-scan the QR to reclaim the session here.']);
    }
    if (liveState === 'TIMEOUT') {
      return report('⚠️', 'READY→TIMEOUT', 'transient',
        'The connection timed out; the client may reconnect on its own.',
        ['Wait ~15 s and call status again.',
         'If it stays TIMEOUT, call logout and re-scan the QR.']);
    }
    if (liveState === 'DEPRECATED_VERSION') {
      return report('❌', 'READY→DEPRECATED_VERSION', 'needs maintenance',
        'WhatsApp rejected the WhatsApp Web version used by whatsapp-web.js.',
        ['This requires a dependency update (whatsapp-web.js) by the developer — the agent cannot fix it. Inform the user.']);
    }
    // getState() failed, or returned some other non-CONNECTED value.
    return report('⚠️', `READY→${liveErr ? 'UNREACHABLE' : (liveState || 'UNKNOWN')}`, 'uncertain',
      'We believe we are connected but the live socket did not confirm it (the headless browser may have hiccuped or crashed).',
      ['Wait a few seconds and call status again.',
       'If it does not recover, call logout and re-scan the QR.']);
  }

  // ── Other lifecycle states ──
  switch (state) {
    case 'INITIALIZING':
      return report('⏳', 'INITIALIZING', 'wait',
        'The client is starting up (launching the browser and/or restoring the saved session).',
        ['Wait ~15-30 s, then call status again.']);
    case 'AUTHENTICATED':
      return report('⏳', 'AUTHENTICATED', 'wait',
        'The QR was scanned and the session is being established — almost there.',
        ['Wait a few seconds, then call status again (it should turn READY).']);
    case 'QR_READY':
      return report('⚠️', 'QR_READY', 'action needed',
        'Not logged in yet — WhatsApp needs the user to link this device with a QR code.',
        ['Call get_qr to obtain the QR code.',
         'Ask the user to scan it: WhatsApp → Settings → Linked Devices → Link a Device.',
         'Poll status until it returns READY.']);
    case 'DISCONNECTED':
    default:
      return report('❌', state, 'action needed',
        'The WhatsApp session is disconnected (lost connection, auth failure, or expired session).',
        ['Call logout to clear any stale session.',
         'Then call get_qr and have the user scan the new QR code.',
         'Poll status until it returns READY.']);
  }
}

async function toolLogout() {
  const prev = client;

  // Reset shared state up front so a concurrent call (or the old client's
  // late events) can't act on a half-torn-down instance.
  client    = null;
  lastQrStr = null;
  state     = 'INITIALIZING';

  if (prev) {
    // Detach listeners so the old client's late 'disconnected'/'qr' events
    // don't clobber the freshly re-initialized client's state.
    try { prev.removeAllListeners(); } catch (_) {}

    // Try a clean logout (tells WhatsApp to unlink this device). It runs JS in
    // the browser page, so when the session has already expired the page is
    // dead and this throws — exactly the case we must tolerate. Fall back to
    // destroy() to at least close the browser and release the profile locks.
    try {
      await prev.logout();
    } catch (e) {
      log(`logout(): clean logout failed (session likely expired): ${e.message}`);
      try { await prev.destroy(); } catch (e2) {
        log(`logout(): destroy after failed logout also failed: ${e2.message}`);
      }
    }
  }

  // Force-remove the on-disk session cache — the stale token that previously
  // had to be deleted by hand before a new login could succeed.
  try {
    await fs.promises.rm(SESSION_DIR, {
      recursive: true, force: true, maxRetries: 5, retryDelay: 200,
    });
  } catch (e) {
    log(`logout(): failed to clear session cache: ${e.message}`);
    return `Error: logged out but could not clear the session cache at ${SESSION_DIR}: ${e.message}. ` +
           `Delete that directory manually, then retry.`;
  }

  // Drop any stale QR artifacts so get_qr won't return an old code.
  cleanupQrFiles();

  // Re-initialize from scratch — a fresh QR is generated within a few seconds.
  initClient();

  return 'Logged out: session cache cleared and client re-initialized (no restart needed). ' +
         'Wait a few seconds, then call get_qr to scan a new QR code and log in again.';
}

async function toolGetQr() {
  if (state === 'READY' || state === 'AUTHENTICATED') {
    return 'Already authenticated. No QR code needed.';
  }
  const QR_PNG = path.join(DATA_DIR, 'whatsapp_qr.png');
  if (fs.existsSync(QR_PNG)) {
    return `QR code ready.
• Local file: ${QR_PNG}
• URL: /data/whatsapp_qr.png
Open the URL in your browser or scan locally.

Current status: ${state}`;
  }
  const QR_HTML = path.join(SECRETS_DIR, 'whatsapp_qr.html');
  if (fs.existsSync(QR_HTML)) {
    return `Open this file in your browser to scan the QR code:\n  ${QR_HTML}\n\nThe page shows a scannable QR image. Go to WhatsApp → Settings → Linked Devices → Link a Device.\n\nCurrent status: ${state}`;
  }
  if (fs.existsSync(QR_FILE)) {
    const qr = fs.readFileSync(QR_FILE, 'utf8');
    return `Scan this QR code with WhatsApp (Settings → Linked Devices → Link a Device):\n\n${qr}`;
  }
  return `No QR code available yet. Current status: ${state}. The client may still be initializing — try again in a few seconds.`;
}

async function toolListChats(args) {
  const err = requireReady(); if (err) return err;
  const max = Math.min(args.max_chats || 20, 50);
  const chats = await client.getChats();
  const slice = chats.slice(0, max);
  const lines = [`Chats (${slice.length} of ${chats.length} total):`];
  for (const chat of slice) {
    const unread = chat.unreadCount > 0 ? ` [${chat.unreadCount} unread]` : '';
    const type   = chat.isGroup ? '[group]' : '[contact]';
    lines.push(`- ${chat.name} ${type}${unread}`);
    lines.push(`  ID: ${chat.id._serialized}`);
  }
  return lines.join('\n');
}

async function toolGetMessages(args) {
  const err = requireReady(); if (err) return err;
  const resolved = await resolveChatId(args);
  if (resolved.error) return resolved.error;
  const limit  = Math.min(args.limit  || 20,  100);
  const offset = Math.max(args.offset ||  0,    0);
  const chat   = await client.getChatById(resolved.id);

  // fetchMessages always returns the most recent N messages (oldest→newest).
  // To support paging, we fetch limit+offset and discard the newest `offset`
  // messages, exposing the preceding window.
  //   offset=0,  limit=20 → last 20 messages
  //   offset=20, limit=20 → messages 21–40 from the end (older batch)
  const toFetch  = Math.min(limit + offset, 200);
  const fetched  = await chat.fetchMessages({ limit: toFetch });
  const windowed = offset > 0 ? fetched.slice(0, fetched.length - offset) : fetched;
  const page     = windowed.slice(-limit);

  if (!page.length) return offset > 0
    ? `No messages found at offset ${offset} (only ${fetched.length} messages available).`
    : 'No messages found.';

  const rangeNote = offset > 0 ? `, skipping newest ${offset}` : '';
  const lines = [`Messages from "${chat.name}" (${page.length} shown, limit=${limit}${rangeNote}):`];
  for (const msg of page) {
    const ts     = formatTimestamp(msg.timestamp);
    const author = msg.fromMe ? 'Me' : (msg.author || msg.from || '?');
    // For media, surface the type and the message id so the agent can fetch it
    // via download_media; text-only lines stay uncluttered.
    const tag    = msg.hasMedia ? ` [${msg.type}, download id="${msg.id._serialized}"]` : '';
    const body   = (msg.body || (msg.hasMedia ? '(no caption)' : '(no text)')).slice(0, 400);
    lines.push(`[${ts}] ${author}${tag}: ${body}`);
  }
  return lines.join('\n');
}

async function toolSendMessage(args) {
  const err = requireReady(); if (err) return err;
  const { message } = args;
  if (!message) return 'Error: Missing required parameter message.';
  const resolved = await resolveChatId(args);
  if (resolved.error) return resolved.error;
  const chat = await client.getChatById(resolved.id);
  await chat.sendMessage(message);
  return `✅ Message sent to "${chat.name}"`;
}

async function toolSendMedia(args) {
  const err = requireReady(); if (err) return err;
  const { source } = args;
  if (!source) return 'Error: Missing required parameter source (a local file path or an http(s) URL).';
  const resolved = await resolveChatId(args);
  if (resolved.error) return resolved.error;

  const { MessageMedia } = require('whatsapp-web.js');
  let media;
  try {
    if (/^https?:\/\//i.test(source)) {
      media = await MessageMedia.fromUrl(source, { unsafeMime: true });
    } else {
      // Resolve relative paths against the project root so the agent can pass
      // e.g. "data/foo.png" without knowing the server's CWD.
      const abs = path.isAbsolute(source) ? source : path.join(PROJECT_ROOT, source);
      if (!fs.existsSync(abs)) return `Error: file not found: ${abs}`;
      media = MessageMedia.fromFilePath(abs);
    }
  } catch (e) {
    return `Error: could not load media from "${source}": ${e.message}`;
  }

  const chat = await client.getChatById(resolved.id);
  await chat.sendMessage(media, {
    caption: args.caption || undefined,
    sendMediaAsDocument: !!args.as_document,
  });
  return `✅ Media sent to "${chat.name}"${args.caption ? ` with caption: ${args.caption}` : ''}`;
}

async function toolDownloadMedia(args) {
  const err = requireReady(); if (err) return err;
  const { message_id } = args;
  if (!message_id) return 'Error: Missing required parameter message_id (from the "download id" field in get_messages).';

  const msg = await client.getMessageById(message_id);
  if (!msg) return `Error: no message found with id "${message_id}".`;
  if (!msg.hasMedia) return 'Error: that message has no downloadable media.';

  const media = await msg.downloadMedia();
  if (!media || !media.data) return 'Error: media could not be downloaded (it may have expired or been deleted).';

  if (!fs.existsSync(MEDIA_DIR)) fs.mkdirSync(MEDIA_DIR, { recursive: true });
  const ext      = extFromMime(media.mimetype);
  const safeBase = (media.filename || `${Date.now()}_${msg.id.id}`).replace(/[^\w.\-]/g, '_');
  const fileName = /\.[a-z0-9]+$/i.test(safeBase) ? safeBase : `${safeBase}.${ext}`;
  const absPath  = path.join(MEDIA_DIR, fileName);
  fs.writeFileSync(absPath, Buffer.from(media.data, 'base64'));

  return `✅ Media downloaded (${media.mimetype}).\n• Local file: ${absPath}\n• URL: /data/whatsapp_media/${fileName}`;
}

async function toolSearchMessages(args) {
  const err = requireReady(); if (err) return err;
  const { query } = args;
  if (!query) return 'Error: Missing required parameter query.';
  const max = Math.min(args.max_results || 20, 50);
  const messages = await client.searchMessages(query, { limit: max });
  if (!messages.length) return `No messages found for query: "${query}"`;
  const lines = [`Search results for "${query}" (${messages.length} found):`];
  for (const msg of messages) {
    const ts   = formatTimestamp(msg.timestamp);
    const from = msg.fromMe ? 'Me' : msg.from;
    const body = (msg.body || '(media)').slice(0, 200);
    lines.push(`[${ts}] ${from} (chat: ${msg.id.remote}): ${body}`);
  }
  return lines.join('\n');
}

async function toolSearchContacts(args) {
  const err = requireReady(); if (err) return err;
  const { query } = args;
  if (!query) return 'Error: Missing required parameter query. Provide a name or partial name to search for.';
  const max = Math.min(args.max_results || 20, 50);
  const contacts = await client.getContacts();
  const q = query.toLowerCase();

  // Deduplicate by serialized ID, then filter by name match, exclude self.
  const seen = new Set();
  const matches = [];
  for (const c of contacts) {
    if (!c.name || c.isMe) continue;
    const id = c.id._serialized;
    if (seen.has(id)) continue;
    seen.add(id);
    if (c.name.toLowerCase().includes(q)) matches.push(c);
    if (matches.length >= max) break;
  }

  if (!matches.length) return `No contacts found matching "${query}".`;
  const lines = [`Contacts matching "${query}" (${matches.length} shown):`];
  for (const c of matches) {
    const type = c.isGroup ? '[group]' : c.isBusiness ? '[business]' : '[contact]';
    lines.push(`- ${c.name} ${type} | ID: ${c.id._serialized}`);
  }
  return lines.join('\n');
}

// ── MCP Tool definitions ───────────────────────────────────────────────────

const TOOLS = [
  {
    name: 'status',
    description: 'Get the current WhatsApp connection status as a plain-language report: the state, what it means, and — when not operational — step-by-step instructions to fix it. Cross-checks the live socket so a silently dropped session is detected even when bookkeeping still says READY. Call this first whenever another WhatsApp tool fails.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'get_qr',
    description: 'Get the WhatsApp authentication QR code. Only relevant when status is QR_READY. Returns a local file path / URL (PNG or HTML page) to open and scan, with ASCII art as a fallback. The user scans it via WhatsApp → Linked Devices → Link a Device.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'logout',
    description: 'Log out of WhatsApp: ends the current session, clears the cached credentials on disk, and re-initializes the client so a fresh QR code is generated immediately — no server restart needed. Use this when the session has expired/become stuck (status DISCONNECTED) or to link a different phone. After calling, wait a few seconds and use get_qr to scan the new code.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'list_chats',
    description: 'List recent WhatsApp chats (individual contacts and groups) with name, ID, and unread count. Use the returned ID in other tools.',
    inputSchema: {
      type: 'object',
      properties: {
        max_chats: {
          type: 'integer',
          description: 'Max chats to return (default 20, max 50).',
        },
      },
    },
  },
  {
    name: 'get_messages',
    description: 'Get messages from a WhatsApp chat or group. Identify the chat with EITHER chat_id ' +
      '(from list_chats) OR a plain phone number for an individual contact. ' +
      'Media messages are tagged with their type and a "download id" — pass that to download_media. ' +
      'Supports pagination: use offset to skip the most recent messages and read older history. ' +
      'Example: limit=20 offset=0 → last 20; limit=20 offset=20 → previous 20; limit=20 offset=40 → older 20.',
    inputSchema: {
      type: 'object',
      properties: {
        chat_id: {
          type: 'string',
          description: 'The chat ID (e.g. "39xxxxxxxxxx@c.us" for a contact, "xxxxxxxxxx-xxxxxxxxxx@g.us" for a group).',
        },
        number: {
          type: 'string',
          description: 'Alternative to chat_id for an individual contact: a phone number with country code (e.g. "393331234567" or "+39 333 123 4567"). Ignored if chat_id is given. Does not work for groups.',
        },
        limit: {
          type: 'integer',
          description: 'Number of messages to return (default 20, max 100).',
        },
        offset: {
          type: 'integer',
          description: 'Number of recent messages to skip before returning results (default 0). ' +
            'Increment by `limit` to page through older history.',
        },
      },
    },
  },
  {
    name: 'send_message',
    description: 'Send a WhatsApp text message. Identify the recipient with EITHER chat_id (from ' +
      'list_chats) OR a plain phone number for an individual contact — no lookup needed first.',
    inputSchema: {
      type: 'object',
      properties: {
        chat_id: {
          type: 'string',
          description: 'The chat ID to send to (from list_chats). Use for groups.',
        },
        number: {
          type: 'string',
          description: 'Alternative to chat_id for an individual contact: a phone number with country code (e.g. "393331234567" or "+39 333 123 4567"). Ignored if chat_id is given.',
        },
        message: {
          type: 'string',
          description: 'The text message to send.',
        },
      },
      required: ['message'],
    },
  },
  {
    name: 'send_media',
    description: 'Send an image, video, audio, or document to a WhatsApp chat or group. Identify the ' +
      'recipient with EITHER chat_id OR a phone number (individual contact). The media comes from a local ' +
      'file path or an http(s) URL.',
    inputSchema: {
      type: 'object',
      properties: {
        chat_id: {
          type: 'string',
          description: 'The chat ID to send to. Use for groups.',
        },
        number: {
          type: 'string',
          description: 'Alternative to chat_id for an individual contact: a phone number with country code. Ignored if chat_id is given.',
        },
        source: {
          type: 'string',
          description: 'The media to send: a local file path (absolute, or relative to the project root) or an http(s) URL.',
        },
        caption: {
          type: 'string',
          description: 'Optional text caption to attach to the media.',
        },
        as_document: {
          type: 'boolean',
          description: 'Send as a file/document instead of an inline photo/video (default false).',
        },
      },
      required: ['source'],
    },
  },
  {
    name: 'download_media',
    description: 'Download the media (photo, video, audio, document) attached to a WhatsApp message and ' +
      'save it locally. Pass the message_id shown as "download id" next to media messages in ' +
      'get_messages. Returns the local file path and a /data/ URL.',
    inputSchema: {
      type: 'object',
      properties: {
        message_id: {
          type: 'string',
          description: 'The serialized message id from the "download id" field in get_messages.',
        },
      },
      required: ['message_id'],
    },
  },
  {
    name: 'search_messages',
    description: 'Search messages across all WhatsApp chats by keyword.',
    inputSchema: {
      type: 'object',
      properties: {
        query: {
          type: 'string',
          description: 'Keyword to search for in message content.',
        },
        max_results: {
          type: 'integer',
          description: 'Max results to return (default 20, max 50).',
        },
      },
      required: ['query'],
    },
  },
  {
    name: 'search_contacts',
    description: 'Search saved WhatsApp contacts by name. Use this to find the ID of a contact ' +
      'you want to message but who does not appear in recent chats. ' +
      'For conversations already open, use list_chats instead.',
    inputSchema: {
      type: 'object',
      properties: {
        query: {
          type: 'string',
          description: 'Name or partial name to search for (case-insensitive).',
        },
        max_results: {
          type: 'integer',
          description: 'Max contacts to return (default 20, max 50).',
        },
      },
      required: ['query'],
    },
  },
];

// ── JSON-RPC helpers ───────────────────────────────────────────────────────

function okResponse(id, result) {
  return JSON.stringify({ jsonrpc: '2.0', id, result });
}

function textResult(id, text, isError = false) {
  const result = { content: [{ type: 'text', text }] };
  if (isError) result.isError = true;
  return JSON.stringify({ jsonrpc: '2.0', id, result });
}

// ── Request dispatch ───────────────────────────────────────────────────────

async function handleRequest(msg) {
  const { method, id, params } = msg;

  if (method === 'initialize') {
    return okResponse(id, {
      protocolVersion: '2024-11-05',
      capabilities: { tools: {} },
      serverInfo: { name: 'whatsapp', version: '1.0.0' },
    });
  }

  if (method === 'notifications/initialized') return null;

  if (method === 'tools/list') {
    return okResponse(id, { tools: TOOLS });
  }

  if (method === 'tools/call') {
    const toolName = params?.name || '';
    const toolArgs = params?.arguments || {};
    let text;
    try {
      switch (toolName) {
        case 'status':          text = await toolStatus(toolArgs);         break;
        case 'get_qr':          text = await toolGetQr(toolArgs);          break;
        case 'logout':          text = await toolLogout(toolArgs);         break;
        case 'list_chats':      text = await toolListChats(toolArgs);      break;
        case 'get_messages':    text = await toolGetMessages(toolArgs);    break;
        case 'send_message':    text = await toolSendMessage(toolArgs);    break;
        case 'send_media':      text = await toolSendMedia(toolArgs);      break;
        case 'download_media':  text = await toolDownloadMedia(toolArgs);  break;
        case 'search_messages': text = await toolSearchMessages(toolArgs); break;
        case 'search_contacts': text = await toolSearchContacts(toolArgs); break;
        default:
          return textResult(id, `Unknown tool: ${toolName}`, true);
      }
    } catch (e) {
      log(`Unhandled error in tool '${toolName}': ${e.message}`);
      return textResult(id, `Error: Internal error in '${toolName}': ${e.message}`, true);
    }
    const isErr = text.startsWith('Error:');
    return textResult(id, text, isErr);
  }

  return JSON.stringify({
    jsonrpc: '2.0', id,
    error: { code: -32601, message: `Method not found: ${method}` },
  });
}

// ── Main ───────────────────────────────────────────────────────────────────

async function main() {
  log('Starting WhatsApp MCP server');
  log(`Session dir: ${SESSION_DIR}`);

  if (!fs.existsSync(SECRETS_DIR)) {
    fs.mkdirSync(SECRETS_DIR, { recursive: true });
  }
  if (!fs.existsSync(DATA_DIR)) {
    fs.mkdirSync(DATA_DIR, { recursive: true });
  }

  initClient();

  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

  rl.on('line', async (line) => {
    line = line.trim();
    if (!line) return;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch (e) {
      log(`Invalid JSON on stdin: ${e.message}`);
      return;
    }
    const resp = await handleRequest(msg);
    if (resp !== null) {
      process.stdout.write(resp + '\n');
    }
  });

  rl.on('close', () => {
    log('stdin closed, shutting down');
    shutdown(0);
  });

  // Graceful shutdown for the signals we *can* trap. The host uses SIGKILL
  // (untrappable) on hard restarts — that path is covered by cleanupStaleBrowser()
  // on the next startup — but a container/OS stop sends SIGTERM first, and here we
  // close the browser cleanly so it releases the profile lock on the way out.
  process.on('SIGTERM', () => { log('SIGTERM received'); shutdown(0); });
  process.on('SIGINT',  () => { log('SIGINT received');  shutdown(0); });
}

// Close the browser (releasing the profile lock) before exiting. Guarded against
// re-entry and bounded by a timeout so a wedged page can never hang shutdown.
let shuttingDown = false;
async function shutdown(code) {
  if (shuttingDown) return;
  shuttingDown = true;
  try {
    if (client) {
      await Promise.race([
        client.destroy(),
        new Promise((resolve) => setTimeout(resolve, 5000)),
      ]);
    }
  } catch (_) {}
  process.exit(code);
}

main().catch((e) => {
  log(`Fatal: ${e.message}`);
  process.exit(1);
});
