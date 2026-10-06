// Snow web frontend - emulator worker.
//
// Loads the Emscripten module (snow_web.js + snow_web.wasm) and runs it.
// The emulator loop blocks this worker for good, so the only message it
// ever handles is the initial start request:
//
//   { type: "start", argv: string[], files: [{ path, data }], io: SharedArrayBuffer }
//
// Everything afterwards flows through the I/O ring (page -> worker, see
// snow-io.js) and postMessage (worker -> page, see src/web.js).

import { IoRing } from "./snow-io.js";

let started = false;

// Forward log lines to the page, at most LOG_BURST per second: a guest that
// runs wild can produce warnings far faster than the page can take them
const LOG_BURST = 100;
let logWindowStart = 0;
let logCount = 0;
let logDropped = 0;
function forwardLog(line) {
    const now = performance.now();
    if (now - logWindowStart >= 1000) {
        if (logDropped) {
            self.postMessage({ type: "log", line: `(${logDropped} log lines dropped)` });
        }
        logWindowStart = now;
        logCount = 0;
        logDropped = 0;
    }
    if (logCount++ < LOG_BURST) {
        self.postMessage({ type: "log", line });
    } else {
        logDropped++;
    }
}

self.addEventListener("message", async (event) => {
    const msg = event.data;
    if (!msg || msg.type !== "start" || started) {
        return;
    }
    started = true;
    self.snowIoRing = new IoRing(msg.io);

    const moduleArg = {
        arguments: msg.argv,
        print: forwardLog,
        printErr: forwardLog,
        preRun: [
            (module) => {
                const FS = module.FS;
                for (const file of msg.files) {
                    const slash = file.path.lastIndexOf("/");
                    const dir = file.path.slice(0, slash) || "/";
                    if (!FS.analyzePath(dir).exists) {
                        FS.mkdirTree(dir);
                    }
                    FS.writeFile(file.path, new Uint8Array(file.data));
                }
            },
        ],
    };
    try {
        const { default: createEmulator } = await import("./snow_web.js");
        await createEmulator(moduleArg);
    } catch (err) {
        // The module never returns normally; anything here is a failure
        self.postMessage({ type: "error", message: "Emulator stopped: " + (err?.message ?? err) });
    }
});
