const BASE = "/api/v1";
let writeToken = "";

export function setWriteToken(token) {
    writeToken = token;
}

export class ApiError extends Error {
    constructor(code, message, status) {
        super(message);
        this.name = "ApiError";
        this.code = code;
        this.status = status;
    }
}

function query(params) {
    const search = new URLSearchParams();
    for (const [key, value] of Object.entries(params || {})) {
        if (value === null || value === undefined || value === "") continue;
        search.set(key, String(value));
    }
    const text = search.toString();
    return text ? `?${text}` : "";
}

function segment(value) {
    return encodeURIComponent(String(value));
}

async function request(method, path, body) {
    let response;
    try {
        response = await fetch(BASE + path, {
            method,
            headers: {
                ...(body === undefined ? {} : { "content-type": "application/json" }),
                ...(method !== "GET" && writeToken ? { authorization: `Bearer ${writeToken}` } : {})
            },
            body: body === undefined ? undefined : JSON.stringify(body)
        });
    } catch {
        throw new ApiError("NodeUnreachable", `cannot reach this hoard node at ${BASE}`, 0);
    }
    const text = await response.text();
    let payload = null;
    if (text) {
        try {
            payload = JSON.parse(text);
        } catch {
            payload = null;
        }
    }
    if (!response.ok) {
        const detail = payload && payload.error ? payload.error : null;
        throw new ApiError(
            (detail && detail.code) || `Http${response.status}`,
            (detail && detail.message) || `${method} ${path} failed with ${response.status}`,
            response.status
        );
    }
    return payload;
}

const get = (path, params) => request("GET", path + query(params));
const post = (path, body) => request("POST", path, body ?? {});
const put = (path, body) => request("PUT", path, body ?? {});
const del = (path) => request("DELETE", path);

const repoBase = (type, org, name) =>
    `/repos/${segment(type)}/${segment(org || "-")}/${segment(name)}`;

export const search = (params) => get("/search", params);
export const projects = (params) => get("/projects", params);
export const parseQuery = (q) => get("/search/parse", { q });

export const artifact = (id, entry) => get(`/artifacts/${segment(id)}`, { entry });
export const artifactCompat = (id, limit) => get(`/artifacts/${segment(id)}/compat`, { limit });
export const pullCommand = (id, entry) => get(`/artifacts/${segment(id)}/pull-command`, { entry });
export const confirmClassification = (id, body) => post(`/triage/${segment(id)}/confirm`, body);
export const rejectClassification = (id) => post(`/triage/${segment(id)}/reject`);
export const clearClassification = (id) => del(`/artifacts/${segment(id)}/override`);

export const repo = (type, org, name, artifact, entry) => get(repoBase(type, org, name), { artifact, entry });
export const repoFiles = (type, org, name, revision, selection = {}) =>
    get(`${repoBase(type, org, name)}/files`, { revision, ...selection });
export const repoReadme = (type, org, name, revision, selection = {}) =>
    get(`${repoBase(type, org, name)}/readme`, { revision, ...selection });
export const repoCompat = (type, org, name, revision, selection = {}) =>
    get(`${repoBase(type, org, name)}/compat`, { revision, ...selection });
export const repoUsage = (type, org, name) => get(`${repoBase(type, org, name)}/usage`);

export const shelves = () => get("/shelves");
export const createShelf = (name, q) => post("/shelves", { name, query: q });
export const deleteShelf = (id) => del(`/shelves/${segment(id)}`);

export const loadouts = () => get("/loadouts");
export const loadout = (id) => get(`/loadouts/${segment(id)}`);
export const createLoadout = (name) => post("/loadouts", name ? { name } : {});
export const renameLoadout = (id, name) => put(`/loadouts/${segment(id)}`, { name });
export const deleteLoadout = (id) => del(`/loadouts/${segment(id)}`);

export const nodes = () => get("/nodes");
export const sources = () => get("/sources");
export const createSource = (source) => post("/sources", source);
export const deleteSource = (name) => del(`/sources/${segment(name)}`);
export const startScan = (source) => post("/scan", { source: source ?? null, hash: "later" });
export const jobs = (state) => get("/jobs", { state });
export const job = (id) => get(`/jobs/${segment(id)}`);
export const cancelJob = (id) => post(`/jobs/${segment(id)}/cancel`);
export const deleteJob = (id) => del(`/jobs/${segment(id)}`);
export const clearScanJobs = () => del("/jobs?kind=scan");

export const settings = () => get("/settings");
export const saveSettings = (body) => put("/settings", body);
export const setup = () => get("/setup");
export const saveSetup = (body) => put("/setup", body);


export const addLoadoutMembers = (id, models) =>
    post(`/loadouts/${segment(id)}/members`, {
        artifact_ids: models.map(model => typeof model === "string" ? model : model.artifact_id),
        entry_ids: Object.fromEntries(models.filter(model => model.entry_id)
            .map(model => [model.artifact_id, model.entry_id]))
    });
export const editLoadoutMember = (id, item, destination) =>
    put(`/loadouts/${segment(id)}/items`, { repo: item.repo, path: item.path, destination });
export const removeLoadoutMember = (id, item) =>
    del(`/loadouts/${segment(id)}/items?${new URLSearchParams({repo: item.repo, path: item.path})}`);
export const loadoutDownload = (id, format, root) =>
    get(`/loadouts/${segment(id)}/download`, { format, root });
