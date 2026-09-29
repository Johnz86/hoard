import * as api from "./api.js";
import { el, mount, toast } from "./dom.js";

let trigger = null;
let addButton = null;
let popover = null;
let saveForm = null;
let library = null;
let heading = null;
let nameField = null;
let queryPreview = null;
let filterField = null;
let list = null;
let openMode = null;
let listedShelves = null;

function currentQuery(app) {
    const query = String(app.state.searchedQuery ?? "").trim();
    const input = document.getElementById("search-input");
    return !app.state.searching && !app.state.searchError && input?.value.trim() === query
        ? query
        : "";
}

function shelfCount(shelf) {
    return typeof shelf.count === "number" ? String(shelf.count) : "—";
}

function renderShelfList(app) {
    if (!list) return;
    if (app.state.shelvesError) {
        mount(
            list,
            el("div", { class: "shelf-menu-empty", text: "Could not load shelves" }),
            el("button", {
                class: "shelf-menu-retry",
                type: "button",
                text: "RETRY",
                onClick: () => loadShelves(app)
            })
        );
        return;
    }
    if (!app.state.shelves) {
        mount(list, el("div", { class: "shelf-menu-empty", text: "Loading shelves…" }));
        return;
    }
    const needle = filterField.value.trim().toLowerCase();
    const shelves = app.state.shelves.filter((shelf) =>
        shelf.name.toLowerCase().includes(needle)
    );
    if (!shelves.length) {
        mount(list, el("div", {
            class: "shelf-menu-empty",
            text: needle ? "No matching shelves" : "No shelves yet"
        }));
        return;
    }
    mount(
        list,
        shelves.map((shelf) =>
            el(
                "div",
                { class: "shelf-menu-row" },
                el(
                    "button",
                    {
                        class: "shelf-menu-open",
                        type: "button",
                        title: shelf.query,
                        onClick: () => {
                            closeShelfMenu();
                            app.commitQuery(shelf.query, { echo: true });
                        }
                    },
                    el("span", { class: "shelf-menu-name", text: shelf.name }),
                    el("span", { class: "shelf-menu-query", text: shelf.query || "all artifacts" })
                ),
                el("span", { class: "shelf-menu-count", text: shelfCount(shelf) }),
                shelf.builtin
                    ? null
                    : el("button", {
                          class: "shelf-menu-remove",
                          type: "button",
                          text: "×",
                          title: `Remove ${shelf.name}`,
                          "aria-label": `Remove ${shelf.name}`,
                          onClick: async (event) => {
                              event.currentTarget.disabled = true;
                              try {
                                  await api.deleteShelf(shelf.id);
                                  app.state.shelves = app.state.shelves.filter(
                                      (saved) => saved.id !== shelf.id
                                  );
                                  renderShelfActions(app);
                                  toast(`removed shelf ${shelf.name}`);
                                  await loadShelves(app);
                                  filterField.focus();
                              } catch (error) {
                                  event.currentTarget.disabled = false;
                                  toast(`delete failed · ${error.message}`);
                              }
                          }
                      })
            )
        )
    );
}

function closeShelfMenu() {
    openMode = null;
    if (popover) popover.hidden = true;
    if (trigger) trigger.setAttribute("aria-expanded", "false");
    if (addButton) addButton.setAttribute("aria-expanded", "false");
}

export function openShelfMenu(app, mode = "list") {
    if (!popover) return;
    if (mode === "save" && addButton.hidden) return;
    if (openMode === mode) {
        closeShelfMenu();
        return;
    }
    openMode = mode;
    popover.hidden = false;
    filterField.value = "";
    trigger.setAttribute("aria-expanded", String(mode === "list"));
    addButton.setAttribute("aria-expanded", String(mode === "save"));
    renderShelfActions(app);
    if (mode === "save") {
        nameField.value = "";
        nameField.focus();
    } else {
        renderShelfList(app);
        if (!app.state.shelves) loadShelves(app);
        filterField.focus();
    }
}

export function renderShelfActions(app) {
    if (!trigger) return;
    const query = currentQuery(app);
    addButton.hidden = !query || !app.state.shelves || app.state.shelves.some(
        (shelf) => shelf.query.trim() === query
    );
    if (openMode === "save" && addButton.hidden) closeShelfMenu();
    saveForm.hidden = openMode !== "save";
    library.hidden = openMode === "save";
    heading.textContent = openMode === "save" ? "ADD TO SHELF" : "SHELVES";
    queryPreview.textContent = query;
    queryPreview.title = query;
    if (app.state.shelves !== listedShelves) {
        listedShelves = app.state.shelves;
        renderShelfList(app);
    }
}

export async function loadShelves(app) {
    try {
        const result = await api.shelves();
        app.state.shelves = Array.isArray(result) ? result : result.shelves || [];
        app.state.shelvesError = null;
    } catch (error) {
        app.state.shelvesError = error;
    }
    app.render();
    renderShelfList(app);
}

export function initShelfActions(app) {
    trigger = document.getElementById("shelf-trigger");
    trigger.addEventListener("click", () => openShelfMenu(app));
    addButton = document.getElementById("add-shelf");
    addButton.addEventListener("click", () => openShelfMenu(app, "save"));
    nameField = el("input", {
        class: "field shelf-menu-input",
        type: "text",
        placeholder: "Name this shelf",
        spellcheck: "false",
        "aria-label": "Shelf name"
    });
    queryPreview = el("div", { class: "shelf-menu-preview" });
    saveForm = el(
        "form",
        {
            class: "shelf-menu-save",
            onSubmit: async (event) => {
                event.preventDefault();
                const name = nameField.value.trim();
                const query = currentQuery(app);
                if (!name || !query) {
                    toast("enter a shelf name and search query");
                    return;
                }
                const button = saveForm.querySelector("button[type=submit]");
                button.disabled = true;
                try {
                    const shelf = await api.createShelf(name, query);
                    nameField.value = "";
                    closeShelfMenu();
                    app.state.shelves = [...app.state.shelves, shelf];
                    renderShelfActions(app);
                    toast(`saved shelf ${name}`);
                    await loadShelves(app);
                } catch (error) {
                    toast(`save failed · ${error.message}`);
                } finally {
                    button.disabled = false;
                }
            }
        },
        queryPreview,
        el(
            "div",
            { class: "shelf-menu-save-row" },
            nameField,
            el("button", { class: "btn shelf-menu-submit", type: "submit", text: "ADD TO SHELF" })
        )
    );
    filterField = el("input", {
        class: "field shelf-menu-input",
        type: "search",
        placeholder: "Find a shelf by name",
        spellcheck: "false",
        "aria-label": "Find a shelf by name",
        onInput: () => renderShelfList(app),
        onKeydown: (event) => {
            if (event.key !== "Enter") return;
            const first = list.querySelector(".shelf-menu-open");
            if (!first) return;
            event.preventDefault();
            first.click();
        }
    });
    list = el("div", { class: "shelf-menu-list" });
    library = el(
        "div",
        { class: "shelf-menu-library" },
        el("div", { class: "shelf-menu-label", text: "OPEN A SAVED SEARCH" }),
        filterField,
        list
    );
    heading = el("span", { text: "SHELVES" });
    popover = el(
        "div",
        {
            class: "shelf-popover",
            id: "shelf-popover",
            role: "dialog",
            "aria-label": "Saved searches",
            hidden: true
        },
        el(
            "div",
            { class: "shelf-menu-head" },
            heading,
            el("button", {
                class: "shelf-menu-close",
                type: "button",
                text: "CLOSE",
                onClick: closeShelfMenu
            })
        ),
        saveForm,
        library
    );
    document.getElementById("search").append(popover);
    document.addEventListener("pointerdown", (event) => {
        if (openMode && !popover.contains(event.target) &&
            !trigger.contains(event.target) && !addButton.contains(event.target)) {
            closeShelfMenu();
        }
    });
    document.addEventListener("keydown", (event) => {
        if (event.key !== "Escape" || !openMode) return;
        event.preventDefault();
        event.stopPropagation();
        const opener = openMode === "save" ? addButton : trigger;
        closeShelfMenu();
        opener.focus();
    });
    renderShelfActions(app);
}
