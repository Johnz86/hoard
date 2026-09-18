import * as api from "./api.js";
import { renderCards } from "./cards.js";
import { renderDetail, TABS, loadDetail, loadTab } from "./detail.js";
import { el, errorPanel, loadingPanel, mount, toast, copyText } from "./dom.js";
import { renderFacets } from "./facets.js";
import { renderInspector } from "./inspector.js";
import { initKeys, renderKeySheet } from "./keys.js";
import { loadLoadout, loadoutItems, renderDrawer } from "./loadout.js";
import { loadNodes, renderNodePulse, renderNodesView } from "./nodes.js";
import { loadJobs, loadSources, renderJobsView, renderScansView } from "./jobs.js";
import { initSearch, renderTokenStrip, setSearchValue } from "./search.js";
import { loadShelves, renderShelfRail, renderShelvesView } from "./shelves.js";
import { renderTable, revealCursor } from "./table.js";
import { applySettings, cycleTheme, settings, themeLabel, updateSettings } from "./theme.js";

const NAV = [
    { label: "INDEX", route: "/index" },
    { label: "SHELVES", route: "/shelves" },
    { label: "NODES", route: "/nodes" },
    { label: "SCANS", route: "/scans" },
    { label: "JOBS", route: "/jobs" }
];
const SEARCH_LIMIT = 100;
const INSPECTOR_DELAY = 160;
const FALLBACK_EXAMPLE = "role:lora";

const state = {
    view: "index",
    q: "",
    query: null,
    rows: [],
    facets: {},
    total: 0,
    nodeCount: 0,
    sort: null,
    dir: "asc",
    sortApplied: "name",
    dirApplied: "asc",
    cursor: 0,
    selection: new Set(),
    viewMode: "TABLE",
    searchError: null,
    searching: false,
    cursorArtifact: null,
    shelves: null,
    shelvesError: null,
    nodes: null,
    nodesError: null,
    sources: null,
    sourcesError: null,
    jobs: null,
    jobsError: null,
    loadoutId: null,
    loadoutCreating: null,
    loadouts: null,
    loadoutDeleting: null,
    loadout: null,
    loadoutError: null,
    loadoutApplied: false,
    drawerOpen: false,
    keySheet: false,
    prefsOpen: false,
    detail: emptyDetail()
};

function emptyDetail() {
    return {
        type: "models",
        org: "",
        name: "",
        rev: "",
        tab: "FILES",
        aid: null,
        repo: null,
        files: null,
        readme: null,
        compat: null,
        usage: null,
        loaded: {},
        errors: {}
    };
}

let inspectorTimer = null;
let suppressHashChange = false;

const app = {
    state,
    previewQuery,
    commitQuery,
    removeToken,
    toggleToken,
    sortBy,
    setCursor,
    moveCursor,
    toggleSelection,
    cursorRow,
    openRow,
    openArtifact,
    addToLoadout,
    removeFromLoadout,
    switchLoadout,
    newLoadout,
    renameLoadout,
    armDelete,
    deleteLoadout,
    yank,
    copyCommand,
    openDrawer,
    closeDrawer,
    setDetailTab,
    cycleDetailTab,
    checkoutRef,
    reloadDetail,
    reloadDetailTab,
    detailArtifact,
    goIndex,
    showKeySheet,
    hideKeySheet,
    selfNodeName,
    resultLine,
    render
};

function repoTypeOf(row) {
    if (!row) return "models";
    if (row.repo_type) return row.repo_type;
    return String(row.role || "").toLowerCase() === "ds" ? "datasets" : "models";
}

function selfNodeName() {
    const nodes = state.nodes || [];
    const own = nodes.find((node) => node.self) || nodes[0];
    return (own && own.name) || "this node";
}

function resultLine() {
    return `${state.total} ARTIFACTS · ${state.nodeCount} NODES`;
}

function exampleQuery() {
    const answering = (state.shelves || []).find(
        (shelf) => shelf.builtin && typeof shelf.count === "number" && shelf.count > 0
    );
    return (answering && answering.query) || FALLBACK_EXAMPLE;
}

function sortLabel() {
    return `SORT ${state.sortApplied.toUpperCase()} ${state.dirApplied === "asc" ? "↑" : "↓"}`;
}

function buildHash() {
    if (state.view === "detail") {
        const detail = state.detail;
        const params = new URLSearchParams();
        params.set("tab", detail.tab);
        if (detail.rev) params.set("rev", detail.rev);
        if (detail.aid) params.set("aid", detail.aid);
        const path = [detail.type, detail.org, detail.name].map(encodeURIComponent).join("/");
        return `#/repo/${path}?${params.toString()}`;
    }
    if (state.view !== "index") return `#/${state.view}`;
    const params = new URLSearchParams();
    if (state.q) params.set("q", state.q);
    if (state.viewMode !== "TABLE") params.set("view", state.viewMode);
    if (state.sort) {
        params.set("sort", state.sort);
        params.set("dir", state.dir);
    }
    const text = params.toString();
    return text ? `#/index?${text}` : "#/index";
}

function syncHash() {
    const next = buildHash();
    if (next === location.hash) return;
    suppressHashChange = true;
    history.replaceState(null, "", next);
    suppressHashChange = false;
}

function navigate(hash) {
    if (hash === location.hash) return;
    location.hash = hash;
}

function readHash() {
    const raw = location.hash.replace(/^#/, "") || "/index";
    const [path, search] = raw.split("?");
    const params = new URLSearchParams(search || "");
    const parts = path.split("/").filter(Boolean);
    const head = parts[0] || "index";
    const repoMatch = /^\/repo\/([^/]*)\/([^/]*)\/(.+)$/.exec(path);

    if (repoMatch) {
        const detail = emptyDetail();
        detail.type = decodeURIComponent(repoMatch[1]);
        detail.org = decodeURIComponent(repoMatch[2]);
        detail.name = decodeURIComponent(repoMatch[3]);
        detail.tab = TABS.includes(params.get("tab")) ? params.get("tab") : "FILES";
        detail.rev = params.get("rev") || "";
        detail.aid = params.get("aid");
        const same =
            state.detail.type === detail.type &&
            state.detail.org === detail.org &&
            state.detail.name === detail.name &&
            state.detail.rev === detail.rev &&
            (state.detail.aid || null) === (detail.aid || null);
        if (same) {
            state.detail.tab = detail.tab;
        } else {
            state.detail = detail;
        }
        state.view = "detail";
        render();
        loadDetail(app);
        return;
    }

    if (["shelves", "nodes", "scans", "jobs"].includes(head)) {
        state.view = head;
        render();
        if (head === "shelves" && !state.shelves) loadShelves(app);
        if (head === "nodes" && !state.nodes) loadNodes(app);
        if (head === "scans" && !state.sources) loadSources(app);
        if (head === "jobs") loadJobs(app);
        return;
    }

    const q = params.get("q") || "";
    const mode = params.get("view") === "CARDS" ? "CARDS" : "TABLE";
    const sort = params.get("sort");
    const dir = params.get("dir") === "desc" ? "desc" : "asc";
    const changed = q !== state.q || sort !== state.sort || dir !== state.dir || state.view !== "index";
    state.view = "index";
    state.viewMode = mode;
    state.q = q;
    state.sort = sort;
    state.dir = dir;
    setSearchValue(q);
    if (!state.searching && (changed || !state.rows.length)) runSearch();
    else render();
}

async function previewQuery(q) {
    state.q = q;
    try {
        const parsed = await api.parseQuery(q);
        state.query = parsed.query || parsed;
    } catch {
        state.query = state.query || null;
    }
    renderTokenStrip(app);
}

function commitQuery(q, options) {
    const next = String(q ?? "");
    if (options && options.echo) setSearchValue(next);
    state.q = next;
    state.cursor = 0;
    state.selection.clear();
    if (state.view !== "index") {
        state.view = "index";
        navigate(buildHash());
    }
    syncHash();
    runSearch();
}

async function runSearch() {
    state.searching = true;
    state.searchError = null;
    render();
    try {
        const data = await api.search({
            q: state.q,
            sort: state.sort,
            dir: state.sort ? state.dir : null,
            limit: SEARCH_LIMIT
        });
        state.rows = data.rows || [];
        state.facets = data.facets || {};
        state.query = data.query || null;
        state.total = data.total ?? state.rows.length;
        state.nodeCount = data.nodes ?? 0;
        state.sortApplied = data.sort || state.sortApplied;
        state.dirApplied = data.dir || state.dirApplied;
        state.cursor = Math.min(state.cursor, Math.max(state.rows.length - 1, 0));
        state.cursorArtifact = null;
    } catch (error) {
        state.searchError = error;
        state.rows = [];
        state.facets = {};
        state.query = { raw: state.q, tokens: [], warnings: [] };
        state.total = 0;
        state.nodeCount = 0;
    }
    state.searching = false;
    render();
    scheduleInspector();
}

function tokenRaws() {
    return ((state.query && state.query.tokens) || []).map((token) => token.raw);
}

function toggleToken(token) {
    const raws = tokenRaws();
    const next = raws.includes(token)
        ? raws.filter((raw) => raw !== token)
        : raws.concat(token);
    commitQuery(next.join(" "), { echo: true });
}

function removeToken(raw) {
    commitQuery(tokenRaws().filter((item) => item !== raw).join(" "), { echo: true });
}

function sortBy(key) {
    state.dir = state.sort === key && state.dir === "asc" ? "desc" : "asc";
    state.sort = key;
    state.cursor = 0;
    syncHash();
    runSearch();
}

function cursorRow() {
    return state.rows[Math.min(state.cursor, state.rows.length - 1)] || null;
}

function scheduleInspector() {
    clearTimeout(inspectorTimer);
    const row = cursorRow();
    if (!row || !row.artifact_id) return;
    inspectorTimer = setTimeout(async () => {
        try {
            const detail = await api.artifact(row.artifact_id);
            if (cursorRow() && cursorRow().artifact_id === row.artifact_id) {
                state.cursorArtifact = detail;
                paintInspector();
            }
        } catch {
            state.cursorArtifact = null;
        }
    }, INSPECTOR_DELAY);
}

function paintInspector() {
    const slot = document.getElementById("inspector-slot");
    if (slot) mount(slot, renderInspector(app));
}

function paintCursor() {
    const body = document.getElementById("result-body");
    if (!body) return;
    body.querySelectorAll("[data-index]").forEach((node) => {
        const index = Number(node.dataset.index);
        const row = state.rows[index];
        node.classList.toggle("is-cursor", index === state.cursor);
        node.classList.toggle("is-selected", !!row && state.selection.has(row.artifact_id));
    });
    revealCursor(body, state.cursor);
    paintInspector();
    paintFooter();
}

function setCursor(index) {
    state.cursor = Math.max(0, Math.min(index, state.rows.length - 1));
    state.cursorArtifact = null;
    paintCursor();
    scheduleInspector();
}

function moveCursor(delta) {
    if (!state.rows.length) return;
    setCursor(state.cursor + delta);
}

function toggleSelection() {
    const row = cursorRow();
    if (!row) return;
    if (state.selection.has(row.artifact_id)) state.selection.delete(row.artifact_id);
    else state.selection.add(row.artifact_id);
    paintCursor();
}

function repoPathOf(row) {
    const parts = String((row && row.repo) || "")
        .split("/")
        .filter(Boolean);
    const [type, org, name] =
        parts.length >= 3
            ? [parts[0], parts[1], parts.slice(2).join("/")]
            : parts.length === 2
              ? [parts[0], "", parts[1]]
              : [repoTypeOf(row), row.org || "", row.name || ""];
    return [type, org, name].map(encodeURIComponent).join("/");
}

function openRow(row) {
    if (!row) return;
    const params = new URLSearchParams({ tab: "FILES" });
    if (row.artifact_id) params.set("aid", row.artifact_id);
    navigate(`#/repo/${repoPathOf(row)}?${params.toString()}`);
}

async function openArtifact(item) {
    if (!item) return;
    if (item.org && item.name) {
        openRow(item);
        return;
    }
    if (!item.artifact_id) return;
    try {
        openRow(await api.artifact(item.artifact_id));
    } catch (error) {
        toast(`cannot open artifact · ${error.message}`);
    }
}

function detailArtifact() {
    const detail = state.detail;
    return { artifact_id: detail.aid || (detail.repo && detail.repo.artifact_id) };
}

async function addToLoadout(row) {
    if (!row || !row.artifact_id) {
        toast("nothing under the cursor to pin");
        return;
    }
    if (!state.loadoutId) {
        try {
            state.loadoutCreating = state.loadoutCreating || api.createLoadout(null);
            const created = await state.loadoutCreating;
            state.loadoutId = created.id;
            updateSettings({ loadoutId: created.id });
        } catch (error) {
            toast(`cannot start a loadout · ${error.message}`);
            return;
        } finally {
            state.loadoutCreating = null;
        }
    }
    try {
        const answer = await api.addLoadoutItem(state.loadoutId, row.artifact_id);
        state.loadoutId = answer.id;
        updateSettings({ loadoutId: answer.id });
        state.loadoutApplied = false;
        toast(answer.notice);
        await loadLoadout(app);
    } catch (error) {
        toast(`pin failed · ${error.message}`);
    }
}

function switchLoadout(id) {
    if (!id) return;
    state.loadoutId = id;
    state.loadoutDeleting = null;
    state.loadout = null;
    updateSettings({ loadoutId: id });
    loadLoadout(app);
}

async function newLoadout(name) {
    try {
        const created = await api.createLoadout(name || null);
        toast(`new loadout "${created.name}"`);
        switchLoadout(created.id);
    } catch (error) {
        toast(`cannot start a loadout · ${error.message}`);
    }
}

async function renameLoadout(name) {
    if (!state.loadoutId) return;
    try {
        const view = await api.renameLoadout(state.loadoutId, name);
        toast(`renamed to ${view.name}`);
    } catch (error) {
        toast(`rename failed · ${error.message}`);
    }
    await loadLoadout(app);
}

function armDelete(id) {
    state.loadoutDeleting = id;
    render();
}

async function deleteLoadout(id) {
    if (state.loadoutDeleting !== id) return;
    const row = (state.loadouts || []).find((entry) => entry.id === id);
    try {
        await api.deleteLoadout(id);
        toast(`removed loadout ${row ? row.name : id}`);
    } catch (error) {
        toast(`remove failed · ${error.message}`);
    }
    state.loadoutDeleting = null;
    if (id === state.loadoutId) {
        state.loadoutId = null;
        state.loadout = null;
        updateSettings({ loadoutId: null });
    }
    await loadLoadout(app);
}

async function removeFromLoadout(item) {
    if (!state.loadoutId || !item.artifact_id) return;
    try {
        await api.removeLoadoutItem(state.loadoutId, item.artifact_id);
        state.loadoutApplied = false;
        await loadLoadout(app);
    } catch (error) {
        toast(`remove failed · ${error.message}`);
    }
}

async function yank(row) {
    if (!row || !row.artifact_id) return;
    try {
        const command = await api.pullCommand(row.artifact_id);
        await copyCommand(command.hf);
    } catch (error) {
        toast(`yank failed · ${error.message}`);
    }
}

async function copyCommand(text) {
    if (!text) return;
    const ok = await copyText(text);
    toast(ok ? `copied: ${text}` : `copy blocked by the browser · ${text}`);
}

function openDrawer() {
    state.drawerOpen = true;
    render();
    if (!state.loadout) loadLoadout(app);
}

function closeDrawer() {
    state.drawerOpen = false;
    state.loadoutDeleting = null;
    render();
}

function setDetailTab(tab) {
    if (!TABS.includes(tab) || state.detail.tab === tab) return;
    state.detail.tab = tab;
    syncHash();
    render();
    loadTab(app);
}

function cycleDetailTab(step) {
    const index = TABS.indexOf(state.detail.tab);
    setDetailTab(TABS[(index + step + TABS.length) % TABS.length]);
}

function checkoutRef(ref) {
    state.detail.rev = ref.name;
    state.detail.tab = "FILES";
    state.detail.loaded = {};
    state.detail.errors = {};
    state.detail.files = null;
    state.detail.readme = null;
    state.detail.compat = null;
    if (state.detail.repo && state.detail.repo.refs) {
        state.detail.repo.refs.forEach((item) => {
            item.current = item.name === ref.name;
        });
    }
    syncHash();
    render();
    loadTab(app);
    toast(`checked out ${ref.name} · ${String(ref.sha || "").slice(0, 7)}`);
}

function reloadDetail() {
    state.detail.loaded = {};
    state.detail.errors = {};
    loadDetail(app);
}

const TAB_SLOT = { FILES: "files", CARD: "readme", COMPAT: "compat", USAGE: "usage" };

function reloadDetailTab() {
    const slot = TAB_SLOT[state.detail.tab];
    if (slot) {
        delete state.detail.loaded[slot];
        delete state.detail.errors[slot];
        state.detail[slot] = null;
    }
    loadTab(app);
}

function goIndex() {
    navigate(buildIndexHash());
}

function buildIndexHash() {
    const params = new URLSearchParams();
    if (state.q) params.set("q", state.q);
    if (state.viewMode !== "TABLE") params.set("view", state.viewMode);
    if (state.sort) {
        params.set("sort", state.sort);
        params.set("dir", state.dir);
    }
    const text = params.toString();
    return text ? `#/index?${text}` : "#/index";
}

function showKeySheet() {
    state.keySheet = true;
    renderKeySheet(app);
}

function hideKeySheet() {
    state.keySheet = false;
    renderKeySheet(app);
}

function setViewMode(mode) {
    if (state.viewMode === mode) return;
    state.viewMode = mode;
    syncHash();
    render();
}

function paintNav() {
    const host = document.getElementById("nav-tabs");
    mount(
        host,
        NAV.map((tab) =>
            el("span", {
                class: `nav-tab${routeMatches(tab.route) ? " is-active" : ""}`,
                text: tab.label,
                onClick: () => navigate(tab.route === "/index" ? buildIndexHash() : `#${tab.route}`)
            })
        )
    );
}

function routeMatches(route) {
    if (route === "/index") return state.view === "index" || state.view === "detail";
    return `/${state.view}` === route;
}

function paintLoadoutButton() {
    const button = document.getElementById("loadout-btn");
    button.textContent = `LOADOUT ${loadoutItems(app).length}`;
}

function paintFooter() {
    const foot = document.getElementById("results-foot");
    if (!foot) return;
    const hints = [
        ["J/K", "move"],
        ["↵", "open"],
        ["SPC", "select"],
        ["L", "loadout"],
        ["Y", "yank"],
        ["/", "search"],
        ["?", "keys"]
    ];
    mount(
        foot,
        hints.map(([key, what]) =>
            el("span", { class: "hint" }, el("span", { class: "hint-key", text: key }), ` ${what}`)
        ),
        el("span", {
            class: "foot-right",
            text: state.selection.size
                ? `${state.selection.size} selected`
                : "double-click a row to open it"
        })
    );
}

function paintIndex() {
    mount(document.getElementById("facet-rail"), renderFacets(app), renderShelfRail(app));

    mount(
        document.getElementById("results-head"),
        el("span", { class: "results-line", text: `${resultLine()} · ${sortLabel()}` }),
        el(
            "span",
            { class: "view-tabs" },
            ["TABLE", "CARDS"].map((mode) =>
                el("span", {
                    class: `view-tab${state.viewMode === mode ? " is-active" : ""}`,
                    text: mode,
                    onClick: () => setViewMode(mode)
                })
            )
        )
    );

    const body = document.getElementById("result-body");
    const scroller = body.firstElementChild;
    const offset = scroller ? scroller.scrollTop : 0;
    if (state.searchError) mount(body, errorPanel(state.searchError, () => runSearch()));
    else if (state.searching && !state.rows.length) mount(body, loadingPanel("searching the index…"));
    else if (state.viewMode === "CARDS") mount(body, renderCards(app));
    else mount(body, renderTable(app, exampleQuery()));
    const next = body.firstElementChild;
    if (next && offset) next.scrollTop = offset;

    paintFooter();
    paintInspector();
}

function render() {
    paintNav();
    paintLoadoutButton();
    renderNodePulse(app);
    renderTokenStrip(app);

    const index = document.getElementById("view-index");
    const detail = document.getElementById("view-detail");
    const panel = document.getElementById("view-panel");
    index.hidden = state.view !== "index";
    detail.hidden = state.view !== "detail";
    panel.hidden = ["index", "detail"].includes(state.view);

    if (state.view === "index") paintIndex();
    else if (state.view === "detail") mount(detail, renderDetail(app));
    else if (state.view === "shelves") mount(panel, renderShelvesView(app));
    else if (state.view === "nodes") mount(panel, renderNodesView(app));
    else if (state.view === "scans") mount(panel, renderScansView(app));
    else if (state.view === "jobs") mount(panel, renderJobsView(app));

    renderDrawer(app);
    renderKeySheet(app);
    paintPrefs();
}

function paintPrefs() {
    const host = document.getElementById("prefs-panel");
    host.hidden = !state.prefsOpen;
    if (!state.prefsOpen) {
        host.replaceChildren();
        return;
    }
    const choice = (label, options, current, onPick) =>
        el(
            "div",
            { class: "prefs-row" },
            el("span", { class: "prefs-label", text: label }),
            el(
                "span",
                { class: "prefs-options" },
                options.map((option) =>
                    el("span", {
                        class: `prefs-option${current === option ? " is-active" : ""}`,
                        text: String(option).toUpperCase(),
                        onClick: () => onPick(option)
                    })
                )
            )
        );
    mount(
        host,
        choice("THEME", ["system", "dark", "light"], settings.theme, (value) => {
            updateSettings({ theme: value });
            paintPrefs();
            paintThemeButton();
        }),
        choice("DENSITY", ["Condensed", "Roomy"], settings.density, (value) => {
            updateSettings({ density: value });
            paintPrefs();
        }),
        choice("ROLE COLOR", ["Color-coded", "Monochrome"], settings.roleColorMode, (value) => {
            updateSettings({ roleColorMode: value });
            paintPrefs();
        }),
        choice("INSPECTOR", ["on", "off"], settings.showInspector ? "on" : "off", (value) => {
            updateSettings({ showInspector: value === "on" });
            paintPrefs();
        })
    );
}

function paintThemeButton() {
    document.getElementById("theme-btn").textContent = themeLabel();
}

function initTopbar() {
    document.getElementById("wordmark").addEventListener("click", () => goIndex());
    document.getElementById("loadout-btn").addEventListener("click", () => {
        if (state.drawerOpen) closeDrawer();
        else openDrawer();
    });
    document.getElementById("node-pulse").addEventListener("click", () => navigate("#/nodes"));
    document.getElementById("theme-btn").addEventListener("click", () => {
        cycleTheme();
        paintThemeButton();
        paintPrefs();
    });
    document.getElementById("prefs-btn").addEventListener("click", (event) => {
        event.stopPropagation();
        state.prefsOpen = !state.prefsOpen;
        paintPrefs();
    });
    document.addEventListener("click", (event) => {
        if (!state.prefsOpen) return;
        if (document.getElementById("prefs-panel").contains(event.target)) return;
        state.prefsOpen = false;
        paintPrefs();
    });
    paintThemeButton();
}

function start() {
    applySettings();
    initTopbar();
    initSearch(app);
    initKeys(app);
    window.addEventListener("hashchange", () => {
        if (suppressHashChange) return;
        readHash();
    });
    render();
    readHash();
    loadShelves(app);
    loadNodes(app);
    loadLoadout(app);
}

start();
