/* Original progressive enhancement. Native forms and page navigation remain authoritative. */
(() => {
    "use strict";
    const root = document.querySelector("[data-live]");
    if (!root) return;
    const button = root.querySelector("[data-live-toggle]");
    const status = root.querySelector("[data-live-status]");
    const freshness = root.querySelector("[data-live-freshness]");
    const signIn = root.querySelector("[data-live-sign-in]");
    const kind = root.dataset.live;
    const native = kind === "transfers";
    const pattern = native ? /^(?:[a-f0-9]{40}|[a-f0-9]{64})$/ : /^[a-f0-9]{32}$/;
    const rows = Array.from(document.querySelectorAll("[data-live-id]"));
    const ids = rows.map(row => row.dataset.liveId);
    if (!["jobs", "transfers"].includes(kind) || ids.length === 0 || ids.length > 50 ||
        new Set(ids).size !== ids.length || ids.some(id => !pattern.test(id))) return;
    if (typeof fetch !== "function" || typeof AbortController !== "function" ||
        typeof TextDecoder !== "function" || typeof ReadableStream !== "function") return;

    const states = native
        ? ["queued", "downloading", "paused", "ready", "selected_ready", "failed", "seed_limited"]
        : ["queued", "processing", "downloading", "scanning", "staged", "ready", "failed", "cancelled"];
    const counters = native
        ? ["downloaded_bytes", "uploaded_bytes", "verified_bytes", "seed_elapsed_secs"]
        : ["attempts"];
    const maximum = 48 * 1024;
    const interval = 10000;
    let enabled = false;
    let epoch = 0;
    let timer = null;
    let flight = null;

    button.disabled = false;
    status.textContent = "Live updates paused.";
    function announce(message) {
        if (status.textContent !== message) status.textContent = message;
    }
    function clearTimer() {
        if (timer !== null) clearTimeout(timer);
        timer = null;
    }
    function interrupt() {
        epoch += 1;
        clearTimer();
        if (flight) flight.controller.abort();
    }
    function stop(message, retry = false, authentication = false) {
        enabled = false;
        interrupt();
        button.textContent = retry ? "Retry live updates" : "Enable live updates";
        button.setAttribute("aria-pressed", "false");
        button.disabled = authentication;
        signIn.hidden = !authentication;
        announce(message);
    }
    function schedule(delay) {
        clearTimer();
        if (enabled && !document.hidden && !flight) {
            timer = setTimeout(poll, delay);
        }
    }
    function validate(value) {
        if (!value || value.kind !== kind || !Array.isArray(value.entries) ||
            value.entries.length !== ids.length) throw new Error("Invalid live reply");
        const entries = new Map();
        for (const entry of value.entries) {
            if (!entry || !ids.includes(entry.id) || entries.has(entry.id)) {
                throw new Error("Invalid live identifiers");
            }
            if (entry.missing !== true) {
                if (!states.includes(entry.state) || !Number.isFinite(entry.progress) ||
                    entry.progress < 0 || entry.progress > 1) throw new Error("Invalid live state");
                for (const key of counters) {
                    if (native ? !/^(0|[1-9][0-9]{0,15})$/.test(entry[key]) ||
                        typeof entry[key] !== "string"
                        : !Number.isSafeInteger(entry[key]) || entry[key] < 0) {
                        throw new Error("Invalid live counter");
                    }
                }
            }
            entries.set(entry.id, entry);
        }
        return entries;
    }
    function apply(entries) {
        for (const row of rows) {
            const entry = entries.get(row.dataset.liveId);
            const state = row.querySelector('[data-live-field="state"]');
            if (state) state.textContent = entry.missing ? "Unavailable" : entry.state.replaceAll("_", " ");
            if (entry.missing) continue;
            const progress = row.querySelector('[data-live-field="progress"]');
            if (progress) {
                const percentage = (entry.progress * 100).toFixed(1);
                const bar = progress.querySelector("progress");
                const label = progress.querySelector("small");
                if (bar) {
                    bar.value = entry.progress * 100;
                    bar.setAttribute("aria-label", percentage + "% complete");
                    bar.textContent = percentage + "%";
                }
                if (label) label.textContent = percentage + "%";
            }
            for (const key of counters) {
                const field = row.querySelector('[data-live-field="' + key + '"]');
                if (field) field.textContent = String(entry[key]);
            }
        }
        freshness.textContent = "Last updated " + new Date().toLocaleTimeString() + ".";
    }
    async function read(response) {
        if (!(response.headers.get("content-type") || "").toLowerCase().startsWith("application/json") ||
            !response.body || typeof response.body.getReader !== "function") {
            throw new Error("Invalid live response");
        }
        const reader = response.body.getReader();
        const decoder = new TextDecoder("utf-8", { fatal: true });
        let size = 0;
        let text = "";
        try {
            while (true) {
                const chunk = await reader.read();
                if (chunk.done) break;
                size += chunk.value.byteLength;
                if (size > maximum) throw new Error("Live reply exceeds the limit");
                text += decoder.decode(chunk.value, { stream: true });
            }
            text += decoder.decode();
            return validate(JSON.parse(text));
        } finally {
            await reader.cancel().catch(() => {});
            reader.releaseLock();
        }
    }
    async function poll() {
        timer = null;
        if (!enabled || document.hidden || flight) return;
        const token = epoch;
        const current = { controller: new AbortController() };
        flight = current;
        const deadline = setTimeout(() => current.controller.abort(), 8000);
        try {
            const response = await fetch("/ui/live/" + kind + "?ids=" + encodeURIComponent(ids.join(",")), {
                method: "GET", mode: "same-origin", credentials: "same-origin", cache: "no-store",
                redirect: "error", referrerPolicy: "same-origin", headers: { Accept: "application/json" },
                signal: current.controller.signal
            });
            if (token !== epoch || !enabled || document.hidden) return;
            if (response.status === 401 || response.status === 403) {
                stop("Live updates stopped. Sign in again.", false, true);
                return;
            }
            if (!response.ok) throw new Error("Live request failed");
            const entries = await read(response);
            if (token !== epoch || !enabled || document.hidden) return;
            apply(entries);
            announce("Live updates running. Checking every 10 seconds.");
        } catch (_) {
            if (token === epoch && enabled && !document.hidden) {
                stop("Live updates paused. Retry or refresh the page.", true);
            }
        } finally {
            clearTimeout(deadline);
            if (flight === current) flight = null;
            schedule(token === epoch ? interval : 0);
        }
    }
    button.addEventListener("click", () => {
        if (enabled) {
            stop("Live updates paused.");
            return;
        }
        enabled = true;
        epoch += 1;
        button.textContent = "Pause updates";
        button.setAttribute("aria-pressed", "true");
        signIn.hidden = true;
        announce(document.hidden ? "Live updates paused while this page is hidden." : "Checking for live updates.");
        schedule(0);
    });
    document.addEventListener("visibilitychange", () => {
        interrupt();
        if (enabled) {
            announce(document.hidden ? "Live updates paused while this page is hidden." : "Checking for live updates.");
            schedule(0);
        }
    });
    window.addEventListener("pagehide", () => stop("Live updates paused."));
})();
