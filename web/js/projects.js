import { ApiError } from "./api.js";
import { el, errorPanel, fmtBytes, loadingPanel } from "./dom.js";
import { renderCard } from "./readme.js";

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
    if (project.tab === "FILES" || project.path) {
        tasks.push(loadPart(app, request, "files", "/files", null));
        if (project.path) {
            tasks.push(loadPart(app, request, "preview", "/file", project.path));
        }
    } else {
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

function revisionPicker(app, project, detail) {
    const current = project.revision || detail.revision;
    const names = new Set((detail.refs || []).map((ref) => ref.name));
    if (current) names.add(current);
    const select = el("select", { class: "field", "aria-label": "Project revision",
        onChange: (event) => app.selectProjectRevision(event.currentTarget.value) },
    [...names].map((name) => el("option", { value: name, text: name, selected: name === current })));
    return el("label", { class: "project-revision" },
        el("span", { text: "REVISION" }), select,
        detail.revision_sha ? el("span", { class: "mono-dim",
            text: detail.revision_sha.slice(0, 12), title: detail.revision_sha }) : null);
}

function projectHeader(app, project, detail) {
    return el("header", { class: "project-header" },
        el("div", { class: "project-title-row" },
            el("button", { class: "btn", type: "button", text: "BACK TO INDEX",
                onClick: () => app.goIndex() }),
            el("div", { class: "project-heading" },
                el("span", { class: "project-kind", text: "GIT PROJECT" }),
                el("h1", { text: detail.name }),
                el("p", { text: `Available on ${detail.node} · source ${detail.source} · ${detail.files} files · ${detail.bytes_human || fmtBytes(detail.bytes)}` }))),
        el("div", { class: "project-share" },
            el("input", { class: "field project-clone", readonly: true,
                value: detail.clone_command || "Clone command unavailable",
                "aria-label": "Git clone command",
                onFocus: (event) => event.currentTarget.select() }),
            el("button", { class: "btn btn-amber", type: "button", text: "COPY CLONE",
                disabled: !detail.clone_command,
                onClick: () => app.copyCommand(detail.clone_command) }),
            el("button", { class: "btn", type: "button", text: "COPY PROJECT LINK",
                onClick: () => app.copyCommand(projectLink(project)) })),
        revisionPicker(app, project, detail));
}

function projectTabs(app, project) {
    return el("nav", { class: "project-tabs", "aria-label": "Project views" },
        ["README", "FILES"].map((tab) => el("button", {
            class: `btn${project.tab === tab ? " is-active" : ""}`, type: "button",
            text: tab, "aria-pressed": project.tab === tab,
            onClick: () => app.selectProjectTab(tab)
        })));
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
        preview.truncated ? el("p", { class: "project-preview-note",
            text: "Preview shortened. Download the file or clone the project for its full content." }) : null);
}

function filesTab(app, project) {
    if (project.errors?.files) return errorPanel(project.errors.files, () => loadProject(app));
    if (!project.files) return loadingPanel("Reading the project files...");
    const rows = project.files.rows || [];
    return el("div", { class: "project-browser" },
        el("nav", { class: "project-file-list", "aria-label": "Project files" },
            rows.length ? rows.map((file) => el("button", {
                class: `project-file${file.path === project.path ? " is-active" : ""}`,
                type: "button", "aria-current": file.path === project.path ? "true" : null,
                title: file.path, onClick: () => app.openProjectFile(file.path)
            }, el("span", { text: file.path }),
            el("span", { class: "mono-dim", text: file.size_human || fmtBytes(file.size) })))
                : el("div", { class: "project-empty", text: "This revision has no files." })),
        filePreview(app, project));
}

export function renderProject(app) {
    const project = app.state.project;
    if (project?.errors?.detail || project?.error) {
        return errorPanel(project.errors?.detail || project.error, () => loadProject(app));
    }
    if (!project?.detail) return loadingPanel("Opening the Git project...");
    return el("article", { class: "project-view" },
        projectHeader(app, project, project.detail),
        projectTabs(app, project),
        el("div", { class: "project-content" },
            project.tab === "FILES" || project.path ? filesTab(app, project) : readmeTab(app, project)));
}

export function renderProjectResults(app, projects) {
    const rows = Array.isArray(projects) ? projects : projects?.rows || [];
    if (!rows.length) return null;
    return el("section", { class: "project-results", "aria-label": "Git projects",
        onKeydown: (event) => {
            if (event.key === "Enter" || event.key === " ") event.stopPropagation();
        } },
        el("div", { class: "project-results-heading", text: `GIT PROJECTS · ${projects.total ?? rows.length}` }),
        rows.map((project) => el("div", { class: "project-result" },
            el("button", { class: "project-result-open", type: "button",
                onClick: () => app.openProject(project.id) },
                el("span", { class: "project-result-name", text: project.name }),
                project.description ? el("span", { class: "project-result-description",
                    text: project.description }) : null,
                el("span", { class: "project-result-meta",
                    text: `Git project${project.available === false ? " (unavailable)" : ""} · ${project.source} · ${project.node} · ${project.revision} · ${project.files} files · ${project.bytes_human || fmtBytes(project.bytes)}` })),
            project.clone_command ? el("button", { class: "btn btn-tiny", type: "button",
                text: "COPY CLONE", "aria-label": `Copy clone command for ${project.name}`,
                disabled: project.available === false,
                onClick: () => app.copyCommand(project.clone_command) }) : null)),
        projects.next_cursor && app.loadMoreProjects ? el("button", { class: "btn", type: "button",
            text: "MORE PROJECTS", onClick: () => app.loadMoreProjects() }) : null);
}
