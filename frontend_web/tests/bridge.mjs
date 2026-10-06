// Test client for snow-bridge
//
// Validates the bridge's two paths end-to-end without a real Mac:
//   1. RARP request  -> NAT engine's RARP server (mactcp helpers)
//   2. ARP request   -> smoltcp gateway
//   3. LocalTalk (LToUDP) datagrams relayed between two clients
//   4. Frame reassembly across WebSocket messages
//
// Usage:  node bridge.mjs [ws://host:port]
// Without a URL, the test starts its own bridge (SNOW_BRIDGE, default
// ../../target/release/snow_bridge) on a free port, without the LAN relay.

import { spawn } from "node:child_process";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

let URL = process.argv[2];
let bridgeProcess = null;
if (!URL) {
    const here = path.dirname(fileURLToPath(import.meta.url));
    const bin = process.env.SNOW_BRIDGE || path.resolve(here, "../../target/release/snow_bridge");
    const port = await new Promise((resolve) => {
        const srv = net.createServer().listen(0, "127.0.0.1", () => {
            const { port } = srv.address();
            srv.close(() => resolve(port));
        });
    });
    bridgeProcess = spawn(bin, ["--addr", "127.0.0.1", "--port", String(port), "--no-lan"], {
        stdio: ["ignore", "ignore", "inherit"],
    });
    process.on("exit", () => bridgeProcess.kill());
    for (let i = 0; i < 100; i++) {
        const up = await new Promise((resolve) => {
            const s = net.connect(port, "127.0.0.1", () => { s.destroy(); resolve(true); });
            s.on("error", () => resolve(false));
        });
        if (up) break;
        await new Promise((r) => setTimeout(r, 100));
    }
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

// -------------------------------------------------------------- ws client

class Client {
    constructor(name) {
        this.name = name;
        this.ws = new WebSocket(URL);
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

    // 3. LocalTalk relay A -> B and B -> A
    console.log("test 3: LocalTalk relay");
    const dgramA = Buffer.concat([Buffer.from("aabbccdd", "hex"), Buffer.from([0x01, 0xff, 0x05, 0xde, 0xad])]);
    const dgramB = Buffer.concat([Buffer.from("11223344", "hex"), Buffer.from([0x02, 0x05, 0xff, 0xbe, 0xef])]);
    // (Match on the 4-byte sender ID so that stray datagrams from real
    // LToUDP nodes on the LAN cannot satisfy the waiters.)
    a.sendFrame(TAG_LOCALTALK, dgramA);
    const [, gotB] = await b.recvFrame(([t, f]) =>
        t === TAG_LOCALTALK && f.subarray(0, 4).equals(Buffer.from("aabbccdd", "hex")));
    check("A->B datagram delivered", Buffer.compare(gotB, dgramA) === 0);
    b.sendFrame(TAG_LOCALTALK, dgramB);
    const [, gotA] = await a.recvFrame(([t, f]) =>
        t === TAG_LOCALTALK && f.subarray(0, 4).equals(Buffer.from("11223344", "hex")));
    check("B->A datagram delivered", Buffer.compare(gotA, dgramB) === 0);
    // A's own datagram must NOT be echoed back to A. Match on the sender ID
    // (aabbccdd) rather than "any LocalTalk frame": a real LToUDP node on
    // the LAN may legitimately retransmit the broadcast with its own sender
    // ID, which the bridge correctly forwards.
    let echoed = false;
    try {
        await a.recvFrame(([t, f]) =>
            t === TAG_LOCALTALK &&
            f.subarray(0, 4).equals(Buffer.from("aabbccdd", "hex")), 1000);
        echoed = true;
    } catch { /* expected */ }
    check("no loopback echo of own datagram", !echoed);

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
