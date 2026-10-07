// Test client for snow-bridge
//
// Validates the bridge's two paths end-to-end without a real Mac:
//   1. RARP request  -> NAT engine's RARP server (mactcp helpers)
//   2. ARP request   -> smoltcp gateway
//   3. LocalTalk (LToUDP) datagrams relayed between two clients
//   4. Frame reassembly across WebSocket messages
//   5. Safe defaults and admission checks: Origin, paths, rooms, limits,
//      Ethernet off by default, static file security headers
//   6. NAT egress policy: no access to the host's loopback by default
//
// Usage:  node bridge.mjs [ws://host:port/bridge]
// Without a URL, the test starts its own bridges (SNOW_BRIDGE, default
// ../../target/release/snow_bridge) on free ports: one with --ethernet for
// tests 1-4, and one with default (safe) settings for the security checks.

import { spawn } from "node:child_process";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const BRIDGE_BIN = process.env.SNOW_BRIDGE || path.resolve(here, "../../target/release/snow_bridge");
const WWW = path.resolve(here, "../www");

async function freePort() {
    return new Promise((resolve) => {
        const srv = net.createServer().listen(0, "127.0.0.1", () => {
            const { port } = srv.address();
            srv.close(() => resolve(port));
        });
    });
}

/// Start a bridge on a free port with extra arguments; returns its port
async function startBridge(extraArgs) {
    const port = await freePort();
    const proc = spawn(BRIDGE_BIN, ["--port", String(port), ...extraArgs], {
        stdio: ["ignore", "ignore", "inherit"],
    });
    process.on("exit", () => proc.kill());
    for (let i = 0; i < 100; i++) {
        const up = await new Promise((resolve) => {
            const s = net.connect(port, "127.0.0.1", () => { s.destroy(); resolve(true); });
            s.on("error", () => resolve(false));
        });
        if (up) return port;
        await new Promise((r) => setTimeout(r, 100));
    }
    throw new Error("bridge did not start");
}

/// Raw WebSocket upgrade request; resolves with the HTTP status line
function rawUpgrade(port, reqPath, headers = {}) {
    return new Promise((resolve) => {
        const s = net.connect(port, "127.0.0.1", () => {
            const extra = Object.entries(headers).map(([k, v]) => `${k}: ${v}\r\n`).join("");
            s.write(`GET ${reqPath} HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\nUpgrade: websocket\r\n` +
                "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n" +
                `Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n${extra}\r\n`);
        });
        let data = "";
        s.on("data", (d) => {
            data += d;
            if (data.includes("\r\n")) {
                resolve({ status: data.split("\r\n")[0], socket: s });
            }
        });
        s.on("error", () => resolve({ status: "error", socket: s }));
    });
}

let URL = process.argv[2];
if (!URL) {
    const port = await startBridge(["--ethernet"]);
    URL = `ws://127.0.0.1:${port}/bridge`;
}

// ---------------------------------------------------------------- framing

const TAG_ETHERNET = 0;
const TAG_LOCALTALK = 1;

function pushFrame(tag, payload) {
    const out = Buffer.alloc(3 + payload.length);
    out[0] = tag;
    out.writeUInt16BE(payload.length, 1);
    payload.copy(out, 3);
    return out;
}

/// Extract complete frames from a stream; returns ([frames], remainder)
function pullFrames(stream) {
    const frames = [];
    let pos = 0;
    while (pos + 3 <= stream.length) {
        const len = stream.readUInt16BE(pos + 1);
        if (pos + 3 + len > stream.length) break;
        frames.push([stream[pos], stream.subarray(pos + 3, pos + 3 + len)]);
        pos += 3 + len;
    }
    return [frames, stream.subarray(pos)];
}

// --------------------------------------------------------------- packets

const BROADCAST = Buffer.from([0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
const GW_MAC = Buffer.from("aa55aa55aa55", "hex");
const GW_IP = Buffer.from([10, 0, 0, 1]);

function ethFrame(dstMac, srcMac, ethertype, payload) {
    const out = Buffer.alloc(14 + payload.length);
    dstMac.copy(out, 0);
    srcMac.copy(out, 6);
    out.writeUInt16BE(ethertype, 12);
    payload.copy(out, 14);
    return out;
}

function arp(hwType, protoType, hLen, pLen, op, shw, sproto, thw, tproto) {
    const out = Buffer.alloc(28);
    out.writeUInt16BE(hwType, 0);
    out.writeUInt16BE(protoType, 2);
    out[4] = hLen;
    out[5] = pLen;
    out.writeUInt16BE(op, 6);
    shw.copy(out, 8);
    sproto.copy(out, 14);
    thw.copy(out, 18);
    tproto.copy(out, 24);
    return out;
}

function zeroMac() { return Buffer.alloc(6); }

function rarpRequest(clientMac) {
    return ethFrame(BROADCAST, clientMac, 0x8035, arp(1, 0x0800, 6, 4, 3, zeroMac(), Buffer.alloc(4), clientMac, Buffer.alloc(4)));
}

function arpRequest(clientMac, clientIp, targetIp) {
    return ethFrame(BROADCAST, clientMac, 0x0806, arp(1, 0x0800, 6, 4, 1, clientMac, clientIp, zeroMac(), targetIp));
}


function checksum16(buf) {
    let sum = 0;
    for (let i = 0; i + 1 < buf.length; i += 2) sum += buf.readUInt16BE(i);
    if (buf.length % 2) sum += buf[buf.length - 1] << 8;
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return ~sum & 0xffff;
}

/// Ethernet + IPv4 + TCP SYN from the guest (10.0.0.2) to dstIp:dstPort
function tcpSyn(srcMac, dstIp, dstPort, srcPort = 40000) {
    const tcp = Buffer.alloc(20);
    tcp.writeUInt16BE(srcPort, 0);
    tcp.writeUInt16BE(dstPort, 2);
    tcp.writeUInt32BE(1000, 4); // sequence number
    tcp[12] = 5 << 4; // header length
    tcp[13] = 0x02; // SYN
    tcp.writeUInt16BE(8192, 14); // window
    const srcIp = Buffer.from([10, 0, 0, 2]);
    const pseudo = Buffer.concat([srcIp, dstIp, Buffer.from([0, 6, 0, 20]), tcp]);
    tcp.writeUInt16BE(checksum16(pseudo), 16);
    const ip = Buffer.alloc(20);
    ip[0] = 0x45;
    ip.writeUInt16BE(40, 2); // total length
    ip[8] = 64; // TTL
    ip[9] = 6; // TCP
    srcIp.copy(ip, 12);
    dstIp.copy(ip, 16);
    ip.writeUInt16BE(checksum16(ip), 10);
    return ethFrame(GW_MAC, srcMac, 0x0800, Buffer.concat([ip, tcp]));
}

/// Whether a guest TCP connection to a server on this host's loopback gets
/// through the bridge's NAT; returns [server saw a connection, guest got RST]
async function probeLoopbackEgress(bridgeUrl) {
    const server = net.createServer((sock) => { server.hit = true; sock.destroy(); });
    server.hit = false;
    await new Promise((r) => server.listen(0, "127.0.0.1", r));
    const port = server.address().port;
    const c = new Client("egress", bridgeUrl);
    await c.opened;
    const mac = Buffer.from("008019123456", "hex");
    c.sendFrame(TAG_ETHERNET, tcpSyn(mac, Buffer.from([127, 0, 0, 1]), port));
    let rst = false;
    try {
        await c.recvFrame(([t, f]) => t === TAG_ETHERNET && f.length >= 54 &&
            f.readUInt16BE(12) === 0x0800 && f[23] === 6 && (f[47] & 0x04) !== 0, 3000);
        rst = true;
    } catch { /* no reset */ }
    await new Promise((r) => setTimeout(r, 500));
    c.close();
    server.close();
    return [server.hit, rst];
}

// -------------------------------------------------------------- ws client

class Client {
    constructor(name, url = URL) {
        this.name = name;
        this.ws = new WebSocket(url);
        // Synchronous data access keeps message processing in arrival order
        this.ws.binaryType = "arraybuffer";
        this.carry = Buffer.alloc(0);
        this.waiters = [];
        this.processQueue = Promise.resolve();
        this.opened = new Promise((resolve, reject) => {
            this.ws.addEventListener("open", resolve);
            this.ws.addEventListener("error", reject);
        });
        this.ws.addEventListener("message", (event) => {
            // Serialize processing so frames are reassembled in arrival order
            // even if the data needs async conversion (e.g. Blob)
            this.processQueue = this.processQueue.then(async () => {
                let data = event.data;
                if (data instanceof Blob) data = Buffer.from(await data.arrayBuffer());
                else if (!Buffer.isBuffer(data)) data = Buffer.from(data);
                this.carry = Buffer.concat([this.carry, data]);
                const [frames, rest] = pullFrames(this.carry);
                this.carry = rest;
                for (const frame of frames) {
                    const i = this.waiters.findIndex(([pred]) => pred(frame));
                    if (i >= 0) {
                        this.waiters[i][1](frame);
                        this.waiters.splice(i, 1);
                    }
                }
            });
        });
    }

    async sendFrame(tag, payload) {
        await this.opened;
        this.ws.send(pushFrame(tag, payload));
    }

    /// Wait for a frame matching `predicate([tag, payload])`
    async recvFrame(predicate, timeoutMs = 5000) {
        await this.opened;
        return await new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                this.waiters.splice(this.waiters.indexOf(entry), 1);
                reject(new Error(`${this.name}: timed out waiting for frame`));
            }, timeoutMs);
            const entry = [predicate, (frame) => { clearTimeout(timer); resolve(frame); }];
            this.waiters.push(entry);
        });
    }

    close() {
        this.ws.close();
    }
}

// ------------------------------------------------------------------ tests

let failures = 0;
function check(name, cond, detail = "") {
    if (cond) {
        console.log(`  PASS  ${name}`);
    } else {
        failures++;
        console.log(`  FAIL  ${name} ${detail}`);
    }
}

async function main() {
    console.log(`Testing bridge at ${URL}`);
    const a = new Client("A");
    const b = new Client("B");
    await Promise.all([a.opened, b.opened]);
    console.log("both clients connected");

    const macA = Buffer.from("008019aabbcc", "hex");
    const macB = Buffer.from("008019ddeeff", "hex");
    const ipA = Buffer.from([10, 0, 0, 2]);

    // 1. RARP request -> RARP reply from the NAT gateway
    console.log("test 1: RARP");
    a.sendFrame(TAG_ETHERNET, rarpRequest(macA));
    const [tag, f] = await a.recvFrame(([t, f]) =>
        t === TAG_ETHERNET && f.length >= 42 &&
        f.readUInt16BE(12) === 0x8035 && f.readUInt16BE(20) === 4 &&
        Buffer.compare(f.subarray(6, 12), GW_MAC) === 0);
    void tag;
    check("rarp reply ethertype/op/src", true);
    check("rarp reply src ip is gateway", Buffer.compare(f.subarray(28, 32), GW_IP) === 0,
        `got ${[...f.subarray(28, 32)].join(".")}`);
    check("rarp reply assigns 10.0.0.2", Buffer.compare(f.subarray(38, 42), ipA) === 0,
        `got ${[...f.subarray(38, 42)].join(".")}`);
    check("rarp reply dst is client mac", Buffer.compare(f.subarray(0, 6), macA) === 0);

    // 2. ARP request for the gateway -> ARP reply
    console.log("test 2: ARP");
    a.sendFrame(TAG_ETHERNET, arpRequest(macA, ipA, GW_IP));
    await a.recvFrame(([t, f]) =>
        t === TAG_ETHERNET && f.length >= 42 &&
        f.readUInt16BE(12) === 0x0806 && f.readUInt16BE(20) === 2 &&
        Buffer.compare(f.subarray(6, 12), GW_MAC) === 0 &&
        Buffer.compare(f.subarray(28, 32), GW_IP) === 0);
    check("arp reply from gateway", true);

    // 3. LocalTalk relay A -> B and B -> A. The bridge replaces the
    // client-supplied sender ID with its own assignment per client.
    console.log("test 3: LocalTalk relay");
    const enqA = Buffer.from([0xaa, 0xbb, 0xcc, 0xdd, 0x4f, 0x4f, 0x81]);
    const enqB = Buffer.from([0x11, 0x22, 0x33, 0x44, 0x20, 0x20, 0x81]);
    a.sendFrame(TAG_LOCALTALK, enqA);
    const [, gotB] = await b.recvFrame(([t, f]) => t === TAG_LOCALTALK && f[4] === 0x4f);
    check("A->B datagram delivered", gotB.subarray(4).equals(enqA.subarray(4)));
    check("sender ID assigned by the bridge", !gotB.subarray(0, 4).equals(enqA.subarray(0, 4)));
    b.sendFrame(TAG_LOCALTALK, enqB);
    const [, gotA] = await a.recvFrame(([t, f]) => t === TAG_LOCALTALK && f[4] === 0x20);
    check("B->A datagram delivered", gotA.subarray(4).equals(enqB.subarray(4)));
    let echoed = false;
    try {
        await a.recvFrame(([t, f]) => t === TAG_LOCALTALK && f[4] === 0x4f, 1000);
        echoed = true;
    } catch { /* expected */ }
    check("no loopback echo of own datagram", !echoed);
    // Malformed LLAP (control packet with a payload) is dropped
    a.sendFrame(TAG_LOCALTALK, Buffer.from([1, 2, 3, 4, 0x55, 0x55, 0x81, 0x00]));
    let relayedBad = false;
    try {
        await b.recvFrame(([t, f]) => t === TAG_LOCALTALK && f[4] === 0x55, 1000);
        relayedBad = true;
    } catch { /* expected */ }
    check("malformed LocalTalk datagram dropped", !relayedBad);

    // 4. Frame reassembly across messages (send a RARP request split into
    // two WebSocket messages; the engine's reply proves it was reassembled)
    console.log("test 4: reassembly");
    const mac4 = Buffer.from("008019999999", "hex");
    const rarpFrame = pushFrame(TAG_ETHERNET, rarpRequest(mac4));
    a.ws.send(rarpFrame.subarray(0, 6)); // incomplete
    a.ws.send(rarpFrame.subarray(6));     // completes it
    const [, f4] = await a.recvFrame(([t, f]) =>
        t === TAG_ETHERNET && f.length >= 42 &&
        f.readUInt16BE(12) === 0x8035 && f.readUInt16BE(20) === 4 &&
        Buffer.compare(f.subarray(32, 38), mac4) === 0);
    void f4;
    check("reassembled across messages (rarp reply)", true);

    a.close();
    b.close();

    // 5. Security checks against bridges with default (safe) settings
    console.log("test 5: safe defaults and admission checks");
    const port = await startBridge(["--www", WWW, "--max-clients-per-ip", "2"]);
    const base = `ws://127.0.0.1:${port}/bridge`;
    let r = await rawUpgrade(port, "/bridge", { Origin: "https://evil.example" });
    check("cross-origin WebSocket refused", r.status.includes(" 403"), r.status);
    r.socket.destroy();
    r = await rawUpgrade(port, "/bridge", { Origin: `http://127.0.0.1:${port}` });
    check("same-origin WebSocket accepted", r.status.includes(" 101"), r.status);
    r.socket.destroy();
    r = await rawUpgrade(port, "/somewhere-else");
    check("unknown path refused", r.status.includes(" 404"), r.status);
    r.socket.destroy();
    r = await rawUpgrade(port, "/bridge/bad%2Froom");
    check("invalid room name refused", r.status.includes(" 404"), r.status);
    r.socket.destroy();

    // Rooms are isolated networks
    const r1 = new Client("room1-A", `${base}/one`);
    const r2 = new Client("room2-B", `${base}/two`);
    await Promise.all([r1.opened, r2.opened]);
    r1.sendFrame(TAG_LOCALTALK, Buffer.from([0, 0, 0, 1, 0x30, 0x30, 0x81]));
    let leaked = false;
    try {
        await r2.recvFrame(([t]) => t === TAG_LOCALTALK, 1000);
        leaked = true;
    } catch { /* expected */ }
    check("rooms are isolated", !leaked);

    // Per-address limit (2): a third connection from 127.0.0.1 is refused
    r = await rawUpgrade(port, "/bridge/one");
    check("per-address connection limit", r.status.includes(" 429"), r.status);
    r.socket.destroy();
    r1.close();
    r2.close();

    // Ethernet is off by default: a RARP request gets no answer
    await new Promise((res) => setTimeout(res, 300));
    const e = new Client("no-ethernet", `${base}/eth`);
    await e.opened;
    e.sendFrame(TAG_ETHERNET, rarpRequest(Buffer.from("008019000001", "hex")));
    let answered = false;
    try {
        await e.recvFrame(([t]) => t === TAG_ETHERNET, 1500);
        answered = true;
    } catch { /* expected */ }
    check("Ethernet/NAT off by default", !answered);
    e.close();

    // Static files carry the security headers; hidden files are not served
    const page = await fetch(`http://127.0.0.1:${port}/`);
    const csp = page.headers.get("content-security-policy") || "";
    check("page served with a Content-Security-Policy", page.ok && csp.includes("script-src 'self'"));
    check("page is cross-origin isolated", page.headers.get("cross-origin-embedder-policy") === "require-corp");
    check("nosniff header", page.headers.get("x-content-type-options") === "nosniff");
    const hidden = await fetch(`http://127.0.0.1:${port}/.gitignore`);
    check("hidden files are not served", hidden.status === 404, String(hidden.status));
    const post = await fetch(`http://127.0.0.1:${port}/`, { method: "POST" });
    check("only GET/HEAD are allowed", post.status === 405, String(post.status));

    // 6. NAT egress policy: the guest cannot reach this host's loopback
    // (or other private addresses) unless explicitly allowed
    console.log("test 6: NAT egress policy");
    const natPort = await startBridge(["--ethernet"]);
    const [hitDefault, rstDefault] = await probeLoopbackEgress(`ws://127.0.0.1:${natPort}/bridge/egress`);
    check("loopback unreachable through NAT by default", !hitDefault);
    check("refused connection is reset (guest fails fast)", rstDefault);
    const openPort = await startBridge(["--ethernet", "--egress-allow-private"]);
    const [hitOpen] = await probeLoopbackEgress(`ws://127.0.0.1:${openPort}/bridge/egress`);
    check("--egress-allow-private reaches loopback", hitOpen);

    if (failures > 0) {
        console.log(`\n${failures} test(s) FAILED`);
        process.exit(1);
    }
    console.log("\nall tests passed");
    process.exit(0);
}

main().catch((e) => {
    console.error(`test error: ${e.message}`);
    process.exit(1);
});
