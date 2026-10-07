// Snow web frontend: page logic (loaded as a module by index.html; kept out
// of the HTML so the page can run under a strict Content Security Policy)

import { IoRing, createIoBuffer, encode, splitFrames, KEY_SCANCODES, REC } from "./snow-io.js";

// Gestalt IDs and RAM sizes mirror core/src/mac/mod.rs and the
// GESTALT_MODEL_MAP in src/main.rs
const MB = 1024 * 1024;
const MACHINES = {
    4: { name: "Macintosh Plus", rams: [1, 2, 4].map((m) => m * MB) },
    5: { name: "Macintosh SE", rams: [1, 2, 4].map((m) => m * MB) },
    17: { name: "Macintosh Classic", rams: [1, 2, 4].map((m) => m * MB) },
    3: { name: "Macintosh 512Ke", rams: [512 * 1024] },
    6: { name: "Macintosh II", rams: [1, 2, 4, 8].map((m) => m * MB) },
    7: { name: "Macintosh IIx", rams: [1, 2, 4, 8, 16, 32].map((m) => m * MB) },
    9: { name: "Macintosh SE/30", rams: [1, 2, 4, 8, 16, 32].map((m) => m * MB) },
};

const $ = (id) => document.getElementById(id);
const screen = $("screen");
const params = new URLSearchParams(location.search);

for (const [id, m] of Object.entries(MACHINES)) {
    $("machine").add(new Option(m.name, id));
}
$("machine").value = params.get("model") || "5";
function refillRam() {
    const rams = MACHINES[$("machine").value].rams;
    $("ram").innerHTML = "";
    for (const ram of rams) {
        $("ram").add(new Option(ram >= MB ? `${ram / MB} MB` : `${ram / 1024} KB`, ram));
    }
    $("ram").value = rams.includes(4 * MB) ? 4 * MB : rams[rams.length - 1];
}
$("machine").addEventListener("change", refillRam);
refillRam();

// The bridge is the page's own origin (its Content Security Policy only
// allows connecting there); each room is a separate AppleTalk network
const bridgeBase = location.protocol.startsWith("http")
    ? `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/bridge`
    : "ws://127.0.0.1:8080/bridge";
const ROOM_NAME = /^[A-Za-z0-9._-]{1,64}$/;
$("room").value = ROOM_NAME.test(params.get("room") || "") ? params.get("room") : "default";
function bridgeUrl() {
    const room = $("room").value.trim();
    return params.get("bridge") || `${bridgeBase}/${encodeURIComponent(room)}`;
}
const syncBridgeField = () => { $("bridgeUrl").value = bridgeUrl(); };
$("room").addEventListener("input", syncBridgeField);
syncBridgeField();
if (params.get("ethernet") === "1") $("ethernet").checked = true;
if (params.get("localtalk") === "0") $("localtalk").checked = false;

// ------------------------------------------------------------------ status

function setStatus(text, isError = false) {
    $("status").textContent = text;
    $("status").classList.toggle("error", isError);
}
const consoleLines = [];
function log(line) {
    consoleLines.push(line);
    if (consoleLines.length > 400) consoleLines.splice(0, consoleLines.length - 400);
    $("console").textContent = consoleLines.join("\n");
    $("console").scrollTop = $("console").scrollHeight;
}

// Counters, exposed for automated tests as window.snow
const stats = {
    video: 0, ltRx: 0, ltTx: 0, ethRx: 0, ethTx: 0, nodes: new Set(), link: false, errors: [], recent: [],
    // LLAP frame counts by direction and kind, and the addresses this Mac
    // probed (lapENQ) while acquiring its node address
    llap: { out: { enq: 0, ack: 0, data: 0 }, in: { enq: 0, ack: 0, data: 0 } },
    probed: [],
};
window.snow = { stats };
function renderNet() {
    $("ltRx").textContent = stats.ltRx;
    $("ltTx").textContent = stats.ltTx;
    $("ethRx").textContent = stats.ethRx;
    $("ethTx").textContent = stats.ethTx;
    $("nodes").textContent = stats.nodes.size
        ? [...stats.nodes].sort((a, b) => a - b).map((n) => n.toString()).join(", ")
        : "none";
}
setInterval(renderNet, 500);

// LToUDP datagram: 4 byte sender id, then LLAP [dst][src][type]. Nodes are
// learned from data (DDP) frames only: ENQ probes carry tentative addresses
// that a node may still give up.
function noteLocalTalk(dgram, dir) {
    if (dgram.length >= 7) {
        const counts = stats.llap[dir.trim()];
        const type = dgram[6];
        if (type === 0x81) counts.enq++;
        else if (type === 0x82) counts.ack++;
        else if (type < 0x80) counts.data++;
        if (dir === "out" && type === 0x81 && stats.probed[stats.probed.length - 1] !== dgram[4]) {
            stats.probed.push(dgram[4]);
        }
    }
    // With ?debug=1, keep a hex dump of the last data frames
    if (params.get("debug") === "1" && dgram.length >= 7 && dgram[6] < 0x80) {
        stats.dump = stats.dump || [];
        stats.dump.push(`${Math.round(performance.now())} ${dir} ${[...dgram.subarray(4)].map((b) => b.toString(16).padStart(2, "0")).join("")}`);
        if (stats.dump.length > 200) stats.dump.shift();
    }
    // Run-length log of the last frames (for diagnostics/tests)
    if (dgram.length >= 7) {
        const key = `${dir} ${[4, 5, 6].map((i) => dgram[i].toString(16).padStart(2, "0")).join(" ")} len=${dgram.length - 4}`;
        const last = stats.recent[stats.recent.length - 1];
        if (last && last.key === key) last.count++;
        else stats.recent.push({ key, count: 1, t: Math.round(performance.now()) });
        if (stats.recent.length > 300) stats.recent.shift();
    }
    if (dgram.length >= 7 && dgram[6] < 0x80 && dgram[5] !== 0 && dgram[5] !== 0xff) {
        stats.nodes.add(dgram[5]);
    }
}

// ----------------------------------------------------------- bridge link

let ring = null;
let worker = null;

class BridgeLink {
    constructor(url) {
        this.url = url;
        this.ws = null;
        this.carry = new Uint8Array(0);
        this.closed = false;
        this.connect();
    }
    connect() {
        if (this.closed) return;
        let ws;
        try {
            ws = new WebSocket(this.url);
        } catch (err) {
            this.setLink(false, `bad URL ${this.url}`);
            return;
        }
        ws.binaryType = "arraybuffer";
        ws.onopen = () => this.setLink(true, this.url);
        ws.onmessage = (event) => this.receive(new Uint8Array(event.data));
        ws.onclose = () => {
            if (this.ws === ws) {
                this.ws = null;
                this.setLink(false, `${this.url} (retrying)`);
                setTimeout(() => this.connect(), 2000);
            }
        };
        this.ws = ws;
    }
    setLink(up, description) {
        stats.link = up;
        $("linkDot").className = "dot " + (up ? "up" : "down");
        $("linkText").textContent = up ? `Bridge: ${this.url}` : `Bridge: ${description}`;
        ring?.push(...encode.link(up, description));
        if (up) log(`bridge connected: ${this.url}`);
    }
    receive(bytes) {
        let stream = bytes;
        if (this.carry.length) {
            stream = new Uint8Array(this.carry.length + bytes.length);
            stream.set(this.carry);
            stream.set(bytes, this.carry.length);
        }
        const [frames, rest] = splitFrames(stream);
        this.carry = rest.slice();
        for (const [tag, payload] of frames) {
            if (tag === REC.LOCALTALK) {
                stats.ltRx++;
                noteLocalTalk(payload, "in ");
            } else if (tag === REC.ETHERNET) {
                stats.ethRx++;
            } else {
                continue;
            }
            ring?.push(tag, payload);
        }
    }
    send(buffer) {
        const bytes = new Uint8Array(buffer);
        for (const [tag, payload] of splitFrames(bytes)[0]) {
            if (tag === REC.LOCALTALK) {
                stats.ltTx++;
                noteLocalTalk(payload, "out");
            } else if (tag === REC.ETHERNET) {
                stats.ethTx++;
            }
        }
        if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(buffer);
    }
    close() {
        this.closed = true;
        this.ws?.close();
        this.ws = null;
    }
}
let bridge = null;

// ------------------------------------------------------------------- audio

let audioCtx = null, audioNode = null, audioRate = 0, audioChannels = 1;
let audioChunks = [], audioHead = 0, audioDrained = 0;

function audioOpen(sampleRate, channels) {
    if (!audioCtx) return;
    audioRate = sampleRate;
    audioChannels = Math.min(2, Math.max(1, channels));
    audioChunks = [];
    audioHead = 0;
    audioNode?.disconnect();
    audioNode = audioCtx.createScriptProcessor(2048, 0, audioChannels);
    audioNode.onaudioprocess = audioProcess;
    audioNode.connect(audioCtx.destination);
}

function audioProcess(e) {
    const frames = e.outputBuffer.length;
    const ratio = audioRate / audioCtx.sampleRate; // source frames per output frame
    for (let ch = 0; ch < audioChannels; ch++) {
        const out = e.outputBuffer.getChannelData(ch);
        let pos = audioHead, chunk = 0;
        for (let i = 0; i < frames; i++, pos += ratio) {
            let c = audioChunks[chunk];
            while (c && pos >= c.length / audioChannels) {
                pos -= c.length / audioChannels;
                c = audioChunks[++chunk];
            }
            out[i] = c ? c[(pos | 0) * audioChannels + ch] : 0;
        }
    }
    // Advance the read position
    let consumed = frames * ratio;
    audioDrained += consumed * audioChannels;
    audioHead += consumed;
    while (audioChunks.length && audioHead >= audioChunks[0].length / audioChannels) {
        audioHead -= audioChunks[0].length / audioChannels;
        audioChunks.shift();
    }
    if (!audioChunks.length) audioHead = 0;
    if (audioDrained >= 256) {
        ring?.push(...encode.audioDrained(audioDrained | 0));
        audioDrained = 0;
    }
}

// ------------------------------------------------------------------- video

let imageData = null;
function videoOpen(width, height) {
    screen.width = width;
    screen.height = height;
    imageData = screen.getContext("2d").createImageData(width, height);
}
function videoFrame(data) {
    if (!imageData) return;
    imageData.data.set(new Uint8Array(data, 0, imageData.data.length));
    screen.getContext("2d").putImageData(imageData, 0, 0);
    stats.video++;
}

// ------------------------------------------------------------------- input

function sendInput(record) {
    ring?.push(...record);
}
function keyEvent(e, down) {
    if (!ring || e.repeat || e.metaKey || e.ctrlKey) return;
    if (e.target instanceof HTMLElement && e.target.closest("input, select, textarea")) return;
    const scancode = KEY_SCANCODES[e.code];
    if (scancode === undefined) return;
    e.preventDefault();
    sendInput(encode.key(scancode, down));
}
window.addEventListener("keydown", (e) => keyEvent(e, true));
window.addEventListener("keyup", (e) => keyEvent(e, false));

screen.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    screen.focus();
    screen.setPointerCapture(e.pointerId);
    sendInput(encode.mouseButton(true));
});
screen.addEventListener("pointerup", (e) => {
    e.preventDefault();
    sendInput(encode.mouseButton(false));
});
screen.addEventListener("pointermove", (e) => {
    const rect = screen.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    const x = Math.round((e.clientX - rect.left) * (screen.width / rect.width));
    const y = Math.round((e.clientY - rect.top) * (screen.height / rect.height));
    sendInput(encode.mouseAbs(x, y));
    if (e.movementX || e.movementY) sendInput(encode.mouseRel(e.movementX, e.movementY));
});
window.addEventListener("blur", () => sendInput(encode.mouseButton(false)));

// ------------------------------------------------------------------- start

async function fetchMedia(url) {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
    return response.arrayBuffer();
}

async function start() {
    if (worker) return;
    if (!self.crossOriginIsolated) {
        setStatus("This page needs cross-origin isolation (SharedArrayBuffer). " +
            "Serve it with snow-bridge --www, which sends the COOP/COEP headers.", true);
        return;
    }
    $("startBtn").disabled = true;
    try {
        audioCtx = new AudioContext();
        audioCtx.resume().catch(() => {});
    } catch (err) {
        audioCtx = null;
    }

    const files = [], transfer = [], argv = [];
    const add = (data, path, flag) => {
        files.push({ path, data });
        transfer.push(data);
        argv.push(flag, path);
    };
    try {
        setStatus("Reading media…");
        const rom = $("romFile").files[0] ? await $("romFile").files[0].arrayBuffer()
            : params.get("rom") ? await fetchMedia(params.get("rom")) : null;
        if (!rom) throw new Error("Choose a ROM file first");
        add(rom, "/media/rom.rom", "--rom");

        const disks = [...$("diskFiles").files];
        for (let i = 0; i < disks.length; i++) add(await disks[i].arrayBuffer(), `/media/disk${i}`, "--disk");
        for (const [i, url] of params.getAll("disk").entries()) add(await fetchMedia(url), `/media/urldisk${i}`, "--disk");
        const floppies = [...$("floppyFiles").files].slice(0, 3);
        for (let i = 0; i < floppies.length; i++) add(await floppies[i].arrayBuffer(), `/media/floppy${i}`, "--floppy");
        if ($("pramFile").files[0]) add(await $("pramFile").files[0].arrayBuffer(), "/media/pram", "--pram");

        argv.push("--gestalt-id", $("machine").value, "--ram-size", $("ram").value);
        if ($("mouseDeltas").checked) argv.push("--use-mouse-deltas");
        if ($("debugLog").checked) argv.push("--debug-log");
        if ($("localtalk").checked) argv.push("--localtalk-bridge");
        if ($("localtalk").checked && $("appletalkPram").checked) {
            argv.push("--appletalk-pram");
            if (params.get("node")) argv.push("--appletalk-node", params.get("node"));
        }
        if ($("ethernet").checked) argv.push("--ethernet-nat");
    } catch (err) {
        setStatus(err.message || String(err), true);
        $("startBtn").disabled = false;
        return;
    }

    const io = createIoBuffer();
    ring = new IoRing(io);
    if ($("localtalk").checked || $("ethernet").checked) {
        if (!ROOM_NAME.test($("room").value.trim())) {
            setStatus("Room names use 1-64 letters, digits, '.', '_' or '-'", true);
            $("startBtn").disabled = false;
            return;
        }
        bridge = new BridgeLink($("bridgeUrl").value.trim());
    }

    worker = new Worker("worker.js", { type: "module" });
    worker.onmessage = ({ data: msg }) => {
        switch (msg?.type) {
            case "video-open": videoOpen(msg.width, msg.height); setStatus("Running."); break;
            case "video": videoFrame(msg.data); break;
            case "audio-open": audioOpen(msg.sampleRate, msg.channels); break;
            case "audio":
                // While the browser keeps audio suspended (no user gesture
                // yet), drop samples instead of letting them pile up
                if (audioNode && audioCtx?.state === "running") audioChunks.push(new Float32Array(msg.data));
                else ring.push(...encode.audioDrained(msg.data.byteLength / 4));
                break;
            case "net-send": bridge?.send(msg.data); break;
            case "clipboard": navigator.clipboard?.writeText(msg.text).catch(() => {}); break;
            case "log": log(msg.line); break;
            case "error":
                stats.errors.push(msg.message);
                setStatus(msg.message, true);
                log("ERROR: " + msg.message);
                break;
        }
    };
    worker.onerror = (e) => setStatus("Worker error: " + (e.message || "unknown"), true);
    worker.postMessage({ type: "start", argv, files, io }, transfer);
    $("stopBtn").disabled = false;
    setStatus("Starting…");
    screen.focus();
}

function stop() {
    worker?.terminate();
    worker = null;
    bridge?.close();
    bridge = null;
    ring = null;
    audioNode?.disconnect();
    audioNode = null;
    audioCtx?.close().catch(() => {});
    audioCtx = null;
    $("startBtn").disabled = false;
    $("stopBtn").disabled = true;
    $("linkDot").className = "dot";
    $("linkText").textContent = "Bridge: not connected";
    setStatus("Stopped.");
}

$("startBtn").addEventListener("click", start);
$("stopBtn").addEventListener("click", stop);
if (params.get("autostart") === "1") start();
