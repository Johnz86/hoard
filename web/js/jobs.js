import * as api from "./api.js";

export async function loadJobs(app, paint = true) {
    try {
        const result = await api.jobs();
        app.state.jobs = Array.isArray(result) ? result : result.jobs || [];
        app.state.jobsError = null;
    } catch (error) {
        app.state.jobsError = error;
    }
    if (paint) app.render();
}

export async function loadSources(app, paint = true) {
    try {
        const result = await api.sources();
        app.state.sources = Array.isArray(result) ? result : result.sources || [];
        app.state.sourcesError = null;
    } catch (error) {
        app.state.sourcesError = error;
    }
    if (paint) app.render();
}
