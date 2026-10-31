import * as api from "./api.js";
import { el, errorPanel } from "./dom.js";

export async function loadSetup(app, paint = true) {
    try {
        app.state.setup = await api.setup();
        app.state.setupError = null;
    } catch (error) {
        app.state.setupError = error;
    }
    if (paint) app.render();
}

function openSharing() {
    const details = document.getElementById("source-sharing");
    details.open = true;
    details.scrollIntoView({ block: "nearest", behavior: "smooth" });
    details.querySelector("input")?.focus({ preventScroll: true });
}

export function renderWelcome(app) {
    const setup = app.state.setup;
    if (!setup || setup.resource_count > 0) return null;
    const scanning = setup.scanning || (app.state.jobs || []).some(job => job.kind === "scan" && !job.finished);
    return el("section", { id: "source-welcome", class: "setup-welcome" },
        el("h2", { text: "Build your shared catalog" }),
        el("p", { text: "Bring your models, datasets and Git projects together. Find what you need and share it from this machine. Your files stay where they are." }),
        el("div", { class: "setup-steps" },
            el("div", null, el("h3", { text: "01 / ADD A FOLDER" }),
                el("p", { text: "Choose a folder on the machine running Hoard. Nothing is uploaded or moved." }),
                el("button", { class: "btn btn-tiny", type: "button", text: "ADD FOLDER", onClick: () => app.openSourceForm() })),
            el("div", null, el("h3", { text: "02 / REVIEW RESOURCES" }),
                el("p", { text: scanning ? "Scanning in the background. Follow progress in Activity below."
                    : setup.configured_sources ? "Your folder is configured. Check its scan and use BROWSE to see what Hoard found."
                        : "Hoard will index your folders. Then browse, search and read their contents." }),
                el("button", { class: "btn btn-tiny", type: "button", text: "OPEN INDEX", onClick: () => app.goIndex() })),
            el("div", null, el("h3", { text: "03 / SHARE ACCESS" }),
                el("p", { text: "Review this node's address before sharing a project link or clone command." }),
                el("button", { class: "btn btn-tiny", type: "button", text: "SHARING SETTINGS", onClick: openSharing }))));
}

function sharingStatus(setup) {
    if (setup.restart_required) return "Sharing address is active. Restart Hoard to apply the saved node name.";
    return setup.address_message;
}

function sharingForm(app) {
    const setup = app.state.setup;
    const values = setup.pending || setup;
    const name = el("input", { class: "field", name: "node_name", value: values.node_name,
        required: true, maxlength: "64", "aria-label": "Node name" });
    const url = el("input", { class: "field", type: "url", name: "public_url", value: values.public_url,
        required: true, placeholder: "http://workstation.lan:7099", "aria-label": "Sharing address" });
    const message = el("span", { class: "source-form-error", role: "status", hidden: true });
    return el("form", { class: "sharing-form", onSubmit: async event => {
        event.preventDefault();
        const button = event.currentTarget.querySelector("button[type=submit]");
        button.disabled = true;
        message.hidden = true;
        try {
            app.state.setup = await api.saveSetup({ node_name: name.value.trim(), public_url: url.value.trim() });
            await app.refreshIndex();
            message.textContent = app.state.setup.restart_required
                ? "Address applied. Restart Hoard to use the saved node name." : "Sharing address saved and applied.";
            message.classList.remove("source-form-error");
            message.classList.add("mono-dim");
        } catch (error) {
            message.textContent = error.status === 401 ? "Owner access required. Enter the node's write token below and try again." : error.message;
            message.classList.add("source-form-error");
        } finally {
            button.disabled = false;
            message.hidden = false;
        }
    } },
        el("div", { class: "sharing-fields" },
            el("label", null, el("span", { text: "NODE NAME" }), name),
            el("label", null, el("span", { text: "SHARING ADDRESS" }), url)),
        el("p", { class: "setup-note", text: "This address is used in copied links and download commands. Changes apply immediately. Node name changes require a restart. Saving does not change the listening address or network settings." }),
        el("div", { class: "source-form-foot" }, message,
            el("button", { class: "btn btn-tiny", type: "submit", text: "SAVE SETTINGS" })));
}

function ownerAccess(app) {
    if (!app.state.setup.write_token_required) return el("p", { class: "setup-note", text: "No write token is configured. Anyone who can reach this node can change its catalog. HOARD_NODE_TOKEN restricts changes; browsing and downloads remain public." });
    const input = el("input", { class: "field", name: "owner_token", type: "password",
        autocomplete: "off", "aria-label": "Owner write token", placeholder: "Node write token", required: true });
    const status = el("span", { class: "mono-dim", role: "status", text: app.state.ownerTokenSet ? "Token set for this tab." : "Owner access required to add folders or change settings." });
    return el("form", { class: "owner-access", onSubmit: event => {
        event.preventDefault();
        api.setWriteToken(input.value);
        input.value = "";
        app.state.ownerTokenSet = true;
        status.textContent = "Token set for this tab. Retry your change to verify access.";
        input.closest("details").querySelector("summary").textContent = "SHARING & SETUP";
    } }, el("p", { class: "setup-note", text: "Browsing and downloads are public. Enter HOARD_NODE_TOKEN to make changes. It stays in this tab's memory and is cleared on reload." }),
        el("div", { class: "owner-access-fields" }, input,
            el("button", { class: "btn btn-tiny", type: "submit", text: "USE TOKEN" })), status);
}

export function renderSharing(app) {
    if (app.state.setupError) return el("section", { id: "source-sharing" },
        errorPanel(app.state.setupError, () => loadSetup(app)));
    if (!app.state.setup) return el("section", { id: "source-sharing" });
    const setup = app.state.setup;
    return el("details", { id: "source-sharing", class: "sharing-panel" },
        el("summary", { text: setup.write_token_required && !app.state.ownerTokenSet
            ? "SHARING & SETUP / OWNER ACCESS REQUIRED" : "SHARING & SETUP" }),
        el("div", { class: "sharing-active" },
            el("p", { class: "sharing-status", role: "status", text: sharingStatus(setup) }),
            el("p", { class: "mono-dim sharing-address", text: `ACTIVE ADDRESS ${setup.public_url} · LISTENING ${setup.bind}` })),
        sharingForm(app), ownerAccess(app));
}

export function paintSetup(app, view) {
    const welcomeKey = JSON.stringify([app.state.setup?.resource_count,
        app.state.setup?.configured_sources, app.state.setup?.scanning,
        (app.state.jobs || []).some(job => job.kind === "scan" && !job.finished)]);
    const keepWelcomeWhileEditing = app.state.sourceFormOpen
        && view.querySelector("#source-welcome") && app.state.setup?.resource_count > 0;
    if (view.setupWelcomeKey !== welcomeKey && !keepWelcomeWhileEditing) {
        const welcome = renderWelcome(app);
        const current = view.querySelector("#source-welcome");
        if (current && welcome) current.replaceWith(welcome);
        else if (current) current.remove();
        else if (welcome) view.querySelector(".operations-top").after(welcome);
        view.setupWelcomeKey = welcomeKey;
    }
    const sharing = view.querySelector("#source-sharing");
    if (!sharing.querySelector(".sharing-form")) {
        sharing.replaceWith(renderSharing(app));
    } else if (app.state.setup) {
        sharing.querySelector(".sharing-status").textContent = app.state.setupError
            ? "Could not refresh sharing status. Your unsaved input is preserved. Retry by reopening Sources."
            : sharingStatus(app.state.setup);
        sharing.querySelector(".sharing-address").textContent =
            `ACTIVE ADDRESS ${app.state.setup.public_url} · LISTENING ${app.state.setup.bind}`;
    }
}

export function renderCatalogEmpty(app) {
    const setup = app.state.setup;
    const query = app.state.q;
    const scanning = setup?.scanning;
    const title = query ? "NO RESOURCE MATCHES THIS QUERY" : scanning ? "INDEXING YOUR FOLDERS" : "YOUR CATALOG IS EMPTY";
    const body = query ? "Clear this search to see all indexed resources. Manage folders and scans in Sources."
        : scanning ? "You can keep browsing while Hoard scans. Follow progress in Sources."
            : setup?.configured_sources ? "Folders are configured, but no resources are indexed yet. Check Activity for scan results, or add a folder with models, datasets or Git repositories."
                : "Add a folder on this machine to start finding and sharing your resources. Your files stay where they are.";
    return el("div", { class: "empty catalog-empty" },
        el("div", { class: "empty-title", text: title }),
        el("div", { class: "empty-body", text: body }),
        el("div", { class: "setup-empty-actions" },
            query ? el("button", { class: "btn", type: "button", text: "CLEAR SEARCH", onClick: () => app.commitQuery("", { echo: true }) }) : null,
            el("button", { class: "btn", type: "button", text: setup?.configured_sources ? "REVIEW SOURCES" : "ADD FOLDER",
                onClick: () => setup?.configured_sources ? app.openSources() : app.openSourceForm() })));
}
