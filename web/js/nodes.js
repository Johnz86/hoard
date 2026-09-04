import * as api from "./api.js";
import { el, errorPanel, fmtAge, fmtBytes, loadingPanel, mount } from "./dom.js";

export async function loadNodes(app) {
    try {
        const list = await api.nodes();
        app.state.nodes = Array.isArray(list) ? list : list.nodes || [];
        app.state.nodesError = null;
    } catch (error) {
        app.state.nodesError = error;
    }
    renderNodePulse(app);
    app.render();
}

export function renderNodePulse(app) {
    const host = document.getElementById("node-pulse");
    if (!host) return;
    const nodes = app.state.nodes;
    const online = nodes ? nodes.filter((node) => node.online).length : null;
    mount(
        host,
        el("span", { class: "pulse-dot" }),
        el("span", { text: online === null ? "— NODES" : `${online} NODES` })
    );
    host.classList.toggle("is-offline", online === 0);
}

export function renderNodesView(app) {
    if (app.state.nodesError) return errorPanel(app.state.nodesError, () => loadNodes(app));
    if (!app.state.nodes) return loadingPanel("reading peers…");
    return el(
        "div",
        { class: "panel-view" },
        el("div", { class: "panel-title", text: "NODES" }),
        el(
            "div",
            { class: "panel-head node-grid" },
            el("span", { text: "" }),
            el("span", { text: "NAME" }),
            el("span", { text: "URL" }),
            el("span", { class: "align-right", text: "INDEXED" }),
            el("span", { class: "align-right", text: "LAST SEEN" })
        ),
        app.state.nodes.map((node) =>
            el(
                "div",
                { class: "panel-row node-grid" },
                el("span", { class: `node-dot${node.online ? " is-online" : ""}`, text: "●" }),
                el(
                    "span",
                    null,
                    node.name,
                    node.self ? el("span", { class: "badge badge-inline", text: "THIS NODE" }) : null
                ),
                el("span", { class: "mono-dim", text: node.url || "" }),
                el("span", { class: "align-right", text: fmtBytes(node.indexed_bytes) }),
                el("span", { class: "align-right mono-dim", text: fmtAge(node.last_seen) })
            )
        )
    );
}
