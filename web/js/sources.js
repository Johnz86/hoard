import * as api from "./api.js";
import { el, errorPanel, fmtAge, fmtBytes, fmtRate, loadingPanel, mount, toast } from "./dom.js";
import { loadJobs, loadSources } from "./jobs.js";
import { loadNodes } from "./nodes.js";
import { renderProjectResults } from "./projects.js";

function section(title, content, id) {
    return el("section", { class: "operations-section", id },
        el("div", { class: "operations-heading", text: title }), content);
}

function scanAction(app, source) {
    return el("button", { class: "btn btn-tiny", type: "button", text: "SCAN",
        onClick: async (event) => {
            event.currentTarget.disabled = true;
            try {
                await api.startScan(source.name);
                toast(`scan queued · ${source.name}`);
                await loadJobs(app);
            } catch (error) {
                event.currentTarget.disabled = false;
                toast(`scan failed · ${error.message}`);
            }
        } });
}

function folderRow(app, source) {
    const job = (app.state.jobs || []).find((item) =>
        item.kind === "scan" && item.subject === source.name && !item.finished);
    const status = job
        ? `${job.phase || "Scanning"} ${Math.round((Number(job.progress) || 0) * 100)}%`
        : source.pending_hashes
            ? `${source.pending_hashes} hashes pending`
            : source.last_scan ? "Indexed" : "Not scanned";
    return el("div", { class: "panel-row source-grid" },
        el("div", { class: "source-identity" },
            el("span", { class: "source-name", text: source.name }),
            el("span", { class: "source-path", text: source.path, title: source.path }),
            source.kind === "git" ? el("button", { class: "btn btn-tiny source-browse",
                type: "button", text: "BROWSE PROJECTS",
                onClick: () => app.browseProjects(source.name) }) : null),
        el("span", { class: "mono-dim", text: source.kind }),
        el("span", { class: "align-right", text: String(source.files ?? 0) }),
        el("span", { class: "align-right", text: fmtBytes(source.bytes) }),
        el("span", { class: "align-right mono-dim", text: fmtAge(source.last_scan) }),
        el("span", { class: job ? "st-served" : "mono-dim", text: status,
            title: source.pending_hashes
                ? `${source.pending_hashes} files · ${fmtBytes(source.pending_bytes)} pending`
                : status }),
        el("span", { class: "align-right" }, job
            ? el("span", { class: "mono-dim", text: "RUNNING" })
            : scanAction(app, source)));
}

function folders(app) {
    if (app.state.sourcesError) {
        return section("FOLDERS ON THIS NODE",
            errorPanel(app.state.sourcesError, () => loadSources(app)), "source-folders");
    }
    if (!app.state.sources) {
        return section("FOLDERS ON THIS NODE", loadingPanel("reading folders…"), "source-folders");
    }
    const local = app.state.sources.filter((source) => source.can_scan);
    return section("FOLDERS ON THIS NODE", local.length ? el("div", null,
        el("div", { class: "panel-head source-grid" },
            ...["FOLDER", "LAYOUT", "FILES", "SIZE", "LAST SCAN", "STATUS", ""].map(
                (label) => el("span", { text: label }))
        ),
        local.map((source) => folderRow(app, source))
    ) : el("div", { class: "operations-empty", text: "No folders yet. Add one to start indexing." }),
    "source-folders");
}

function managedStorage(app) {
    const managed = (app.state.sources || []).filter((source) =>
        !source.can_scan && source.managed_by !== "peer");
    if (!managed.length) return null;
    return section("OTHER LOCAL STORAGE", managed.map((source) =>
        el("div", { class: "panel-row managed-source-row" },
            el("span", { class: "source-path", text: source.path || source.name }),
            el("span", { class: "mono-dim", text: source.managed_by === "nobody"
                ? "No longer configured" : `Managed by ${source.managed_by}` }),
            el("span", { class: "align-right", text: fmtBytes(source.bytes) }))),
    "source-managed");
}

function nodes(app) {
    if (app.state.nodesError) {
        return section("NODES", errorPanel(app.state.nodesError, () => loadNodes(app)),
            "source-nodes");
    }
    if (!app.state.nodes) {
        return section("NODES", loadingPanel("reading nodes…"), "source-nodes");
    }
    const ordered = [...app.state.nodes].sort((a, b) => Number(b.self) - Number(a.self));
    return section("NODES", el("div", null,
        el("div", { class: "panel-head operations-node-grid" },
            el("span", { text: "NODE" }), el("span", { text: "ADDRESS" }),
            el("span", { text: "INDEXED" }), el("span", { text: "LAST SEEN" })),
        ordered.map((node) => el("div", { class: "panel-row operations-node-grid" },
            el("span", { class: "operations-node-name" },
                el("span", { class: `node-dot${node.online ? " is-online" : ""}`, text: "●" }),
                el("span", { text: node.name }),
                node.self ? el("span", { class: "badge badge-inline", text: "THIS NODE" }) : null),
            el("span", { class: "mono-dim source-path", text: node.url || "—" }),
            el("span", { class: "align-right", text: fmtBytes(node.indexed_bytes) }),
            el("span", { class: "align-right mono-dim", text: fmtAge(node.last_seen) })))),
    "source-nodes");
}

function jobRow(app, job) {
    const progress = Math.round((Number(job.progress) || 0) * 100);
    return el("div", { class: "panel-row operations-job-grid" },
        el("span", { class: "source-identity" },
            el("span", { class: "source-name", text: job.subject ||
                (job.kind === "scan" ? "All folders" : job.kind) }),
            el("span", { class: "source-path", text: job.kind })),
        el("span", { class: job.error ? "st-stale" : "mono-dim",
            text: job.error || (job.finished ? job.state : job.phase || job.state) }),
        job.finished
            ? el("span", { class: "job-complete", text: `${progress}%` })
            : el("span", { class: "job-progress" },
                el("span", { class: "job-track" },
                    el("span", { class: "job-bar", style: { width: `${progress}%` } })),
                el("span", { class: "job-pct", text: `${progress}%` })),
        el("span", { class: "align-right", text: job.finished && !job.bytes
            ? "—" : `${fmtBytes(job.bytes)} · ${fmtRate(job.rate_bps)}` }),
        el("span", { class: "align-right mono-dim", text: fmtAge(job.started) }),
        el("span", { class: "align-right" }, job.finished ? null : el("button", {
            class: "btn btn-tiny", type: "button", text: "CANCEL",
            onClick: async () => {
                try {
                    await api.cancelJob(job.id);
                    toast("job cancelled");
                    await loadJobs(app);
                } catch (error) {
                    toast(`cancel failed · ${error.message}`);
                }
            } })));
}

function activity(app) {
    if (app.state.jobsError) {
        return section("ACTIVITY", errorPanel(app.state.jobsError, () => loadJobs(app)),
            "source-activity");
    }
    if (!app.state.jobs) {
        return section("ACTIVITY", loadingPanel("reading jobs…"), "source-activity");
    }
    const active = app.state.jobs.filter((job) => !job.finished);
    const recent = app.state.jobs.filter((job) => job.finished).slice(0, 8);
    const rows = active.length || recent.length ? el("div", null,
        el("div", { class: "panel-head operations-job-grid" },
            ...["ITEM", "PHASE", "PROGRESS", "WORK", "STARTED", ""].map((label) =>
                el("span", { text: label }))),
        active.map((job) => jobRow(app, job)),
        recent.length ? el("details", { class: "operations-history",
            open: app.state.recentJobsOpen,
            onToggle: (event) => { app.state.recentJobsOpen = event.currentTarget.open; } },
            el("summary", { text: `RECENT JOBS · ${recent.length}` }),
            recent.map((job) => jobRow(app, job))) : null
    ) : el("div", { class: "operations-empty", text: "No jobs yet." });
    return section(`ACTIVITY · ${active.length} ACTIVE`, rows, "source-activity");
}

function addFolderForm(app) {
    let nameEdited = false;
    const name = el("input", { class: "field", type: "text", name: "name",
        placeholder: "Folder name", "aria-label": "Folder name",
        onInput: () => { nameEdited = true; } });
    const path = el("input", { class: "field", type: "text", name: "path", id: "source-path",
        placeholder: "/path/on/this/node", "aria-label": "Folder path on this node",
        onInput: (event) => {
            if (!nameEdited) {
                name.value = event.currentTarget.value.replace(/\/+$/, "").split("/").pop();
            }
        } });
    const kind = el("select", { class: "field", name: "kind", "aria-label": "Folder layout" },
        ["auto", "loose", "hf-cache", "git"].map((value) =>
            el("option", { value, text: value === "auto" ? "Auto-detect layout" : value })));
    const message = el("span", { class: "source-form-error", hidden: true });
    return el("form", { class: "source-add-form", onSubmit: async (event) => {
        event.preventDefault();
        const button = event.currentTarget.querySelector("button[type=submit]");
        button.disabled = true;
        message.hidden = true;
        try {
            const source = await api.createSource({ path: path.value.trim(),
                name: name.value.trim(), kind: kind.value });
            app.state.sourceFormOpen = false;
            toast(`folder added · scanning ${source.name}`);
            await Promise.all([loadSources(app), loadJobs(app)]);
        } catch (error) {
            button.disabled = false;
            message.textContent = error.message;
            message.hidden = false;
        }
    } },
        el("div", { class: "operations-heading", text: "ADD A FOLDER ON THIS NODE" }),
        el("div", { class: "source-form-fields" },
            el("label", null, el("span", { text: "PATH ON THIS NODE" }), path),
            el("label", null, el("span", { text: "NAME" }), name),
            el("label", null, el("span", { text: "LAYOUT" }), kind)),
        el("div", { class: "source-form-foot" }, message,
            el("button", { class: "btn btn-amber", type: "submit", text: "ADD & SCAN" })));
}

function projectResults(app) {
    const source = app.state.sourceProjectName;
    if (!source) return el("div", { id: "source-projects", hidden: true });
    const contents = app.state.sourceProjectsError
        ? errorPanel(app.state.sourceProjectsError, () => app.loadSourceProjects())
        : app.state.sourceProjects
            ? renderProjectResults(app, app.state.sourceProjects) || el("div", {
                class: "operations-empty", text: "No Git projects found yet. Wait for the scan to finish or scan this folder again." })
            : loadingPanel("reading Git projects...");
    return section(`PROJECTS IN ${source}`, el("div", null,
        el("button", { class: "btn btn-tiny", type: "button", text: "CLOSE PROJECTS",
            onClick: () => app.browseProjects(null) }), contents), "source-projects");
}

function renderSourcesView(app) {
    const count = (app.state.sources || []).filter((source) => source.can_scan).length;
    const active = (app.state.jobs || []).filter((job) => !job.finished).length;
    return el("div", { class: "panel-view operations-view" },
        el("div", { class: "operations-top" },
            el("div", null,
                el("h1", { text: "Sources" }),
                el("p", { text: `${count} folders · ${(app.state.nodes || []).length} nodes · ${active} active jobs` })),
            el("button", { class: "btn btn-amber", type: "button",
                text: app.state.sourceFormOpen ? "CANCEL" : "+ ADD FOLDER",
                onClick: () => {
                    app.state.sourceFormOpen = !app.state.sourceFormOpen;
                    app.render();
                    if (app.state.sourceFormOpen) document.getElementById("source-path").focus();
                } })),
        app.state.sourceFormOpen ? addFolderForm(app) : null,
        folders(app), projectResults(app), managedStorage(app), nodes(app), activity(app));
}

function sourcesSnapshot(app) {
    const minute = Math.floor(Date.now() / 60000);
    const sources = JSON.stringify([app.state.sources, app.state.sourcesError?.message]);
    const activeScans = (app.state.jobs || []).filter((job) =>
        job.kind === "scan" && !job.finished);
    return {
        projects: JSON.stringify([app.state.sourceProjectName, app.state.sourceProjects,
            app.state.sourceProjectsError?.message]),
        folders: JSON.stringify([sources, activeScans, minute]),
        managed: sources,
        nodes: JSON.stringify([app.state.nodes, app.state.nodesError?.message, minute]),
        activity: JSON.stringify([app.state.jobs, app.state.jobsError?.message, minute])
    };
}

export function paintSourcesView(app, panel) {
    let view = panel.firstElementChild;
    if (!view?.classList.contains("operations-view")) {
        view = renderSourcesView(app);
        mount(panel, view);
        view.sourcesSnapshot = sourcesSnapshot(app);
        return;
    }

    const count = (app.state.sources || []).filter((source) => source.can_scan).length;
    const active = (app.state.jobs || []).filter((job) => !job.finished).length;
    view.querySelector(".operations-top p").textContent =
        `${count} folders · ${(app.state.nodes || []).length} nodes · ${active} active jobs`;
    view.querySelector(".operations-top button").textContent =
        app.state.sourceFormOpen ? "CANCEL" : "+ ADD FOLDER";

    const form = view.querySelector(".source-add-form");
    if (app.state.sourceFormOpen && !form) {
        view.querySelector(".operations-top").after(addFolderForm(app));
    } else if (!app.state.sourceFormOpen && form) {
        form.remove();
    }

    const next = sourcesSnapshot(app);
    const previous = view.sourcesSnapshot;
    if (next.projects !== previous.projects) {
        view.querySelector("#source-projects").replaceWith(projectResults(app));
    }
    if (next.folders !== previous.folders) {
        view.querySelector("#source-folders").replaceWith(folders(app));
    }
    if (next.managed !== previous.managed) {
        const managed = managedStorage(app);
        const current = view.querySelector("#source-managed");
        if (current && managed) current.replaceWith(managed);
        else if (current) current.remove();
        else if (managed) view.querySelector("#source-nodes").before(managed);
    }
    if (next.nodes !== previous.nodes) {
        view.querySelector("#source-nodes").replaceWith(nodes(app));
    }
    if (next.activity !== previous.activity) {
        const history = view.querySelector(".operations-history");
        if (history) app.state.recentJobsOpen = history.open;
        view.querySelector("#source-activity").replaceWith(activity(app));
    }
    view.sourcesSnapshot = next;
}
