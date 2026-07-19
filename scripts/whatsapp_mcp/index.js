#!/usr/bin/env node
'use strict';

/**
 * WhatsApp MCP Server (JSON-RPC 2.0 over stdio) — Baileys edition.
 *
 * Runs INSIDE the user's per-user container (blueprint §6/§7). Unlike the old
 * whatsapp-web.js server, this one uses `@whiskeysockets/baileys`: a pure-WebSocket
 * WhatsApp multi-device client with **no browser** — so it fits the slim
 * `skald-runtime` image (node, no Chromium) and needs no puppeteer self-healing.
 *
 * ── Interactive login contract (the generic §15 seam) ───────────────────────────
 * A per-user connector that needs an interactive login exposes ONE standard tool,
 * `login_status`, that Skald's login API calls directly (never the agent). It
 * returns a small JSON object the login panel renders:
 *
 *   { "state": "connecting" | "need_scan" | "ready" | "logged_out",
 *     "qr":    "data:image/png;base64,…"   // present only while state == need_scan
 *     "message": "human-readable line" }
 *
 * The panel polls it; when `state == "ready"` Skald flips the connector's
 * `auth_state` to `ready`. WhatsApp's credential is the persisted session on disk
 * (`./auth/`, under the bind-mounted home → survives a container recreate), not a
 * token — so there is nothing to paste back, only a QR to scan.
 */

// Baileys uses the Web Crypto global (`crypto.subtle`), which Node only exposes as
// `globalThis.crypto` from v20+. The container ships Node 18 (Debian bookworm), so
// polyfill it from `node:crypto` — without this, the socket dies on connect with
// "crypto is not defined" and never reaches the QR.
const nodeCrypto = require('crypto');
if (!globalThis.crypto) globalThis.crypto = nodeCrypto.webcrypto;

const fs   = require('fs');
const path = require('path');
const readline = require('readline');
const qrcode   = require('qrcode');

let makeWASocket, useMultiFileAuthState, DisconnectReason, fetchLatestBaileysVersion, jidNormalizedUser;
try {
  const baileys = require('@whiskeysockets/baileys');
  makeWASocket            = baileys.default || baileys.makeWASocket;
  useMultiFileAuthState   = baileys.useMultiFileAuthState;
  DisconnectReason        = baileys.DisconnectReason;
  fetchLatestBaileysVersion = baileys.fetchLatestBaileysVersion;
  jidNormalizedUser       = baileys.jidNormalizedUser;
} catch (e) {
  process.stderr.write(`[whatsapp_mcp] FATAL: baileys not installed (${e.message}). Run npm install.\n`);
}

// ── Paths ──────────────────────────────────────────────────────────────────
// Everything hangs off __dirname (the connector dir inside the container home,
// `~/.skald/mcp/<name>/`), which is bind-mounted and therefore durable.
const AUTH_DIR  = path.join(__dirname, 'auth');   // multi-file auth state (the "session")
const MEDIA_DIR = path.join(__dirname, 'media');

function log(msg) { process.stderr.write(`[whatsapp_mcp] ${msg}\n`); }

// A silent logger: Baileys requires one, and anything it prints must never reach
// stdout (that channel is reserved for JSON-RPC framing).
const silentLogger = (() => {
  const noop = () => {};
  const l = { level: 'silent', trace: noop, debug: noop, info: noop, warn: noop, error: noop, fatal: noop };
  l.child = () => l;
  return l;
})();

// ── Connection state ─────────────────────────────────────────────────────────
//   connecting  – socket starting or reconnecting
//   need_scan   – a QR is available; the user must scan it
//   ready       – authenticated and connected; tools operational
//   logged_out  – the phone unlinked this device; a fresh QR + scan is required
let state   = 'connecting';
let sock    = null;
let curQr   = null;   // latest raw QR string (null once scanned / connected)
let meJid   = null;
let starting = false;

// ── Lightweight in-memory store ───────────────────────────────────────────────
// Baileys keeps no chat/contact store of its own; we build a minimal one from the
// history-sync event and live upserts. It lives for the process lifetime — enough
// for "what's going on now", not a full archive.
const chats    = new Map();   // jid -> { id, name, unread, conversationTimestamp }
const contacts = new Map();   // jid -> { id, name }
const messages = new Map();   // jid -> [ { id, fromMe, ts, text, author } ]  (capped)

const MAX_MSGS_PER_CHAT = 200;

function pushMessage(jid, m) {
  if (!jid) return;
  let arr = messages.get(jid);
  if (!arr) { arr = []; messages.set(jid, arr); }
  arr.push(m);
  if (arr.length > MAX_MSGS_PER_CHAT) arr.splice(0, arr.length - MAX_MSGS_PER_CHAT);
}

function contactName(jid) {
  const c = contacts.get(jid);
  if (c && c.name) return c.name;
  const ch = chats.get(jid);
  if (ch && ch.name) return ch.name;
  return jid ? jid.split('@')[0] : 'unknown';
}

function textOf(msg) {
  const m = msg.message;
  if (!m) return '';
  return (
    m.conversation ||
    m.extendedTextMessage?.text ||
    m.imageMessage?.caption ||
    m.videoMessage?.caption ||
    m.documentMessage?.caption ||
    (m.imageMessage ? '[image]' : '') ||
    (m.videoMessage ? '[video]' : '') ||
    (m.audioMessage ? '[audio]' : '') ||
    (m.documentMessage ? '[document]' : '') ||
    (m.stickerMessage ? '[sticker]' : '') ||
    ''
  );
}

// ── WhatsApp socket lifecycle ──────────────────────────────────────────────────

async function startSock() {
  if (starting) return;
  starting = true;
  try {
    if (!makeWASocket) { state = 'connecting'; return; }
    fs.mkdirSync(AUTH_DIR, { recursive: true });

    const { state: authState, saveCreds } = await useMultiFileAuthState(AUTH_DIR);
    let version;
    try { ({ version } = await fetchLatestBaileysVersion()); } catch (_) { /* baileys default */ }

    sock = makeWASocket({
      version,
      auth: authState,
      logger: silentLogger,
      browser: ['Skald', 'Chrome', '1.0.0'],
      syncFullHistory: false,
      markOnlineOnConnect: false,
      generateHighQualityLinkPreview: false,
    });

    sock.ev.on('creds.update', saveCreds);

    sock.ev.on('connection.update', (u) => {
      const { connection, lastDisconnect, qr } = u;
      if (qr) { curQr = qr; state = 'need_scan'; log('QR ready — awaiting scan'); }
      if (connection === 'open') {
        curQr = null;
        state = 'ready';
        meJid = sock?.user?.id ? jidNormalizedUser(sock.user.id) : null;
        log('connection open — ready');
      }
      if (connection === 'close') {
        const code = lastDisconnect?.error?.output?.statusCode;
        if (code === DisconnectReason.loggedOut) {
          state = 'logged_out';
          curQr = null;
          log('logged out by phone — clearing session');
          try { fs.rmSync(AUTH_DIR, { recursive: true, force: true }); } catch (_) {}
          // Re-init so a fresh QR is produced immediately.
          starting = false;
          setTimeout(() => startSock(), 500);
        } else {
          state = 'connecting';
          log(`connection closed (code ${code ?? '?'}) — reconnecting`);
          starting = false;
          setTimeout(() => startSock(), 1500);
        }
      }
    });

    // Initial history sync: chats, contacts and a batch of messages.
    sock.ev.on('messaging-history.set', ({ chats: hc, contacts: hcs, messages: hm }) => {
      for (const c of hc || []) {
        chats.set(c.id, {
          id: c.id,
          name: c.name || c.subject || null,
          unread: c.unreadCount || 0,
          conversationTimestamp: Number(c.conversationTimestamp) || 0,
        });
      }
      for (const c of hcs || []) {
        contacts.set(c.id, { id: c.id, name: c.name || c.notify || c.verifiedName || null });
      }
      for (const m of hm || []) ingestMessage(m, false);
    });

    sock.ev.on('chats.upsert', (cs) => {
      for (const c of cs) chats.set(c.id, {
        id: c.id, name: c.name || c.subject || null,
        unread: c.unreadCount || 0,
        conversationTimestamp: Number(c.conversationTimestamp) || 0,
      });
    });
    sock.ev.on('contacts.upsert', (cs) => {
      for (const c of cs) contacts.set(c.id, { id: c.id, name: c.name || c.notify || c.verifiedName || null });
    });
    sock.ev.on('contacts.update', (cs) => {
      for (const c of cs) {
        const prev = contacts.get(c.id) || { id: c.id };
        contacts.set(c.id, { ...prev, name: c.name || c.notify || prev.name || null });
      }
    });

    sock.ev.on('messages.upsert', ({ messages: ms, type }) => {
      for (const m of ms) ingestMessage(m, type === 'notify');
    });
  } catch (e) {
    log(`startSock error: ${e.message}`);
    state = 'connecting';
  } finally {
    starting = false;
  }
}

function ingestMessage(m, live) {
  try {
    const jid = m.key?.remoteJid;
    if (!jid || jid === 'status@broadcast') return;
    const text = textOf(m);
    pushMessage(jid, {
      id: m.key?.id,
      fromMe: !!m.key?.fromMe,
      ts: Number(m.messageTimestamp) || 0,
      text,
      author: m.key?.participant || (m.key?.fromMe ? meJid : jid),
    });
    if (live && !chats.has(jid)) {
      chats.set(jid, { id: jid, name: m.pushName || null, unread: 0, conversationTimestamp: Number(m.messageTimestamp) || 0 });
    } else if (live) {
      const ch = chats.get(jid);
      ch.conversationTimestamp = Number(m.messageTimestamp) || ch.conversationTimestamp;
      if (m.pushName && !ch.name) ch.name = m.pushName;
    }
  } catch (_) {}
}

// ── Helpers ────────────────────────────────────────────────────────────────────

// Turn a plain phone number or a chat id into a WhatsApp jid.
function toJid(chat_id, number) {
  if (chat_id && chat_id.includes('@')) return chat_id;
  const raw = (chat_id || number || '').replace(/[^0-9]/g, '');
  if (!raw) return null;
  return `${raw}@s.whatsapp.net`;
}

function requireReady() {
  if (state !== 'ready') {
    throw new Error(`WhatsApp is not connected (state: ${state}). ` +
      (state === 'need_scan' || state === 'logged_out'
        ? 'Open the connector in Skald and scan the QR code to sign in.'
        : 'It is still connecting — try again in a few seconds.'));
  }
}

// ── Tools: interactive login (the §15 generic contract) ─────────────────────────

async function toolLoginStatus() {
  let qrDataUrl = null;
  if (state === 'need_scan' && curQr) {
    try { qrDataUrl = await qrcode.toDataURL(curQr, { width: 320, margin: 2 }); } catch (_) {}
  }
  const message = {
    connecting: 'Connecting to WhatsApp…',
    need_scan:  'Scan this QR code: WhatsApp → Settings → Linked Devices → Link a Device.',
    ready:      'WhatsApp is connected.',
    logged_out: 'This device was unlinked. Scan the new QR code to sign in again.',
  }[state] || state;
  // Returned as a JSON string in a text content part; the login API parses it.
  return JSON.stringify({ state, qr: qrDataUrl, message });
}

async function toolStatus() {
  const s = await toolLoginStatus();
  const { state: st, message } = JSON.parse(s);
  const chatCount = chats.size;
  return `WhatsApp status: ${st.toUpperCase()}\n${message}` +
    (st === 'ready' ? `\nKnown chats: ${chatCount}` : '');
}

async function toolLogout() {
  try { if (sock) await sock.logout(); } catch (_) {}
  try { fs.rmSync(AUTH_DIR, { recursive: true, force: true }); } catch (_) {}
  chats.clear(); contacts.clear(); messages.clear();
  curQr = null; state = 'connecting'; starting = false; meJid = null;
  setTimeout(() => startSock(), 500);
  return 'Logged out and cleared the session. A new QR code will be generated — open the connector in Skald and scan it.';
}

// ── Tools: messaging ────────────────────────────────────────────────────────────

async function toolListChats(args) {
  requireReady();
  const max = Math.min(Math.max(1, args.max_chats || 20), 50);
  const list = [...chats.values()]
    .sort((a, b) => (b.conversationTimestamp || 0) - (a.conversationTimestamp || 0))
    .slice(0, max);
  if (!list.length) return 'No chats known yet. History may still be syncing — try again in a few seconds.';
  const lines = [`Recent WhatsApp chats (${list.length}):`];
  for (const c of list) {
    const kind = c.id.endsWith('@g.us') ? '[group]' : '[chat]';
    const unread = c.unread ? ` (${c.unread} unread)` : '';
    lines.push(`- ${c.name || contactName(c.id)} ${kind}${unread} | ID: ${c.id}`);
  }
  return lines.join('\n');
}

async function toolGetMessages(args) {
  requireReady();
  const jid = toJid(args.chat_id, args.number);
  if (!jid) return 'Error: provide chat_id or number.';
  const limit = Math.min(Math.max(1, args.limit || 20), 100);
  const offset = Math.max(0, args.offset || 0);
  const arr = (messages.get(jid) || []).slice().sort((a, b) => (a.ts || 0) - (b.ts || 0));
  if (!arr.length) return `No messages buffered for ${contactName(jid)} (${jid}). Only messages seen since sign-in are available.`;
  const end = arr.length - offset;
  const slice = arr.slice(Math.max(0, end - limit), Math.max(0, end));
  const lines = [`Messages with ${contactName(jid)} (${jid}):`];
  for (const m of slice) {
    const who = m.fromMe ? 'me' : (jid.endsWith('@g.us') ? contactName(m.author) : contactName(jid));
    const when = m.ts ? new Date(m.ts * 1000).toISOString().replace('T', ' ').slice(0, 16) : '';
    lines.push(`[${when}] ${who}: ${m.text}`);
  }
  return lines.join('\n');
}

async function toolSendMessage(args) {
  requireReady();
  const jid = toJid(args.chat_id, args.number);
  if (!jid) return 'Error: provide chat_id or number.';
  if (!args.message) return 'Error: message is required.';
  await sock.sendMessage(jid, { text: String(args.message) });
  return `Message sent to ${contactName(jid)} (${jid}).`;
}

async function toolSearchContacts(args) {
  requireReady();
  const q = String(args.query || '').toLowerCase();
  if (!q) return 'Error: query is required.';
  const max = Math.min(Math.max(1, args.max_results || 20), 50);
  const seen = new Set();
  const out = [];
  for (const c of contacts.values()) {
    if (out.length >= max) break;
    const name = c.name || '';
    if (name.toLowerCase().includes(q) || c.id.includes(q)) {
      if (seen.has(c.id)) continue;
      seen.add(c.id);
      out.push(`- ${name || contactName(c.id)} | ID: ${c.id}`);
    }
  }
  if (!out.length) return `No contacts found matching "${args.query}".`;
  return [`Contacts matching "${args.query}" (${out.length}):`, ...out].join('\n');
}

// ── MCP tool definitions ────────────────────────────────────────────────────────

const TOOLS = [
  {
    name: 'login_status',
    description: 'Interactive-login status for this connector (used by the Skald login panel). Returns a JSON object {state, qr, message}: state is connecting|need_scan|ready|logged_out; qr is a data-URL PNG present only while a scan is needed. Safe to poll.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'status',
    description: 'WhatsApp connection status as a short human-readable report. Call this first when another WhatsApp tool fails.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'logout',
    description: 'Log out of WhatsApp: end the session, clear the stored credentials, and generate a fresh QR code to link a (possibly different) phone. After calling, the user must scan the new QR in the Skald connector page.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'list_chats',
    description: 'List recent WhatsApp chats (contacts and groups) with name, ID and unread count. Only chats seen since sign-in / history sync are known.',
    inputSchema: {
      type: 'object',
      properties: { max_chats: { type: 'integer', description: 'Max chats to return (default 20, max 50).' } },
    },
  },
  {
    name: 'get_messages',
    description: 'Get buffered messages from a chat. Identify it with EITHER chat_id (from list_chats) OR a phone number with country code for an individual contact. Only messages seen since sign-in are available (no deep history).',
    inputSchema: {
      type: 'object',
      properties: {
        chat_id: { type: 'string', description: 'Chat ID, e.g. "39XXXXXXXXXX@s.whatsapp.net" or "…@g.us".' },
        number:  { type: 'string', description: 'Alternative to chat_id: phone number with country code (e.g. "393331234567"). Ignored if chat_id is given.' },
        limit:   { type: 'integer', description: 'Number of messages (default 20, max 100).' },
        offset:  { type: 'integer', description: 'Skip this many of the most recent messages (default 0).' },
      },
    },
  },
  {
    name: 'send_message',
    description: 'Send a WhatsApp text message. Identify the recipient with EITHER chat_id (from list_chats, use for groups) OR a phone number with country code for an individual contact.',
    inputSchema: {
      type: 'object',
      properties: {
        chat_id: { type: 'string', description: 'Chat ID to send to (use for groups).' },
        number:  { type: 'string', description: 'Alternative to chat_id: phone number with country code. Ignored if chat_id is given.' },
        message: { type: 'string', description: 'The text to send.' },
      },
      required: ['message'],
    },
  },
  {
    name: 'search_contacts',
    description: 'Search known WhatsApp contacts by name or number. Use to find a contact ID to message.',
    inputSchema: {
      type: 'object',
      properties: {
        query:       { type: 'string', description: 'Name or partial name/number (case-insensitive).' },
        max_results: { type: 'integer', description: 'Max contacts to return (default 20, max 50).' },
      },
      required: ['query'],
    },
  },
];

// ── JSON-RPC framing ─────────────────────────────────────────────────────────

function okResponse(id, result) { return JSON.stringify({ jsonrpc: '2.0', id, result }); }
function textResult(id, text, isError = false) {
  const result = { content: [{ type: 'text', text }] };
  if (isError) result.isError = true;
  return JSON.stringify({ jsonrpc: '2.0', id, result });
}

async function handleRequest(msg) {
  const { method, id, params } = msg;

  if (method === 'initialize') {
    return okResponse(id, {
      protocolVersion: '2024-11-05',
      capabilities: { tools: {} },
      serverInfo: { name: 'whatsapp', version: '2.0.0' },
    });
  }
  if (method === 'notifications/initialized') return null;
  if (method === 'tools/list') return okResponse(id, { tools: TOOLS });

  if (method === 'tools/call') {
    const toolName = params?.name || '';
    const toolArgs = params?.arguments || {};
    let text;
    try {
      switch (toolName) {
        case 'login_status':    text = await toolLoginStatus();       break;
        case 'status':          text = await toolStatus();            break;
        case 'logout':          text = await toolLogout();            break;
        case 'list_chats':      text = await toolListChats(toolArgs); break;
        case 'get_messages':    text = await toolGetMessages(toolArgs); break;
        case 'send_message':    text = await toolSendMessage(toolArgs); break;
        case 'search_contacts': text = await toolSearchContacts(toolArgs); break;
        default:
          return textResult(id, `Unknown tool: ${toolName}`, true);
      }
    } catch (e) {
      log(`tool '${toolName}' error: ${e.message}`);
      return textResult(id, `Error: ${e.message}`, true);
    }
    const isErr = typeof text === 'string' && text.startsWith('Error:');
    return textResult(id, text, isErr);
  }

  return JSON.stringify({ jsonrpc: '2.0', id, error: { code: -32601, message: `Method not found: ${method}` } });
}

// ── Main ─────────────────────────────────────────────────────────────────────

async function main() {
  log('Starting WhatsApp MCP server (Baileys)');
  fs.mkdirSync(MEDIA_DIR, { recursive: true });
  startSock().catch((e) => log(`initial startSock failed: ${e.message}`));

  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  rl.on('line', async (line) => {
    line = line.trim();
    if (!line) return;
    let msg;
    try { msg = JSON.parse(line); } catch (e) { log(`bad JSON on stdin: ${e.message}`); return; }
    const resp = await handleRequest(msg);
    if (resp !== null) process.stdout.write(resp + '\n');
  });
  rl.on('close', () => { log('stdin closed, shutting down'); process.exit(0); });

  process.on('SIGTERM', () => { log('SIGTERM'); process.exit(0); });
  process.on('SIGINT',  () => { log('SIGINT');  process.exit(0); });
}

main().catch((e) => { log(`Fatal: ${e.message}`); process.exit(1); });
