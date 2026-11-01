import { el, dash, sizeText, stateText } from "./dom.js";

export const COLUMNS = [
    { label: "ROLE", key: "role", align: "left" },
    { label: "NAME", key: "name", align: "left" },
    { label: "BASE", key: "base", align: "left" },
    { label: "FORMAT", key: "format", align: "left" },
    { label: "PREC", key: "precision", align: "left" },
    { label: "SIZE", key: "size", align: "right" },
    { label: "REVISION", key: "revision", align: "left" },
    { label: "NODE", key: "node", align: "left" },
    { label: "STATE", key: "state", align: "right" }
];

export function roleClass(role) {
    return `r-${String(role || "unk").toLowerCase()}`;
}

export function roleCode(row) {
    const role = String(row.role || "unk").toLowerCase();
    return row.role_code || (role === "unk" ? "?" : role.toUpperCase());
}

export function stateClass(state) {
    return `st-${String(state || "").toLowerCase()}`;
}

function header(app) {
    return el(
        "div",
        { class: "table-head" },
        COLUMNS.map((column) =>
            el("span", {
                class: [
                    "th",
                    `align-${column.align}`,
                    app.state.sort === column.key ? "is-sorted" : ""
                ].join(" ").trim(),
                text: app.state.sort === column.key
                    ? `${column.label} ${app.state.dir === "asc" ? "↑" : "↓"}`
                    : column.label,
                onClick: () => app.sortBy(column.key)
            })
        )
    );
}

function row(app, item, index) {
    const selected = app.state.selection.has(app.rowKey(item));
    const classes = ["trow", roleClass(item.role)];
    if (index === app.state.cursor) classes.push("is-cursor");
    if (selected) classes.push("is-selected");
    return el(
        "div",
        {
            class: classes.join(" "),
            dataset: { index: String(index) },
            onClick: () => app.setCursor(index),
            onDblclick: () => app.openRow(item)
        },
        el(
            "span",
            { class: "cell-role" },
            el("span", { class: "role-mark" }),
            el("span", { class: "role-code", text: roleCode(item) })
        ),
        el(
            "span",
            { class: "cell-name" },
            item.prefix ? el("span", { class: "cell-org", text: item.prefix }) : null,
            item.name
        ),
        el("span", { class: "cell-muted", text: dash(item.base) }),
        el("span", { class: "cell-muted", text: dash(item.format) }),
        el("span", { class: "cell-muted", text: dash(item.precision) }),
        el("span", { class: "cell-size", text: sizeText(item) }),
        el("span", { class: "cell-muted", text: dash(item.revision) }),
        el("span", { class: "cell-muted", text: dash(item.node) }),
        el("span", {
            class: `cell-state ${stateClass(item.state)}`,
            text: stateText(item.state, item.pull_progress)
        })
    );
}

function emptyState(app, example) {
    return el(
        "div",
        { class: "empty" },
        el("div", { class: "empty-title", text: "NO RESOURCE MATCHES THIS QUERY" }),
        el(
            "div",
            { class: "empty-body" },
            "drop a token above, or try ",
            el("span", {
                class: "empty-example",
                text: example,
                onClick: () => app.commitQuery(example, { echo: true })
            })
        )
    );
}

export function renderTable(app, example) {
    const rows = app.state.rows;
    return el(
        "div",
        { class: "table-scroll" },
        el(
            "div",
            { class: "table" },
            header(app),
            rows.map((item, index) => row(app, item, index)),
            rows.length ? null : emptyState(app, example)
        ),
        app.state.resourcesNext ? el("button", { class: "btn resource-more", type: "button",
            text: app.state.resourcesLoading ? "LOADING..." : "MORE RESULTS",
            disabled: app.state.resourcesLoading, onClick: () => app.loadMoreResources() }) : null
    );
}

export function revealCursor(container, cursor) {
    const node = container.querySelector(`[data-index="${cursor}"]`);
    if (node) node.scrollIntoView({ block: "nearest" });
}
