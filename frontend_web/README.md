# Snow web frontend

A standalone WebAssembly build of Snow that runs classic 68k Macs (System 6
and later) in a browser tab, with AppleTalk over LocalTalk and Ethernet
networking through the `snow-bridge` host process (`../bridge`). User
documentation: [docs/src/manual/network/web.md](../docs/src/manual/network/web.md).

```sh
./build-web.sh                                          # builds www/snow_web.{js,wasm}
cargo run --release -p snow_bridge -- --www frontend_web/www
open http://localhost:8080/
```

URL parameters for scripted use: `rom=<url>`, `disk=<url>` (repeatable),
`model=<gestalt id>`, `node=<LocalTalk node hint>`, `ethernet=1`,
`localtalk=0`, `bridge=<ws url>`, `autostart=1`, `debug=1` (keeps a hex dump of
recent AppleTalk data frames in `window.snow.stats.dump`).

## How it fits together

```text
 page (www/index.html, main thread)            worker (www/worker.js + snow_web.wasm)
 ─────────────────────────────────            ──────────────────────────────────────
 WebSocket to snow-bridge  ──frames──▶ I/O ring ──▶ src/input.rs ──▶ snow_core::net hub
 keyboard / mouse / audio drain ─────▶ (SharedArrayBuffer,              │  ▲
                                        www/snow-io.js)                  ▼  │
 canvas, Web Audio, ws.send ◀──── postMessage ◀── src/web.js ◀── LocalTalk bridge / DaynaPORT
```

- The emulator loop never returns to the worker's event loop, so nothing can
  be `postMessage`d to it. The page writes input events and received packets
  into a single-producer/single-consumer ring in a `SharedArrayBuffer`
  (record format: `src/io_protocol.rs`), which the worker drains every loop
  iteration. Output (video, audio, outgoing packets) is posted to the page.
- In the core, the LocalTalk bridge and the DaynaPORT adapter exchange
  packets with `snow_core::net`, a frontend-provided transport; on native
  builds they use UDP multicast and the NAT engine directly instead.
- `snow-bridge` relays LocalTalk datagrams between tabs and the LToUDP
  multicast group, and runs one NAT engine per tab for Ethernet. Wire
  format: `[tag u8][length u16 BE][payload]`, tag 0 = Ethernet, 1 = LToUDP.
- `src/media.rs` makes bare HFS volumes bootable (SCSI driver headers in
  `assets/`, the same ones Infinite Mac uses) and generates a PRAM with
  AppleTalk active and a random node address hint.

## Tests

Manual test cases inside System 6 (Chooser, MacPing, EZChat, Bolo) are in
[docs/src/manual/network/web-testing.md](../docs/src/manual/network/web-testing.md);
`tools/network-apps/make-disk.sh` builds the disk with those programs
(downloaded from the Info-Mac archive, not shipped here).

```sh
tests/run-all.sh                     # Rust + JS unit tests, bridge protocol, browser e2e
docker build -f frontend_web/Dockerfile --target test -t snow-web-test . && docker run --rm snow-web-test
```

The browser end-to-end test (`tests/e2e.mjs`, Playwright) checks the
plumbing with a dummy ROM. With `SNOW_ROM` (e.g. a Mac SE ROM) and
`SNOW_DISK` (a System 6 image) it boots two Macs and plays out an LLAP node
address collision through the bridge: the second Mac probes the first one's
address, gets its lapACK and moves to another address.
