import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { access, copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import net from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const options = parseOptions(process.argv.slice(2));
const width = 1600;
const height = 900;
const ownedChildren = new Set();
const pause = (milliseconds) => new Promise(resolvePause => setTimeout(resolvePause, milliseconds));
const env = { ...process.env, RAYON_NUM_THREADS: "2", TOKIO_WORKER_THREADS: "2",
    GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_SYSTEM: "/dev/null", GIT_LFS_SKIP_SMUDGE: "1",
    GIT_TERMINAL_PROMPT: "0", GIT_AUTHOR_DATE: "2026-01-01T12:00:00Z",
    GIT_COMMITTER_DATE: "2026-01-01T12:00:00Z", NO_PROXY: "127.0.0.1,localhost", no_proxy: "127.0.0.1,localhost" };
delete env.HOARD_NODE_TOKEN;
delete env.HF_TOKEN;

function parseOptions(args) {
    const parsed = { output: join(root, "docs/media/hoard-introduction.mp4"),
        binary: join(root, "target/debug/hoard"), browser: process.env.HOARD_BROWSER || "/usr/bin/brave-browser",
        storyboard: join(root, "scripts/introduction-storyboard.json"), overwrite: false,
        verify: false, keepWork: false };
    for (let index = 0; index < args.length; index += 1) {
        const argument = args[index];
        if (argument === "--overwrite") parsed.overwrite = true;
        else if (argument === "--verify") parsed.verify = true;
        else if (argument === "--keep-work") parsed.keepWork = true;
        else if (argument === "--help") {
            process.stdout.write("Usage: node scripts/record-introduction.mjs [--verify] [--output FILE.mp4] [--binary FILE] [--browser FILE] [--storyboard FILE] [--overwrite] [--keep-work]\n");
            process.exit(0);
        } else if (["--output", "--binary", "--browser", "--storyboard"].includes(argument)) {
            const value = args[++index];
            if (!value || value.startsWith("--")) throw new Error(`Missing value for ${argument}`);
            parsed[argument.slice(2)] = resolve(value);
        } else throw new Error(`Unknown argument: ${argument}`);
    }
    if (!parsed.output.endsWith(".mp4")) throw new Error("Output must end in .mp4");
    return parsed;
}

async function exists(path) {
    try { await access(path); return true; }
    catch (error) { if (error.code === "ENOENT") return false; throw error; }
}

function ownedSpawn(command, args, options) {
    const child = spawn(command, args, { ...options, detached: process.platform !== "win32" });
    ownedChildren.add(child);
    return child;
}

function killOwned(child, signal) {
    try {
        if (process.platform === "win32") child.kill(signal);
        else process.kill(-child.pid, signal);
    } catch (error) {
        if (error.code !== "ESRCH") throw error;
    }
}

for (const signal of ["SIGINT", "SIGTERM"]) {
    process.once(signal, async () => {
        await Promise.allSettled([...ownedChildren].map(stop));
        process.stderr.write("Interrupted; stopped only processes launched by this recording.\n");
        process.exit(130);
    });
}

function run(command, args, cwd = root, diagnostics = false) {
    return new Promise((resolveRun, reject) => {
        const child = ownedSpawn("nice", ["-n", "10", command, ...args], { cwd, env, stdio: ["ignore", "pipe", "pipe"] });
        let output = "";
        let errorOutput = "";
        child.stdout.on("data", data => output += data);
        child.stderr.on("data", data => errorOutput += data);
        child.on("error", reject);
        child.on("exit", (code, signal) => {
            ownedChildren.delete(child);
            if (code === 0) resolveRun(diagnostics ? { output: output.trim(), diagnostics: errorOutput } : output.trim());
            else reject(new Error(`${command} exited ${code ?? signal}: ${errorOutput.slice(-5000)}`));
        });
    });
}

async function stop(child) {
    if (!child) return;
    if (child.exitCode !== null || child.signalCode) {
        killOwned(child, "SIGTERM");
        ownedChildren.delete(child);
        return;
    }
    await new Promise(resolveStop => {
        const timer = setTimeout(() => killOwned(child, "SIGKILL"), 5000);
        child.once("exit", () => { clearTimeout(timer); resolveStop(); });
        killOwned(child, "SIGTERM");
    });
    ownedChildren.delete(child);
}

async function freePort() {
    const socket = net.createServer();
    await new Promise((resolveListen, reject) => { socket.once("error", reject); socket.listen(0, "127.0.0.1", resolveListen); });
    const port = socket.address().port;
    await new Promise(resolveClose => socket.close(resolveClose));
    assert.notEqual(port, 8080);
    return port;
}

async function until(check, label, milliseconds = 20000) {
    const start = Date.now();
    while (Date.now() - start < milliseconds) {
        if (await check()) return;
        await pause(100);
    }
    throw new Error(`Timed out: ${label}`);
}

async function probe(path) {
    return JSON.parse(await run("ffprobe", ["-v", "error", "-show_format", "-show_streams", "-of", "json", path]));
}

function filterPath(path) {
    if (/[\n'\[\];,]/.test(path)) throw new Error(`Unsupported filter path: ${path}`);
    return path.replaceAll("\\", "/").replaceAll(":", "\\:");
}

function concatPath(path) {
    if (/[\n']/.test(path)) throw new Error(`Unsupported concat path: ${path}`);
    return `file '${path}'`;
}

function wrapped(text, maximum = 100) {
    const lines = [];
    for (const word of text.split(/\s+/)) {
        const last = lines.length - 1;
        if (last < 0 || lines[last].length + word.length + 1 > maximum) lines.push(word);
        else lines[last] += ` ${word}`;
    }
    return lines.join("\n");
}

function timestamp(seconds) {
    return new Date(Math.round(seconds * 1000)).toISOString().slice(11, 23);
}

async function fixtures(scratch, origin) {
    const project = join(scratch, "pose-tools");
    const models = join(scratch, "studio-models");
    await mkdir(join(project, "docs"), { recursive: true });
    await mkdir(join(project, "src"));
    await mkdir(models);
    const git = args => run("git", args, project);
    await git(["init", "--quiet", "--initial-branch=main"]);
    await git(["config", "user.name", "Hoard demo"]);
    await git(["config", "user.email", "demo@example.invalid"]);
    await git(["config", "commit.gpgsign", "false"]);
    await writeFile(join(project, "README.md"), "# Pose Tools\n\nA small toolkit for character pose exploration.\n\nFind and share reusable inverse-kinematics helpers with your team.\n\n[Read the getting started guide](docs/getting-started.md)\n\n## What is here?\n\n- Committed project documentation\n- A tiny Python pose helper\n- Resources you can review before cloning\n\nThis is a synthetic demonstration repository.\n");
    await writeFile(join(project, "docs/getting-started.md"), "# Getting started\n\nClone the repository from your Hoard node.\n\nThe source files and documentation travel together.\n\nThis fixture has no runtime dependencies.\n");
    await writeFile(join(project, "src/pose.py"), "def interpolate_pose(start: float, end: float, weight: float) -> float:\n    return start + (end - start) * weight\n");
    await git(["add", "."]);
    await git(["commit", "--quiet", "-m", "Add synthetic pose toolkit"]);
    const head = await git(["rev-parse", "HEAD"]);
    for (const [index, name] of ["portrait-light", "cinematic-grain", "studio-color"].entries()) {
        const rank = 16 + index;
        const header = Buffer.from(JSON.stringify({ __metadata__: { ss_network_dim: String(rank),
            ss_network_alpha: "8", ss_base_model_version: "sdxl_base_v1-0" },
            [`lora_unet_layer_${index}.lora_down.weight`]: { dtype: "F16", shape: [rank, 8], data_offsets: [0, rank * 16] },
            [`lora_unet_layer_${index}.lora_up.weight`]: { dtype: "F16", shape: [8, rank], data_offsets: [rank * 16, rank * 32] } }));
        const length = Buffer.alloc(8);
        length.writeBigUInt64LE(BigInt(header.length));
        await writeFile(join(models, `${name}.safetensors`), Buffer.concat([length, header, Buffer.alloc(rank * 32)]));
    }
    const config = join(scratch, "hoard.toml");
    await writeFile(config, `[server]\nbind = "127.0.0.1:${new URL(origin).port}"\npublic_url = "${origin}"\ndata_dir = "${scratch}/data"\n[node]\nname = "New node"\ndiscovery = "none"\n[index]\nhash_policy = "never"\nhash_threads = 2\nwatch = true\nreconcile_interval_secs = 0\n[mirror]\noffline = true\n`);
    return { project, models, config, head };
}

class Browser {
    constructor(socket) {
        this.socket = socket;
        this.next = 0;
        this.pending = new Map();
        this.exceptions = [];
        socket.onmessage = event => {
            const message = JSON.parse(event.data);
            if (message.method === "Runtime.exceptionThrown") this.exceptions.push(message.params.exceptionDetails);
            if (message.id) {
                const task = this.pending.get(message.id);
                this.pending.delete(message.id);
                if (message.error) task.reject(new Error(JSON.stringify(message.error)));
                else task.resolve(message.result);
            }
        };
        socket.onclose = () => {
            for (const task of this.pending.values()) task.reject(new Error("Browser connection closed"));
            this.pending.clear();
        };
    }
    send(method, params = {}) {
        return new Promise((resolveSend, reject) => {
            const id = ++this.next;
            this.pending.set(id, { resolve: resolveSend, reject });
            this.socket.send(JSON.stringify({ id, method, params }));
        });
    }
    async evaluate(expression) {
        const result = await this.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
        if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
        return result.result.value;
    }
    async wait(expression) {
        await until(() => this.evaluate(expression), expression);
    }
    async initialize() {
        await this.send("Runtime.enable");
        await this.send("Page.enable");
        await this.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
        await this.send("Page.addScriptToEvaluateOnNewDocument", { source: `addEventListener("DOMContentLoaded",()=>{
            const pointer=document.createElement("div");pointer.id="recording-pointer";
            Object.assign(pointer.style,{position:"fixed",left:"1540px",top:"860px",width:"14px",height:"14px",border:"2px solid #fa901e",borderRadius:"50%",pointerEvents:"none",zIndex:"2147483647",transform:"translate(-50%,-50%)",boxShadow:"0 0 0 3px #fa901e22"});
            document.body.append(pointer);
        })` });
    }
}

class Capture {
    constructor(browser, scratch, storyboard) {
        this.browser = browser;
        this.scratch = scratch;
        this.storyboard = storyboard;
        this.frames = [];
        this.scenes = [];
        this.position = { x: 1540, y: 860 };
    }
    async frame(duration = 0.25) {
        if (options.verify) return;
        const screenshot = await this.browser.send("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
        const path = join(this.scratch, `frame-${String(this.frames.length).padStart(5, "0")}.png`);
        await writeFile(path, Buffer.from(screenshot.data, "base64"));
        this.frames.push({ path, duration });
    }
    async scene(id, action) {
        const specification = this.storyboard.scenes.find(scene => scene.id === id);
        assert(specification, `Missing storyboard scene: ${id}`);
        process.stdout.write(`Scene: ${id}\n`);
        const first = this.frames.length;
        await this.frame(0.4);
        await action();
        await pause(options.verify ? 0 : 300);
        await this.browser.evaluate("document.fonts.ready");
        await this.frame(0.4);
        const actionDuration = this.frames.slice(first).reduce((sum, frame) => sum + frame.duration, 0);
        const duration = Math.max(specification.seconds, actionDuration + 1.5, (specification.audioDuration || 0) + 1.5);
        if (!options.verify) this.frames.at(-1).duration += duration - actionDuration;
        this.scenes.push({ ...specification, first, duration });
    }
    async move(selector) {
        const rectangle = await this.browser.evaluate(`(()=>{const node=document.querySelector(${JSON.stringify(selector)});if(!node)throw new Error("Missing target: "+${JSON.stringify(selector)});node.scrollIntoView({block:"nearest"});const r=node.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()`);
        const from = this.position;
        const steps = options.verify ? 1 : 7;
        for (let step = 1; step <= steps; step += 1) {
            const weight = step / steps;
            const x = from.x + (rectangle.x - from.x) * weight;
            const y = from.y + (rectangle.y - from.y) * weight;
            await this.browser.evaluate(`(()=>{const p=document.querySelector("#recording-pointer");if(p){p.style.left=${x}+"px";p.style.top=${y}+"px"}})()`);
            await this.browser.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
            await this.frame(0.045);
        }
        this.position = rectangle;
    }
    async click(selector, count = 1) {
        await this.move(selector);
        const point = await this.browser.evaluate(`(()=>{const node=document.querySelector(${JSON.stringify(selector)});if(!node)throw new Error("Target disappeared");node.scrollIntoView({block:"nearest"});const r=node.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()`);
        if (point.x !== this.position.x || point.y !== this.position.y) {
            await this.browser.evaluate(`(()=>{const p=document.querySelector("#recording-pointer");if(p){p.style.left=${point.x}+"px";p.style.top=${point.y}+"px"}})()`);
            await this.browser.send("Input.dispatchMouseEvent", { type: "mouseMoved", ...point });
            await this.frame(0.08);
        }
        this.position = point;
        await this.browser.send("Input.dispatchMouseEvent", { type: "mousePressed", ...point, button: "left", clickCount: count });
        await this.browser.send("Input.dispatchMouseEvent", { type: "mouseReleased", ...point, button: "left", clickCount: count });
        await pause(options.verify ? 50 : 200);
        await this.frame(0.35);
    }
    async type(selector, text) {
        await this.click(selector);
        await this.browser.evaluate(`(()=>{const n=document.querySelector(${JSON.stringify(selector)});n.value="";n.dispatchEvent(new Event("input",{bubbles:true}))})()`);
        const chunks = options.verify ? [text] : text.match(/.{1,5}/g) || [""];
        for (const chunk of chunks) {
            await this.browser.send("Input.insertText", { text: chunk });
            await this.frame(0.075);
        }
    }
    async key(key, code = key) {
        await this.browser.send("Input.dispatchKeyEvent", { type: "keyDown", key, code });
        await this.browser.send("Input.dispatchKeyEvent", { type: "keyUp", key, code });
        await pause(options.verify ? 50 : 200);
        await this.frame(0.3);
    }
}

async function prepareNarration(storyboard, scratch) {
    if (options.verify) return;
    for (const scene of storyboard.scenes) {
        const text = join(scratch, `${scene.id}-narration.txt`);
        const audio = join(scratch, `${scene.id}-voice.wav`);
        await writeFile(text, scene.narration);
        await run("ffmpeg", ["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
            `flite=textfile='${filterPath(text)}':voice=${storyboard.voice}`, "-af", "atempo=0.96,loudnorm=I=-18:TP=-2:LRA=7",
            "-ar", "48000", "-ac", "1", "-threads", "1", "-y", audio]);
        scene.audio = audio;
        scene.audioDuration = Number((await probe(audio)).format.duration);
        const wordsPerMinute = scene.narration.split(/\s+/).length / scene.audioDuration * 60;
        assert(wordsPerMinute >= 100 && wordsPerMinute <= 200, `Narration pace outside the introductory range for ${scene.id}`);
    }
}

async function sourceSnapshot() {
    const names = (await run("git", ["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--",
        "web", "crates", "src", "Cargo.toml", "scripts/record-introduction.mjs", "scripts/introduction-storyboard.json"]))
        .split("\0").filter(name => name && (/\.(rs|js|mjs|css|html|toml|woff2)$/.test(name)
            || name === "scripts/introduction-storyboard.json")).sort();
    const files = {};
    const combined = createHash("sha256");
    for (const name of names) {
        const bytes = await readFile(join(root, name));
        files[name] = createHash("sha256").update(bytes).digest("hex");
        combined.update(name).update("\0").update(bytes).update("\0");
    }
    return { sha256: combined.digest("hex"), files };
}

async function audioLevels(path) {
    const result = await run("ffmpeg", ["-hide_banner", "-threads", "2", "-i", path,
        "-af", "volumedetect", "-f", "null", "-"], root, true);
    const mean = /mean_volume: (-?[\d.]+) dB/.exec(result.diagnostics);
    const peak = /max_volume: (-?[\d.]+) dB/.exec(result.diagnostics);
    assert(mean && peak, "Audio measurements missing");
    const levels = { mean_db: Number(mean[1]), peak_db: Number(peak[1]) };
    assert(levels.mean_db > -40 && levels.mean_db < -6, "Narration must be audible at a reasonable level");
    assert(levels.peak_db < -0.1 && levels.peak_db > -15, "Narration must not clip or be silent");
    return levels;
}

async function render(capture, scratch, destination) {
    const visual = [];
    for (const frame of capture.frames) visual.push(concatPath(frame.path), `duration ${frame.duration.toFixed(6)}`);
    visual.push(concatPath(capture.frames.at(-1).path));
    const visualList = join(scratch, "frames.ffconcat");
    await writeFile(visualList, visual.join("\n"));
    const audio = [];
    const captions = ["WEBVTT", ""];
    const transcript = ["# Hoard introduction transcript", "", "Narration is synthesized locally with FFmpeg/libflite (voice: slt).", ""];
    const plannedDuration = capture.scenes.reduce((sum, scene) => sum + scene.duration, 0);
    const filters = ["fps=24", `tpad=stop_mode=clone:stop_duration=${plannedDuration}`,
        "pad=1920:1080:160:0:color=0x181919", "drawbox=x=160:y=908:w=1600:h=2:color=0xfa901e:t=fill"];
    const sans = filterPath(join(root, "web/fonts/space-grotesk-latin.woff2"));
    const mono = filterPath(join(root, "web/fonts/space-mono-400-latin.woff2"));
    let time = 0;
    for (const scene of capture.scenes) {
        const segment = join(scratch, `${scene.id}-padded.wav`);
        await run("ffmpeg", ["-hide_banner", "-loglevel", "error", "-i", scene.audio,
            "-af", `adelay=450,apad=whole_dur=${scene.duration},atrim=duration=${scene.duration}`,
            "-ar", "48000", "-ac", "1", "-threads", "1", "-y", segment]);
        audio.push(concatPath(segment));
        const heading = join(scratch, `${scene.id}-heading.txt`);
        const caption = join(scratch, `${scene.id}-caption.txt`);
        await writeFile(heading, `HOARD  /  ${scene.heading}`);
        await writeFile(caption, wrapped(scene.caption));
        const enabled = `gte(t,${time.toFixed(6)})*lt(t,${(time + scene.duration).toFixed(6)})`;
        filters.push(`drawtext=fontfile='${mono}':textfile='${filterPath(heading)}':expansion=none:fontcolor=0xfa901e:fontsize=18:x=160:y=931:enable='${enabled}'`);
        filters.push(`drawtext=fontfile='${sans}':textfile='${filterPath(caption)}':expansion=none:fontcolor=0xf4f0e7:fontsize=30:line_spacing=10:x=160:y=973:enable='${enabled}'`);
        captions.push(`${timestamp(time)} --> ${timestamp(time + scene.duration)}`, scene.caption, "");
        transcript.push(`## ${timestamp(time)} - ${scene.heading}`, "", scene.narration, "", `On screen: ${scene.caption}`, "");
        time += scene.duration;
    }
    const audioList = join(scratch, "audio.ffconcat");
    await writeFile(audioList, audio.join("\n"));
    const filter = join(scratch, "render-filter.txt");
    await writeFile(filter, filters.join(",\n"));
    const temporary = join(scratch, "introduction.mp4");
    await run("ffmpeg", ["-hide_banner", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", visualList,
        "-f", "concat", "-safe", "0", "-i", audioList, "-filter_script:v", filter, "-c:v", "libx264", "-preset", "fast",
        "-crf", "23", "-pix_fmt", "yuv420p", "-r", "24", "-c:a", "aac", "-b:a", "96k", "-threads", "2",
        "-filter_threads", "1", "-t", time.toFixed(6), "-movflags", "+faststart", "-metadata", `title=${capture.storyboard.title}`,
        "-metadata", "comment=Recorded from the real Hoard UI with isolated synthetic resources. Local synthetic narration.", "-y", temporary]);
    const metadata = await probe(temporary);
    const levels = await audioLevels(temporary);
    const video = metadata.streams.find(stream => stream.codec_type === "video");
    const sound = metadata.streams.find(stream => stream.codec_type === "audio");
    assert.equal(video.codec_name, "h264");
    assert.equal(video.width, 1920);
    assert.equal(video.height, 1080);
    assert.equal(sound.codec_name, "aac");
    assert(Math.abs(Number(metadata.format.duration) - time) < 0.25);
    assert(Math.abs(Number(video.duration) - time) < 0.1, "Video must hold through the final narration");
    assert(Math.abs(Number(sound.duration) - time) < 0.1, "Audio must match the scene timeline");
    await run("ffmpeg", ["-hide_banner", "-loglevel", "error", "-threads", "2", "-i", temporary, "-f", "null", "-"]);
    await mkdir(dirname(destination), { recursive: true });
    await copyFile(temporary, destination);
    const stem = destination.slice(0, -4);
    await writeFile(`${stem}.vtt`, captions.join("\n"));
    await writeFile(`${stem}-transcript.md`, transcript.join("\n"));
    await run("ffmpeg", ["-hide_banner", "-loglevel", "error", "-ss", "2", "-i", destination,
        "-frames:v", "1", "-threads", "1", "-update", "1", "-y", `${stem}-poster.jpg`]);
    return { duration: Number(metadata.format.duration), width: video.width, height: video.height,
        codec: video.codec_name, audio: sound.codec_name, bytes: Number(metadata.format.size),
        video_seconds: Number(video.duration), audio_seconds: Number(sound.duration), audio_levels: levels };
}

async function main() {
    await access(options.binary);
    const outputStem = options.output.slice(0, -4);
    const outputs = [options.output, `${outputStem}.vtt`, `${outputStem}-transcript.md`, `${outputStem}-poster.jpg`, `${outputStem}-verification.json`];
    if (!options.verify && !options.overwrite) {
        for (const output of outputs) if (await exists(output)) throw new Error(`Output exists: ${output}; use --overwrite or another --output`);
    }
    const scratch = await mkdtemp(join(tmpdir(), "hoard-intro-"));
    const origin = `http://127.0.0.1:${await freePort()}`;
    const source = await fixtures(scratch, origin);
    const storyboard = JSON.parse(await readFile(options.storyboard, "utf8"));
    assert(["slt", "rms", "awb", "kal", "kal16"].includes(storyboard.voice), "Choose an installed libflite voice");
    assert.equal(new Set(storyboard.scenes.map(scene => scene.id)).size, storyboard.scenes.length, "Scene IDs must be unique");
    for (const scene of storyboard.scenes) {
        assert(/^[a-z][a-z-]*$/.test(scene.id), "Scene IDs must be lowercase words separated by hyphens");
        assert(Number.isFinite(scene.seconds) && scene.seconds > 0, `Invalid duration for ${scene.id}`);
        assert(scene.caption && scene.heading && scene.narration, `Missing copy for ${scene.id}`);
    }
    let server;
    let browserProcess;
    let browser;
    let log = "";
    const checks = [];
    let succeeded = false;
    const startServer = async () => {
        server = ownedSpawn("nice", ["-n", "10", options.binary, "serve", "--config", source.config],
            { cwd: scratch, env, stdio: ["ignore", "pipe", "pipe"] });
        server.stdout.on("data", data => log += data);
        server.stderr.on("data", data => log += data);
        server.on("error", error => log += error.message);
        await until(async () => {
            if (server.exitCode !== null) throw new Error(`Server stopped: ${log}`);
            try { return (await fetch(`${origin}/healthz`)).ok; } catch { return false; }
        }, "isolated server ready");
    };
    const api = async (path) => {
        const response = await fetch(`${origin}/api/v1${path}`);
        assert(response.ok, `${path}: ${response.status}`);
        return response.json();
    };
    try {
        await prepareNarration(storyboard, scratch);
        await startServer();
        assert.equal((await api("/sources")).length, 0);
        assert.equal((await api("/search")).resource_total, 0);
        checks.push("fresh-catalog-has-no-content");
        browserProcess = ownedSpawn("nice", ["-n", "10", options.browser, "--headless=new", "--no-sandbox", "--disable-gpu",
            "--disable-background-networking", "--disable-component-update", "--no-first-run", "--disable-sync", "--disable-extensions",
            `--window-size=${width},${height}`, `--user-data-dir=${scratch}/browser`, "--remote-debugging-port=0", "about:blank"],
            { env, stdio: ["ignore", "ignore", "ignore"] });
        browserProcess.on("error", error => log += error.message);
        let debugPort;
        await until(async () => {
            try { debugPort = (await readFile(join(scratch, "browser/DevToolsActivePort"), "utf8")).split("\n")[0]; return true; }
            catch { return false; }
        }, "isolated browser ready");
        const tabs = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
        const socket = new WebSocket(tabs.find(tab => tab.type === "page").webSocketDebuggerUrl);
        await new Promise((resolveOpen, reject) => { socket.onopen = resolveOpen; socket.onerror = reject; });
        browser = new Browser(socket);
        await browser.initialize();
        await browser.send("Browser.grantPermissions", { origin, permissions: ["clipboardReadWrite", "clipboardSanitizedWrite"] });
        await browser.send("Page.navigate", { url: origin });
        await browser.wait('location.hash.startsWith("#/sources") && !!document.querySelector("#source-welcome")');
        await browser.evaluate('import("/js/theme.js").then(m=>m.updateSettings({theme:"light",density:"Condensed",showInspector:true}))');
        const sourcesBefore = await sourceSnapshot();
        const capture = new Capture(browser, scratch, storyboard);
        await capture.scene("welcome", async () => {
            assert.equal(await browser.evaluate('document.documentElement.scrollWidth>innerWidth'), false);
            checks.push("fresh-arrival-opens-source-setup");
        });
        await capture.scene("sharing", async () => {
            await capture.click("#source-sharing summary");
            await browser.wait('!!document.querySelector(".sharing-form input[name=node_name]")');
            await capture.type('.sharing-form input[name="node_name"]', "Studio");
            await capture.frame(2.2);
            await capture.click('.sharing-form button[type="submit"]');
            await until(async () => (await api("/setup")).restart_required === true, "saved sharing settings require restart");
            await browser.wait('document.querySelector(".sharing-status")?.textContent.includes("Restart")');
            await capture.frame(2.2);
            await stop(server);
            await startServer();
            await browser.send("Page.reload");
            await browser.wait('!!document.querySelector("#source-welcome")');
            const setup = await api("/setup");
            assert.equal(setup.node_name, "Studio");
            assert.equal(setup.restart_required, false);
            checks.push("sharing-name-persisted-and-applied-after-owned-restart");
        });
        const openFolderForm = async () => {
            if (!await browser.evaluate('!!document.querySelector(".source-add-form")')) await capture.click(".operations-top button");
            await browser.wait('!!document.querySelector(".source-add-form")');
        };
        await capture.scene("project-folder", async () => {
            await openFolderForm();
            await capture.type('.source-add-form input[name="path"]', source.project);
            await capture.type('.source-add-form input[name="name"]', "pose-tools");
            assert.equal(await browser.evaluate('document.querySelector(".source-add-form select[name=kind]").value'), "auto");
        });
        await capture.scene("project-ready", async () => {
            await capture.click('.source-add-form button[type="submit"]');
            await until(async () => (await api("/sources")).some(item => item.name === "pose-tools" && item.last_scan), "first folder scanned");
            await browser.wait('document.querySelectorAll(".source-grid.panel-row").length===1');
            assert.equal((await api("/search?q=pose")).projects.total, 1);
            await browser.wait('!document.querySelector("#source-welcome")');
            checks.push("first-folder-added-and-scanned-through-web-ui");
        });
        await capture.scene("models-folder", async () => {
            await openFolderForm();
            await capture.type('.source-add-form input[name="path"]', source.models);
            await capture.type('.source-add-form input[name="name"]', "studio-models");
            await capture.click('.source-add-form button[type="submit"]');
            await until(async () => (await api("/search")).resource_total === 4, "both resource types indexed");
            checks.push("second-folder-added-and-mixed-resources-indexed");
        });
        await capture.scene("index", async () => {
            await capture.click('.nav-tab:first-child');
            await browser.wait('document.querySelectorAll(".trow").length===4');
            await capture.click('.trow[data-index="0"]');
            checks.push("shared-index-shows-project-and-models");
        });
        await capture.scene("search", async () => {
            await capture.type("#search-input", "inverse-kinematics");
            await capture.key("Enter");
            await capture.key("Escape");
            await browser.wait('document.querySelectorAll(".trow").length===1');
            assert.equal((await api("/search?q=inverse-kinematics")).projects.total, 1);
            await capture.click(".trow");
            checks.push("committed-readme-content-is-findable");
        });
        await capture.scene("readme", async () => {
            await capture.click(".trow", 2);
            await browser.wait('document.querySelector(".project-document")?.textContent.includes("Pose Tools")');
            checks.push("project-readme-is-reviewable");
        });
        await capture.scene("files", async () => {
            await capture.click('.project-document a');
            await browser.wait('document.querySelector(".project-source")?.textContent.includes("Clone the repository")');
            await capture.frame(1);
            await capture.key("Escape");
            await browser.wait('document.querySelectorAll(".project-file").length===3');
            await capture.click('button[title="src/pose.py"]');
            await browser.wait('document.querySelector(".project-source")?.textContent.includes("interpolate_pose")');
            checks.push("relative-document-links-and-source-preview-work");
        });
        await capture.scene("clone", async () => {
            await capture.click('.detail-rail button.btn-amber');
            const command = await browser.evaluate("navigator.clipboard.readText()");
            const found = await api("/search?q=pose");
            const id = found.projects.rows[0].id;
            assert(command.includes(`/git/projects/${id}.git`), command);
            const destination = join(scratch, "cloned-pose-tools");
            await run("git", ["clone", "--quiet", `${origin}/git/projects/${id}.git`, destination], scratch);
            assert.equal(await run("git", ["rev-parse", "HEAD"], destination), source.head);
            assert((await readFile(join(destination, "README.md"), "utf8")).includes("Pose Tools"));
            checks.push("clipboard-command-and-real-http-clone-match-indexed-commit");
        });
        await capture.scene("finish", async () => {
            await capture.key("Escape");
            await capture.key("Escape");
            await capture.click('.nav-tab:nth-child(2)');
            await browser.wait('document.querySelectorAll(".source-grid.panel-row").length===2');
            await capture.click("#source-sharing summary");
            checks.push("sharing-configuration-remains-accessible-after-setup");
        });
        assert.equal(browser.exceptions.length, 0, JSON.stringify(browser.exceptions));
        checks.push("no-browser-exceptions");
        const sourcesAfter = await sourceSnapshot();
        assert.equal(sourcesAfter.sha256, sourcesBefore.sha256, "Application/recording sources changed during capture; repeat with stable sources");
        checks.push("application-and-recording-sources-stable-during-capture");
        const artifact = options.verify ? null : await render(capture, scratch, options.output);
        if (artifact) checks.push("video-and-audio-streams-match-full-timeline", "narration-is-audible-and-below-clipping", "narration-pace-and-reading-pauses-verified");
        const report = { passed: true, mode: options.verify ? "verify" : "record", generated_at: new Date().toISOString(),
            git_commit: await run("git", ["rev-parse", "HEAD"]),
            git_dirty: Boolean(await run("git", ["status", "--porcelain"])),
            git_dirty_paths: (await run("git", ["status", "--porcelain"])).split("\n").filter(Boolean),
            source_snapshot: sourcesBefore, isolated: true, production_touched: false,
            resources: { projects: 1, models: 3 }, checks, artifact,
            scenes: capture.scenes.map(({ id, duration, narration, audioDuration }) => ({ id, duration,
                ...(audioDuration ? { narration_seconds: audioDuration,
                    narration_words_per_minute: Math.round(narration.split(/\s+/).length / audioDuration * 60),
                    voice_lead_in_seconds: 0.45, trailing_pause_seconds: Number((duration - audioDuration - 0.45).toFixed(3)) } : {}) })),
            ...(options.keepWork ? { work_directory: scratch } : {}) };
        if (!options.verify) await writeFile(`${outputStem}-verification.json`, `${JSON.stringify(report, null, 2)}\n`);
        process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
        succeeded = true;
    } finally {
        browser?.socket.close();
        await stop(browserProcess);
        await stop(server);
        await writeFile(join(scratch, "server.log"), log);
        if (succeeded && !options.keepWork) await rm(scratch, { recursive: true, force: true });
        else process.stdout.write(`Work directory retained: ${scratch}\n`);
    }
}

export { render, sourceSnapshot, audioLevels };

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
    main().catch(error => { process.stderr.write(`${error.stack || error}\n`); process.exitCode = 1; });
}
