// Page <-> emulator worker I/O for the Snow web frontend.
//
// The emulator's main loop never returns to the worker's event loop, so the
// worker cannot receive postMessage()s once it is running. Input events and
// received network packets are therefore written by the page into a
// single-producer/single-consumer ring buffer in a SharedArrayBuffer, which
// the worker glue (src/web.js) drains from inside the emulator loop.
//
// Ring layout: Int32 [readIndex, writeIndex] followed by RING_BYTES of data.
// Indices increase monotonically (wrapping at 2^32); data offsets are the
// index masked by RING_BYTES - 1. A record is
//
//     [length u16 BE][type u8][payload]     (length = 1 + payload length)
//
// Record types and payloads are documented in src/io_protocol.rs.

export const RING_BYTES = 1 << 20;
const HEADER_BYTES = 8;
const READ = 0;
const WRITE = 1;

export const REC = Object.freeze({
    ETHERNET: 0x00,
    LOCALTALK: 0x01,
    KEY: 0x10,
    MOUSE_ABS: 0x11,
    MOUSE_REL: 0x12,
    MOUSE_BUTTON: 0x13,
    AUDIO_DRAINED: 0x14,
    LINK: 0x15,
});

const EMPTY = new Uint8Array(0);

export function createIoBuffer() {
    return new SharedArrayBuffer(HEADER_BYTES + RING_BYTES);
}

export class IoRing {
    constructor(sab) {
        this.index = new Int32Array(sab, 0, 2);
        this.data = new Uint8Array(sab, HEADER_BYTES);
        this.mask = this.data.length - 1;
        if ((this.data.length & this.mask) !== 0) {
            throw new Error("ring size must be a power of two");
        }
    }

    // Bytes currently queued
    used() {
        return (Atomics.load(this.index, WRITE) - Atomics.load(this.index, READ)) >>> 0;
    }

    // Append a record; returns false (dropping it) when the ring is full
    push(type, payload = EMPTY) {
        const length = 1 + payload.length;
        if (length > 0xffff) {
            return false;
        }
        const w = Atomics.load(this.index, WRITE);
        if (2 + length > this.data.length - this.used()) {
            return false;
        }
        this.#put(w, length >> 8);
        this.#put(w + 1, length & 0xff);
        this.#put(w + 2, type);
        this.#copyIn(w + 3, payload);
        Atomics.store(this.index, WRITE, (w + 2 + length) | 0);
        return true;
    }

    // Remove and return the next record as [type, ...payload], or null
    pop() {
        const r = Atomics.load(this.index, READ);
        if (r === Atomics.load(this.index, WRITE)) {
            return null;
        }
        const length = (this.data[r & this.mask] << 8) | this.data[(r + 1) & this.mask];
        const out = new Uint8Array(length);
        const start = (r + 2) & this.mask;
        const first = Math.min(length, this.data.length - start);
        out.set(this.data.subarray(start, start + first), 0);
        if (first < length) {
            out.set(this.data.subarray(0, length - first), first);
        }
        Atomics.store(this.index, READ, (r + 2 + length) | 0);
        return out;
    }

    #put(at, byte) {
        this.data[at & this.mask] = byte;
    }

    #copyIn(at, bytes) {
        const start = at & this.mask;
        const first = Math.min(bytes.length, this.data.length - start);
        this.data.set(bytes.subarray(0, first), start);
        if (first < bytes.length) {
            this.data.set(bytes.subarray(first), 0);
        }
    }
}

// ---------------------------------------------------------------- records

function be16(...values) {
    const out = new Uint8Array(values.length * 2);
    const view = new DataView(out.buffer);
    values.forEach((v, i) => view.setUint16(i * 2, v & 0xffff));
    return out;
}

export const encode = {
    key: (scancode, down) => [REC.KEY, Uint8Array.of(scancode, down ? 1 : 0)],
    mouseAbs: (x, y) => [REC.MOUSE_ABS, be16(Math.max(0, x), Math.max(0, y))],
    mouseRel: (dx, dy) => [
        REC.MOUSE_REL,
        be16(Math.max(-32768, Math.min(32767, dx)), Math.max(-32768, Math.min(32767, dy))),
    ],
    mouseButton: (down) => [REC.MOUSE_BUTTON, Uint8Array.of(down ? 1 : 0)],
    audioDrained: (samples) => {
        const out = new Uint8Array(4);
        new DataView(out.buffer).setUint32(0, samples >>> 0);
        return [REC.AUDIO_DRAINED, out];
    },
    link: (up, description) => {
        const text = new TextEncoder().encode(description);
        const out = new Uint8Array(1 + text.length);
        out[0] = up ? 1 : 0;
        out.set(text, 1);
        return [REC.LINK, out];
    },
};

// ------------------------------------------------------------ net framing

// Split a byte stream of [tag u8][length u16 BE][payload] frames (the
// snow-bridge wire format, see core/src/net.rs). Returns the complete
// frames and the unconsumed remainder.
export function splitFrames(bytes) {
    const frames = [];
    let pos = 0;
    while (pos + 3 <= bytes.length) {
        const length = (bytes[pos + 1] << 8) | bytes[pos + 2];
        if (pos + 3 + length > bytes.length) {
            break;
        }
        frames.push([bytes[pos], bytes.subarray(pos + 3, pos + 3 + length)]);
        pos += 3 + length;
    }
    return [frames, bytes.subarray(pos)];
}

// ----------------------------------------------------------------- keymap

// DOM KeyboardEvent.code -> Apple M0115 scancode (core/src/keymap/aekm0115.rs)
export const KEY_SCANCODES = Object.freeze({
    Escape: 0x35,
    F1: 0x7a, F2: 0x78, F3: 0x63, F4: 0x76, F5: 0x60, F6: 0x61,
    F7: 0x62, F8: 0x64, F9: 0x65, F10: 0x6d, F11: 0x67, F12: 0x6f,
    PrintScreen: 0x69, ScrollLock: 0x6b, Pause: 0x71, Power: 0x7f,
    Backquote: 0x32, Digit1: 0x12, Digit2: 0x13, Digit3: 0x14, Digit4: 0x15,
    Digit5: 0x17, Digit6: 0x16, Digit7: 0x1a, Digit8: 0x1c, Digit9: 0x19,
    Digit0: 0x1d, Minus: 0x1b, Equal: 0x18, Backspace: 0x33,
    Insert: 0x72, Home: 0x73, PageUp: 0x74, NumLock: 0x47,
    NumpadAdd: 0x51, NumpadDivide: 0x4b, NumpadMultiply: 0x43,
    Tab: 0x30, KeyQ: 0x0c, KeyW: 0x0d, KeyE: 0x0e, KeyR: 0x0f, KeyT: 0x11,
    KeyY: 0x10, KeyU: 0x20, KeyI: 0x22, KeyO: 0x1f, KeyP: 0x23,
    BracketLeft: 0x21, BracketRight: 0x1e, Backslash: 0x2a,
    Delete: 0x75, End: 0x77, PageDown: 0x79,
    Numpad7: 0x59, Numpad8: 0x5b, Numpad9: 0x5c, NumpadSubtract: 0x4e,
    CapsLock: 0x39, KeyA: 0x00, KeyS: 0x01, KeyD: 0x02, KeyF: 0x03,
    KeyG: 0x05, KeyH: 0x04, KeyJ: 0x26, KeyK: 0x28, KeyL: 0x25,
    Semicolon: 0x29, Quote: 0x27, Enter: 0x24,
    Numpad4: 0x56, Numpad5: 0x57, Numpad6: 0x58, NumpadEqual: 0x45,
    ShiftLeft: 0x38, KeyZ: 0x06, KeyX: 0x07, KeyC: 0x08, KeyV: 0x09,
    KeyB: 0x0b, KeyN: 0x2d, KeyM: 0x2e, Comma: 0x2b, Period: 0x2f,
    Slash: 0x2c, ShiftRight: 0x7b, ArrowUp: 0x3e,
    Numpad1: 0x53, Numpad2: 0x54, Numpad3: 0x55,
    ControlLeft: 0x36, AltLeft: 0x3a, MetaLeft: 0x37, Space: 0x31,
    MetaRight: 0x37, AltRight: 0x7c, ControlRight: 0x7d,
    ArrowLeft: 0x3b, ArrowDown: 0x3d, ArrowRight: 0x3c,
    Numpad0: 0x52, NumpadDecimal: 0x41, NumpadEnter: 0x4c,
});
