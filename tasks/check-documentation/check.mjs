import assert from "node:assert/strict";
import { access, readdir, readFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { dirname, extname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const failures = [];
const obsolete = ["docs/handoff.md", "docs/feature-plan.md", "docs/finish-the-memory-work.md",
    "docs/superpowers/", "docs/design/handoff/", "docs/architecture.md", "docs/native-catalog.md",
    "docs/api/", "docs/first-run.md", "docs/introduction-video.md", "docs/use-case.md", "scripts/"];

async function walk(directory) {
    const files = [];
    for (const entry of await readdir(directory, { withFileTypes: true })) {
        const path = join(directory, entry.name);
        if (entry.isDirectory()) files.push(...await walk(path));
        else files.push(path);
    }
    return files;
}

function headings(text) {
    const ids = new Set();
    for (const line of text.split("\n")) {
        const match = /^#{1,6}\s+(.+?)\s*#*$/.exec(line);
        if (!match) continue;
        const base = match[1].toLowerCase().replace(/<[^>]*>/g, "")
            .replace(/[^\p{L}\p{N}_\-\s]/gu, "").replace(/\s/g, "-");
        let id = base;
        let duplicate = 0;
        while (ids.has(id)) id = `${base}-${++duplicate}`;
        ids.add(id);
    }
    return ids;
}

async function checkRepositoryPaths(text, label) {
    for (const match of text.matchAll(/`((?:docs|tasks|\.claude)\/[\w./-]+\.(?:md|mjs|json|wav|txt|mp4|gz))`/g)) {
        try { await access(join(root, match[1])); }
        catch { failures.push(`${label}: missing repository path ${match[1]}`); }
    }
}

const files = ["README.md", "CONTRIBUTING.md", "CLAUDE.md", "AGENTS.md"].map(name => join(root, name));
for (const directory of ["docs", "tasks", ".claude"]) files.push(...await walk(join(root, directory)));
const markdown = files.filter(path => extname(path) === ".md");
for (const entry of await readdir(join(root, "tasks"), { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    try { await access(join(root, "tasks", entry.name, "README.md")); }
    catch { failures.push(`tasks/${entry.name}: missing task README.md`); }
}
let links = 0;
for (const path of markdown) {
    const text = await readFile(path, "utf8");
    const label = relative(root, path);
    for (const old of obsolete) {
        const pattern = new RegExp(`(?<![\\w/])${old.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`);
        if (pattern.test(text)) failures.push(`${label}: obsolete path ${old}`);
    }
    const prose = text.replace(/^(`{3,}|~{3,})[^\n]*\n[\s\S]*?^\1\s*$/gm, "");
    for (const match of prose.matchAll(/\]\((<[^>]+>|[^)\n]+)\)/g)) {
        const target = match[1].startsWith("<") ? match[1].slice(1, -1) : match[1].split(/\s+"/)[0];
        if (/^[a-z][a-z\d+.-]*:/i.test(target)) continue;
        const [filename, fragment] = target.split("#");
        const linked = filename ? resolve(dirname(path), decodeURIComponent(filename)) : path;
        try {
            await access(linked);
            if (fragment && extname(linked) === ".md") {
                assert(headings(await readFile(linked, "utf8")).has(decodeURIComponent(fragment)), "missing heading");
            }
            links += 1;
        } catch (error) {
            failures.push(`${label}: ${target} (${error.message})`);
        }
    }
    await checkRepositoryPaths(text, label);
}
for (const directory of ["crates", "src"]) {
    for (const path of (await walk(join(root, directory))).filter(path => path.endsWith(".rs"))) {
        const comments = (await readFile(path, "utf8")).split("\n")
            .filter(line => /^\s*\/\/[!/]/.test(line)).join("\n");
        await checkRepositoryPaths(comments, relative(root, path));
    }
}
for (const path of files.filter(path => path.endsWith(".mjs"))) {
    const result = spawnSync(process.execPath, ["--check", path], { encoding: "utf8" });
    if (result.status !== 0) failures.push(`${relative(root, path)}: ${result.stderr || result.error}`);
}
if (failures.length) {
    process.stderr.write(`${failures.join("\n")}\n`);
    process.exitCode = 1;
} else {
    process.stdout.write(`Checked ${markdown.length} Markdown files, ${links} local links/anchors and task JavaScript syntax.\n`);
}
