# Testing AppleTalk inside System 6

These checks exercise the web frontend's AppleTalk networking from inside the
emulated Macs, complementing the automated tests (`frontend_web/tests`). They
were verified with a Macintosh SE ROM, System 6.0.8 (the Infinite Mac image)
and two browser tabs or browsers connected to one `snow-bridge`.

## Setup

1. Build the test disk with the network programs (needs Docker):

   ```sh
   frontend_web/tools/network-apps/make-disk.sh
   ```

   This downloads Bolo, EZChat, MacPing and TeleTalk from the Info-Mac archive
   (checksums are verified) and writes `frontend_web/www/media/NetworkApps.dsk`.
   The programs are 1990s shareware and demos; they are not part of this
   repository.

2. Put a ROM and a System 6 disk image in `frontend_web/www/media/` and start
   the bridge: `cargo run --release -p snow_bridge -- --www frontend_web/www`.

3. Open this page in two browsers (or tabs) and press **Start** in each:

   ```text
   http://127.0.0.1:8080/?rom=media/<rom>&disk=media/<system 6 disk>&disk=media/NetworkApps.dsk
   ```

   Use `127.0.0.1` rather than `localhost`. The bar under the screen shows
   LocalTalk frames sent and received and the AppleTalk nodes seen; with
   `&debug=1`, recent data frames are kept as hex in `window.snow.stats.dump`.

Both Macs boot with the same user name, "Infinite Mac". Several programs
refuse duplicate names, so give each Mac its own name as described below.

## Test cases

| # | Test | Expected result |
|---|------|-----------------|
| 1 | Boot both Macs | Each sends 640 LocalTalk frames (LLAP address acquisition) and goes quiet; each browser's "in" count matches the other's "out" |
| 2 | Apple menu → **Chooser** | **AppleTalk: Active** is selected (generated PRAM) |
| 3 | Chooser: type a different **User Name** on one Mac | The name is used by Responder and network programs after a restart |
| 4 | Chooser: click **AppleShare** (or **LaserWriter**) on Mac 1 | Mac 1 searches about twice a second; Mac 2's "in" counter climbs and both node addresses appear under "AppleTalk nodes". The server list stays empty (there is no server) |
| 5 | **MacPing**: open Network Apps → MacPing → MacPing™ 3.0 Demo, click OK | Both Macs are listed (type "Macintosh SE", addresses 0/*node*) and pinged continuously with 0% dropped |
| 6 | **EZChat**, see below | Messages typed on either Mac appear on both |
| 7 | **Bolo**, see below | Both Macs play in the same game; the Players menu lists both |
| 8 | Boot a second Mac with `&node=N` set to the first Mac's address | The second Mac's address enquiries are answered by the first and it moves to another address (this is also the automated end-to-end test) |

### EZChat (chat)

1. On both Macs: Network Apps → EZChat → **EZChat 1.2**; click the splash
   screen to dismiss it.
2. On Mac 2: **Configure → Change Identity…**, enter another name, **OK**.
3. On Mac 1: **Configure → Host**, then **File → Begin Chat…**. The window
   shows "Hosting Chat".
4. On Mac 2: **File → Begin Chat…**, click the `*` zone, select Mac 1's chat
   under **Hosts**, **OK**. Mac 1 reports that Mac 2 joined.
5. Click the input box at the bottom, type, press Return. `/List` lists the
   users, `/name: text` sends a private message.

Without step 2 the host refuses the second Mac ("user name is not valid").

### Bolo (multiplayer game)

1. On both Macs: Network Apps → Bolo → **Bolo 0.99.7**; choose **AppleTalk**,
   **OK**.
2. On Mac 2: type a different player name (the field is already selected).
3. On Mac 1: **New**, then **OK** on the game options.
4. On Mac 2: Mac 1's player appears under "Bolo Players in this zone"; select
   it and click **Join**.

Click the Mac screen before playing so the keyboard goes to the game.

## Known issues

- **TeleTalk 1.1.1** finds the other Mac (Scan, after giving both Macs
  different Chooser names) but stops at "Trying to reach…": it never sends
  the connection request, although the network delivers its lookups and
  replies. At launch it also reports error −17 while scanning for zones (the
  zone call needs a newer AppleTalk than the Mac SE ROM's); dismiss it.
  Installing a newer AppleTalk into System 6 may help but has not been tried.
- Bolo's documentation asks for AppleTalk version 52 or later; version 0.99.7
  nevertheless worked on the Mac SE in these tests.
