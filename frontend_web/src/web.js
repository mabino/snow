// JavaScript glue for the standalone Snow web frontend
//
// Linked into the wasm module with --js-library. The module runs inside a
// Web Worker (www/worker.js) and never returns to the worker's event loop,
// so communication with the page works like this:
//
//   worker -> page:  postMessage (video frames, audio, outgoing network
//                    packets, errors) - posting from a busy worker is fine
//   page -> worker:  the I/O ring in a SharedArrayBuffer (www/snow-io.js),
//                    drained by web_js_poll_io from the emulator loop
//
// worker.js publishes the ring as self.snowIoRing before the module starts.
//
// IMPORTANT: Emscripten only inlines the *functions* of the mergeInto
// library object into the module; top-level variables in this file would be
// silently dropped. Persistent state therefore lives on `self`.

mergeInto(LibraryManager.library, {
    $snowState() {
        if (!self.__snowWebState) {
            self.__snowWebState = {
                video: null, // { width, height }
                audioRate: 0,
                audioChannels: 0,
                audioQueued: 0, // interleaved samples handed to the page
                lastBlitTime: 0,
                // Private SharedArrayBuffer word used purely to block in
                // web_js_sleep (the wasm heap itself is not shared)
                sleepView: null,
            };
            try {
                self.__snowWebState.sleepView = new Int32Array(new SharedArrayBuffer(4));
            } catch (err) {
                // No cross-origin isolation; web_js_sleep busy-waits
            }
        }
        return self.__snowWebState;
    },

    $snowPost(message, transfer) {
        if (transfer && transfer.length) {
            postMessage(message, transfer);
        } else {
            postMessage(message);
        }
    },

    web_js_init__deps: ["$snowState"],
    web_js_init() {
        snowState();
        if (!self.snowIoRing) {
            console.warn("snow web: no I/O ring; input and networking are disabled");
        }
    },

    // ------------------------------------------------------------ runtime

    web_js_sleep__deps: ["$snowState"],
    web_js_sleep(seconds) {
        if (seconds <= 0) {
            return;
        }
        const ms = Math.min(seconds * 1000, 500);
        const view = snowState().sleepView;
        if (view) {
            // The word never changes, so this always times out after `ms`
            Atomics.wait(view, 0, 0, ms);
        } else {
            const end = performance.now() + ms;
            while (performance.now() < end) {
                // busy-wait
            }
        }
    },

    // Pop the next record of the page's I/O ring into the wasm heap. Audio
    // drain reports are consumed here (the audio accounting lives in JS).
    web_js_poll_io__deps: ["$snowState"],
    web_js_poll_io(ptr, cap) {
        const ring = self.snowIoRing;
        if (!ring) {
            return 0;
        }
        for (;;) {
            const rec = ring.pop();
            if (!rec) {
                return 0;
            }
            if (rec[0] === 0x14 /* AUDIO_DRAINED */ && rec.length === 5) {
                const samples = new DataView(rec.buffer).getUint32(1);
                const state = snowState();
                state.audioQueued = Math.max(0, state.audioQueued - samples);
                continue;
            }
            if (rec.length > cap) {
                return -1;
            }
            HEAPU8.set(rec, ptr);
            return rec.length;
        }
    },

    web_js_net_send__deps: ["$snowPost"],
    web_js_net_send(ptr, len) {
        const data = HEAPU8.slice(ptr, ptr + len);
        snowPost({ type: "net-send", data: data.buffer }, [data.buffer]);
    },

    web_js_report_error__deps: ["$snowPost"],
    web_js_report_error(messagePtr) {
        const message = UTF8ToString(messagePtr);
        console.error("snow: " + message);
        snowPost({ type: "error", message: message });
    },

    // -------------------------------------------------------------- video

    web_js_did_open_video__deps: ["$snowState", "$snowPost"],
    web_js_did_open_video(width, height) {
        snowState().video = { width: width, height: height };
        snowPost({ type: "video-open", width: width, height: height });
    },

    web_js_blit__deps: ["$snowState", "$snowPost"],
    web_js_blit(ptr, size) {
        const state = snowState();
        const video = state.video;
        if (!video) {
            return;
        }
        const needed = video.width * video.height * 4;
        if (size < needed) {
            return;
        }
        // Cap the posted frame rate at ~60 fps
        const now = performance.now();
        if (state.lastBlitTime > 0 && now - state.lastBlitTime < 14) {
            return;
        }
        state.lastBlitTime = now;
        const data = HEAPU8.slice(ptr, ptr + needed);
        snowPost(
            { type: "video", width: video.width, height: video.height, data: data.buffer },
            [data.buffer]
        );
    },

    // -------------------------------------------------------------- audio

    // The page owns the Web Audio graph; the worker only tracks how much
    // audio it has handed over (the page reports playback through the I/O
    // ring) so that Rust can apply backpressure.
    web_js_did_open_audio__deps: ["$snowState", "$snowPost"],
    web_js_did_open_audio(sampleRate, sampleBits, channels) {
        const state = snowState();
        if (sampleBits !== 32) {
            console.warn("snow web: unsupported audio sample size " + sampleBits);
            return;
        }
        state.audioRate = sampleRate;
        state.audioChannels = channels;
        state.audioQueued = 0;
        snowPost({ type: "audio-open", sampleRate: sampleRate, channels: channels });
    },

    web_js_audio_buffer_size__deps: ["$snowState"],
    web_js_audio_buffer_size() {
        const state = snowState();
        if (!state.audioRate) {
            return -1;
        }
        return state.audioQueued * 4;
    },

    web_js_enqueue_audio__deps: ["$snowState", "$snowPost"],
    web_js_enqueue_audio(ptr, size) {
        const state = snowState();
        if (!state.audioRate) {
            return;
        }
        const samples = size >> 2;
        state.audioQueued += samples;
        // If the page is not draining (no audio output), bound the backlog
        if (state.audioQueued > state.audioRate * state.audioChannels * 2) {
            state.audioQueued = 0;
            return;
        }
        const data = HEAPF32.slice(ptr >> 2, (ptr >> 2) + samples);
        snowPost({ type: "audio", data: data.buffer }, [data.buffer]);
    },

    web_js_set_clipboard_text__deps: ["$snowPost"],
    web_js_set_clipboard_text(textPtr) {
        snowPost({ type: "clipboard", text: UTF8ToString(textPtr) });
    },
});
