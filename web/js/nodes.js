import * as api from "./api.js";
import { el, mount } from "./dom.js";

export async function loadNodes(app, paint = true) {
    try {
        const list = await api.nodes();
        app.state.nodes = Array.isArray(list) ? list : list.nodes || [];
        app.state.nodesError = null;
    } catch (error) {
        app.state.nodesError = error;
    }
    renderNodePulse(app);
    if (paint) app.render();
}

export function renderNodePulse(app) {
    const host = document.getElementById("node-pulse");
    if (!host) return;
    const nodes = app.state.nodes;
    const online = nodes ? nodes.filter((node) => node.online).length : null;
    const count = online === null ? "— NODES" : `${online} NODES`;
    if (host.dataset.count === count) return;
    mount(
        host,
        el("span", { class: "pulse-dot" }),
        el("span", { text: count })
    );
    host.classList.toggle("is-offline", online === 0);
    host.dataset.count = count;
}
