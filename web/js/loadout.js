import * as api from "./api.js";
import { el, mount, toast, copyText, fmtBytes } from "./dom.js";
import { roleClass, roleCode } from "./table.js";
import { renderOwnerAccess } from "./setup.js";
import { settings, updateSettings } from "./theme.js";

let readVersion = 0;
let scriptVersion = 0;
let scriptTimer = null;
let scriptKey = "";
let scriptResult = null;
let scriptError = null;
let scriptLoading = false;

function loadoutItems(app) {
    return app.state.loadout?.items || [];
}

export async function loadLoadout(app, wanted = app.state.loadoutId || settings.loadoutId) {
    const version = ++readVersion;
    app.state.loadoutLoading = true;
    app.state.loadoutError = null;
    renderDrawer(app);
    try {
        const rows = await api.loadouts();
        const current = rows.find(row => row.id === wanted) || (wanted ? null : rows[0]) || null;
        const doc = current ? await api.loadout(current.id) : null;
        if (version !== readVersion) return;
        app.state.loadouts = rows;
        app.state.loadoutId = current?.id || null;
        app.state.loadout = doc;
        if (wanted && !current) app.state.loadoutError = "This loadout is no longer available. Choose another collection or create one.";
        updateSettings({ loadoutId: app.state.loadoutId });
    } catch (error) {
        if (version !== readVersion) return;
        app.state.loadoutError = error.message;
    }
    if (version !== readVersion) return;
    app.state.loadoutLoading = false;
    app.render();
}

function chooseCollection(app, id) {
    if (app.state.loadoutBusy || id === app.state.loadoutId) return;
    app.state.loadoutId = id || null;
    app.state.loadout = null;
    app.state.loadoutDeleting = null;
    updateSettings({ loadoutId: app.state.loadoutId });
    loadLoadout(app, id);
}

export function addToCollection(app, row) {
    if (!row?.artifact_id) {
        toast("Select models to add to a loadout.");
        return;
    }
    const selected = app.state.selection.has(app.rowKey(row))
        ? app.state.rows.filter(item => item.artifact_id && app.state.selection.has(app.rowKey(item)))
        : [row];
    app.state.loadoutPendingModels = [...new Set(selected.map(item => item.artifact_id))];
    app.openDrawer();
}

async function mutate(app, work) {
    if (app.state.loadoutBusy) return;
    const focused = document.activeElement;
    ++readVersion;
    app.state.loadoutLoading = false;
    app.state.loadoutBusy = true;
    app.state.loadoutError = null;
    app.state.loadoutAuthRequired = false;
    renderDrawer(app);
    try {
        await work();
        updateSettings({ loadoutId: app.state.loadoutId });
    } catch (error) {
        app.state.loadoutError = error.message;
        app.state.loadoutAuthRequired = error.status === 401 || error.status === 403;
    }
    try {
        app.state.loadouts = await api.loadouts();
    } catch (error) {
        app.state.loadoutError ||= error.message;
    }
    app.collectionChanged();
    app.state.loadoutBusy = false;
    app.render();
    if (document.activeElement === document.body && focused?.isConnected && !focused.disabled && focused.getClientRects().length) {
        focused.focus({ preventScroll: true });
    }
}

async function addPending(app) {
    const ids = app.state.loadoutPendingModels || [];
    if (!app.state.loadoutId || !ids.length) return;
    await mutate(app, async () => {
        app.state.loadout = await api.addLoadoutMembers(app.state.loadoutId, ids);
        app.state.loadoutPendingModels = [];
        toast("Models added to loadout");
    });
}

async function createCollection(app, field) {
    const name = field.value.trim();
    if (!name) {
        app.state.loadoutError = "Enter a name for this loadout.";
        renderDrawer(app);
        field.focus();
        return;
    }
    await mutate(app, async () => {
        const created = await api.createLoadout(name);
        app.state.loadoutId = created.id;
        app.state.loadout = created;
        if (field.value.trim() === name) field.value = "";
        const ids = app.state.loadoutPendingModels || [];
        if (ids.length) {
            app.state.loadout = await api.addLoadoutMembers(created.id, ids);
            app.state.loadoutPendingModels = [];
        }
        toast("Loadout created");
    });
}

async function renameCollection(app, field) {
    const value = field.value;
    await mutate(app, async () => {
        app.state.loadout = await api.renameLoadout(app.state.loadoutId, value);
        if (field.value === value) delete field.dataset.dirty;
        toast("Loadout name saved");
    });
}

async function savePath(app, item, field) {
    const value = field.value;
    await mutate(app, async () => {
        app.state.loadout = await api.editLoadoutMember(app.state.loadoutId, item, value);
        if (field.value === value) delete field.dataset.dirty;
        toast("Download path saved");
    });
}

async function removeMember(app, item) {
    await mutate(app, async () => {
        app.state.loadout = await api.removeLoadoutMember(app.state.loadoutId, item);
        toast("Model removed from loadout; original file kept");
    });
}

async function deleteCollection(app) {
    await mutate(app, async () => {
        await api.deleteLoadout(app.state.loadoutId);
        app.state.loadout = null;
        app.state.loadoutId = null;
        app.state.loadoutDeleting = null;
        app.state.drawerReturnFocus = { id: "loadout-btn", selection: null };
        if (location.hash.startsWith("#/loadout/")) history.replaceState(null, "", "#/index");
        toast("Loadout deleted; model files kept");
    });
    if (!app.state.loadoutId && !app.state.loadoutError) await loadLoadout(app, null);
}

function setText(node, value) {
    if (node.textContent !== value) node.textContent = value;
}

function action(text, handler, extra = {}) {
    return el("button", { class: "btn loadout-action", type: "button", text,
        onClick: handler, ...extra });
}

function labeledField(label, id, value = "") {
    const field = el("input", { id, class: "field", type: "text", value, spellcheck: "false",
        onInput: () => { field.dataset.dirty = "true"; } });
    return { field, label: el("label", { for: id, class: "rail-title", text: label }) };
}

function createShell(app, root) {
    const name = labeledField("LOADOUT NAME", "loadout-name");
    const fresh = labeledField("NEW LOADOUT", "loadout-new-name");
    fresh.field.placeholder = "Name this collection";
    const create = () => createCollection(app, fresh.field);
    fresh.field.addEventListener("keydown", event => {
        if (event.key === "Enter") { event.preventDefault(); create(); }
    });
    const downloadRoot = labeledField("DESTINATION ON YOUR COMPUTER", "loadout-download-root",
        settings.loadoutDownloadRoot || "./models");
    downloadRoot.field.addEventListener("input", () => {
        updateSettings({ loadoutDownloadRoot: downloadRoot.field.value });
        renderDownload(app, root);
    });
    const format = el("select", { id: "loadout-script-format", class: "field",
        onChange: () => {
            updateSettings({ loadoutScriptFormat: format.value });
            renderDownload(app, root);
        } }, el("option", { value: "powershell", text: "Windows PowerShell" }),
        el("option", { value: "shell", text: "Linux / macOS shell" }));
    format.value = settings.loadoutScriptFormat || "powershell";
    const chooser = el("select", { id: "loadout-chooser", class: "field",
        onChange: () => chooseCollection(app, chooser.value) });
    const confirmDelete = el("div", { class: "loadout-delete-confirm", hidden: true },
        el("p", { class: "loadout-delete-message" }),
        action("DELETE LOADOUT", () => deleteCollection(app), { class: "btn loadout-action btn-danger" }),
        action("CANCEL", () => {
            app.state.loadoutDeleting = null;
            renderDrawer(app);
            document.getElementById("loadout-delete").focus();
        }));
    const shell = el("div", { class: "drawer", onClick: event => event.stopPropagation() },
        el("div", { class: "drawer-head" },
            el("span", { class: "drawer-title", text: "loadouts" }),
            el("button", { id: "loadout-close", class: "btn btn-close", type: "button", text: "CLOSE",
                onClick: () => app.closeDrawer() })),
        el("div", { class: "drawer-body" },
            el("p", { class: "loadout-help", text: "Group models from anywhere in the Index. Download only this collection to your computer." }),
            el("label", { for: "loadout-chooser", class: "rail-title", text: "CURRENT LOADOUT" }), chooser,
            el("div", { class: "loadout-read-status", role: "status", "aria-live": "polite" }),
            el("div", { class: "loadout-error", role: "status", "aria-live": "polite" }),
            el("div", { class: "loadout-owner" }),
            action("RETRY", () => loadLoadout(app), { id: "loadout-read-retry", hidden: true }),
            el("details", { class: "loadout-create", open: !app.state.loadout },
                el("summary", { text: "NEW LOADOUT" }),
                el("div", { class: "drawer-new" }, fresh.label, fresh.field, action("CREATE", create))),
            el("div", { class: "loadout-pending", hidden: true },
                el("p", { class: "loadout-pending-label" }),
                action("ADD TO THIS LOADOUT", () => addPending(app), { id: "loadout-add-pending" }),
                action("CANCEL", () => { app.state.loadoutPendingModels = []; renderDrawer(app); })),
            el("section", { class: "loadout-editor", hidden: true },
                name.label,
                el("div", { class: "loadout-name-form" }, name.field,
                    action("SAVE NAME", () => renameCollection(app, name.field))),
                el("div", { class: "loadout-member-summary rail-title" }),
                el("div", { class: "loadout-members" }),
                action("DELETE LOADOUT", () => {
                    app.state.loadoutDeleting = app.state.loadoutId;
                    renderDrawer(app);
                    root.querySelector(".loadout-delete-confirm button").focus();
                }, { id: "loadout-delete" }), confirmDelete)),
        el("div", { class: "drawer-foot" },
            el("div", { class: "rail-title", text: "DOWNLOAD ON YOUR COMPUTER" }),
            downloadRoot.label, downloadRoot.field,
            el("p", { class: "loadout-help", text: "Paths below are relative to this folder on the client. For ComfyUI, use its models folder. Nothing is copied on the server." }),
            el("label", { for: "loadout-script-format", class: "rail-title", text: "SCRIPT FOR" }), format,
            el("div", { class: "loadout-download-status", role: "status", "aria-live": "polite" }),
            el("details", { class: "loadout-layout", hidden: true },
                el("summary", { text: "PREVIEW FOLDER LAYOUT" }),
                el("pre", { class: "code-box loadout-layout-text" })),
            el("details", { class: "loadout-script-review" },
                el("summary", { text: "REVIEW DOWNLOAD SCRIPT" }),
                el("pre", { class: "code-box loadout-script" })),
            el("div", { class: "loadout-download-actions" },
                action("COPY DOWNLOAD SCRIPT", () => copyScript(app, root), { id: "loadout-copy", class: "btn btn-primary" }),
                action("SAVE SCRIPT", () => saveScript(), { id: "loadout-save-script" }),
                action("RETRY SCRIPT", () => { scriptKey = ""; renderDownload(app, root); },
                    { id: "loadout-retry-script", hidden: true }))));
    mount(root, el("div", { class: "scrim", onClick: () => app.closeDrawer() }, shell));
}

function memberRow(app, item) {
    const path = labeledField("DOWNLOAD PATH", `loadout-path-${encodeURIComponent(item.repo + ":" + item.path)}`);
    const row = el("div", { class: `loadout-member ${roleClass(item.role)}` });
    row.item = item;
    path.field.addEventListener("input", () => {
        if (path.field.value === row.item.destination) delete path.field.dataset.dirty;
        renderDownload(app, document.getElementById("drawer-root"));
    });
    mount(row,
        el("div", { class: "loadout-member-head" }, el("span", { class: "role-mark" }),
            el("span", { class: "loadout-member-name" }),
            action("REMOVE", () => removeMember(app, row.item), { "aria-label": `Remove ${item.name} from loadout` })),
        el("div", { class: "loadout-member-source mono-dim" }),
        path.label, el("div", { class: "loadout-path-form" }, path.field,
            action("SAVE PATH", () => savePath(app, row.item, path.field))));
    return row;
}

function renderMembers(app, root, reset) {
    const container = root.querySelector(".loadout-members");
    if (reset) container.replaceChildren();
    const existing = new Map([...container.children].map(row => [row.dataset.key, row]));
    const items = loadoutItems(app);
    const wanted = new Set();
    for (const item of items) {
        const key = `${item.repo}\n${item.path}`;
        wanted.add(key);
        let row = existing.get(key);
        if (!row) {
            row = memberRow(app, item);
            row.dataset.key = key;
            container.append(row);
        }
        row.item = item;
        setText(row.querySelector(".loadout-member-name"), item.name);
        setText(row.querySelector(".loadout-member-source"),
            `${roleCode(item)} · ${item.repo} · ${item.size_human} · ${item.resolution?.status === "present" ? "available" : "unavailable"}`);
        const field = row.querySelector("input");
        if (!field.dataset.dirty && field.value !== item.destination) field.value = item.destination;
        row.querySelectorAll("button").forEach(button => { button.disabled = app.state.loadoutBusy; });
    }
    for (const [key, row] of existing) if (!wanted.has(key)) row.remove();
    root.querySelector(".loadout-member-summary").textContent = items.length
        ? `${items.length} MODELS · ${fmtBytes(items.reduce((sum, item) => sum + item.size, 0))}`
        : "EMPTY COLLECTION · ADD MODELS FROM THE INDEX";
}

export function renderDrawer(app) {
    const root = document.getElementById("drawer-root");
    if (!app.state.drawerOpen) {
        if (root.childElementCount) root.replaceChildren();
        clearTimeout(scriptTimer);
        return;
    }
    if (!root.querySelector(".drawer")) {
        createShell(app, root);
        root.dataset.loadoutId = "";
        scriptKey = "";
    }
    const owner = root.querySelector(".loadout-owner");
    if (app.state.setup?.write_token_required && !owner.childElementCount) {
        owner.append(el("details", null,
            el("summary", { text: "OWNER ACCESS", dataset: { ownerTitle: "OWNER ACCESS" } }),
            renderOwnerAccess(app)));
    }
    if (app.state.loadoutAuthRequired && owner.firstElementChild) owner.firstElementChild.open = true;
    const doc = app.state.loadout;
    const id = doc?.id || "";
    const reset = root.dataset.loadoutId !== id;
    const name = root.querySelector("#loadout-name");
    if (reset) {
        delete name.dataset.dirty;
        app.state.loadoutDeleting = null;
        if (doc && !root.querySelector("#loadout-new-name").value) root.querySelector(".loadout-create").open = false;
    }
    if (!name.dataset.dirty && name.value !== (doc?.name || "")) name.value = doc?.name || "";
    const chooser = root.querySelector("#loadout-chooser");
    const rows = app.state.loadouts || [];
    const placeholder = rows.length > 0 && !rows.some(row => row.id === app.state.loadoutId);
    const options = JSON.stringify([rows.map(row => [row.id, row.name, row.items]), placeholder]);
    if (chooser.dataset.options !== options) {
        chooser.replaceChildren(...(placeholder ? [el("option", { value: "", text: "Choose a loadout" })] : []), ...(rows.length ? rows.map(row => el("option", {
            value: row.id, text: `${row.name} (${row.items})`
        })) : [el("option", { value: "", text: "No loadouts yet" })]));
        chooser.dataset.options = options;
    }
    if (chooser.value !== (app.state.loadoutId || "")) chooser.value = app.state.loadoutId || "";
    chooser.disabled = app.state.loadoutBusy || app.state.loadoutLoading;
    root.querySelector(".loadout-editor").hidden = !doc;
    root.querySelector(".loadout-read-status").textContent = app.state.loadoutLoading ? "Reading loadout…" : "";
    root.querySelector(".loadout-error").textContent = app.state.loadoutError || "";
    root.querySelector("#loadout-read-retry").hidden = !app.state.loadoutError || !!doc;
    const pending = app.state.loadoutPendingModels?.length || 0;
    root.querySelector(".loadout-pending").hidden = !pending;
    root.querySelector(".loadout-pending-label").textContent = `${pending} selected model${pending === 1 ? "" : "s"}. Choose a loadout, or create one below.`;
    renderMembers(app, root, reset);
    root.dataset.loadoutId = id;
    root.querySelectorAll(".loadout-action").forEach(button => { button.disabled = !!app.state.loadoutBusy; });
    root.querySelector("#loadout-add-pending").disabled = !doc || app.state.loadoutBusy;
    root.querySelector(".loadout-delete-confirm").hidden = app.state.loadoutDeleting !== id;
    root.querySelector(".loadout-delete-message").textContent = `Delete “${doc?.name || ""}”? Model files will stay on disk.`;
    root.querySelector("#loadout-delete").hidden = app.state.loadoutDeleting === id;
    renderDownload(app, root);
}

function renderDownload(app, root) {
    const doc = app.state.loadout;
    const folder = root.querySelector("#loadout-download-root").value;
    const format = root.querySelector("#loadout-script-format").value;
    const next = JSON.stringify([doc?.id, doc?.updated_at, folder, format]);
    if (scriptKey !== next) {
        scriptKey = next;
        scriptResult = null;
        scriptError = null;
        scriptLoading = !!doc?.items.length;
        clearTimeout(scriptTimer);
        const version = ++scriptVersion;
        if (scriptLoading) scriptTimer = setTimeout(async () => {
            try {
                const result = await api.loadoutDownload(doc.id, format, folder);
                if (version !== scriptVersion) return;
                scriptResult = result;
            } catch (error) {
                if (version !== scriptVersion) return;
                scriptError = error.message;
            }
            scriptLoading = false;
            if (app.state.drawerOpen) renderDownload(app, document.getElementById("drawer-root"));
        }, 250);
    }
    const dirty = !!root.querySelector(".loadout-member input[data-dirty]");
    const status = root.querySelector(".loadout-download-status");
    status.textContent = dirty ? "Save download path changes before copying a script." : scriptError || (scriptLoading ? "Preparing download script…" : scriptResult
        ? "Ready to run on your computer. Existing files are skipped, not validated."
        : "Add models to prepare a download script.");
    status.classList.toggle("is-error", !!scriptError);
    setText(root.querySelector(".loadout-script"), scriptResult?.text || "");
    root.querySelector(".loadout-layout").hidden = !scriptResult;
    setText(root.querySelector(".loadout-layout-text"), scriptResult?.layout || "");
    root.querySelector("#loadout-copy").disabled = !scriptResult || dirty || !!app.state.loadoutBusy;
    root.querySelector("#loadout-save-script").disabled = !scriptResult || dirty || !!app.state.loadoutBusy;
    root.querySelector("#loadout-retry-script").hidden = !scriptError;
}

async function copyScript(app, root) {
    if (!scriptResult) return;
    const ok = await copyText(scriptResult.text);
    if (ok) toast("Download script copied; run it on your computer");
    else {
        root.querySelector(".loadout-script-review").open = true;
        app.state.loadoutError = "Clipboard unavailable. Select the script to copy, or use SAVE SCRIPT.";
        renderDrawer(app);
    }
}

function saveScript() {
    if (!scriptResult) return;
    const prefix = scriptResult.filename.endsWith(".ps1") ? "\uFEFF" : "";
    const blob = new Blob([prefix, scriptResult.text], { type: "text/plain;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const link = el("a", { href: url, download: scriptResult.filename });
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
}
