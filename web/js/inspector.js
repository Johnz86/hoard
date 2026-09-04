import { el } from "./dom.js";
import { roleClass, roleCode } from "./table.js";

export function factRows(facts) {
    return (facts || []).map((fact) =>
        el(
            "div",
            { class: "fact" },
            el("span", { class: "fact-key", text: fact.key ?? fact.k ?? "" }),
            el("span", { class: "fact-val", text: String(fact.value ?? fact.v ?? "") })
        )
    );
}

function plugLabel(plug) {
    const name = plug.org ? `${plug.org}/${plug.name}` : plug.name;
    return `${String(plug.role || "").toLowerCase()} · ${name}`;
}

export function renderInspector(app) {
    const row = app.cursorRow();
    const detail = app.state.cursorArtifact;
    const facts = detail && detail.artifact_id === (row && row.artifact_id) ? detail.facts : null;
    const plugs = detail && detail.artifact_id === (row && row.artifact_id) ? detail.plugs_into : null;

    if (!row) {
        return el(
            "aside",
            { class: "inspector" },
            el("div", { class: "rail-title", text: "INSPECTOR" }),
            el("div", { class: "inspector-name", text: "—" }),
            el("div", { class: "inspector-path", text: "no match" })
        );
    }

    return el(
        "aside",
        { class: `inspector ${roleClass(row.role)}` },
        el(
            "div",
            { class: "inspector-head" },
            el("div", { class: "rail-title", text: "INSPECTOR" }),
            el(
                "div",
                { class: "inspector-role" },
                el("span", { class: "role-mark role-mark-lg" }),
                el("span", { class: "role-code", text: roleCode(row) })
            ),
            el("div", { class: "inspector-name", text: row.name }),
            el("div", { class: "inspector-path", text: row.path || "" })
        ),
        el(
            "div",
            { class: "fact-list" },
            facts
                ? factRows(facts)
                : el("div", { class: "fact-loading", text: "reading index record…" })
        ),
        el(
            "div",
            { class: "inspector-actions" },
            el("button", {
                class: "btn btn-primary",
                type: "button",
                text: "OPEN REPO ↵",
                onClick: () => app.openRow(row)
            }),
            el("button", {
                class: "btn",
                type: "button",
                text: "ADD TO LOADOUT",
                onClick: () => app.addToLoadout(row)
            }),
            el("button", {
                class: "btn",
                type: "button",
                text: "YANK PULL COMMAND",
                onClick: () => app.yank(row)
            })
        ),
        el(
            "div",
            null,
            el("div", { class: "rail-title", text: "PLUGS INTO" }),
            (plugs && plugs.length
                ? plugs.slice(0, 4).map((plug) =>
                      el("div", {
                          class: "plug",
                          text: plugLabel(plug),
                          onClick: () => app.openArtifact(plug)
                      })
                  )
                : [el("div", { class: "plug is-empty", text: "nothing resolves against this" })])
        )
    );
}
