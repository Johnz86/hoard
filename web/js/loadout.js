import * as api from "./api.js";
import { el, errorPanel, fmtRate, loadingPanel, mount, toast } from "./dom.js";
import { roleClass, roleCode } from "./table.js";
import { settings, updateSettings } from "./theme.js";

export async function loadLoadout(app) {
    try {
        const list = await api.loadouts();
        const rows = Array.isArray(list) ? list : list.loadouts || [];
        app.state.loadouts = rows;
        const wanted = app.state.loadoutId || settings.loadoutId;
        const current = rows.find((row) => row.id === wanted) || rows[0] || null;
        app.state.loadoutId = current ? current.id : null;
        updateSettings({ loadoutId: app.state.loadoutId });
        app.state.loadout = current ? await api.loadout(current.id) : null;
        app.state.loadoutError = null;
    } catch (error) {
        app.state.loadoutError = error;
    }
    app.render();
}

export function loadoutItems(app) {
    const doc = app.state.loadout;
    if (!doc) return [];
    return doc.items || (doc.document && doc.document.items) || [];
}

function weightLabel(item) {
    if (item.weight_label) return item.weight_label;
    if (item.weight === null || item.weight === undefined) return "—";
    return String(item.weight);
}

function applyLine(app, items) {
    const preview = (app.state.loadout && app.state.loadout.apply_preview) || null;
    if (app.state.loadoutApplied) {
        return `→ done · ${items.length} artifacts resolved, symlinks written for comfyui`;
    }
    if (!preview) return `→ ${items.length} pinned`;
    const rate = preview.rate_bps ? ` (${fmtRate(preview.rate_bps)})` : "";
    return `→ ${preview.present ?? 0} present, ${preview.pulling ?? 0} pulling over LAN${rate}`;
}

function emptyState(name) {
    return el(
        "div",
        { class: "drawer-empty" },
        name ? "EMPTY LOADOUT " : "NO LOADOUT YET",
        name ? el("span", { class: "amber", text: name }) : null,
        el("br"),
        "a checkpoint and the adapters you use with it",
        el("br"),
        name ? "select artifacts in the index and press " : "press ",
        el("span", { class: "amber", text: "L" }),
        name ? ", or start another with " : " on a row to start one, or ",
        el("span", { class: "amber", text: "NEW" }),
        " below"
    );
}

function itemRows(app, items) {
    return items.map((item) =>
        el(
            "div",
            { class: `drawer-row ${roleClass(item.role)}` },
            el(
                "span",
                { class: "cell-role" },
                el("span", { class: "role-mark" }),
                el("span", { class: "role-code", text: roleCode(item) })
            ),
            el(
                "span",
                { class: "drawer-name" },
                item.prefix ? el("span", { class: "cell-org", text: item.prefix }) : null,
                item.name
            ),
            el("span", { class: "drawer-rev", text: item.revision || item.rev || "—" }),
            el("span", { class: "drawer-weight", text: weightLabel(item) }),
            el("span", {
                class: "drawer-x",
                text: "×",
                title: "remove",
                onClick: () => app.removeFromLoadout(item)
            })
        )
    );
}

function pinnedBlock(app, items) {
    if (app.state.loadoutError) return errorPanel(app.state.loadoutError, () => loadLoadout(app));
    if (!app.state.loadouts) return loadingPanel("reading loadouts…");
    if (!app.state.loadoutId) return emptyState(null);
    if (!app.state.loadout) return loadingPanel("reading loadout…");
    if (!items.length) return emptyState(app.state.loadout.name);
    return itemRows(app, items);
}

function listBlock(app) {
    const rows = app.state.loadouts || [];
    if (!rows.length) return null;
    return el(
        "div",
        { class: "drawer-list" },
        el("div", { class: "rail-title", text: "LOADOUTS" }),
        rows.map((row) => {
            const armed = app.state.loadoutDeleting === row.id;
            const current = row.id === app.state.loadoutId ? " is-current" : "";
            return el(
                "div",
                { class: `drawer-list-row${current}` },
                el("span", {
                    class: "link",
                    text: row.name,
                    onClick: () => app.switchLoadout(row.id)
                }),
                el("span", { class: "mono-dim", text: `${row.items} items` }),
                el("span", {
                    class: armed ? "drawer-confirm" : "row-x",
                    text: armed ? "CONFIRM" : "DELETE",
                    onClick: () => (armed ? app.deleteLoadout(row.id) : app.armDelete(row.id))
                })
            );
        })
    );
}

function newForm(app) {
    const field = el("input", {
        class: "field",
        type: "text",
        placeholder: "name, or blank for loadout-N",
        spellcheck: "false",
        onKeyDown: (event) => {
            if (event.key === "Enter") app.newLoadout(field.value);
        }
    });
    return el(
        "div",
        { class: "drawer-new" },
        field,
        el("button", {
            class: "btn",
            type: "button",
            text: "NEW",
            onClick: () => app.newLoadout(field.value)
        })
    );
}

function nameField(app) {
    const doc = app.state.loadout;
    if (!doc) return el("span", { class: "drawer-slug", text: "none" });
    return el("input", {
        class: "field drawer-rename",
        type: "text",
        value: doc.name,
        spellcheck: "false",
        onChange: (event) => app.renameLoadout(event.target.value)
    });
}

export function renderDrawer(app) {
    const root = document.getElementById("drawer-root");
    if (!app.state.drawerOpen) {
        root.replaceChildren();
        return;
    }
    const doc = app.state.loadout;
    const items = loadoutItems(app);
    const name = (doc && doc.name) || "none";
    const node = app.selfNodeName();

    mount(
        root,
        el(
            "div",
            { class: "scrim", onClick: () => app.closeDrawer() },
            el(
                "div",
                { class: "drawer", onClick: (event) => event.stopPropagation() },
                el(
                    "div",
                    { class: "drawer-head" },
                    el("span", { class: "drawer-title", text: "loadout" }),
                    nameField(app),
                    el("button", {
                        class: "btn btn-close",
                        type: "button",
                        text: "CLOSE",
                        onClick: () => app.closeDrawer()
                    })
                ),
                el(
                    "div",
                    { class: "drawer-body" },
                    pinnedBlock(app, items),
                    listBlock(app),
                    newForm(app)
                ),
                el(
                    "div",
                    { class: "drawer-foot" },
                    el(
                        "div",
                        { class: "code-box" },
                        el("div", { class: "code-comment", text: `$ hoard apply ${name} --node ${node}` }),
                        el("div", { class: "code-ok", text: applyLine(app, items) })
                    ),
                    el("button", {
                        class: "btn btn-primary btn-wide",
                        type: "button",
                        text: `APPLY TO ${node.toUpperCase()}`,
                        disabled: !doc,
                        onClick: () => applyNow(app, node)
                    })
                )
            )
        )
    );
}

async function applyNow(app, node) {
    if (!app.state.loadoutId) return;
    try {
        await api.applyLoadout(app.state.loadoutId, node, "comfyui");
        app.state.loadoutApplied = true;
        toast(`applied to ${node} · symlinks written for comfyui`);
    } catch (error) {
        toast(`apply failed · ${error.message}`);
    }
    app.render();
}
