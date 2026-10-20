#!/usr/bin/env python3
"""Native macOS smoke test: Photos/Finder's Open Documents handoff, cold and warm.

Run after cargo build -p lightcraft. Uses an isolated temporary .app and in-memory library;
never opens Photos or sends synthetic input. --app tests an existing bundle with the same
isolated in-memory session, without changing that bundle or the user's library.
"""

import argparse
import json
import os
from pathlib import Path
import plistlib
import shutil
import socket
import struct
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]


def raf(path, value):
    """Original synthetic uncompressed RAF, same container layout as raw's unit fixtures."""
    width = height = 16
    strip = struct.pack("<H", value) * (width * height)
    sub = 26
    white_balance = sub + 2 + 12 * 8 + 4
    strip_offset = white_balance + 12
    entries = [(0xF001, 4, 1, width), (0xF002, 4, 1, height), (0xF003, 4, 1, 14),
               (0xF006, 4, 1, 0), (0xF007, 4, 1, strip_offset), (0xF008, 4, 1, len(strip)),
               (0xF00A, 4, 1, 64), (0xF00E, 4, 3, white_balance)]
    block = b"II*\0\x08\0\0\0" + struct.pack("<HHHIII", 1, 0xF000, 13, 1, sub, 0)
    block += struct.pack("<H", len(entries))
    block += b"".join(struct.pack("<HHII", *entry) for entry in entries)
    block += struct.pack("<IIII", 0, 300, 600, 450) + strip
    records = [(0x100, struct.pack(">HH", height, width)), (0x110, struct.pack(">HH", 2, 4)),
               (0x111, struct.pack(">HH", height - 2, width - 4))]
    directory = struct.pack(">I", len(records))
    directory += b"".join(struct.pack(">HH", tag, len(data)) + data for tag, data in records)
    header = bytearray(b"FUJIFILMCCD-RAW 0201FF000000TEST".ljust(160, b"\0"))
    header[84:108] = struct.pack(">IIIIII", 160, 0, 160, len(directory), 160 + len(directory), len(block))
    path.write_bytes(header + directory + block)


def request(port, method, params=None):
    with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
        sock.settimeout(15)
        sock.sendall((json.dumps({"id": 1, "method": method, "params": params or {}}) + "\n").encode())
        reply = json.loads(sock.makefile().readline())
        assert reply["ok"], reply
        return reply.get("result")


def wait_for(port, predicate, description, timeout=30):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = predicate()
            if last:
                return last
        except (OSError, ValueError) as error:
            last = str(error)
        time.sleep(0.1)
    raise AssertionError(f"{description}: {last}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/lightcraft")
    parser.add_argument("--app", type=Path, help="test this existing .app instead of assembling a temporary bundle")
    parser.add_argument("--artifacts", type=Path, default=ROOT / "target/macos-open-test")
    parser.add_argument("--port", type=int, default=18762)
    args = parser.parse_args()
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", args.port))
    args.artifacts.mkdir(parents=True, exist_ok=True)
    for name in ("stdout.log", "stderr.log"):
        (args.artifacts / name).write_text("")
    with tempfile.TemporaryDirectory(prefix="lightcraft-open-") as scratch:
        scratch = Path(scratch)
        bundle = args.app.resolve() if args.app else scratch / "LightCraft Open Test.app"
        if not args.app:
            contents = bundle / "Contents"
            (contents / "MacOS").mkdir(parents=True)
            shutil.copy2(args.binary, contents / "MacOS/LightCraft")
            with (ROOT / "packaging/macos/Info.plist.in").open("rb") as source:
                info = plistlib.load(source)
            info.update(CFBundleIdentifier=f"ai.storyteller.lightcraft.open-test.{os.getpid()}",
                        CFBundleVersion="1", CFBundleShortVersionString="1.0")
            with (contents / "Info.plist").open("wb") as dest:
                plistlib.dump(info, dest)
        cold, warm = scratch / "cold photo.RAF", scratch / "warm 富士 %20 photo.RAF"
        raf(cold, 1000)
        raf(warm, 2000)
        env = {"LIGHTCRAFT_CONFIG_DIR": str(scratch / "config"), "LIGHTCRAFT_NO_PREFS": "1",
               "LIGHTCRAFT_NO_NATIVE_MENU": "1", "LIGHTCRAFT_GPU": "0"}
        command = ["/usr/bin/open", "-n", "-a", str(bundle), str(cold)]
        for name, value in env.items():
            command += ["--env", f"{name}={value}"]
        command += ["--stdout", str(args.artifacts.resolve() / "stdout.log"), "--stderr",
                    str(args.artifacts.resolve() / "stderr.log"), "--args", "--memory", "--control", str(args.port)]
        try:
            subprocess.run(command, check=True)
            wait_for(args.port, lambda: request(args.port, "ui.inspect"), "native window did not start")
            def imported(count):
                return request(args.port, "engine.execute", {"command": "library.info"})["photos"] == count
            wait_for(args.port, lambda: imported(1), "cold Open Documents RAF was not imported")
            print("PASS: cold launch imports a RAF delivered by macOS", flush=True)
            subprocess.run(["/usr/bin/open", "-a", str(bundle), str(warm)], check=True)
            wait_for(args.port, lambda: imported(2), "warm Open Documents RAF was not imported")
            print("PASS: running app imports a second RAF (space and literal %20 path)", flush=True)
            screenshot = args.artifacts.resolve() / "open-documents.png"
            request(args.port, "ui.screenshot", {"path": str(screenshot)})
            assert "rendering headlessly" not in (args.artifacts / "stderr.log").read_text(), "native screenshot fell back to headless"
            state = request(args.port, "ui.inspect")
            (args.artifacts / "inspect.json").write_text(json.dumps(state, indent=2))
            print(f"PASS: screenshot and perf captured in {args.artifacts}", flush=True)
        finally:
            try:
                (args.artifacts / "last-inspect.json").write_text(json.dumps(request(args.port, "ui.inspect"), indent=2))
            except (OSError, ValueError, AssertionError):
                pass
            try:
                request(args.port, "engine.execute", {"command": "app.quit"})
                request(args.port, "ui.clickWidget", {"id": "button:quitAnyway"})
            except (OSError, ValueError, AssertionError):
                pass


if __name__ == "__main__":
    main()
