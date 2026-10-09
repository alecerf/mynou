/* CI-only, original browser behavior fixtures using Node's standard library. */
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const source = fs.readFileSync("src/web/live.js", "utf8");

class Element {
    constructor(text = "") {
        this.textContent = text;
        this.dataset = {};
        this.hidden = false;
        this.disabled = false;
        this.attributes = {};
        this.children = new Map();
        this.events = new Map();
    }
    querySelector(selector) { return this.children.get(selector) || null; }
    setAttribute(name, value) { this.attributes[name] = value; }
    addEventListener(name, fn) { this.events.set(name, fn); }
    emit(name) { this.events.get(name)?.(); }
    set innerHTML(_) { throw new Error("Replacing HTML would destroy user state"); }
    replaceChildren() { throw new Error("Replacing children would destroy user state"); }
    replaceWith() { throw new Error("Replacing rows would destroy user state"); }
}

function fixture({ kind = "jobs", count = 1, supported = true } = {}) {
    const native = kind === "transfers";
    const root = new Element();
    root.dataset.live = kind;
    const button = new Element("Enable live updates");
    button.disabled = true;
    const status = new Element("Use Refresh page.");
    const freshness = new Element("Showing the page as loaded.");
    const signIn = new Element();
    signIn.hidden = true;
    root.children = new Map([
        ["[data-live-toggle]", button], ["[data-live-status]", status],
        ["[data-live-freshness]", freshness], ["[data-live-sign-in]", signIn]
    ]);
    const rows = Array.from({ length: count }, (_, index) => {
        const row = new Element();
        row.dataset.liveId = (index + 1).toString(16).padStart(native ? 40 : 32, "0");
        const progress = new Element();
        progress.children = new Map([["progress", new Element()], ["small", new Element("0.0%")]]);
        progress.children.get("progress").value = 0;
        row.children.set('[data-live-field="progress"]', progress);
        row.children.set('[data-live-field="state"]', new Element("queued"));
        for (const key of native
            ? ["downloaded_bytes", "uploaded_bytes", "verified_bytes", "seed_elapsed_secs"]
            : ["attempts"]) {
            row.children.set('[data-live-field="' + key + '"]', new Element("0"));
        }
        row.checkbox = { checked: true };
        row.policy = { value: "unfinished custom limit" };
        return row;
    });
    const document = {
        hidden: false,
        activeElement: rows[0]?.policy,
        events: new Map(),
        querySelector: selector => selector === "[data-live]" ? root : null,
        querySelectorAll: selector => selector === "[data-live-id]" ? rows : [],
        addEventListener(name, fn) { this.events.set(name, fn); }
    };
    const window = { events: new Map(), addEventListener(name, fn) { this.events.set(name, fn); } };
    let clock = 0;
    let next = 0;
    const timers = new Map();
    const requests = [];
    vm.runInNewContext(source, {
        document, window, AbortController, TextDecoder, ReadableStream, Date,
        fetch: supported ? (url, options) => new Promise((resolve, reject) => requests.push({ url, options, resolve, reject })) : undefined,
        setTimeout(fn, delay) {
            const id = ++next;
            timers.set(id, { at: clock + delay, fn });
            return id;
        },
        clearTimeout(id) { timers.delete(id); }
    });
    function advance(milliseconds) {
        const end = clock + milliseconds;
        while (true) {
            const due = Array.from(timers).filter(([, timer]) => timer.at <= end)
                .sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
            if (!due) break;
            timers.delete(due[0]);
            clock = due[1].at;
            due[1].fn();
        }
        clock = end;
    }
    const data = (overrides = {}) => ({
        kind,
        entries: rows.map(row => ({
            id: row.dataset.liveId, state: native ? "paused" : "downloading", progress: 0.485,
            ...(native ? {
                downloaded_bytes: "18446744073709551615", uploaded_bytes: "123",
                verified_bytes: "456", seed_elapsed_secs: "12"
            } : { attempts: 4 }), ...overrides
        }))
    });
    function hide(hidden) { document.hidden = hidden; document.events.get("visibilitychange")(); }
    return { root, button, status, freshness, signIn, rows, document, window, requests, advance, data, hide };
}

function response(value, status = 200, raw) {
    const bytes = raw || new TextEncoder().encode(JSON.stringify(value));
    let consumed = false;
    return {
        status, ok: status >= 200 && status < 300,
        headers: { get: () => "application/json; charset=utf-8" },
        body: { getReader: () => ({
            async read() {
                if (consumed) return { done: true };
                consumed = true;
                return { done: false, value: bytes };
            },
            async cancel() {},
            releaseLock() {}
        }) }
    };
}
async function settle() { for (let i = 0; i < 16; i++) await Promise.resolve(); }
function enable(f) { f.button.emit("click"); f.advance(0); }

test("disabled by default; enabling changes only progress fields and requests exact displayed IDs", async () => {
    const f = fixture();
    f.advance(60000);
    assert.equal(f.requests.length, 0);
    assert.equal(f.button.disabled, false);
    const row = f.rows[0], focused = f.document.activeElement;
    enable(f);
    const request = f.requests[0];
    assert.equal(request.url, "/ui/live/jobs?ids=" + row.dataset.liveId);
    assert.equal(request.options.credentials, "same-origin");
    assert.equal(request.options.cache, "no-store");
    assert.equal(request.options.redirect, "error");
    assert.equal(request.options.referrerPolicy, "same-origin");
    assert.equal(request.options.method, "GET");
    f.advance(1000);
    assert.equal(f.requests.length, 1);
    request.resolve(response(f.data()));
    await settle();
    assert.equal(row.querySelector('[data-live-field="state"]').textContent, "downloading");
    assert.equal(row.querySelector('[data-live-field="progress"]').querySelector("progress").value, 48.5);
    assert.equal(row.querySelector('[data-live-field="progress"]').querySelector("progress").attributes["aria-label"], "48.5% complete");
    assert.equal(row.querySelector('[data-live-field="attempts"]').textContent, "4");
    assert.match(f.freshness.textContent, /^Last updated /);
    assert.equal(row.checkbox.checked, true);
    assert.equal(row.policy.value, "unfinished custom limit");
    assert.equal(f.document.activeElement, focused);
    f.advance(9999);
    assert.equal(f.requests.length, 1);
    f.advance(1);
    assert.equal(f.requests.length, 2);
});

test("pause and immediate resume cannot overlap a still settling aborted request", async () => {
    const f = fixture();
    enable(f);
    f.button.emit("click");
    assert.equal(f.requests[0].options.signal.aborted, true);
    f.button.emit("click");
    f.advance(0);
    assert.equal(f.requests.length, 1);
    f.requests[0].resolve(response(f.data()));
    await settle();
    assert.equal(f.rows[0].querySelector('[data-live-field="state"]').textContent, "queued");
    assert.equal(f.freshness.textContent, "Showing the page as loaded.");
    f.advance(0);
    assert.equal(f.requests.length, 2);
});

test("hidden pages abort and ignore stale success; visible pages resume without overlapping", async () => {
    const f = fixture();
    enable(f);
    f.hide(true);
    assert.equal(f.requests[0].options.signal.aborted, true);
    f.advance(60000);
    assert.equal(f.requests.length, 1);
    f.hide(false);
    f.advance(0);
    assert.equal(f.requests.length, 1);
    f.requests[0].resolve(response(f.data()));
    await settle();
    assert.equal(f.freshness.textContent, "Showing the page as loaded.");
    f.advance(0);
    assert.equal(f.requests.length, 2);
    f.requests[1].resolve(response(f.data()));
    await settle();
    assert.match(f.freshness.textContent, /^Last updated /);
});

test("missing rows remain present with a fixed unavailable state", async () => {
    const f = fixture();
    enable(f);
    f.requests[0].resolve(response({ kind: "jobs", entries: [{ id: f.rows[0].dataset.liveId, missing: true }] }));
    await settle();
    assert.equal(f.rows.length, 1);
    assert.equal(f.rows[0].querySelector('[data-live-field="state"]').textContent, "Unavailable");
    assert.equal(f.rows[0].checkbox.checked, true);
});

for (const code of [401, 403]) {
    test("authentication " + code + " stops updates and offers ordinary sign-in", async () => {
        const f = fixture();
        enable(f);
        f.requests[0].resolve(response({}, code));
        await settle();
        assert.equal(f.button.disabled, true);
        assert.equal(f.signIn.hidden, false);
        assert.match(f.status.textContent, /Sign in again/);
        f.advance(60000);
        assert.equal(f.requests.length, 1);
    });
}

test("transient failure pauses until the user explicitly retries", async () => {
    const f = fixture();
    enable(f);
    f.requests[0].resolve(response({}, 503));
    await settle();
    assert.equal(f.button.textContent, "Retry live updates");
    f.advance(60000);
    assert.equal(f.requests.length, 1);
    f.button.emit("click");
    f.advance(0);
    assert.equal(f.requests.length, 2);
});

test("all entries are validated before any field or freshness is changed", async () => {
    const f = fixture({ count: 2 });
    enable(f);
    const value = f.data();
    value.entries[1].progress = 2;
    f.requests[0].resolve(response(value));
    await settle();
    assert.equal(f.rows[0].querySelector('[data-live-field="state"]').textContent, "queued");
    assert.equal(f.freshness.textContent, "Showing the page as loaded.");
    assert.equal(f.button.textContent, "Retry live updates");
});

test("oversized or invalid UTF-8 replies pause instead of applying data", async () => {
    for (const bytes of [new Uint8Array(48 * 1024 + 1), new Uint8Array([0xff])]) {
        const f = fixture();
        enable(f);
        f.requests[0].resolve(response({}, 200, bytes));
        await settle();
        assert.equal(f.freshness.textContent, "Showing the page as loaded.");
        assert.equal(f.button.textContent, "Retry live updates");
        f.advance(60000);
        assert.equal(f.requests.length, 1);
    }
});

test("the eight-second deadline aborts the only request and exposes retry", async () => {
    const f = fixture();
    enable(f);
    f.advance(8000);
    assert.equal(f.requests[0].options.signal.aborted, true);
    assert.equal(f.requests.length, 1);
    f.requests[0].reject(new Error("Aborted"));
    await settle();
    assert.equal(f.button.textContent, "Retry live updates");
    f.advance(60000);
    assert.equal(f.requests.length, 1);
});

test("unsigned 64-bit transfer counters are displayed exactly without floating point conversion", async () => {
    const f = fixture({ kind: "transfers" });
    enable(f);
    f.requests[0].resolve(response(f.data()));
    await settle();
    assert.equal(f.rows[0].querySelector('[data-live-field="downloaded_bytes"]').textContent, "18446744073709551615");
    const invalid = fixture({ kind: "transfers" });
    enable(invalid);
    invalid.requests[0].resolve(response(invalid.data({ uploaded_bytes: "18446744073709551616" })));
    await settle();
    assert.equal(invalid.freshness.textContent, "Showing the page as loaded.");
});

test("unsupported platforms or excessive scope retain the disabled manual fallback", () => {
    for (const options of [{ supported: false }, { count: 0 }, { count: 51 }]) {
        const f = fixture(options);
        f.advance(60000);
        assert.equal(f.button.disabled, true);
        assert.equal(f.requests.length, 0);
    }
});

test("leaving the page cancels the request and all further scheduling", async () => {
    const f = fixture();
    enable(f);
    f.window.events.get("pagehide")();
    assert.equal(f.requests[0].options.signal.aborted, true);
    f.requests[0].resolve(response(f.data()));
    await settle();
    f.advance(60000);
    assert.equal(f.requests.length, 1);
    assert.equal(f.freshness.textContent, "Showing the page as loaded.");
});
