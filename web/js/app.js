import * as api from "./api.js";
import { renderCards } from "./cards.js";
import { renderDetail, TABS, loadDetail, loadTab } from "./detail.js";
import { el, errorPanel, loadingPanel, mount, toast, copyText } from "./dom.js";
import { renderFacets } from "./facets.js";
import { renderInspector } from "./inspector.js";
import { initKeys, renderKeySheet } from "./keys.js";
import { addToCollection, loadLoadout, renderDrawer } from "./loadout.js";
import { loadNodes, renderNodePulse } from "./nodes.js";
import { loadJobs, loadSources } from "./jobs.js";
import { paintSourcesView } from "./sources.js";
import { loadProject, renderProject } from "./projects.js";
import { initSearch, renderTokenStrip, setSearchValue } from "./search.js";
import { initShelfActions, loadShelves, openShelfMenu } from "./shelves.js";
import { renderTable, revealCursor } from "./table.js";
import { applySettings, settings, updateSettings } from "./theme.js";
import { loadSetup, renderCatalogEmpty } from "./setup.js";

const NAV = [
    { label: "INDEX", route: "/index" },
    { label: "SOURCES", route: "/sources" }
];
const SEARCH_LIMIT = 100;
const INSPECTOR_DELAY = 160;
const FALLBACK_EXAMPLE = "role:lora";

const state = {
    view: "index",
    q: "",
    query: null,
    rows: [],
    projects: { rows: [], total: 0 },
    resourcesNext: null,
    resourcesLoading: false,
    project: null,
    sourceDeleting: null,
    sourceRemovalPending: null,
    jobDeleting: null,
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
    searchedQuery: null,
    cursorArtifact: null,
    classificationReview: null,
    shelves: null,
    shelvesError: null,
    nodes: null,
    nodesError: null,
    sources: null,
    sourcesError: null,
    jobs: null,
    jobsError: null,
    sourceFormOpen: false,
    setup: null,
    setupError: null,
    ownerTokenSet: false,
    recentJobsOpen: false,
    loadoutId: null,
    loadouts: null,
    loadoutDeleting: null,
    loadout: null,
    loadoutError: null,
    loadoutBusy: false,
    loadoutLoading: false,
    loadoutPendingModels: [],
    loadoutIndexDirty: false,
    loadoutTotal: 0,
    loadoutReturnHash: null,
    drawerOpen: false,
    drawerReturnFocus: null,
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
        artifact: null,
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
let refreshingSources = false;
let searchVersion = 0;

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
    openProject,
    openProjectFile,
    selectProjectRevision,
    selectProjectTab,
    browseProjects,
    loadMoreResources,
    rowKey,
    cancelSourceRemoval,
    openSourceForm,
    cancelSourceForm: () => {
        state.sourceFormOpen = false;
        render();
        document.querySelector(".operations-top button")?.focus();
    },
    openSources: () => navigate("#/sources"),
    refreshSetup: () => loadSetup(app),
    collectionChanged: () => {
        state.loadoutIndexDirty = true;
        delete state.detail.loaded.artifact;
        state.detail.artifact = null;
    },
    addToLoadout,
    openCollection,
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
    render,
    paintInspector,
    async refreshIndex(selected) {
        await Promise.all([runSearch(selected), loadShelves(app), loadSetup(app)]);
    }
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
    if (state.loadoutTotal) {
        return `${state.total} ARTIFACTS · ${state.projects.total} GIT PROJECTS · ${state.loadoutTotal} LOADOUTS`;
    }
    return state.projects.total
        ? `${state.total} ARTIFACTS · ${state.projects.total} GIT PROJECTS`
        : `${state.total} ARTIFACTS · ${state.nodeCount} NODES`;
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
    if (state.view === "project") {
        const project = state.project;
        const params = new URLSearchParams();
        if (project.revision) params.set("rev", project.revision);
        if (project.path) params.set("path", project.path);
        if (project.tab !== "README") params.set("tab", project.tab);
        const query = params.toString();
        return `#/project/${encodeURIComponent(project.id)}${query ? `?${query}` : ""}`;
    }
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

function openSourceForm() {
    state.sourceFormOpen = true;
    if (state.view !== "sources") navigate("#/sources?add=1");
    else {
        render();
        document.getElementById("source-path")?.focus();
    }
}

function readHash() {
    const raw = location.hash.replace(/^#/, "") || "/index";
    const [path, search] = raw.split("?");
    const params = new URLSearchParams(search || "");
    const parts = path.split("/").filter(Boolean);
    const head = parts[0] || "index";
    if (head === "loadout" && parts[1]) {
        state.view = "index";
        state.loadoutReturnHash = buildIndexHash();
        if (!state.rows.length && !state.searching) runSearch();
        openCollection(decodeURIComponent(parts[1]));
        return;
    }
    if (state.drawerOpen && state.loadoutReturnHash) {
        state.drawerOpen = false;
        state.loadoutReturnHash = null;
    }
    if (head === "project" && parts[1]) {
        const id = decodeURIComponent(parts[1]);
        const revision = params.get("rev") || "";
        const path = params.get("path") || "";
        const requested = params.get("tab");
        const tab = ["README", "FILES", "REFS"].includes(requested) ? requested : path ? "FILES" : "README";
        if (state.project?.id !== id || state.project.revision !== revision) {
            state.project = { id, revision, path, tab };
        } else {
            state.project.path = path;
            state.project.tab = tab;
        }
        state.view = "project";
        render();
        loadProject(app);
        return;
    }
    if (!["sources", "nodes", "scans", "jobs"].includes(head)) state.sourceFormOpen = false;
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

    if (head === "shelves") {
        state.view = "index";
        syncHash();
        render();
        openShelfMenu(app);
        if (!state.rows.length && !state.searching) runSearch();
        return;
    }

    if (["sources", "nodes", "scans", "jobs"].includes(head)) {
        state.view = "sources";
        if (params.get("add") === "1") state.sourceFormOpen = true;
        const source = params.get("source");
        if (source) {
            commitQuery(`source:${JSON.stringify(source)}`, { echo: true });
            return;
        }
        if (head !== "sources") syncHash();
        render();
        if (state.sourceFormOpen) document.getElementById("source-path")?.focus();
        loadSetup(app);
        if (!state.nodes) loadNodes(app);
        if (!state.sources) loadSources(app);
        loadJobs(app);
        return;
    }

    const q = params.get("q") || "";
    const mode = params.get("view") === "CARDS" ? "CARDS" : "TABLE";
    const sort = params.get("sort");
    const dir = params.get("dir") === "desc" ? "desc" : "asc";
    const changed = q !== state.q || sort !== state.sort || dir !== state.dir
        || state.searchedQuery !== q;
    state.view = "index";
    state.viewMode = mode;
    state.q = q;
    state.sort = sort;
    state.dir = dir;
    setSearchValue(q);
    if (changed || (!state.searching && !state.rows.length && !state.projects.rows.length)) runSearch();
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

async function runSearch(selected) {
    const query = state.q;
    const version = ++searchVersion;
    state.searching = true;
    state.searchError = null;
    render();
    try {
        const wanted = selected ? Math.max(SEARCH_LIMIT, state.rows.length) : SEARCH_LIMIT;
        const params = {
            q: query,
            sort: state.sort,
            dir: state.sort ? state.dir : null,
            limit: Math.min(1000, wanted)
        };
        const data = await api.search(params);
        let rows = data.resources || data.rows || [];
        let next = data.resource_next_cursor;
        while (rows.length < wanted && next) {
            if (version !== searchVersion) return;
            const more = await api.search({ ...params, resource_cursor: next,
                limit: Math.min(1000, wanted - rows.length) });
            rows = rows.concat(more.resources || []);
            next = more.resource_next_cursor;
        }
        data.resources = rows;
        data.resource_next_cursor = next;
        if (version !== searchVersion) return;
        state.loadoutTotal = data.loadout_total || 0;
        state.rows = data.resources || data.rows || [];
        state.resourcesNext = data.resource_next_cursor || null;
        state.projects = data.projects || { rows: [], total: 0 };
        state.facets = data.facets || {};
        state.query = data.query || null;
        state.total = data.total ?? state.rows.length;
        state.nodeCount = data.nodes ?? 0;
        state.sortApplied = data.sort || state.sortApplied;
        state.dirApplied = data.dir || state.dirApplied;
        state.cursor = Math.min(state.cursor, Math.max(state.rows.length - 1, 0));
        if (selected) {
            const kept = state.rows.findIndex((row) => rowKey(row) === selected);
            if (kept >= 0) state.cursor = kept;
        }
        state.cursorArtifact = null;
        state.searchedQuery = query;
    } catch (error) {
        if (version !== searchVersion) return;
        state.searchError = error;
        state.searchedQuery = null;
        state.rows = [];
        state.resourcesNext = null;
        state.projects = { rows: [], total: 0 };
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

function rowKey(row) {
    return row && (row.artifact_id || row.project_id || row.loadout_id);
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
    if (!slot) return;
    const key = rowKey(cursorRow());
    const same = slot.dataset.resource === key;
    const reviewPanel = slot.querySelector(".classification-review");
    if (same && reviewPanel && state.classificationReview?.id === key) {
        state.classificationReview.open = reviewPanel.open;
        state.classificationReview.evidenceOpen =
            slot.querySelector(".classification-evidence")?.open || false;
    }
    const scroll = same ? slot.querySelector(".inspector")?.scrollTop || 0 : 0;
    const focused = same && slot.contains(document.activeElement) ? document.activeElement : null;
    const id = focused?.id;
    const selection = focused?.tagName === "INPUT"
        ? [focused.selectionStart, focused.selectionEnd] : null;
    mount(slot, renderInspector(app));
    slot.dataset.resource = key || "";
    const inspector = slot.querySelector(".inspector");
    if (inspector) inspector.scrollTop = scroll;
    if (id) {
        const field = document.getElementById(id);
        field?.focus({ preventScroll: true });
        if (selection) field?.setSelectionRange(...selection);
    }
}

function paintCursor() {
    const body = document.getElementById("result-body");
    if (!body) return;
    body.querySelectorAll("[data-index]").forEach((node) => {
        const index = Number(node.dataset.index);
        const row = state.rows[index];
        node.classList.toggle("is-cursor", index === state.cursor);
        node.classList.toggle("is-selected", !!row && state.selection.has(rowKey(row)));
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
    if (state.selection.has(rowKey(row))) state.selection.delete(rowKey(row));
    else state.selection.add(rowKey(row));
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
    return [type, org || "-", name].map(encodeURIComponent).join("/");
}

function openProject(id, revision = "") {
    const query = revision ? `?${new URLSearchParams({ rev: revision })}` : "";
    navigate(`#/project/${encodeURIComponent(id)}${query}`);
}

function openProjectFile(path) {
    state.project.path = path;
    state.project.tab = "FILES";
    syncHash();
    render();
    loadProject(app);
}

function selectProjectRevision(revision) {
    state.project.revision = revision;
    state.project.tab = state.project.path ? "FILES" : "README";
    syncHash();
    loadProject(app);
    render();
}

function selectProjectTab(tab) {
    state.project.tab = tab;
    if (tab === "README") state.project.path = "";
    syncHash();
    render();
    loadProject(app);
}

function browseProjects(source) {
    commitQuery(source.browse_query, { echo: true });
}

async function loadMoreResources() {
    if (state.resourcesLoading || !state.resourcesNext) return;
    const query = state.q;
    const version = searchVersion;
    state.resourcesLoading = true;
    render();
    try {
        const next = await api.search({ q: query, sort: state.sort,
            dir: state.sort ? state.dir : null, resource_cursor: state.resourcesNext, limit: SEARCH_LIMIT });
        if (version !== searchVersion) return;
        state.rows = [...state.rows, ...next.resources];
        state.resourcesNext = next.resource_next_cursor;
    } catch (error) {
        toast(`results could not be loaded · ${error.message}`);
    } finally {
        state.resourcesLoading = false;
        render();
    }
}

function cancelSourceRemoval() {
    if (state.sourceRemovalPending) return;
    const name = state.sourceDeleting;
    state.sourceDeleting = null;
    render();
    document.querySelectorAll('button[data-action="remove"]').forEach(button => {
        if (button.dataset.source === name) button.focus();
    });
}

function openRow(row) {
    if (!row) return;
    if (row.loadout_id) return openCollection(row.loadout_id);
    if (row.project_id) return openProject(row.project_id);
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

function addToLoadout(row) {
    addToCollection(app, row);
}

function openCollection(id) {
    if (state.loadoutBusy) { toast("Wait for the loadout to finish saving."); return; }
    if (state.loadoutId !== id) {
        state.loadoutId = id;
        state.loadout = null;
    }
    if (!state.drawerOpen) openDrawer();
    else loadLoadout(app, id);
    if (!state.loadoutReturnHash) state.loadoutReturnHash = buildHash();
    history.replaceState(null, "", `#/loadout/${encodeURIComponent(id)}`);
}

async function yank(row) {
    if (row?.loadout_id) return openCollection(row.loadout_id);
    if (row?.project_id) return copyCommand(row.clone_command);
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
    if (state.drawerOpen) { renderDrawer(app); return; }
    const active = document.activeElement;
    state.drawerReturnFocus = {
        id: active?.id || "loadout-btn",
        selection: typeof active?.selectionStart === "number"
            ? [active.selectionStart, active.selectionEnd] : null
    };
    state.drawerOpen = true;
    render();
    loadLoadout(app);

    document.querySelector(".drawer .btn-close")?.focus();
}

function closeDrawer() {
    const previous = state.drawerReturnFocus;
    state.drawerOpen = false;
    state.loadoutDeleting = null;
    state.loadoutPendingModels = [];
    if (state.loadoutReturnHash) {
        history.replaceState(null, "", state.loadoutReturnHash);
        state.loadoutReturnHash = null;
    }
    render();
    if (state.loadoutIndexDirty) {
        state.loadoutIndexDirty = false;
        runSearch(rowKey(cursorRow())).then(() => {
            if (!state.drawerOpen && document.activeElement === document.body) {
                (document.getElementById(previous?.id) || document.getElementById("loadout-btn"))
                    .focus({ preventScroll: true });
            }
        });
        loadShelves(app);
        if (state.view === "detail" && state.detail.aid) {
            delete state.detail.loaded.artifact;
            loadDetail(app);
        }
    }
    const target = document.getElementById(previous?.id) || document.getElementById("loadout-btn");
    target.focus({ preventScroll: true });
    if (previous?.selection && target.setSelectionRange) {
        target.setSelectionRange(...previous.selection);
    }
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
    if (route === "/index") return ["index", "detail", "project"].includes(state.view);
    return `/${state.view}` === route;
}

function paintLoadoutButton() {
    const button = document.getElementById("loadout-btn");
    button.textContent = "LOADOUTS";
}

function paintFooter() {
    const foot = document.getElementById("results-foot");
    if (!foot) return;
    const hints = [
        ["J/K", "move"],
        ["↵", "open"],
        ["SPC", "select"],
        ...(cursorRow()?.artifact_id ? [["L", "add to loadout"]] : []),
        ["Y", cursorRow()?.loadout_id ? "download" : cursorRow()?.project_id ? "clone" : "yank"],
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
    mount(document.getElementById("facet-rail"), renderFacets(app));

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
    else if (!state.rows.length) mount(body, renderCatalogEmpty(app));
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
    panel.classList.toggle("project-host", state.view === "project");
    index.hidden = state.view !== "index";
    detail.hidden = state.view !== "detail";
    panel.hidden = ["index", "detail"].includes(state.view);

    if (state.view === "index") paintIndex();
    else if (state.view === "detail") mount(detail, renderDetail(app));
    else if (state.view === "sources") paintSourcesView(app, panel);
    else if (state.view === "project") paintProject(panel);

    renderDrawer(app);
    renderKeySheet(app);
    paintPrefs();
}

function paintProject(panel) {
    const project = state.project;
    if (!project) {
        mount(panel, el("div", { class: "state-panel" },
            el("div", { class: "state-panel-title", text: "PROJECT NO LONGER INDEXED" }),
            el("div", { class: "state-panel-body", text: "Its source was removed. The files remain on disk." }),
            el("button", { class: "btn", type: "button", text: "BACK TO INDEX", onClick: () => goIndex() })));
        return;
    }
    const key = `${project.id}@${project.revision}`;
    const same = panel.dataset.projectKey === key;
    const scroll = (selector) => panel.querySelector(selector)?.scrollTop || 0;
    if (same && panel.dataset.projectTab === "FILES" && !panel.dataset.projectPath) {
        project.fileScroll = scroll(".project-content");
    }
    const previewOffset = same && panel.dataset.projectPath === project.path
        ? scroll(".project-source") : 0;
    const contentOffset = same && panel.dataset.projectTab === project.tab
        ? scroll(".project-content") : 0;
    const focused = panel.contains(document.activeElement)
        ? document.activeElement.closest(".project-file")?.getAttribute("title") : null;
    mount(panel, renderProject(app));
    const restore = (selector, offset) => {
        const node = panel.querySelector(selector);
        if (node) node.scrollTop = offset;
    };
    restore(".project-source", previewOffset);
    restore(".project-content", project.tab === "FILES" && !project.path
        ? project.fileScroll || 0 : contentOffset);
    if (focused) {
        const file = [...panel.querySelectorAll(".project-file")].find(
            (node) => node.getAttribute("title") === focused);
        file?.focus({ preventScroll: true });
    }
    panel.dataset.projectKey = key;
    panel.dataset.projectPath = project.path;
    panel.dataset.projectTab = project.tab;
}

function paintPrefs() {
    const host = document.getElementById("prefs-panel");
    document.getElementById("prefs-btn").setAttribute("aria-expanded", String(state.prefsOpen));
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

function initTopbar() {
    document.getElementById("wordmark").addEventListener("click", () => goIndex());
    document.getElementById("loadout-btn").addEventListener("click", () => {
        if (state.drawerOpen) closeDrawer();
        else openDrawer();
    });
    document.getElementById("node-pulse").addEventListener("click", () => navigate("#/sources"));
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
}

async function start() {
    applySettings();
    initTopbar();
    initSearch(app);
    initKeys(app);
    initShelfActions(app);
    window.addEventListener("hashchange", () => {
        if (suppressHashChange) return;
        readHash();
    });
    const initialHash = location.hash;
    const bareArrival = !initialHash || initialHash === "#/" || initialHash === "#";
    state.searching = bareArrival;
    render();
    if (bareArrival) await loadSetup(app, false);
    else if (!initialHash.startsWith("#/sources")) loadSetup(app);
    state.searching = false;
    if (bareArrival && location.hash === initialHash && state.setup?.first_run) {
        history.replaceState(null, "", "#/sources");
    }
    readHash();
    loadShelves(app);
    loadNodes(app);
    loadLoadout(app);
    setInterval(async () => {
        const waitingForResources = state.view === "index" && !state.rows.length
            && state.setup?.resource_count === 0 && state.setup.configured_sources > 0;
        if ((state.view !== "sources" && !waitingForResources) || refreshingSources) return;
        refreshingSources = true;
        try {
            if (waitingForResources) {
                await loadSetup(app, false);
                if (state.view === "index") await runSearch();
                return;
            }
            await Promise.all([
                loadSources(app, false),
                loadJobs(app, false),
                loadNodes(app, false),
                loadSetup(app, false)
            ]);
            if (state.view === "sources") {
                paintSourcesView(app, document.getElementById("view-panel"));
            }
        } finally {
            refreshingSources = false;
        }
    }, 5000);
}

start();
