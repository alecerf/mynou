const TOPICS = ['status', 'jobs', 'transfers', 'library', 'series', 'indexers', 'feeds', 'irc', 'requesters', 'stats'];

/** One WebSocket with request/response calls, pushed topics and an HTTP fallback. */
class Link extends EventTarget {
  #socket = null;
  #pending = new Map();
  #next = 1;
  #attempt = 0;
  #timer = 0;
  #extra = new Set();   // topics a sheet watches while it is open
  #stopped = false;
  state = 'closed';

  connect() {
    this.#stopped = false;
    this.#open();
  }

  /** Closes the socket and stops reconnecting until `connect`. */
  disconnect() {
    this.#stopped = true;
    clearTimeout(this.#timer);
    this.#socket?.close(1000);
  }

  #setState(state) {
    this.state = state;
    this.dispatchEvent(new CustomEvent('state', { detail: state }));
  }

  #open() {
    clearTimeout(this.#timer);
    this.#setState('connecting');
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    const socket = new WebSocket(`${scheme}://${location.host}/ws`, 'mynou.v1');
    this.#socket = socket;
    socket.addEventListener('open', () => {
      this.#attempt = 0;
      this.#setState('open');
      this.#subscribe();
    });
    socket.addEventListener('message', (event) => this.#receive(event.data));
    socket.addEventListener('close', () => {
      if (this.#socket !== socket) return;
      this.#socket = null;
      for (const { reject } of this.#pending.values()) reject(new Error('Connection lost'));
      this.#pending.clear();
      this.#setState('closed');
      if (this.#stopped) return;
      const delay = Math.min(15000, 500 * 2 ** this.#attempt++) * (0.7 + Math.random() * 0.6);
      this.#timer = setTimeout(() => this.#open(), delay);
    });
  }

  #subscribe() {
    if (this.#socket?.readyState === WebSocket.OPEN) this.#socket.send(JSON.stringify({ subscribe: [...TOPICS, ...this.#extra] }));
  }

  /** Pushes one more topic, such as `transfer:ID`, until `unwatch`. */
  watch(topic) { this.#extra.add(topic); this.#subscribe(); }
  unwatch(topic) { if (this.#extra.delete(topic)) this.#subscribe(); }

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
    return this.http(method, path, body);
  }

  /** Plain HTTP, for bodies larger than a WebSocket frame may carry. */
  async http(method, path, body) {
    const response = await fetch(path, {
      method,
      headers: body ? { 'Content-Type': 'application/json' } : {},
      body: body ? JSON.stringify(body) : undefined,
      signal: AbortSignal.timeout(60000),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error ?? `Request failed (${response.status})`);
    return data;
  }
}

export const link = new Link();
