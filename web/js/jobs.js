import * as api from "./api.js";
import { el, errorPanel, fmtAge, fmtBytes, fmtRate, loadingPanel, toast } from "./dom.js";

export async function loadJobs(app) {
    try {
        const list = await api.jobs();
        app.state.jobs = Array.isArray(list) ? list : list.jobs || [];
        app.state.jobsError = null;
    } catch (error) {
        app.state.jobsError = error;
    }
    app.render();
}

export async function loadSources(app) {
    try {
        const list = await api.sources();
        app.state.sources = Array.isArray(list) ? list : list.sources || [];
        app.state.sourcesError = null;
    } catch (error) {
        app.state.sourcesError = error;
    }
    app.render();
}

export function renderScansView(app) {
    if (app.state.sourcesError) return errorPanel(app.state.sourcesError, () => loadSources(app));
    if (!app.state.sources) return loadingPanel("reading sources…");
    return el(
        "div",
        { class: "panel-view" },
        el("div", { class: "panel-title", text: "SCANS" }),
        el(
            "div",
            { class: "panel-head scan-grid" },
            el("span", { text: "SOURCE" }),
            el("span", { text: "KIND" }),
            el("span", { class: "align-right", text: "FILES" }),
            el("span", { class: "align-right", text: "BLOBS" }),
            el("span", { class: "align-right", text: "BYTES" }),
            el("span", { class: "align-right", text: "PENDING" }),
            el("span", { class: "align-right", text: "LAST SCAN" }),
            el("span", { class: "align-right", text: "" })
        ),
        app.state.sources.map((source) =>
            el(
                "div",
                { class: "panel-row scan-grid" },
                el("span", { text: source.path || source.name || source.id }),
                el("span", { class: "mono-dim", text: source.kind || "" }),
                el("span", { class: "align-right", text: String(source.files ?? "—") }),
                el("span", { class: "align-right", text: String(source.blobs ?? "—") }),
                el("span", { class: "align-right", text: fmtBytes(source.bytes) }),
                el("span", { class: "align-right", text: String(source.pending_hashes ?? 0) }),
                el("span", { class: "align-right mono-dim", text: fmtAge(source.last_scan) }),
                el("span", { class: "align-right" }, scanAction(app, source))
            )
        )
    );
}

function scanAction(app, source) {
    if (!source.can_scan) {
        const label =
            source.managed_by === "nobody"
                ? "not configured"
                : `managed by ${source.managed_by}`;
        return el("span", { class: "mono-dim", text: label });
    }
    return el("button", {
        class: "btn btn-tiny",
        type: "button",
        text: "SCAN",
        onClick: async () => {
            try {
                await api.startScan(source.id ?? source.path);
                toast(`scan queued · ${source.path || source.id}`);
                loadJobs(app);
            } catch (error) {
                toast(`scan failed · ${error.message}`);
            }
        }
    });
}

export function renderJobsView(app) {
    if (app.state.jobsError) return errorPanel(app.state.jobsError, () => loadJobs(app));
    if (!app.state.jobs) return loadingPanel("reading jobs…");
    if (!app.state.jobs.length) {
        return el(
            "div",
            { class: "panel-view" },
            el("div", { class: "panel-title", text: "JOBS" }),
            el("div", { class: "rail-empty", text: "NO JOBS RUNNING" })
        );
    }
    return el(
        "div",
        { class: "panel-view" },
        el("div", { class: "panel-title", text: "JOBS" }),
        el(
            "div",
            { class: "panel-head job-grid" },
            el("span", { text: "KIND" }),
            el("span", { text: "PHASE" }),
            el("span", { text: "PROGRESS" }),
            el("span", { class: "align-right", text: "BYTES" }),
            el("span", { class: "align-right", text: "RATE" }),
            el("span", { class: "align-right", text: "STARTED" }),
            el("span", { class: "align-right", text: "" })
        ),
        app.state.jobs.map((job) => {
            const progress = Math.round((Number(job.progress) || 0) * 100);
            return el(
                "div",
                { class: "panel-row job-grid" },
                el("span", { text: job.kind || job.type || "job" }),
                el(
                    "span",
                    { class: job.error ? "st-stale" : "mono-dim" },
                    job.error ? job.error : job.phase || ""
                ),
                el(
                    "span",
                    { class: "job-progress" },
                    el("span", { class: "job-bar", style: { width: `${progress}%` } }),
                    el("span", { class: "job-pct", text: `${progress}%` })
                ),
                el("span", { class: "align-right", text: fmtBytes(job.bytes) }),
                el("span", { class: "align-right st-served", text: fmtRate(job.rate_bps) }),
                el("span", { class: "align-right mono-dim", text: fmtAge(job.started) }),
                el(
                    "span",
                    { class: "align-right" },
                    job.finished
                        ? null
                        : el("button", {
                              class: "btn btn-tiny",
                              type: "button",
                              text: "CANCEL",
                              onClick: async () => {
                                  try {
                                      await api.cancelJob(job.id);
                                      toast("job cancelled");
                                      loadJobs(app);
                                  } catch (error) {
                                      toast(`cancel failed · ${error.message}`);
                                  }
                              }
                          })
                )
            );
        })
    );
}
