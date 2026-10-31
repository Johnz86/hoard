import * as api from "./api.js";
import { el, toast } from "./dom.js";

const ROLES = [
    ["ckpt", "Checkpoint"], ["lora", "LoRA"], ["ctrl", "ControlNet"],
    ["upsc", "Upscaler"], ["vae", "VAE"], ["emb", "Embedding"],
    ["llm", "Language model"], ["ds", "Dataset"], ["proj", "Projector"],
    ["unk", "Unclassified"]
];

function reviewState(app, detail) {
    if (app.state.classificationReview?.id !== detail.artifact_id) {
        app.state.classificationReview = {
            id: detail.artifact_id, role: detail.role, base: detail.base || "",
            open: false, evidenceOpen: false, pending: false, rejectArmed: false, error: null
        };
    }
    return app.state.classificationReview;
}

function reviewReason(detail) {
    if (detail.incomplete) {
        return `${detail.shard_count} of ${detail.shard_total ?? "unknown"} shards present. `
            + "Classification cannot complete a missing shard set.";
    }
    if (detail.role === "unk") return "Role is unclassified. Choose a role to resolve it.";
    const confidence = Math.round(detail.role_confidence * 100);
    return detail.triage ? `Role confidence ${confidence}% needs review.`
        : detail.override_applied ? "A saved user decision is active."
        : `Inferred role confidence: ${confidence}%.`;
}

function evidenceList(detail, review) {
    const evidence = (detail.evidence || []).filter((item) =>
        ["role", "base", "sub_role"].includes(item.field));
    return el("details", { class: "classification-evidence", open: review.evidenceOpen },
        el("summary", { text: "INFERENCE EVIDENCE" }),
        evidence.length ? evidence.map((item) => el("div", { class: "classification-evidence-row" },
            el("span", { text: `${item.field}: ${item.value} · `
                + `${item.source.replaceAll("_", " ")} · ${Math.round(item.confidence * 100)}%` }),
            item.detail ? el("div", { class: "mono-dim", text: item.detail }) : null))
            : el("p", { class: "mono-dim", text: "No classification evidence was recorded." }));
}

async function saveReview(app, review, action) {
    if (review.pending) return;
    review.pending = true;
    review.error = null;
    app.paintInspector();
    try {
        if (action === "confirm") {
            await api.confirmClassification(review.id, {
                role: review.role, base: review.base.trim() || null
            });
        } else if (action === "clear") {
            await api.clearClassification(review.id);
        } else {
            await api.rejectClassification(review.id);
        }
    } catch (error) {
        review.error = error;
        review.pending = false;
        if (app.state.classificationReview === review) app.paintInspector();
        return;
    }
    if (app.state.classificationReview === review) {
        app.state.classificationReview = null;
        app.state.cursorArtifact = null;
    }
    const selected = app.rowKey(app.cursorRow());
    await app.refreshIndex(selected);
    toast(action === "confirm" ? "Classification saved"
        : action === "clear" ? "Override cleared · facts update when files are re-inferred"
        : "Marked as noise · future scans skip its paths; files stay on disk");
}

export function renderClassification(app, detail) {
    const review = reviewState(app, detail);
    const role = el("select", { id: "classification-role", class: "field", name: "role",
        onChange: (event) => { review.role = event.target.value; } },
        ROLES.map(([value, label]) => el("option", {
            value, text: label, selected: review.role === value
        })));
    const base = el("input", { id: "classification-base", class: "field",
        name: "base", type: "text",
        value: review.base, placeholder: "e.g. sdxl", spellcheck: "false",
        onInput: (event) => { review.base = event.target.value; } });
    const error = review.error;
    return el("details", { class: "classification-review", open: review.open,
        onKeyDown: (event) => {
            if (event.key !== "Escape") return;
            event.preventDefault();
            event.stopPropagation();
            const panel = event.currentTarget;
            review.open = false;
            panel.open = false;
            panel.querySelector("summary").focus();
        } },
        el("summary", { id: "classification-summary", text: "REVIEW CLASSIFICATION" }),
        el("p", { class: "mono-dim", text: reviewReason(detail) }),
        evidenceList(detail, review),
        el("form", { onSubmit: (event) => {
            event.preventDefault();
            saveReview(app, review, "confirm");
        } },
            el("fieldset", { disabled: review.pending },
                el("label", { for: "classification-role" }, "ROLE", role),
                el("label", { for: "classification-base" }, "MODEL FAMILY", base),
                el("p", { class: "mono-dim", text: "Blank keeps the current family. "
                    + "Unclassified roles remain in triage." }),
                el("button", { id: "classification-confirm", class: "btn btn-primary",
                    type: "submit",
                    text: "CONFIRM CLASSIFICATION" }),
                detail.override_applied ? el("button", { id: "classification-clear", class: "btn",
                    type: "button", text: "CLEAR OVERRIDE",
                    onClick: () => saveReview(app, review, "clear") }) : null,
                detail.override_applied ? el("p", { class: "mono-dim",
                    text: "Clearing removes the saved decision. "
                        + "Current facts remain until the files are re-inferred." }) : null,
                review.rejectArmed ? [
                    el("p", { class: "mono-dim", text: "Skip all indexed paths for this artifact "
                        + "on future scans. Files stay on disk." }),
                    el("button", { id: "classification-reject", class: "btn", type: "button",
                        text: "CONFIRM REJECT",
                        onClick: () => saveReview(app, review, "reject") }),
                    el("button", { class: "btn", type: "button", text: "CANCEL",
                        onClick: () => {
                            review.rejectArmed = false;
                            app.paintInspector();
                            document.getElementById("classification-reject")?.focus();
                        } })
                ] : el("button", { id: "classification-reject", class: "btn", type: "button",
                    text: "REJECT AS NOISE",
                    onClick: () => { review.rejectArmed = true; app.paintInspector(); } })
            ),
            el("div", { class: error ? "source-form-error" : "mono-dim", role: "status" },
                review.pending ? "Saving classification…" : error?.status === 401
                    ? "Owner access is required. Set the node token in Sources, then retry."
                    : error?.message || "",
                error?.status === 401 ? el("a", { href: "#/sources", text: " Open Sources" })
                    : null)
        ));
}
