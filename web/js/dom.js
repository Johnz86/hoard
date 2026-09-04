const SIZE_UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];
const RATE_UNITS = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"];
const MINUTE = 60000;
const TOAST_MS = 2400;

let toastTimer = null;

function appendChildren(node, children) {
    for (const child of children) {
        if (child === null || child === undefined || child === false) continue;
        if (Array.isArray(child)) appendChildren(node, child);
        else node.append(child.nodeType ? child : String(child));
    }
}

export function el(tag, props, ...children) {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(props || {})) {
        if (value === null || value === undefined || value === false) continue;
        if (key === "class") node.className = value;
        else if (key === "text") node.textContent = value;
        else if (key === "html") node.innerHTML = value;
        else if (key === "style") Object.assign(node.style, value);
        else if (key === "dataset") Object.assign(node.dataset, value);
        else if (key.startsWith("on")) node.addEventListener(key.slice(2).toLowerCase(), value);
        else node.setAttribute(key, value === true ? "" : value);
    }
    appendChildren(node, children);
    return node;
}

export function mount(node, ...children) {
    node.replaceChildren();
    appendChildren(node, children);
    return node;
}

function scale(value, units, step) {
    const n = Number(value);
    if (!Number.isFinite(n)) return "—";
    let v = Math.abs(n);
    let i = 0;
    while (v >= step && i < units.length - 1) {
        v /= step;
        i += 1;
    }
    const digits = v >= 100 ? 0 : v >= 10 ? 1 : 2;
    const shown = v.toFixed(digits).replace(/\.?0+$/, "");
    return `${n < 0 ? "-" : ""}${shown} ${units[i]}`;
}

export function fmtBytes(value) {
    return scale(value, SIZE_UNITS, 1000);
}

export function fmtRate(value) {
    return scale(value, RATE_UNITS, 1000);
}

export function fmtAge(value) {
    const then = Date.parse(value);
    if (!Number.isFinite(then)) return String(value ?? "—");
    const mins = Math.max(0, Math.round((Date.now() - then) / MINUTE));
    if (mins < 1) return "just now";
    if (mins < 60) return `${mins}m ago`;
    const hours = Math.round(mins / 60);
    if (hours < 48) return `${hours}h ago`;
    return `${Math.round(hours / 24)}d ago`;
}

export function fmtDay(value) {
    const when = Date.parse(value);
    if (!Number.isFinite(when)) return String(value ?? "");
    return new Date(when)
        .toLocaleDateString("en-US", { month: "short", day: "numeric" })
        .toLowerCase();
}

export function stateText(state, progress) {
    const label = String(state || "").toUpperCase();
    if (label === "PULLING" && Number.isFinite(Number(progress))) {
        return `PULLING ${Math.round(Number(progress) * 100)}%`;
    }
    return label || "—";
}

export function dash(value) {
    return value === null || value === undefined || value === "" ? "—" : String(value);
}

export function sizeText(row) {
    return row.size_human || fmtBytes(row.size);
}

export function toast(message) {
    const root = document.getElementById("toast-root");
    if (!root) return;
    clearTimeout(toastTimer);
    mount(root, el("div", { class: "toast", text: message }));
    toastTimer = setTimeout(() => root.replaceChildren(), TOAST_MS);
}

export async function copyText(text) {
    try {
        await navigator.clipboard.writeText(text);
        return true;
    } catch {
        const field = el("textarea", { class: "copy-sink" });
        field.value = text;
        document.body.append(field);
        field.select();
        let ok = false;
        try {
            ok = document.execCommand("copy");
        } catch {
            ok = false;
        }
        field.remove();
        return ok;
    }
}

export function errorPanel(error, retry) {
    return el(
        "div",
        { class: "state-panel" },
        el("div", { class: "state-panel-title", text: error.code || "REQUEST FAILED" }),
        el("div", { class: "state-panel-body", text: error.message || String(error) }),
        retry ? el("button", { class: "btn", type: "button", onClick: retry, text: "RETRY" }) : null
    );
}

export function loadingPanel(label) {
    return el("div", { class: "state-panel" }, el("div", { class: "state-panel-body", text: label }));
}

export function highlightUrl(line) {
    const match = /https?:\/\/[^\s]+/.exec(line || "");
    if (!match) return [String(line ?? "")];
    return [
        line.slice(0, match.index),
        el("span", { class: "amber", text: match[0] }),
        line.slice(match.index + match[0].length)
    ];
}
