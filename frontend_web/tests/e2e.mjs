// End-to-end test: two browser tabs running the Snow web frontend network
// with each other through snow-bridge.
//
//   node e2e.mjs
//
// Environment:
//   SNOW_BRIDGE   path to the snow_bridge binary (default: ../../target/release/snow_bridge)
//   SNOW_ROM      Macintosh ROM (e.g. a Mac SE ROM). Without it, a zero-filled
//                 ROM is used and only the plumbing is tested (worker start,
//                 video, bridge link).
//   SNOW_DISK     System 6 disk image (a bare HFS volume works). Required
//                 together with SNOW_ROM for the AppleTalk test.
//   SNOW_MODEL    gestalt ID (default 5, Macintosh SE)
//   E2E_SECONDS   time budget for the AppleTalk exchange (default 120)
//   E2E_SHOTS     directory for screenshots of both tabs (optional)
//
// With ROM and disk, the test plays out an LLAP node address collision
// across the bridge: Mac A boots System 6 with AppleTalk active and settles
// on node 42; Mac B then boots remembering the same address. B's address
// enquiries (lapENQ) must reach A, A must answer them with lapACK
// ("address taken"), and B must move on to a different address - the
// System 6 LocalTalk drivers of both machines talking through snow-bridge.

import { spawn } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const here = path.dirname(fileURLToPath(import.meta.url));
const www = path.resolve(here, "../www");
const bridgeBin = process.env.SNOW_BRIDGE || path.resolve(here, "../../target/release/snow_bridge");
const seconds = Number(process.env.E2E_SECONDS || 120);
const fullBoot = Boolean(process.env.SNOW_ROM && process.env.SNOW_DISK);

function fail(message) {
    console.error(`E2E FAILED: ${message}`);
    process.exitCode = 1;
}

async function freePort() {
    return new Promise((resolve) => {
        const srv = net.createServer().listen(0, "127.0.0.1", () => {
            const { port } = srv.address();
            srv.close(() => resolve(port));
        });
    });
}

async function waitFor(what, fn, timeoutMs) {
    const end = Date.now() + timeoutMs;
    for (;;) {
        const value = await fn();
        if (value) return value;
        if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
        await new Promise((r) => setTimeout(r, 500));
    }
}

// --------------------------------------------------------------- media

for (const f of ["index.html", "worker.js", "snow-io.js", "snow_web.js", "snow_web.wasm"]) {
    if (!fs.existsSync(path.join(www, f))) {
        console.error(`missing www/${f} - run frontend_web/build-web.sh first`);
        process.exit(2);
    }
}
const media = path.join(www, "media");
fs.mkdirSync(media, { recursive: true });
if (fullBoot) {
    fs.copyFileSync(process.env.SNOW_ROM, path.join(media, "e2e.rom"));
    fs.copyFileSync(process.env.SNOW_DISK, path.join(media, "e2e.dsk"));
} else {
    console.log("SNOW_ROM/SNOW_DISK not set: plumbing test only (zero-filled ROM)");
    fs.writeFileSync(path.join(media, "e2e.rom"), Buffer.alloc(128 * 1024));
}

// -------------------------------------------------------------- bridge

const port = await freePort();
const bridge = spawn(bridgeBin, ["--addr", "127.0.0.1", "--port", String(port), "--www", www, "--no-lan"], {
    stdio: ["ignore", "inherit", "inherit"],
});
const cleanup = [];
cleanup.push(() => bridge.kill());
await waitFor("bridge", () => new Promise((resolve) => {
    const s = net.connect(port, "127.0.0.1", () => { s.destroy(); resolve(true); });
    s.on("error", () => resolve(false));
}), 15000);

// ------------------------------------------------------------- browser

const browser = await chromium.launch();
cleanup.push(() => browser.close());

const model = process.env.SNOW_MODEL || "5";
const NODE = 42;
const query = new URLSearchParams({ rom: "media/e2e.rom", model, autostart: "1", node: String(NODE) });
if (fullBoot) query.append("disk", "media/e2e.dsk");
const url = `http://127.0.0.1:${port}/?${query}`;

async function openTab(name) {
    const page = await browser.newPage();
    page.on("pageerror", (err) => console.log(`[${name}] page error: ${err.message}`));
    page.on("console", (msg) => {
        if (msg.type() === "error") console.log(`[${name}] console: ${msg.text()}`);
    });
    await page.goto(url);
    return page;
}

const stats = (page) => page.evaluate(() => ({
    ...window.snow.stats,
    nodes: [...window.snow.stats.nodes],
    isolated: self.crossOriginIsolated,
}));

let a, b;
const tabs = [];

// Wait until a Mac stops sending LocalTalk frames (address acquired)
async function settled(name, page) {
    let last = -1, since = Date.now();
    return waitFor(`${name}: LocalTalk address acquisition`, async () => {
        const s = await stats(page);
        if (s.errors.length) throw new Error(`${name}: ${s.errors.join("; ")}`);
        if (s.ltTx !== last) {
            last = s.ltTx;
            since = Date.now();
            return null;
        }
        return s.llap.out.enq > 0 && Date.now() - since > 5000 ? s : null;
    }, seconds * 1000);
}

try {
    a = await openTab("A");
    tabs.push(["A", a]);
    if (!fullBoot) {
        b = await openTab("B");
        tabs.push(["B", b]);
    }
    for (const [name, page] of tabs) {
        const s = await waitFor(`${name}: video + bridge link`, async () => {
            const s = await stats(page);
            if (s.errors.length) throw new Error(`${name}: ${s.errors.join("; ")}`);
            return s.video > 0 && s.link ? s : null;
        }, 60000);
        console.log(`[${name}] running: crossOriginIsolated=${s.isolated} video frames=${s.video}`);
    }

    if (fullBoot) {
        const sa = await settled("A", a);
        console.log(`[A] settled: probed ${sa.probed} with ${sa.llap.out.enq} ENQs`);
        if (sa.probed.join() !== String(NODE)) fail(`A should keep node ${NODE}, probed ${sa.probed}`);

        b = await openTab("B");
        tabs.push(["B", b]);
        const sb = await settled("B", b);
        const sa2 = await stats(a);
        console.log(`[B] settled: probed ${sb.probed} with ${sb.llap.out.enq} ENQs, ACKs received ${sb.llap.in.ack}`);
        console.log(`[A] ENQs received ${sa2.llap.in.enq}, ACKs sent ${sa2.llap.out.ack}`);
        if (sb.probed[0] !== NODE) fail(`B should first probe node ${NODE}, probed ${sb.probed}`);
        if (sa2.llap.in.enq === 0) fail("A never received B's ENQs");
        if (sa2.llap.out.ack === 0 || sb.llap.in.ack === 0) fail("A's ACK did not reach B");
        const final = sb.probed[sb.probed.length - 1];
        if (sb.probed.length < 2 || final === NODE) fail(`B should have moved off node ${NODE}, probed ${sb.probed}`);
        else console.log(`collision resolved: A is node ${NODE}, B moved to node ${final}`);

        // Once on the network, B broadcasts AppleTalk traffic (RTMP, NBP
        // name registration); A must receive it from B's new address
        const seen = await waitFor("A receiving AppleTalk (DDP) traffic from B", async () => {
            const s = await stats(a);
            return s.nodes.includes(final) ? s : null;
        }, 30000).catch((err) => fail(err.message));
        if (seen) console.log(`A received ${seen.llap.in.data} DDP frames; AppleTalk nodes seen by A: ${seen.nodes}`);
    }

    if (process.env.E2E_SHOTS) {
        fs.mkdirSync(process.env.E2E_SHOTS, { recursive: true });
        for (const [name, page] of tabs) {
            await page.locator("#screen").screenshot({ path: path.join(process.env.E2E_SHOTS, `tab-${name.toLowerCase()}.png`) });
        }
        await a.screenshot({ path: path.join(process.env.E2E_SHOTS, "page-a.png"), fullPage: true });
    }
    if (!process.exitCode) console.log("E2E PASSED");
} catch (err) {
    fail(err.message);
} finally {
    if (process.env.E2E_DEBUG) {
        for (const [name, page] of tabs) {
            const recent = await page.evaluate(() => window.snow.stats.recent).catch(() => []);
            const lines = recent.map((r) => `${String(r.t).padStart(7)}ms ${String(r.count).padStart(4)}x ${r.key}`);
            console.log(`--- LocalTalk frames in tab ${name} (time, count, dir dst src type):\n${lines.join("\n")}`);
        }
    }
    for (const fn of cleanup.reverse()) await fn();
}
