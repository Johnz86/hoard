import { el, fmtBytes } from "./dom.js";

const SECTIONS = [
    { title: "ROLE", groups: [["role", "role"]] },
    { title: "SOURCE LAYOUT", groups: [["layout", "layout"]] },
    { title: "BASE ARCHITECTURE", groups: [["base", "base"]] },
    { title: "FORMAT · PRECISION", groups: [["format", "fmt"], ["precision", "prec"]] },
    { title: "NODE", groups: [["node", "node"]] }
];

const DISK_BARS = 6;
const AMBER_SHARE = 0.7;
const BASE_SECTION = "BASE ARCHITECTURE";
const FILTER_ROWS = 15;
const LABEL_FITS = 22;

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

function facetTitle(item) {
    const value = String(item.value ?? "");
    const label = String(item.label || value);
    if (label !== value) return `${label} (${value})`;
    return label.length > LABEL_FITS ? label : null;
}

function facetRow(app, item, key, tier = "") {
    const count = Number(item.count) || 0;
    return el(
        "div",
        {
            class: `facet-row${tier}${item.active ? " is-active" : ""}`,
            onClick: () => selectFacet(app, item, key)
        },
        el("span", { class: "facet-mark" }),
        el("span", {
            class: `facet-label${count ? "" : " is-empty"}`,
            text: item.label || item.value,
            title: facetTitle(item)
        }),
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

function baseCapNote(facets, shown) {
    const total = Number(facets.base_total) || 0;
    if (total <= shown) return null;
    return el("div", {
        class: "rail-note",
        text: `${shown} of ${total} — showing the most common`
    });
}

function railFilter(rows) {
    const input = el("input", {
        class: "field rail-filter",
        type: "text",
        placeholder: "filter",
        spellcheck: "false",
        onInput: () => {
            const needle = input.value.trim().toLowerCase();
            for (const row of rows) row.node.hidden = !!needle && !row.text.includes(needle);
        }
    });
    return input;
}

function facetText(item) {
    return String(item.label || item.value || "");
}

function baseItems(facets) {
    const buckets = facets.base || [];
    const families = facets.base_families || [];
    const headed = new Set(families.map((family) => family.value));
    const items = [];
    for (const family of families) {
        const members = buckets.filter((item) => item.group === family.value);
        items.push({
            item: family,
            key: "base",
            tier: " is-family",
            text: [family, ...members].map(facetText).join(" ")
        });
        for (const member of members) {
            items.push({
                item: member,
                key: "base",
                tier: " is-grouped",
                text: `${facetText(member)} ${facetText(family)}`
            });
        }
    }
    for (const item of buckets) {
        if (!item.group || !headed.has(item.group)) {
            items.push({ item, key: "base", tier: "", text: facetText(item) });
        }
    }
    return items;
}

function sectionItems(section, facets) {
    if (section.title === BASE_SECTION) return baseItems(facets);
    return section.groups.flatMap(([field, key]) =>
        (facets[field] || []).map((item) => ({ item, key, tier: "", text: facetText(item) }))
    );
}

export function renderFacets(app) {
    const facets = app.state.facets || {};
    const blocks = SECTIONS.map((section) => {
        const items = sectionItems(section, facets);
        if (!items.length) return null;
        const rows = items.map(({ item, key, tier, text }) => ({
            node: facetRow(app, item, key, tier),
            text: text.toLowerCase()
        }));
        return el(
            "div",
            { class: "rail-block" },
            el("div", { class: "rail-title", text: section.title }),
            rows.length > FILTER_ROWS ? railFilter(rows) : null,
            rows.map((row) => row.node),
            section.title === BASE_SECTION
                ? baseCapNote(facets, (facets.base || []).length)
                : null
        );
    }).filter(Boolean);
    if (!blocks.length) {
        blocks.push(el("div", { class: "rail-empty", text: "NO FACETS" }));
    }
    blocks.push(diskPressure(facets.disk_pressure, facets.node));
    return blocks.filter(Boolean);
}
