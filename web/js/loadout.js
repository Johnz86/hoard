import * as api from "./api.js";
import { el, errorPanel, fmtRate, loadingPanel, mount, toast } from "./dom.js";
import { roleClass, roleCode } from "./table.js";

export async function loadLoadout(app) {
    try {
        if (!app.state.loadoutId) {
            const list = await api.loadouts();
            const first = (Array.isArray(list) ? list : list.loadouts || [])[0];
            if (!first) {
                app.state.loadout = { items: [], name: "loadout" };
                app.state.loadoutError = null;
                app.render();
                return;
            }
            app.state.loadoutId = first.id;
        }
        app.state.loadout = await api.loadout(app.state.loadoutId);
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

export function renderDrawer(app) {
    const root = document.getElementById("drawer-root");
    if (!app.state.drawerOpen) {
        root.replaceChildren();
        return;
    }
    const doc = app.state.loadout;
    const items = loadoutItems(app);
    const name = (doc && (doc.name || (doc.document && doc.document.name))) || "loadout";
    const node = app.selfNodeName();

    let body;
    if (app.state.loadoutError) body = errorPanel(app.state.loadoutError, () => loadLoadout(app));
    else if (!doc) body = loadingPanel("reading loadout…");
    else if (!items.length) {
        body = el(
            "div",
            { class: "drawer-empty" },
            "EMPTY LOADOUT",
            el("br"),
            "select artifacts in the index and press ",
            el("span", { class: "amber", text: "L" })
        );
    } else {
        body = items.map((item) =>
            el(
                "div",
                { class: `drawer-row ${roleClass(item.role)}` },
                el(
                    "span",
                    { class: "cell-role" },
                    el("span", { class: "role-mark" }),
                    el("span", { class: "role-code", text: roleCode(item) })
                ),
                el("span", { class: "drawer-name", text: item.name }),
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
                    el("span", { class: "drawer-slug", text: name }),
                    el("button", {
                        class: "btn btn-close",
                        type: "button",
                        text: "CLOSE",
                        onClick: () => app.closeDrawer()
                    })
                ),
                el("div", { class: "drawer-body" }, body),
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
