/**
 * Singleton client for the `/api/file/watch` WebSocket.
 *
 * One persistent connection for the whole app. Multi-subscriber: if several
 * components ask to watch the same path, only one subscribe message is sent
 * over the wire; the OS watcher is shared. Unsubscribe ref-counts down and
 * only sends `unsubscribe` when the last consumer for a path goes away.
 *
 * Auto-reconnects on close (2 s backoff) and re-issues every active
 * subscription on reconnect, so consumers don't have to handle disconnects.
 *
 * Usage:
 *   import { fileWatcher } from '../lib/file-watcher.js';
 *   const unsub = await fileWatcher.watch('docs/index.md', (path) => { ... });
 *   unsub();   // stop watching
 */
class FileWatcher {
    constructor() {
        this._ws             = null;
        this._subscriptions  = new Map();   // path -> Set<callback>
        this._reconnectTimer = null;
        this._connectPromise = null;
    }

    _ensureConnected() {
        if (this._ws && this._ws.readyState === WebSocket.OPEN) return Promise.resolve();
        if (this._connectPromise) return this._connectPromise;

        this._connectPromise = new Promise((resolve, reject) => {
            const proto = location.protocol === 'https:' ? 'wss' : 'ws';
            const ws = new WebSocket(`${proto}://${location.host}/api/file/watch`);
            this._ws = ws;

            ws.onopen = () => {
                this._connectPromise = null;
                // Re-subscribe everything (covers both first connect and reconnect).
                for (const path of this._subscriptions.keys()) {
                    ws.send(JSON.stringify({ op: 'subscribe', path }));
                }
                resolve();
            };

            ws.onmessage = (e) => {
                let msg;
                try { msg = JSON.parse(e.data); } catch { return; }
                if (msg.type === 'changed') {
                    const cbs = this._subscriptions.get(msg.path);
                    if (cbs) cbs.forEach(cb => { try { cb(msg.path); } catch { /* swallow */ } });
                }
                // 'subscribed' / 'unsubscribed' / 'error' acks are informational;
                // we don't currently surface them to consumers.
            };

            ws.onerror = () => {
                if (this._connectPromise) {
                    this._connectPromise = null;
                    reject(new Error('file-watch WS error'));
                }
            };

            ws.onclose = () => {
                this._ws = null;
                this._connectPromise = null;
                if (this._reconnectTimer) clearTimeout(this._reconnectTimer);
                this._reconnectTimer = setTimeout(() => {
                    this._reconnectTimer = null;
                    this._ensureConnected().catch(() => { /* silent retry */ });
                }, 2000);
            };
        });
        return this._connectPromise;
    }

    async watch(path, cb) {
        await this._ensureConnected();
        let cbs = this._subscriptions.get(path);
        const isNew = !cbs;
        if (!cbs) {
            cbs = new Set();
            this._subscriptions.set(path, cbs);
        }
        cbs.add(cb);
        if (isNew && this._ws && this._ws.readyState === WebSocket.OPEN) {
            this._ws.send(JSON.stringify({ op: 'subscribe', path }));
        }
        return () => this.unwatch(path, cb);
    }

    unwatch(path, cb) {
        const cbs = this._subscriptions.get(path);
        if (!cbs) return;
        cbs.delete(cb);
        if (cbs.size === 0) {
            this._subscriptions.delete(path);
            if (this._ws && this._ws.readyState === WebSocket.OPEN) {
                this._ws.send(JSON.stringify({ op: 'unsubscribe', path }));
            }
        }
    }
}

export const fileWatcher = new FileWatcher();
