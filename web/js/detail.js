import * as api from "./api.js";
import { el, dash, errorPanel, fmtDay, fmtRate, highlightUrl, loadingPanel, sizeText, stateText } from "./dom.js";
import { factRows, renderRelatedLoadouts } from "./inspector.js";
import { renderCard } from "./readme.js";
import { roleClass, roleCode, stateClass } from "./table.js";

export const TABS = ["FILES", "REFS", "CARD", "COMPAT", "USAGE"];

const AMBER_SHARE = 0.7;
const FILE_STATE_LABEL = { present: "PRESENT", deduped: "DEDUPED", lfs_ptr: "LFS PTR" };

let revealed = null;

function key(detail) {
    return `${detail.type}/${detail.org}/${detail.name}@${detail.rev || ""}#${detail.aid || ""}@${detail.entry || ""}`;
}

async function fetchInto(app, slot, loader) {
    const detail = app.state.detail;
    const stamp = key(detail);
    if (detail.loaded[slot] === stamp) return;
    detail.loaded[slot] = stamp;
    detail.errors[slot] = null;
    try {
        detail[slot] = await loader();
        if (slot === "repo" && app.state.view === "detail" && app.state.detail === detail && !detail.aid
            && detail.repo.project_id) {
            app.openProject(detail.repo.project_id, detail.rev);
            return;
        }
    } catch (error) {
        detail.loaded[slot] = null;
        detail.errors[slot] = error;
    }
    if (app.state.view === "detail") app.render();
}

export function loadDetail(app) {
    const detail = app.state.detail;
    fetchInto(app, "repo", () => api.repo(detail.type, detail.org, detail.name, detail.aid, detail.entry));
    if (detail.aid) fetchInto(app, "artifact", () => api.artifact(detail.aid, detail.entry));
    loadTab(app);
}

export function loadTab(app) {
    const detail = app.state.detail;
    const { type, org, name, rev, aid, entry } = detail;
    if (detail.tab === "FILES") fetchInto(app, "files", () => api.repoFiles(type, org, name, rev, { artifact: aid, entry }));
    if (detail.tab === "CARD") fetchInto(app, "readme", () => api.repoReadme(type, org, name, rev, { artifact: aid, entry }));
    if (detail.tab === "COMPAT") fetchInto(app, "compat", () => api.repoCompat(type, org, name, rev, { artifact: aid, entry }));
    if (detail.tab === "USAGE") fetchInto(app, "usage", () => api.repoUsage(type, org, name));
}

function header(app, repo) {
    const detail = app.state.detail;
    return el(
        "div",
        { class: `detail-head ${roleClass(repo.role)}` },
        el("button", { class: "btn btn-back", type: "button", text: "← ESC", onClick: () => app.goIndex() }),
        el("span", { class: "role-mark role-mark-xl" }),
        el(
            "span",
            { class: "detail-title" },
            el("span", { class: "detail-org", text: `${detail.org}/` }),
            detail.name
        ),
        el("span", { class: "badge", text: repo.role_full || roleCode(repo) }),
        el("span", {
            class: `badge ${stateClass(repo.state)}`,
            text: repo.state_full || stateText(repo.state, repo.pull_progress)
        }),
        el("span", { class: "detail-meta", text: repo.meta || "" }),
        el("button", {
            class: "btn",
            type: "button",
            text: "+ LOADOUT",
            onClick: () => app.addToLoadout({ artifact_id: detail.aid || repo.artifact_id, entry_id: detail.entry })
        })
    );
}

function tabStrip(app) {
    return el(
        "div",
        { class: "detail-tabs" },
        TABS.map((label) =>
            el("span", {
                class: `detail-tab${app.state.detail.tab === label ? " is-active" : ""}`,
                text: label,
                onClick: () => app.setDetailTab(label)
            })
        ),
        el("span", { class: "detail-tabs-fill" })
    );
}

function revisionBar(app, repo) {
    return el(
        "div",
        { class: "rev-bar" },
        el("span", { class: "rev-label", text: "REVISION" }),
        el("span", { class: "rev-chip", text: repo.revision_label || repo.revision || "" }),
        el("span", { class: "rev-meta", text: repo.revision_summary || "" }),
        el("span", {
            class: "rev-switch",
            text: "SWITCH ▾",
            onClick: () => app.setDetailTab("REFS")
        })
    );
}

function revealOpened(stamp) {
    if (revealed === stamp) return;
    revealed = stamp;
    requestAnimationFrame(() => {
        const row = document.querySelector(".file-row.is-opened");
        if (row) row.scrollIntoView({ block: "center" });
    });
}

function filesTab(app) {
    const detail = app.state.detail;
    if (detail.errors.files) return errorPanel(detail.errors.files, () => app.reloadDetailTab());
    if (!detail.files) return loadingPanel("reading the clone…");
    const rows = detail.files.rows || detail.files;
    revealOpened(key(detail));
    return el(
        "div",
        { class: "files" },
        rows.map((file) => {
            const state = String(file.state || "").toLowerCase();
            return el(
                "div",
                {
                    class: `file-row${file.opened ? " is-opened" : ""}`,
                    onClick: () => file.artifact_id && app.openArtifact(file)
                },
                el("span", { class: `file-path st-file-${state}`, text: file.path }),
                el("span", { class: "file-kind", text: dash(file.kind) }),
                el("span", { class: "file-size", text: sizeText(file) }),
                el("span", {
                    class: `file-state st-file-${state}`,
                    text: FILE_STATE_LABEL[state] || String(file.state || "").toUpperCase()
                })
            );
        })
    );
}

function refsTab(app, repo) {
    const refs = (repo && repo.refs) || [];
    return el(
        "div",
        { class: "refs" },
        refs.map((ref) =>
            el(
                "div",
                {
                    class: `ref-row${ref.current ? " is-current" : ""}`,
                    onClick: () => app.checkoutRef(ref)
                },
                el("span", { class: "ref-dot", text: ref.kind === "tag" ? "⊢" : "●" }),
                el("span", { class: "ref-name", text: ref.name }),
                el("span", { class: "ref-sha", text: String(ref.sha || "").slice(0, 7) }),
                el("span", { class: "ref-when", text: dash(ref.when) }),
                el("span", { class: "ref-kind", text: dash(ref.kind) })
            )
        ),
        el("div", {
            class: "refs-note",
            text: "Refs are read straight from the clone. Pinning a revision in a loadout stores the resolved sha, not the branch name."
        })
    );
}

function cardTab(app) {
    const detail = app.state.detail;
    if (detail.errors.readme) return errorPanel(detail.errors.readme, () => app.reloadDetailTab());
    if (!detail.readme) return loadingPanel("rendering the card…");
    return renderCard(detail.readme, detail.rev || "main");
}

function compatTab(app) {
    const detail = app.state.detail;
    if (detail.errors.compat) return errorPanel(detail.errors.compat, () => app.reloadDetailTab());
    if (!detail.compat) return loadingPanel("resolving compatibility…");
    const rows = detail.compat.rows || [];
    return el(
        "div",
        null,
        el(
            "div",
            { class: "compat-banner" },
            "RESOLVED FROM TENSOR KEYS AND RANK — NOT FROM THE MODEL CARD · ",
            el("span", { class: "amber", text: detail.compat.banner || detail.compat.line || "" })
        ),
        rows.map((item) =>
            el(
                "div",
                { class: `compat-row ${roleClass(item.role)}`, onClick: () => app.openArtifact(item) },
                el(
                    "span",
                    { class: "cell-role" },
                    el("span", { class: "role-mark" }),
                    el("span", { class: "role-code", text: roleCode(item) })
                ),
                el(
                    "span",
                    { class: "compat-name" },
                    item.prefix ? el("span", { class: "cell-org", text: item.prefix }) : null,
                    item.display_name || item.name
                ),
                el("span", { class: "compat-detail", text: dash(item.detail) }),
                el("span", { class: "compat-size", text: sizeText(item) }),
                el("span", {
                    class: `compat-state ${stateClass(item.state)}`,
                    text: stateText(item.state, item.pull_progress)
                })
            )
        ),
        rows.length ? null : el("div", { class: "refs-note", text: "Nothing on the LAN resolves against this artifact." })
    );
}

function usageTab(app) {
    const detail = app.state.detail;
    if (detail.errors.usage) return errorPanel(detail.errors.usage, () => app.reloadDetailTab());
    if (!detail.usage) return loadingPanel("reading pull log…");
    const days = detail.usage.days || [];
    const pulls = detail.usage.pulls || [];
    const peak = Math.max(1, ...days.map((day) => Number(day.pulls) || 0));
    const total = days.reduce((sum, day) => sum + (Number(day.pulls) || 0), 0);
    return el(
        "div",
        { class: "usage" },
        el(
            "section",
            null,
            el("div", { class: "rail-title", text: "LAN ACTIVITY — LAST 7 DAYS" }),
            el(
                "div",
                { class: "usage-chart" },
                days.map((day, index) => {
                    const share = (Number(day.pulls) || 0) / peak;
                    return el("span", {
                        class: `usage-bar${share >= AMBER_SHARE ? " is-hot" : index % 2 ? " is-odd" : ""}`,
                        style: { height: `${Math.max(2, Math.round(share * 100))}%` },
                        title: `${day.date}: ${day.pulls}`
                    });
                })
            ),
            el(
                "div",
                { class: "usage-foot" },
                el("span", { text: days.length ? fmtDay(days[0].date) : "" }),
                el("span", { text: `${total} pulls · ${detail.usage.clients ?? 0} clients` })
            )
        ),
        el(
            "section",
            null,
            el("div", { class: "rail-title", text: "RECENT PULLS" }),
            pulls.map((pull) =>
                el(
                    "div",
                    { class: "pull-row" },
                    el("span", { class: "pull-when", text: dash(pull.when) }),
                    el("span", { class: "pull-who", text: dash(pull.who) }),
                    el("span", { class: "pull-what", text: dash(pull.what) }),
                    el("span", { class: "pull-rate", text: fmtRate(pull.rate_bps) })
                )
            )
        )
    );
}

function tabPanel(app, repo) {
    const tab = app.state.detail.tab;
    if (tab === "FILES") return filesTab(app);
    if (tab === "REFS") return refsTab(app, repo);
    if (tab === "CARD") return cardTab(app);
    if (tab === "COMPAT") return compatTab(app);
    return usageTab(app);
}

function rightRail(app, repo) {
    const pull = repo.pull || {};
    return el(
        "aside",
        { class: "detail-rail" },
        el(
            "div",
            null,
            el("div", { class: "rail-title", text: "PULL TO THIS MACHINE" }),
            pullBox(pull),
            el("button", {
                class: "btn btn-wide",
                type: "button",
                text: "COPY",
                onClick: () => app.copyCommand(pull.git || pull.hf || "")
            })
        ),
        el(
            "div",
            null,
            el("div", { class: "rail-title", text: "DERIVED FACTS" }),
            el("div", { class: "fact-list" }, factRows(repo.facts))
        ),
        app.state.detail.aid ? renderRelatedLoadouts(app, app.state.detail.artifact?.loadouts, "detail") : null,
        el(
            "div",
            null,
            el("div", { class: "rail-title", text: "PATHS ON DISK" }),
            (repo.paths || []).map((entry) =>
                el(
                    "div",
                    { class: "disk-path" },
                    entry.path,
                    entry.blob_path
                        ? el("span", { class: "disk-blob", text: ` -> ${entry.blob_path}` })
                        : null,
                    el("span", {
                        class: "disk-node",
                        text: ` · ${entry.node}${entry.symlink ? " · symlink" : ""}`
                    })
                )
            )
        )
    );
}

function pullBox(pull) {
    const lines = [];
    if (pull.git) {
        lines.push(el("div", { class: "code-comment", text: "# git, refs preserved" }));
        lines.push(el("div", { class: "code-line", text: pull.git }));
        lines.push(el("div", { class: "code-comment", text: "# or python, unchanged" }));
    } else {
        lines.push(el("div", { class: "code-comment", text: "# python, unchanged" }));
    }
    lines.push(el("div", { class: "code-line" }, highlightUrl(pull.hf || "")));
    if (pull.repository) {
        lines.push(el("div", { class: "code-comment", text: "# whole repository" }));
        lines.push(el("div", { class: "code-line" }, highlightUrl(pull.repository)));
    }
    return el("div", { class: "code-box" }, lines);
}

export function renderDetail(app) {
    const detail = app.state.detail;
    if (detail.errors.repo) {
        return el("div", { class: "detail" }, errorPanel(detail.errors.repo, () => app.reloadDetail()));
    }
    if (!detail.repo) return el("div", { class: "detail" }, loadingPanel("opening repo…"));
    const repo = detail.repo;
    return el(
        "div",
        { class: "detail" },
        header(app, repo),
        el(
            "div",
            { class: "detail-body" },
            el(
                "div",
                { class: "detail-main" },
                tabStrip(app),
                revisionBar(app, repo),
                el("div", { class: "detail-panel" }, tabPanel(app, repo))
            ),
            rightRail(app, repo)
        )
    );
}
