import { el, mount } from "./dom.js";

const PARSE_DELAY = 120;
const SEARCH_DELAY = 300;

let input = null;
let clearBtn = null;
let hint = null;
let strip = null;
let parseTimer = null;
let searchTimer = null;

function splitToken(token) {
    const raw = String(token.raw ?? "");
    if (!token.key) return { key: token.fuzzy ? "~" : "", value: raw.replace(/^~/, "") };
    const colon = raw.indexOf(":");
    if (colon < 0) return { key: `${token.key}:`, value: String(token.value ?? "") };
    return { key: raw.slice(0, colon + 1), value: raw.slice(colon + 1) };
}

export function initSearch(app) {
    input = document.getElementById("search-input");
    clearBtn = document.getElementById("search-clear");
    hint = document.getElementById("search-hint");
    strip = document.getElementById("token-strip");

    input.addEventListener("input", () => {
        const value = input.value;
        clearBtn.hidden = value.length === 0;
        clearTimeout(parseTimer);
        clearTimeout(searchTimer);
        parseTimer = setTimeout(() => app.previewQuery(value), PARSE_DELAY);
        searchTimer = setTimeout(() => app.commitQuery(value), SEARCH_DELAY);
    });
    input.addEventListener("keydown", (event) => {
        if (event.key !== "Enter") return;
        event.preventDefault();
        clearTimeout(parseTimer);
        clearTimeout(searchTimer);
        app.commitQuery(input.value);
    });
    input.addEventListener("focus", () => {
        document.getElementById("search").classList.add("is-focused");
        hint.textContent = "ESC";
    });
    input.addEventListener("blur", () => {
        document.getElementById("search").classList.remove("is-focused");
        hint.textContent = "/";
    });
    clearBtn.addEventListener("click", () => {
        setSearchValue("");
        app.commitQuery("");
    });
}

export function setSearchValue(value) {
    if (!input) return;
    input.value = value;
    clearBtn.hidden = value.length === 0;
}

export function focusSearch() {
    if (input) input.focus();
}

export function blurSearch() {
    if (input) input.blur();
}

export function searchHasFocus() {
    return !!input && document.activeElement === input;
}

export function renderTokenStrip(app) {
    if (!strip) return;
    const raw = String(app.state.q || "").trim();
    strip.hidden = raw.length === 0;
    if (strip.hidden) {
        strip.replaceChildren();
        return;
    }
    const tokens = (app.state.query && app.state.query.tokens) || [];
    const warnings = (app.state.query && app.state.query.warnings) || [];
    mount(
        strip,
        el("span", { class: "strip-label", text: "PARSED" }),
        tokens.map((token) => {
            const parts = splitToken(token);
            return el(
                "span",
                { class: "chip" },
                parts.key
                    ? el("span", {
                          class: token.key ? "chip-key" : "chip-key chip-key-text",
                          text: parts.key
                      })
                    : null,
                el("span", { class: "chip-val", text: parts.value }),
                el("span", {
                    class: "chip-x",
                    title: "remove token",
                    text: "×",
                    onClick: () => app.removeToken(token.raw)
                })
            );
        }),
        warnings.map((warning) => el("span", { class: "strip-warning", text: warning })),
        el("span", { class: "strip-result", text: app.resultLine() })
    );
}
