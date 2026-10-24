const TOPICS = ['status', 'jobs', 'transfers', 'library', 'series', 'indexers', 'feeds', 'irc', 'requesters'];

/** One WebSocket with request/response calls, pushed topics and an HTTP fallback. */
class Link extends EventTarget {
  #socket = null;
  #pending = new Map();
  #next = 1;
  #attempt = 0;
  #timer = 0;
  #token = '';
  state = 'closed';

  get token() { return this.#token; }

  connect(token) {
    this.#token = token;
    this.#open();
  }

  disconnect() {
    clearTimeout(this.#timer);
    this.#token = '';
    this.#socket?.close(1000);
  }

  #setState(state) {
    this.state = state;
    this.dispatchEvent(new CustomEvent('state', { detail: state }));
  }

  #open() {
    clearTimeout(this.#timer);
    if (!this.#token) return;
    this.#setState('connecting');
    const hex = [...new TextEncoder().encode(this.#token)].map((b) => b.toString(16).padStart(2, '0')).join('');
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    const socket = new WebSocket(`${scheme}://${location.host}/ws`, ['mynou.v1', `auth.${hex}`]);
    this.#socket = socket;
    let opened = false;
    socket.addEventListener('open', () => {
      opened = true;
      this.#attempt = 0;
      this.#setState('open');
      socket.send(JSON.stringify({ subscribe: TOPICS }));
    });
    socket.addEventListener('message', (event) => this.#receive(event.data));
    socket.addEventListener('close', () => {
      if (this.#socket !== socket) return;
      this.#socket = null;
      for (const { reject } of this.#pending.values()) reject(new Error('Connection lost'));
      this.#pending.clear();
      this.#setState('closed');
      if (!opened && this.#attempt === 0) this.#probe();
      const delay = Math.min(15000, 500 * 2 ** this.#attempt++) * (0.7 + Math.random() * 0.6);
      this.#timer = setTimeout(() => this.#open(), delay);
    });
  }

  /** A failed handshake is a wrong token only if the plain API says so. */
  async #probe() {
    try {
      const response = await fetch('/api/status', { headers: { Authorization: `Bearer ${this.#token}` }, signal: AbortSignal.timeout(5000) });
      if (response.status === 401) this.dispatchEvent(new Event('rejected'));
    } catch { /* unreachable service: keep retrying */ }
  }

  #receive(text) {
    let message;
    try { message = JSON.parse(text); } catch { return; }
    if (message.topic) {
      this.dispatchEvent(new CustomEvent('topic', { detail: message }));
    } else if (this.#pending.has(message.id)) {
      const { resolve, reject } = this.#pending.get(message.id);
      this.#pending.delete(message.id);
      if (message.status >= 200 && message.status < 300) resolve(message.body);
      else reject(new Error(message.body?.error ?? `Request failed (${message.status})`));
    }
  }

  async call(method, path, body) {
    if (this.#socket?.readyState === WebSocket.OPEN) {
      const id = this.#next++;
      return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => {
          this.#pending.delete(id);
          reject(new Error('The request timed out'));
        }, 60000);
        this.#pending.set(id, {
          resolve: (v) => { clearTimeout(timeout); resolve(v); },
          reject: (e) => { clearTimeout(timeout); reject(e); },
        });
        this.#socket.send(JSON.stringify({ id, method, path, body }));
      });
    }
    const response = await fetch(path, {
      method,
      headers: { Authorization: `Bearer ${this.#token}`, ...(body ? { 'Content-Type': 'application/json' } : {}) },
      body: body ? JSON.stringify(body) : undefined,
      signal: AbortSignal.timeout(60000),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error ?? `Request failed (${response.status})`);
    return data;
  }
}

export const link = new Link();
