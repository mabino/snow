"""Build the "Network Apps" HFS disk image with AppleTalk test programs.

Downloads each program from the Info-Mac archive, unpacks the BinHex and
StuffIt archives with unar (keeping resource forks and Finder types as
AppleDouble files), and writes them to a bare HFS volume that the web
frontend (or any Snow build) can mount as a second hard disk.

usage: build.py OUTPUT.dsk [WORKDIR]
"""
import os
import pathlib
import shutil
import struct
import subprocess
import sys
import hashlib
import time
import urllib.request

import machfs

MIRRORS = [
    "https://ftp.funet.fi/pub/mac/info-mac",
    "https://sunsite.icm.edu.pl/packages/info-mac",
]

# (folder name on the disk, archive path in Info-Mac, folder inside the
# archive, SHA-256 of the archive)
PROGRAMS = [
    ("Bolo", "game/bolo/bolo-0997.hqx", "Bolo",
     "c83deab0eefdde13d8868446530cc763536b9366b223da549b94b701f16af205"),
    ("EZChat", "comm/atlk/ez-chat-12.hqx", "EZChat 1.2 Folder",
     "77e0ab937ebf8ccda2f7619815f399e01615403adcd5c8fe4714de56d3af05f0"),
    ("MacPing", "comm/atlk/mac-ping-30-demo.hqx", "mac-ping-30-demo",
     "1efd782978d62b83eb896654a6e20071f16cc6b90641bc4c0916ac3bbee5950e"),
    ("TeleTalk", "comm/atlk/teletalk-111.hqx", "TeleTalk 1.1.1",
     "799a34153d3af1efa9de06dc77f2d67c98552176c981ed3932a134fa63b3719d"),
]
ARCHIVE_SUFFIXES = (".sit", ".sea", ".cpt")
VOLUME_NAME = "Network Apps"
VOLUME_SIZE = 8 * 1024 * 1024


def download(path, sha256, dest):
    """Fetch an archive from the first mirror that works, verifying it"""
    if dest.exists() and hashlib.sha256(dest.read_bytes()).hexdigest() == sha256:
        return
    for attempt in range(3):
        for mirror in MIRRORS:
            try:
                print(f"downloading {mirror}/{path}")
                with urllib.request.urlopen(f"{mirror}/{path}", timeout=600) as r:
                    data = r.read()
            except OSError as err:
                print(f"  failed: {err}")
                continue
            if hashlib.sha256(data).hexdigest() != sha256:
                print("  checksum mismatch, trying the next mirror")
                continue
            dest.write_bytes(data)
            return
        time.sleep(5 * (attempt + 1))
    sys.exit(f"could not download {path}")


def unpack(archive, dest):
    dest.mkdir(parents=True, exist_ok=True)
    subprocess.run(["unar", "-q", "-f", "-k", "hidden", "-o", str(dest), str(archive)], check=True)
    # BinHex usually wraps a StuffIt archive: unpack nested archives too
    for _ in range(3):
        nested = [p for p in dest.rglob("*") if p.is_file() and p.suffix.lower() in ARCHIVE_SUFFIXES
                  and not p.name.startswith("._")]
        for p in nested:
            subprocess.run(["unar", "-q", "-f", "-k", "hidden", "-o", str(p.parent), str(p)], check=True)
            p.unlink()
            p.with_name("._" + p.name).unlink(missing_ok=True)


def appledouble(path):
    """(resource fork, type, creator, Finder flags) from an AppleDouble file"""
    if not path.exists():
        return b"", b"????", b"????", 0
    d = path.read_bytes()
    rsrc, ftype, creator, flags = b"", b"????", b"????", 0
    (count,) = struct.unpack(">H", d[24:26])
    for i in range(count):
        eid, off, ln = struct.unpack(">III", d[26 + 12 * i:38 + 12 * i])
        if eid == 2:
            rsrc = d[off:off + ln]
        elif eid == 9:
            ftype, creator = d[off:off + 4], d[off + 4:off + 8]
            (flags,) = struct.unpack(">H", d[off + 8:off + 10])
    return rsrc, ftype, creator, flags


def add_folder(folder, src):
    names = sorted({n[2:] if n.startswith("._") else n for n in os.listdir(src)})
    for name in names:
        if name.startswith("Icon"):  # custom folder icon file (Icon\r)
            continue
        p = src / name
        macname = name.replace(":", "/")[:31]
        if p.is_dir():
            sub = machfs.Folder()
            folder[macname] = sub
            add_folder(sub, p)
            continue
        f = machfs.File()
        f.data = p.read_bytes() if p.exists() else b""
        f.rsrc, f.type, f.creator, f.flags = appledouble(src / ("._" + name))
        f.flags &= ~0x0100  # clear "inited" so the Finder places the icon
        folder[macname] = f


def main():
    out = pathlib.Path(sys.argv[1])
    work = pathlib.Path(sys.argv[2] if len(sys.argv) > 2 else "/tmp/network-apps")
    (work / "dl").mkdir(parents=True, exist_ok=True)
    volume = machfs.Volume()
    volume.name = VOLUME_NAME
    for disk_name, path, inner, sha256 in PROGRAMS:
        archive = work / "dl" / os.path.basename(path)
        download(path, sha256, archive)
        unpacked = work / "out" / disk_name
        shutil.rmtree(unpacked, ignore_errors=True)
        unpack(archive, unpacked)
        src = next(unpacked.rglob(inner), None)
        if src is None or not src.is_dir():
            sys.exit(f"{path}: folder {inner!r} not found in the archive")
        folder = machfs.Folder()
        volume[disk_name] = folder
        add_folder(folder, src)
    out.write_bytes(volume.write(size=VOLUME_SIZE, align=512))
    print(f"wrote {out} ({VOLUME_NAME})")


if __name__ == "__main__":
    main()
