import { el, mount } from "./dom.js";
import { focusSearch } from "./search.js";

const SHEET = [
    ["GLOBAL", [
        ["/", "focus the search field"],
        ["ESC", "blur search, else close the drawer, else leave detail"],
        ["?", "this sheet"]
    ]],
    ["INDEX", [
        ["J  ↓", "move the cursor down"],
        ["K  ↑", "move the cursor up"],
        ["↵", "open the repo under the cursor"],
        ["SPC", "toggle selection"],
        ["L", "add to the loadout"],
        ["S", "serve it on the LAN"],
        ["Y", "yank the pull command"]
    ]],
    ["DETAIL", [
        ["[  ]", "cycle FILES / REFS / CARD / COMPAT / USAGE"],
        ["L", "add this repo to the loadout"],
        ["ESC", "back to the index"]
    ]]
];

function isTyping(target) {
    if (!target) return false;
    const tag = target.tagName;
    return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target.isContentEditable;
}

export function renderKeySheet(app) {
    const root = document.getElementById("keysheet-root");
    if (!app.state.keySheet) {
        root.replaceChildren();
        return;
    }
    mount(
        root,
        el(
            "div",
            { class: "scrim scrim-center", onClick: () => app.hideKeySheet() },
            el(
                "div",
                { class: "keysheet", onClick: (event) => event.stopPropagation() },
                el(
                    "div",
                    { class: "keysheet-head" },
                    el("span", { class: "keysheet-title", text: "keys" }),
                    el("button", {
                        class: "btn btn-close",
                        type: "button",
                        text: "CLOSE",
                        onClick: () => app.hideKeySheet()
                    })
                ),
                SHEET.map(([title, entries]) =>
                    el(
                        "div",
                        { class: "keysheet-block" },
                        el("div", { class: "rail-title", text: title }),
                        entries.map(([keys, what]) =>
                            el(
                                "div",
                                { class: "keysheet-row" },
                                el("span", { class: "keysheet-key", text: keys }),
                                el("span", { class: "keysheet-what", text: what })
                            )
                        )
                    )
                )
            )
        )
    );
}

export function initKeys(app) {
    window.addEventListener("keydown", (event) => {
        const typing = isTyping(event.target);

        if (event.key === "Escape") {
            if (typing) {
                event.target.blur();
                return;
            }
            if (app.state.keySheet) return app.hideKeySheet();
            if (app.state.drawerOpen) return app.closeDrawer();
            if (app.state.view === "detail") return app.goIndex();
            return;
        }
        if (typing || event.ctrlKey || event.metaKey || event.altKey) return;

        if (event.key === "/") {
            event.preventDefault();
            focusSearch();
            return;
        }
        if (event.key === "?") {
            event.preventDefault();
            app.showKeySheet();
            return;
        }
        if (app.state.keySheet) return;

        const key = event.key.toLowerCase();

        if (app.state.view === "detail") {
            if (event.key === "[" || event.key === "]") {
                event.preventDefault();
                app.cycleDetailTab(event.key === "]" ? 1 : -1);
                return;
            }
            if (key === "l") {
                event.preventDefault();
                app.addToLoadout(app.detailArtifact());
            }
            return;
        }
        if (app.state.view !== "index") return;

        if (key === "j" || event.key === "ArrowDown") {
            event.preventDefault();
            app.moveCursor(1);
        } else if (key === "k" || event.key === "ArrowUp") {
            event.preventDefault();
            app.moveCursor(-1);
        } else if (event.key === "Enter") {
            event.preventDefault();
            app.openRow(app.cursorRow());
        } else if (event.key === " ") {
            event.preventDefault();
            app.toggleSelection();
        } else if (key === "l") {
            event.preventDefault();
            app.addToLoadout(app.cursorRow());
        } else if (key === "s") {
            event.preventDefault();
            app.serve(app.cursorRow());
        } else if (key === "y") {
            event.preventDefault();
            app.yank(app.cursorRow());
        }
    });
}
