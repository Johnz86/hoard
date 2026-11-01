import { el, dash, sizeText, stateText } from "./dom.js";
import { roleClass, roleCode, stateClass } from "./table.js";

function card(app, item, index) {
    const classes = ["card", roleClass(item.role)];
    if (index === app.state.cursor) classes.push("is-cursor");
    if (app.state.selection.has(app.rowKey(item))) classes.push("is-selected");
    const line = [item.prefix || "", dash(item.base)].join("");
    return el(
        "div",
        {
            class: classes.join(" "),
            dataset: { index: String(index) },
            onClick: () => app.setCursor(index),
            onDblclick: () => app.openRow(item)
        },
        el(
            "div",
            { class: "card-top" },
            el("span", { class: "role-mark" }),
            el("span", { class: "role-code", text: roleCode(item) }),
            el("span", {
                class: `card-state ${stateClass(item.state)}`,
                text: stateText(item.state, item.pull_progress)
            })
        ),
        el("div", { class: "card-name", text: item.name }),
        el("div", {
            class: "card-meta",
            text: item.loadout_id ? `${item.items} models · ${sizeText(item)} · ${item.node}`
                : item.project_id ? `${item.revision} · ${sizeText(item)} · ${item.node}`
                : `${line} · ${dash(item.precision)} · ${sizeText(item)}`
        }),
        el("div", { class: "card-trigger", text: item.loadout_id ? "Review collection and download" : item.project_id ? item.source : dash(item.trigger) })
    );
}

export function renderCards(app) {
    const rows = app.state.rows;
    if (!rows.length) {
        return el(
            "div",
            { class: "cards-empty" },
            el("div", { class: "empty-title", text: "NO RESOURCE MATCHES THIS QUERY" })
        );
    }
    return el(
        "div",
        { class: "cards-grid" },
        rows.map((item, index) => card(app, item, index)),
        app.state.resourcesNext ? el("button", { class: "btn resource-more", type: "button",
            text: app.state.resourcesLoading ? "LOADING..." : "MORE RESULTS",
            disabled: app.state.resourcesLoading, onClick: () => app.loadMoreResources() }) : null
    );
}
