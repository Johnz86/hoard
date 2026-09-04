import * as api from "./api.js";
import { el, errorPanel, loadingPanel, toast } from "./dom.js";

export async function loadShelves(app) {
    try {
        const list = await api.shelves();
        app.state.shelves = Array.isArray(list) ? list : list.shelves || [];
        app.state.shelvesError = null;
    } catch (error) {
        app.state.shelvesError = error;
    }
    app.render();
}

export function renderShelfRail(app) {
    const shelves = app.state.shelves || [];
    return el(
        "div",
        { class: "rail-block rail-shelves" },
        el("div", { class: "rail-title", text: "SAVED SHELVES" }),
        shelves.length
            ? shelves.map((shelf) =>
                  el(
                      "div",
                      { class: "shelf-row", onClick: () => app.commitQuery(shelf.query, { echo: true }) },
                      el("span", { class: "shelf-name", text: shelf.name }),
                      el("span", { class: "shelf-chevron", text: "›" })
                  )
              )
            : el("div", { class: "rail-empty", text: "NO SHELVES" })
    );
}

export function renderShelvesView(app) {
    if (app.state.shelvesError) return errorPanel(app.state.shelvesError, () => loadShelves(app));
    if (!app.state.shelves) return loadingPanel("reading shelves…");
    const nameField = el("input", {
        class: "field",
        type: "text",
        placeholder: "shelf name",
        spellcheck: "false"
    });
    return el(
        "div",
        { class: "panel-view" },
        el("div", { class: "panel-title", text: "SHELVES" }),
        el(
            "div",
            { class: "panel-head shelf-grid" },
            el("span", { text: "NAME" }),
            el("span", { text: "QUERY" }),
            el("span", { text: "KIND" }),
            el("span", { class: "align-right", text: "" })
        ),
        app.state.shelves.map((shelf) =>
            el(
                "div",
                { class: "panel-row shelf-grid" },
                el("span", {
                    class: "link",
                    text: shelf.name,
                    onClick: () => app.commitQuery(shelf.query, { echo: true, goIndex: true })
                }),
                el("span", { class: "mono-dim", text: shelf.query }),
                el("span", { class: "mono-dim", text: shelf.builtin ? "builtin" : "saved" }),
                el(
                    "span",
                    { class: "align-right" },
                    shelf.builtin
                        ? null
                        : el("span", {
                              class: "row-x",
                              text: "×",
                              title: "delete shelf",
                              onClick: async () => {
                                  try {
                                      await api.deleteShelf(shelf.id);
                                      toast(`removed shelf ${shelf.name}`);
                                      loadShelves(app);
                                  } catch (error) {
                                      toast(`delete failed · ${error.message}`);
                                  }
                              }
                          })
                )
            )
        ),
        el(
            "div",
            { class: "panel-form" },
            nameField,
            el("span", { class: "mono-dim", text: app.state.q || "(empty query)" }),
            el("button", {
                class: "btn",
                type: "button",
                text: "SAVE CURRENT QUERY",
                onClick: async () => {
                    const name = nameField.value.trim();
                    if (!name) {
                        toast("name the shelf first");
                        return;
                    }
                    try {
                        await api.createShelf(name, app.state.q);
                        toast(`saved shelf ${name}`);
                        nameField.value = "";
                        loadShelves(app);
                    } catch (error) {
                        toast(`save failed · ${error.message}`);
                    }
                }
            })
        )
    );
}
