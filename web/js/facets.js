import { el, fmtBytes } from "./dom.js";

const SECTIONS = [
    { title: "ROLE", groups: [["role", "role"]] },
    { title: "BASE ARCHITECTURE", groups: [["base", "base"]] },
    { title: "FORMAT · PRECISION", groups: [["format", "fmt"], ["precision", "prec"]] },
    { title: "NODE", groups: [["node", "node"]] }
];

const DISK_BARS = 6;
const AMBER_SHARE = 0.7;

export function facetToken(item, key) {
    if (item.token) return item.token;
    const value = String(item.value ?? "");
    return /\s/.test(value) ? `${key}:"${value}"` : `${key}:${value}`;
}

function selectFacet(app, item, key) {
    if (!item.active && typeof item.query === "string") {
        app.commitQuery(item.query, { echo: true });
        return;
    }
    app.toggleToken(facetToken(item, key));
}

function facetRow(app, item, key) {
    const count = Number(item.count) || 0;
    return el(
        "div",
        {
            class: `facet-row${item.active ? " is-active" : ""}`,
            onClick: () => selectFacet(app, item, key)
        },
        el("span", { class: "facet-mark" }),
        el("span", { class: `facet-label${count ? "" : " is-empty"}`, text: item.label || item.value }),
        el("span", { class: "facet-count", text: count ? String(count) : "—" })
    );
}

function diskPressure(pressure, nodeFacet) {
    if (!pressure) return null;
    const source = Array.isArray(pressure.bars) && pressure.bars.length
        ? pressure.bars
        : (nodeFacet || []).map((item) => Number(item.count) || 0);
    const bars = source.slice(0, DISK_BARS);
    const peak = Math.max(1, ...bars);
    const reclaimable = fmtBytes(pressure.reclaimable_bytes);
    const dupes = Number(pressure.duplicate_blobs) || 0;
    return el(
        "div",
        { class: "rail-block" },
        el("div", { class: "rail-title", text: "DISK PRESSURE" }),
        el(
            "div",
            { class: "disk-bars" },
            bars.map((value) => {
                const share = value / peak;
                return el("span", {
                    class: `disk-bar${share >= AMBER_SHARE ? " is-hot" : ""}`,
                    style: { height: `${Math.max(6, Math.round(share * 100))}%` }
                });
            })
        ),
        el("div", {
            class: "disk-note",
            text: `${reclaimable} reclaimable — ${dupes} duplicate blob${dupes === 1 ? "" : "s"}`
        })
    );
}

export function renderFacets(app) {
    const facets = app.state.facets || {};
    const blocks = SECTIONS.map((section) => {
        const items = section.groups.flatMap(([field, key]) =>
            (facets[field] || []).map((item) => ({ item, key }))
        );
        if (!items.length) return null;
        return el(
            "div",
            { class: "rail-block" },
            el("div", { class: "rail-title", text: section.title }),
            items.map(({ item, key }) => facetRow(app, item, key))
        );
    }).filter(Boolean);
    if (!blocks.length) {
        blocks.push(el("div", { class: "rail-empty", text: "NO FACETS" }));
    }
    blocks.push(diskPressure(facets.disk_pressure, facets.node));
    return blocks.filter(Boolean);
}
