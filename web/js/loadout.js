import * as api from "./api.js";
import { el, errorPanel, fmtRate, loadingPanel, mount, toast } from "./dom.js";
import { roleClass, roleCode } from "./table.js";
import { settings, updateSettings } from "./theme.js";

export async function loadApplyProfiles(app, retry = false) {
    if (app.state.loadoutProfilesLoading || (app.state.loadoutProfiles && !retry)) return;
    app.state.loadoutProfilesLoading = true;
    app.state.loadoutProfilesError = null;
    paintApplyStatus(app);
    try {
        const info = await api.loadoutProfiles();
        if (!info.node || !info.profiles?.length ||
            info.profiles.some(profile => !profile.id || !profile.label || !profile.destination) ||
            !info.profiles.some(profile => profile.id === info.default_profile)) {
            throw new Error("The server did not provide tool layouts.");
        }
        app.state.loadoutProfiles = info;
    } catch {
        app.state.loadoutProfiles = null;
        app.state.loadoutProfilesError = "Could not read tool layouts. Check the connection, then retry.";
    }
    app.state.loadoutProfilesLoading = false;
    paintApplyStatus(app);
    if (retry && app.state.loadoutProfiles) {
        document.getElementById("loadout-profile")?.focus();
    }
}

function selectedProfile(app) {
    const info = app.state.loadoutProfiles;
    if (!info) return null;
    const apply = app.state.loadoutApplies.get(app.state.loadoutId);
    const wanted = apply?.state === "running" ? apply.profile
        : settings.loadoutProfiles?.[app.state.loadoutId];
    return info.profiles.find(profile => profile.id === wanted)
        || info.profiles.find(profile => profile.id === info.default_profile);
}

function canApply(app) {
    return Boolean(app.state.loadout?.id === app.state.loadoutId && app.state.loadout
        && !app.state.loadoutError && loadoutItems(app).length
        && selectedProfile(app) && !app.state.loadoutProfilesLoading
        && !app.state.loadoutProfilesError
        && app.state.loadoutApplies.get(app.state.loadoutId)?.state !== "running");
}

export async function loadLoadout(app) {
    try {
        const list = await api.loadouts();
        const rows = Array.isArray(list) ? list : list.loadouts || [];
        app.state.loadouts = rows;
        const wanted = app.state.loadoutId || settings.loadoutId;
        const current = rows.find((row) => row.id === wanted) || rows[0] || null;
        app.state.loadoutId = current ? current.id : null;
        updateSettings({ loadoutId: app.state.loadoutId });
        app.state.loadout = current ? await api.loadout(current.id) : null;
        app.state.loadoutError = null;
    } catch (error) {
        app.state.loadoutError = error;
    }
    app.render();
}

export function loadoutItems(app) {
    const doc = app.state.loadout;
    if (!doc) return [];
    return doc.items || (doc.document && doc.document.items) || [];
}

function weightLabel(item) {
    if (item.weight_label) return item.weight_label;
    if (item.weight === null || item.weight === undefined) return "—";
    return String(item.weight);
}

function applyLine(app, items) {
    const latest = app.state.loadoutApplies.get(app.state.loadoutId);
    const apply = latest?.profile === selectedProfile(app)?.id ? latest : null;
    if (apply) {
        if (apply.pollError) return `Status unavailable · ${apply.pollError} · checking again…`;
        if (apply.state === "done") return `Last apply complete · ${apply.phase}`;
        if (apply.state === "failed") return `Apply failed · ${apply.error}`;
        if (apply.state === "cancelled") return "Apply cancelled · some links may already exist";
        return `Applying · ${apply.phase}`;
    }
    if (!items.length) return "Pin an artifact before applying.";
    const preview = (app.state.loadout && app.state.loadout.apply_preview) || null;
    if (!preview) return `→ ${items.length} pinned`;
    const rate = preview.rate_bps ? ` (${fmtRate(preview.rate_bps)})` : "";
    return `→ ${preview.present ?? 0} present, ${preview.pulling ?? 0} pulling over LAN${rate}`;
}

function emptyState(name) {
    return el(
        "div",
        { class: "drawer-empty" },
        name ? "EMPTY LOADOUT " : "NO LOADOUT YET",
        name ? el("span", { class: "amber", text: name }) : null,
        el("br"),
        "a checkpoint and the adapters you use with it",
        el("br"),
        name ? "select artifacts in the index and press " : "press ",
        el("span", { class: "amber", text: "L" }),
        name ? ", or start another with " : " on a row to start one, or ",
        el("span", { class: "amber", text: "NEW" }),
        " below"
    );
}

function itemRows(app, items) {
    return items.map((item) =>
        el(
            "div",
            { class: `drawer-row ${roleClass(item.role)}` },
            el(
                "span",
                { class: "cell-role" },
                el("span", { class: "role-mark" }),
                el("span", { class: "role-code", text: roleCode(item) })
            ),
            el(
                "span",
                { class: "drawer-name" },
                item.prefix ? el("span", { class: "cell-org", text: item.prefix }) : null,
                item.name
            ),
            el("span", { class: "drawer-rev", text: item.revision || item.rev || "—" }),
            el("span", { class: "drawer-weight", text: weightLabel(item) }),
            el("span", {
                class: "drawer-x",
                text: "×",
                title: "remove",
                onClick: () => app.removeFromLoadout(item)
            })
        )
    );
}

function pinnedBlock(app, items) {
    if (app.state.loadoutError) return errorPanel(app.state.loadoutError, () => loadLoadout(app));
    if (!app.state.loadouts) return loadingPanel("reading loadouts…");
    if (!app.state.loadoutId) return emptyState(null);
    if (!app.state.loadout) return loadingPanel("reading loadout…");
    if (!items.length) return emptyState(app.state.loadout.name);
    return itemRows(app, items);
}

function listBlock(app) {
    const rows = app.state.loadouts || [];
    if (!rows.length) return null;
    return el(
        "div",
        { class: "drawer-list" },
        el("div", { class: "rail-title", text: "LOADOUTS" }),
        rows.map((row) => {
            const armed = app.state.loadoutDeleting === row.id;
            const current = row.id === app.state.loadoutId ? " is-current" : "";
            return el(
                "div",
                { class: `drawer-list-row${current}` },
                el("span", {
                    class: "link",
                    text: row.name,
                    onClick: () => app.switchLoadout(row.id)
                }),
                el("span", { class: "mono-dim", text: `${row.items} items` }),
                el("span", {
                    class: armed ? "drawer-confirm" : "row-x",
                    text: armed ? "CONFIRM" : "DELETE",
                    onClick: () => (armed ? app.deleteLoadout(row.id) : app.armDelete(row.id))
                })
            );
        })
    );
}

function newForm(app) {
    const field = el("input", {
        id: "loadout-new-name",
        class: "field",
        type: "text",
        placeholder: "name, or blank for loadout-N",
        spellcheck: "false",
        onKeyDown: (event) => {
            if (event.key === "Enter") app.newLoadout(field.value);
        }
    });
    return el(
        "div",
        { class: "drawer-new" },
        field,
        el("button", {
            class: "btn",
            type: "button",
            text: "NEW",
            onClick: () => app.newLoadout(field.value)
        })
    );
}

function nameField(app) {
    const doc = app.state.loadout;
    if (!doc) return el("span", { class: "drawer-slug", text: "none" });
    return el("input", {
        id: "loadout-name",
        class: "field drawer-rename",
        type: "text",
        value: doc.name,
        spellcheck: "false",
        onChange: (event) => app.renameLoadout(event.target.value)
    });
}

export function renderDrawer(app) {
    const root = document.getElementById("drawer-root");
    if (!app.state.drawerOpen) {
        root.replaceChildren();
        return;
    }
    const doc = app.state.loadout;
    const items = loadoutItems(app);
    const sameLoadout = root.dataset.loadoutId === (app.state.loadoutId || "");
    const active = root.contains(document.activeElement) ? document.activeElement : null;
    const focused = sameLoadout || active?.id === "loadout-close" ? active : null;
    const focusId = focused?.id;
    const selection = focused?.tagName === "INPUT"
        ? [focused.selectionStart, focused.selectionEnd] : null;
    const draft = focused?.value;
    const newName = sameLoadout ? root.querySelector("#loadout-new-name")?.value : null;
    const scroll = sameLoadout ? root.querySelector(".drawer-body")?.scrollTop : 0;

    mount(
        root,
        el(
            "div",
            { class: "scrim", onClick: () => app.closeDrawer() },
            el(
                "div",
                { class: "drawer", onClick: (event) => event.stopPropagation() },
                el(
                    "div",
                    { class: "drawer-head" },
                    el("span", { class: "drawer-title", text: "loadout" }),
                    nameField(app),
                    el("button", {
                        id: "loadout-close",
                        class: "btn btn-close",
                        type: "button",
                        text: "CLOSE",
                        onClick: () => app.closeDrawer()
                    })
                ),
                el(
                    "div",
                    { class: "drawer-body" },
                    pinnedBlock(app, items),
                    listBlock(app),
                    newForm(app)
                ),
                el(
                    "div",
                    { class: "drawer-foot" },
                    el("label", { for: "loadout-profile", class: "rail-title", text: "TOOL LAYOUT" }),
                    el("select", {
                        id: "loadout-profile", class: "field drawer-profile",
                        "aria-describedby": "loadout-destination",
                        onChange: (event) => {
                            if (app.state.loadoutApplies.get(app.state.loadoutId)?.state === "running") return;
                            const saved = settings.loadoutProfiles;
                            const profiles = saved && typeof saved === "object" && !Array.isArray(saved)
                                ? saved : {};
                            updateSettings({ loadoutProfiles: {
                                ...profiles, [app.state.loadoutId]: event.target.value
                            } });
                            paintApplyStatus(app);
                        }
                    }),
                    el("div", { id: "loadout-destination", class: "drawer-destination" }),
                    el("div", { class: "drawer-profile-error", role: "status" }),
                    el("button", {
                        id: "loadout-profile-retry",
                        class: "btn drawer-profile-retry", type: "button", text: "RETRY LAYOUTS",
                        onClick: () => loadApplyProfiles(app, true)
                    }),
                    el(
                        "div",
                        { class: "code-box" },
                        el("div", { class: "code-comment drawer-apply-target" }),
                        el("div", { class: "drawer-apply-status", role: "status",
                            style: { overflowWrap: "anywhere" },
                            "aria-live": "polite" })
                    ),
                    el("button", {
                        id: "loadout-apply",
                        class: "btn btn-primary btn-wide drawer-apply",
                        type: "button",
                        text: "APPLY",
                        disabled: !doc,
                        onClick: () => applyNow(app)
                    })
                )
            )
        )
    );
    root.dataset.loadoutId = app.state.loadoutId || "";
    if (newName !== null && newName !== undefined) {
        root.querySelector("#loadout-new-name").value = newName;
    }
    paintApplyStatus(app);
    root.querySelector(".drawer-body").scrollTop = scroll || 0;
    const restored = focusId && document.getElementById(focusId);
    if (restored && !restored.disabled) {
        if (selection) restored.value = draft;
        restored.focus({ preventScroll: true });
        if (selection) restored.setSelectionRange(...selection);
    }
}

function paintApplyStatus(app) {
    const root = document.getElementById("drawer-root");
    const status = root.querySelector(".drawer-apply-status");
    const button = root.querySelector(".drawer-apply");
    if (!status || !button) return;
    const latest = app.state.loadoutApplies.get(app.state.loadoutId);
    const profile = selectedProfile(app);
    const apply = latest?.profile === profile?.id ? latest : null;
    const info = app.state.loadoutProfiles;
    const select = root.querySelector(".drawer-profile");
    const placeholder = app.state.loadoutProfilesError ? "Layouts unavailable" : "Loading layouts…";
    if (select.profileOptions !== info || (!info && select.options[0]?.text !== placeholder)) {
        select.replaceChildren(...(info ? info.profiles.map(option => el("option", {
            value: option.id, text: option.label
        })) : [el("option", { text: placeholder, value: "" })]));
        select.profileOptions = info;
    }
    if (select.value !== (profile?.id || "")) select.value = profile?.id || "";
    select.disabled = !app.state.loadout || !info || app.state.loadoutProfilesLoading
        || latest?.state === "running";
    const node = apply?.state === "running" ? apply.node : info?.node;
    const destination = apply?.state === "running" ? apply.destination : profile?.destination;
    const destinationRoot = root.querySelector(".drawer-destination");
    const destinationText = profile
        ? `DESTINATION ON ${node}\n${destination}\nLinks in role subfolders. Originals stay in place. This folder is on the server.`
        : app.state.loadoutProfilesError ? "Server destination unavailable."
            : "Reading the server destination…";
    if (destinationRoot.textContent !== destinationText) destinationRoot.textContent = destinationText;
    root.querySelector(".drawer-profile-error").textContent = app.state.loadoutProfilesError || "";
    root.querySelector(".drawer-profile-retry").hidden = !app.state.loadoutProfilesError;
    root.querySelector(".drawer-apply-target").textContent = profile
        ? `${profile.label} · ${node}` : "Tool layout unavailable";
    const line = applyLine(app, loadoutItems(app));
    if (status.textContent !== line) status.textContent = line;
    status.style.color = apply?.state === "failed" ? "var(--red)"
        : apply?.pollError || apply?.state === "running" || apply?.state === "cancelled"
            ? "var(--amb)" : "var(--green)";
    button.disabled = !canApply(app);
    button.textContent = apply?.state === "running" ? "APPLYING…"
        : profile ? `APPLY ${profile.label} TO ${node}` : "APPLY";
}

async function applyNow(app) {
    if (!canApply(app)) return;
    const id = app.state.loadoutId;
    const profile = selectedProfile(app);
    const node = app.state.loadoutProfiles.node;
    const apply = { state: "running", phase: "queuing job…", error: null, pollError: null,
        profile: profile.id, node, destination: profile.destination };
    const hadFocus = document.activeElement?.classList.contains("drawer-apply");
    app.state.loadoutApplies.set(id, apply);
    paintApplyStatus(app);
    if (hadFocus) document.getElementById("loadout-close")?.focus();
    try {
        const started = await api.applyLoadout(id, node, profile.id);
        apply.profile = started.profile;
        apply.node = started.node;
        apply.destination = started.destination;
        apply.phase = "job queued";
        while (apply.state === "running") {
            try {
                const job = await api.job(started.job_id);
                apply.state = job.state;
                apply.phase = job.phase || "job queued";
                apply.error = job.error;
                apply.pollError = null;
            } catch (error) {
                apply.pollError = error.message;
            }
            paintApplyStatus(app);
            if (apply.state === "running") {
                await new Promise((resolve) => setTimeout(resolve, 1000));
            }
        }
        toast(`apply ${apply.state === "done" ? "complete" : apply.state} on ${node}`);
    } catch (error) {
        apply.state = "failed";
        apply.error = error.message;
        toast(`apply failed on ${node}`);
    }
    paintApplyStatus(app);
}
