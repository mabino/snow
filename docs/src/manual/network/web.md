# Web (WebAssembly) and AppleTalk

Snow can run in a web browser (`frontend_web`) with networking: browser tabs
reach each other and the local network through `snow-bridge`, a small host
process that the page connects to over a WebSocket.

```text
 browser tab (Snow wasm) ─┐                          ┌─ room "default" ─ LToUDP multicast (optional, --lan)
 browser tab (Snow wasm) ─┼─ WebSocket ─ snow-bridge ┼─ room "bolo"      (each room: one AppleTalk network)
 browser tab (Snow wasm) ─┘                          └─ NAT engine per tab (optional, --ethernet)
```

- **LocalTalk/AppleTalk** works with System 6 out of the box: the guest's
  printer port talks LLAP, and the bridge relays the frames between the
  tabs in the same *room*. Every room is an isolated AppleTalk network; the
  page picks one with its **Room** field or `?room=<name>` (default
  `default`). With `--lan`, the default room is also linked to the
  [LToUDP](ltoudp.md) multicast group, reaching other emulators and real
  Macs (e.g. via TashTalk).
- **Ethernet** (DaynaPORT SCSI/Link) reaches the internet through a
  [NAT engine](ethernet.md) per tab, only with `--ethernet`; the guest needs
  the DaynaPORT driver and MacTCP.

## Running

```sh
frontend_web/build-web.sh                      # needs the Emscripten SDK
cargo run --release -p snow_bridge -- --www frontend_web/www
# open http://127.0.0.1:8080/ in one or more tabs
```

or with Docker (the image listens on all interfaces inside the container;
publish the port on loopback unless you mean to share it):

```sh
docker build -f frontend_web/Dockerfile -t snow-web .
docker run --rm -p 127.0.0.1:8080:8080 snow-web
docker run --rm --network host snow-web --lan  # LAN relay needs host networking
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

## snow-bridge options and safe defaults

The defaults are safe for running on your own machine: the bridge listens on
loopback only, relays nothing to the LAN, provides no internet access, only
accepts WebSockets from its own origin, and limits every client. Each of
these is an explicit opt-in:

| Option | Default | Meaning |
|---|---|---|
| `--addr <ip>` | `127.0.0.1` | listen address (`0.0.0.0` exposes the bridge to the network) |
| `--port <n>` | `8080` | listen port |
| `--www <dir>` | none | also serve the web frontend, with cross-origin isolation and a strict Content Security Policy |
| `--allow-origin <origin>` | same origin | web origins allowed to connect (repeatable); `--allow-any-origin` disables the check (development only) |
| `--require-origin` | off | refuse clients without an Origin header (non-browser clients) |
| `--trust-proxy` | off | take client addresses from `X-Forwarded-For` (only behind a proxy that sets it) |
| `--lan` | off | link the default room to the LToUDP multicast group |
| `--ethernet` | off | NAT engine per client (internet access for the emulated Mac) |
| `--egress-allow-private` | off | let NAT reach loopback, private, link-local and other non-public addresses |
| `--egress-ports <list>` | any | only allow these NAT destination ports, e.g. `80,443` |
| `--https-stripping` | off | let vintage browsers reach `https://` sites over `http://` (needs `--ethernet`) |
| `--max-clients <n>` | 64 | simultaneous clients |
| `--max-clients-per-ip <n>` | 8 | simultaneous clients per address |
| `--max-rooms <n>` / `--max-room-clients <n>` | 32 / 16 | rooms, and clients per room |
| `--max-nat-flows <n>` | 64 | NAT connections per client |
| `--rate-frames <n>` / `--rate-bytes <n>` | 1000 / 1 MiB | per-client frames and bytes per second (NAT downloads are capped at the byte rate too) |
| `--idle-timeout <s>` / `--max-session <s>` | 120 / unlimited | drop unresponsive clients / limit session length |

Clients connect to `/bridge/<room>` (room names: 1–64 of `A-Z a-z 0-9 . _ -`).
The bridge assigns each client its LToUDP sender ID, drops malformed
LocalTalk and Ethernet frames, and disconnects clients that keep sending
them. Even with `--ethernet`, the NAT engine only connects to public internet
addresses unless `--egress-allow-private` is given; refused connections are
reset so the guest fails fast.

The page must be cross-origin isolated (it uses `SharedArrayBuffer`); serving
it with `snow-bridge --www` takes care of that. Its Content Security Policy
only allows connections to the page's own origin, so the bridge and the page
are served together.

## Running it as a public service

A public deployment should look like Infinite Mac's networking: rooms that
relay AppleTalk between browsers, and no internet access for the guests.

- Build the bridge without NAT: `cargo build --release -p snow_bridge
  --no-default-features`. `--ethernet` is then rejected at startup.
- Run it behind a TLS-terminating reverse proxy (pages over `https://`,
  WebSockets over `wss://`), for example:

  ```sh
  snow-bridge --www /srv/www --addr 127.0.0.1 --port 8080 \
      --allow-origin https://mac.example.com --require-origin --trust-proxy \
      --max-clients 200 --max-clients-per-ip 4 --max-session 43200
  ```

  with the proxy forwarding `https://mac.example.com/` (including WebSocket
  upgrades on `/bridge/`) to `127.0.0.1:8080` and setting `X-Forwarded-For`.
- Leave `--lan` off: it would put internet users on the server's local
  network.
- Shareable private sessions use unguessable room names
  (`?room=<random>`); any visitor can join a room whose name they know.
- If internet access for guests is really needed, keep `--ethernet` behind
  authentication, add `--egress-ports 80,443`, never use
  `--egress-allow-private` or `--https-stripping`, and send the traffic from
  a separate egress address with an abuse contact.

What the bridge does not provide: user accounts, abuse reporting, or
protection against floods larger than the limits above (use the proxy or
platform for that). Emulated Macs have no network security of their own: a
peer in the same room can crash an emulated Mac or read what it shares over
AppleTalk, but it cannot reach beyond the browser's WebAssembly sandbox.
Hosting Apple ROMs and system software publicly needs its own legal basis;
letting visitors load their own ROM avoids the question.
