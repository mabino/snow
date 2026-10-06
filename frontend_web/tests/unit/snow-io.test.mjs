// Unit tests for www/snow-io.js (the page -> worker I/O ring)
import assert from "node:assert/strict";
import { test } from "node:test";
import { IoRing, createIoBuffer, encode, splitFrames, REC, RING_BYTES, KEY_SCANCODES } from "../../www/snow-io.js";

test("records round-trip in order", () => {
    const ring = new IoRing(createIoBuffer());
    assert.equal(ring.pop(), null);
    ring.push(...encode.key(KEY_SCANCODES.KeyA, true));
    ring.push(...encode.mouseAbs(300, 200));
    ring.push(...encode.mouseRel(-2, 3));
    ring.push(REC.LOCALTALK, Uint8Array.of(0, 0, 0, 1, 0xff, 0x20, 0x01));
    ring.push(...encode.link(true, "ws://x/bridge"));

    assert.deepEqual([...ring.pop()], [REC.KEY, 0x00, 1]);
    assert.deepEqual([...ring.pop()], [REC.MOUSE_ABS, 0x01, 0x2c, 0x00, 0xc8]);
    assert.deepEqual([...ring.pop()], [REC.MOUSE_REL, 0xff, 0xfe, 0x00, 0x03]);
    assert.deepEqual([...ring.pop()], [REC.LOCALTALK, 0, 0, 0, 1, 0xff, 0x20, 0x01]);
    const link = ring.pop();
    assert.equal(link[0], REC.LINK);
    assert.equal(link[1], 1);
    assert.equal(new TextDecoder().decode(link.subarray(2)), "ws://x/bridge");
    assert.equal(ring.pop(), null);
    assert.equal(ring.used(), 0);
});

test("records wrap around the end of the ring", () => {
    const ring = new IoRing(createIoBuffer());
    const payload = new Uint8Array(1500).map((_, i) => i & 0xff);
    // Push/pop enough to cross the wrap point several times
    for (let i = 0; i < (RING_BYTES / 1503) * 3; i++) {
        assert.ok(ring.push(REC.ETHERNET, payload));
        const rec = ring.pop();
        assert.equal(rec.length, 1501);
        assert.deepEqual(rec.subarray(1), payload);
    }
});

test("a full ring drops records instead of overwriting", () => {
    const ring = new IoRing(createIoBuffer());
    const payload = new Uint8Array(1000);
    let pushed = 0;
    while (ring.push(REC.ETHERNET, payload)) pushed++;
    assert.equal(pushed, Math.floor(RING_BYTES / 1003));
    assert.ok(ring.push(...encode.mouseButton(true)), "small records still fit");
    for (let i = 0; i < pushed; i++) assert.equal(ring.pop().length, 1001);
    assert.deepEqual([...ring.pop()], [REC.MOUSE_BUTTON, 1]);
});

test("two views of the same buffer share the ring (page vs worker)", () => {
    const sab = createIoBuffer();
    const page = new IoRing(sab);
    const worker = new IoRing(sab);
    page.push(...encode.audioDrained(0x01020304));
    assert.deepEqual([...worker.pop()], [REC.AUDIO_DRAINED, 1, 2, 3, 4]);
    assert.equal(page.used(), 0);
});

test("splitFrames handles partial and multiple frames", () => {
    const stream = Uint8Array.of(1, 0, 2, 0xaa, 0xbb, 0, 0, 1, 0xcc, 1, 0);
    const [frames, rest] = splitFrames(stream);
    assert.equal(frames.length, 2);
    assert.deepEqual([frames[0][0], ...frames[0][1]], [1, 0xaa, 0xbb]);
    assert.deepEqual([frames[1][0], ...frames[1][1]], [0, 0xcc]);
    assert.deepEqual([...rest], [1, 0]);
});
