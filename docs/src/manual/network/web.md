# Web (WebAssembly) and AppleTalk

Snow can run in a web browser (`frontend_web`) with networking: browser tabs
reach each other and the local network through `snow-bridge`, a small host
process that the page connects to over a WebSocket.

```text
 browser tab (Snow wasm) ─┐
 browser tab (Snow wasm) ─┼─ WebSocket ─ snow-bridge ─┬─ LToUDP multicast (LAN: other emulators, TashTalk)
 browser tab (Snow wasm) ─┘                           └─ NAT engine (Ethernet: internet via MacTCP)
```

- **LocalTalk/AppleTalk** works with System 6 out of the box: the guest's
  printer port talks LLAP, the bridge relays the frames between all tabs and
  to the [LToUDP](ltoudp.md) multicast group.
- **Ethernet** (DaynaPORT SCSI/Link) reaches the internet through the
  bridge's [NAT engine](ethernet.md); the guest needs the DaynaPORT driver
  and MacTCP.

## Running

```sh
frontend_web/build-web.sh                      # needs the Emscripten SDK
cargo run --release -p snow_bridge -- --www frontend_web/www
# open http://localhost:8080/ in one or more tabs
```

or with Docker:

```sh
docker build -f frontend_web/Dockerfile -t snow-web .
docker run --rm --network host snow-web       # host networking for the LAN relay
```

Pick a ROM and a System 6 hard disk image and press **Start**. Bare HFS
volumes (as used by Infinite Mac) are fine: the frontend adds a SCSI driver
and partition map on the fly.

## AppleTalk on System 6

With freshly initialized PRAM, AppleTalk is *inactive* and System 6 never
opens the LocalTalk driver. Unless you load your own PRAM file, the web
frontend therefore boots with a generated PRAM that has AppleTalk active
("AppleTalk active at boot"). It also stores a random LocalTalk node address
hint: emulation is deterministic, so two machines booted from identical PRAM
would keep choosing the same addresses and colliding.

Address acquisition takes the System 6 LLAP driver up to 640 enquiries; the
status bar under the screen shows the LocalTalk frames and the AppleTalk
nodes seen on the network. See [Testing AppleTalk in System 6](web-testing.md)
for checks to run inside the emulated Macs, including multiplayer Bolo, chat
(EZChat) and MacPing.

`snow-bridge` options:

| Option               | Meaning                                                   |
|----------------------|-----------------------------------------------------------|
| `--port <n>`         | listen port (default 8080)                                |
| `--addr <ip>`        | listen address (default 0.0.0.0)                          |
| `--www <dir>`        | serve the web frontend (with the required COOP/COEP headers) |
| `--no-lan`           | relay LocalTalk between tabs only, not to the LToUDP group |
| `--https-stripping`  | let vintage browsers reach `https://` sites over `http://` |

The page must be cross-origin isolated (it uses `SharedArrayBuffer`); serving
it with `snow-bridge --www` takes care of that.
