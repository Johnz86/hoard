import { ApiError } from "./api.js";
import { el, errorPanel, fmtBytes, loadingPanel } from "./dom.js";
import { renderCard } from "./readme.js";
import { factRows } from "./inspector.js";

const requests = new WeakMap();

function projectBase(project) {
    return `/api/v1/projects/${encodeURIComponent(project.id)}`;
}

function projectQuery(project, path) {
    const query = new URLSearchParams();
    if (project.revision) query.set("revision", project.revision);
    if (path) query.set("path", path);
    const text = query.toString();
    return text ? `?${text}` : "";
}

async function readProject(url, signal) {
    const response = await fetch(url, { signal });
    const body = await response.json();
    if (!response.ok) {
        throw new ApiError(body.error?.code || `Http${response.status}`,
            body.error?.message || "The project could not be read.", response.status);
    }
    return body;
}

async function loadPart(app, request, slot, suffix, path) {
    const project = request.project;
    const key = `${slot}:${path || ""}`;
    if (request.pending.has(key)) return;
    if (slot === "preview" ? project.preview?.path === path : request.loaded.has(key)) return;
    request.pending.add(key);
    delete project.errors[slot];
    try {
        const value = await readProject(projectBase(project) + suffix + projectQuery(project, path),
            request.controller.signal);
        if (app.state.project !== project || request.controller.signal.aborted) return;
        if (slot === "preview" && project.path !== path) return;
        project[slot] = value;
        request.loaded.add(key);
    } catch (error) {
        if (app.state.project !== project || request.controller.signal.aborted) return;
        if (slot === "preview" && project.path !== path) return;
        project.errors[slot] = error;
    } finally {
        request.pending.delete(key);
        if (app.state.project === project && !request.controller.signal.aborted) app.render();
    }
}

export async function loadProject(app) {
    const project = app.state.project;
    if (!project?.id) return;
    const key = `${project.id}@${project.revision || ""}`;
    let request = requests.get(app);
    if (request?.project !== project || request.key !== key) {
        request?.controller.abort();
        request = { project, key, controller: new AbortController(),
            pending: new Set(), loaded: new Set() };
        requests.set(app, request);
        project.detail = null;
        project.files = null;
        project.readme = null;
        project.preview = null;
        project.errors = {};
    }
    const tasks = [loadPart(app, request, "detail", "", null)];
    if (project.tab === "FILES") {
        tasks.push(loadPart(app, request, "files", "/files", null));
        if (project.path) {
            tasks.push(loadPart(app, request, "preview", "/file", project.path));
        }
    } else if (project.tab === "README") {
        tasks.push(loadPart(app, request, "readme", "/readme", null));
    }
    await Promise.all(tasks);
}

function projectLink(project) {
    const url = new URL(project.detail?.project_url || location.href);
    const query = new URLSearchParams();
    if (project.revision) query.set("rev", project.revision);
    if (project.path) query.set("path", project.path);
    if (project.tab && project.tab !== "README") query.set("tab", project.tab);
    const suffix = query.toString();
    url.hash = `/project/${encodeURIComponent(project.id)}${suffix ? `?${suffix}` : ""}`;
    return url.href;
}

function projectHeader(app, project, detail) {
    return el("header", { class: "detail-head r-git" },
        el("button", { class: "btn btn-back", type: "button", text: "← ESC",
            onClick: () => app.goIndex() }),
        el("span", { class: "role-mark role-mark-xl" }),
        el("span", { class: "detail-title", text: detail.name }),
        el("span", { class: "badge", text: "GIT PROJECT" }),
        el("span", { class: "badge st-local", text: "LOCAL" }),
        el("span", { class: "detail-meta", text: `${detail.source} · ${detail.files} files · ${detail.bytes_human || fmtBytes(detail.bytes)} · ${detail.node}` }));
}

function projectTabs(app, project) {
    return el("nav", { class: "detail-tabs", "aria-label": "Project views" },
        ["README", "FILES", "REFS"].map((tab) => el("button", {
            class: `detail-tab${project.tab === tab ? " is-active" : ""}`, type: "button",
            text: tab, "aria-pressed": project.tab === tab,
            onClick: () => app.selectProjectTab(tab)
        })), el("span", { class: "detail-tabs-fill" }));
}

function revisionBar(app, project, detail) {
    return el("div", { class: "rev-bar" },
        el("span", { class: "rev-label", text: "REVISION" }),
        el("span", { class: "rev-chip", text: project.revision || detail.revision }),
        el("span", { class: "rev-meta", text: `${String(detail.revision_sha || "").slice(0, 7)} · ${detail.files} files` }),
        el("button", { class: "rev-switch", type: "button", text: "SWITCH ▾",
            onClick: () => app.selectProjectTab("REFS") }));
}

function refsTab(app, project) {
    return el("div", { class: "refs" }, (project.detail.refs || []).map((ref) =>
        el("button", { class: `ref-row${ref.name === (project.revision || project.detail.revision) ? " is-current" : ""}`,
            type: "button", onClick: () => app.selectProjectRevision(ref.name) },
            el("span", { class: "ref-dot", text: ref.kind === "tag" ? "⊢" : "●" }),
            el("span", { class: "ref-name", text: ref.name }),
            el("span", { class: "ref-sha", text: String(ref.sha || "").slice(0, 7) }),
            el("span", { class: "ref-when", text: "" }),
            el("span", { class: "ref-kind", text: ref.kind }))));
}

function projectRail(app, project, detail) {
    return el("aside", { class: "detail-rail" },
        el("div", null, el("div", { class: "rail-title", text: "CLONE TO THIS MACHINE" }),
            el("div", { class: "code-box" }, el("div", { class: "code-line", text: detail.clone_command })),
            el("button", { class: "btn btn-wide btn-amber", type: "button", text: "COPY CLONE",
                onClick: () => app.copyCommand(detail.clone_command) }),
            el("button", { class: "btn btn-wide", type: "button", text: "COPY PROJECT LINK",
                onClick: () => app.copyCommand(projectLink(project)) })),
        el("div", null, el("div", { class: "rail-title", text: "PROJECT FACTS" }),
            el("div", { class: "fact-list" }, factRows([
                { key: "type", value: "Git project" }, { key: "source", value: detail.source },
                { key: "node", value: detail.node }, { key: "files", value: detail.files },
                { key: "size", value: detail.bytes_human }, { key: "revision", value: project.revision || detail.revision }
            ]))));
}

function readmeTab(app, project) {
    if (project.errors?.readme) {
        return errorPanel(project.errors.readme, () => loadProject(app));
    }
    const readme = project.readme;
    if (!readme) return loadingPanel("Reading the project README...");
    if (readme.missing || readme.message) {
        return el("div", { class: "project-empty", text: readme.message ||
            "This revision has no README. Browse Files to explore the project." });
    }
    const document = el("div", { class: "project-document" },
        renderCard(readme, readme.revision || project.detail.revision));
    document.addEventListener("click", (event) => {
        const link = event.target.closest("a[href]");
        if (!link || !document.contains(link)) return;
        const href = link.getAttribute("href");
        if (!href || href.startsWith("//") || /^[a-z][a-z\d+.-]*:/i.test(href)) return;
        if (href.startsWith("#")) {
            event.preventDefault();
            return;
        }
        try {
            const target = new URL(href, `https://project.invalid/${readme.path || "README.md"}`);
            const path = decodeURIComponent(target.pathname.slice(1));
            event.preventDefault();
            app.openProjectFile(path);
        } catch {
            event.preventDefault();
        }
    });
    return document;
}

function filePreview(app, project) {
    if (!project.path) {
        return el("div", { class: "project-empty", text: "Choose a file to read its content." });
    }
    const download = projectBase(project) + "/raw" + projectQuery(project, project.path);
    const header = el("div", { class: "project-preview-header" },
        el("button", { class: "btn btn-tiny", type: "button", text: "← FILES",
            onClick: () => app.openProjectFile("") }),
        el("span", { class: "project-file-path", text: project.path }),
        el("a", { class: "btn btn-tiny", href: download, text: "DOWNLOAD FILE" }));
    if (project.errors?.preview) {
        return el("section", { class: "project-preview" }, header,
            errorPanel(project.errors.preview, () => loadProject(app)));
    }
    const preview = project.preview?.path === project.path ? project.preview : null;
    if (!preview) {
        return el("section", { class: "project-preview" }, header,
            loadingPanel("Reading the selected file..."));
    }
    return el("section", { class: "project-preview", "aria-label": "File preview" }, header,
        preview.text !== null && preview.text !== undefined
            ? el("pre", { class: "project-source", tabindex: "0" },
                el("code", { text: preview.text }))
            : el("div", { class: "project-empty", text: preview.message ||
                "This file cannot be previewed as text. Download it or clone the project to inspect it." }),
        null);
}

function filesTab(app, project) {
    if (project.errors?.files) return errorPanel(project.errors.files, () => loadProject(app));
    if (!project.files) return loadingPanel("Reading the project files...");
    const rows = project.files.rows || [];
    if (project.path) return filePreview(app, project);
    return el("div", { class: "files project-file-list", "aria-label": "Project files" },
        rows.length ? rows.map((file) => el("button", {
            class: "file-row project-file", type: "button", title: file.path,
            onClick: () => app.openProjectFile(file.path)
        }, el("span", { class: "file-path", text: file.path }),
        el("span", { class: "file-kind", text: file.kind }),
        el("span", { class: "file-size", text: file.size_human || fmtBytes(file.size) }),
        el("span", { class: "file-state st-local", text: "LOCAL" })))
            : el("div", { class: "project-empty", text: "This revision has no files." }));
}

export function renderProject(app) {
    const project = app.state.project;
    if (project?.errors?.detail || project?.error) {
        return errorPanel(project.errors?.detail || project.error, () => loadProject(app));
    }
    if (!project?.detail) return loadingPanel("Opening the Git project...");
    return el("div", { class: "detail project-view" },
        projectHeader(app, project, project.detail),
        el("div", { class: "detail-body" },
            el("div", { class: "detail-main" }, projectTabs(app, project),
                revisionBar(app, project, project.detail),
                el("div", { class: "detail-panel project-content" },
                    project.tab === "FILES" ? filesTab(app, project)
                        : project.tab === "REFS" ? refsTab(app, project) : readmeTab(app, project))),
            projectRail(app, project, project.detail)));
}
