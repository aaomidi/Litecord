#!/usr/bin/env python3
"""Gives a test build of Litecord a window to share without the desktop portal's dialog.

Opens a Mutter ScreenCast session (org.gnome.Mutter.ScreenCast: CreateSession, RecordWindow
with the cursor embedded, Start), which is what xdg-desktop-portal-gnome does for a shared
window, and writes "<PipeWire node id>,window" to NODE_FILE for a build with
`--features automation` started with LITECORD_TEST_SHARE_NODE=@NODE_FILE (see
src/utils/test_hooks.rs). The session lives as long as this process: end it (SIGTERM, or
close its stdin with --stdin) when the share is over. It also ends, with exit status 3, when
Mutter closes the session (the window was closed).

RecordWindow is given no window id, so Mutter records the focused window. To make sure that
is the scripted test window (testwin.py) and nothing else:
  --status FILE   testwin.py's status file: its last "active N" line must say 1 before and
                  after the call
  --verify        one picture is read from the node (system memory, no VA-API) and must show
                  the test window's borders (red on top, green left) in its top left corner
Without a match nothing is written and the session is stopped (exit status 2).

  --keep-picture PNG  keep the picture --verify looked at

Usage: test_share_node.py NODE_FILE [--status FILE] [--verify] [--keep-picture PNG] [--stdin]
"""
import os
import signal
import subprocess
import sys
import tempfile
import time

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib

BUS = "org.gnome.Mutter.ScreenCast"
args = sys.argv[1:]
if not args or args[0].startswith("-"):
    sys.exit(__doc__)
node_file = args[0]
status_file = args[args.index("--status") + 1] if "--status" in args else None
verify = "--verify" in args
t0 = time.monotonic()


def say(text):
    print("%.3f %s" % (time.monotonic() - t0, text), flush=True)


def window_active():
    if status_file is None:
        return True
    last = None
    for line in open(status_file):
        parts = line.split()
        if len(parts) >= 3 and parts[1] == "active":
            last = parts[2]
        if len(parts) >= 2 and parts[1] in ("closed", "exit"):
            last = "0"
    return last == "1"


def picture_is_test_window(node):
    """One picture of the node: the test window sits in the buffer's top left corner."""
    from PIL import Image
    with tempfile.TemporaryDirectory() as d:
        png = os.path.join(d, "frame.png")
        cmd = ["gst-launch-1.0", "-q", "pipewiresrc", "path=%d" % node, "num-buffers=1",
               "stream-properties=props,node.dont-fallback=(string)true", "!",
               "videoconvert", "!", "pngenc", "!", "filesink", "location=" + png]
        try:
            subprocess.run(cmd, timeout=8, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except subprocess.TimeoutExpired:
            say("verify: no picture within 8 s")
            return False
        if not os.path.exists(png) or os.path.getsize(png) == 0:
            say("verify: no picture")
            return False
        image = Image.open(png).convert("RGB")
        image.load()
    keep = args[args.index("--keep-picture") + 1] if "--keep-picture" in args else None
    if keep:
        image.save(keep)
    top, left = image.getpixel((300, 5)), image.getpixel((5, 300))
    say("verify: buffer %dx%d, top border pixel %s, left border pixel %s" % (image.width, image.height, top, left))
    return top[0] > 200 and top[1] < 60 and top[2] < 60 and left[1] > 200 and left[0] < 60 and left[2] < 60


conn = Gio.bus_get_sync(Gio.BusType.SESSION)


def call(path, iface, method, params, reply=None):
    return conn.call_sync(BUS, path, iface, method, params, reply, Gio.DBusCallFlags.NONE, 5000, None)


if not window_active():
    say("the test window is not the active window: nothing recorded")
    sys.exit(2)
session = call("/org/gnome/Mutter/ScreenCast", BUS, "CreateSession", GLib.Variant("(a{sv})", ({},)), GLib.VariantType("(o)")).unpack()[0]
SESSION = BUS + ".Session"
loop = GLib.MainLoop()
state = {"node": None, "code": 0, "stopped": False}


def stop_session():
    if not state["stopped"]:
        state["stopped"] = True
        try:
            call(session, SESSION, "Stop", None)
            say("session stopped")
        except GLib.Error as e:
            say("Stop did not answer (%s); the session ends with this process" % e.message)


def on_closed(*_):
    say("Mutter closed the session (the window is gone)")
    state["stopped"] = True
    state["code"] = 3
    loop.quit()


conn.signal_subscribe(BUS, SESSION, "Closed", session, None, Gio.DBusSignalFlags.NONE, on_closed)
stream = call(session, SESSION, "RecordWindow", GLib.Variant("(a{sv})", ({"cursor-mode": GLib.Variant("u", 1)},)), GLib.VariantType("(o)")).unpack()[0]


def on_added(_c, _s, _p, _i, _n, params):
    state["node"] = params.unpack()[0]
    loop.quit()


conn.signal_subscribe(BUS, BUS + ".Stream", "PipeWireStreamAdded", stream, None, Gio.DBusSignalFlags.NONE, on_added)
call(session, SESSION, "Start", None)
timeout = GLib.timeout_add(3000, lambda: (loop.quit(), False)[1])
loop.run()
if state["node"] is not None:
    GLib.source_remove(timeout)
node = state["node"]
if node is None:
    say("Mutter did not announce a PipeWire node")
    stop_session()
    sys.exit(1)
say("RecordWindow (the focused window), cursor embedded: PipeWire node %d" % node)
if not window_active():
    say("the test window stopped being the active window: node not given")
    stop_session()
    sys.exit(2)
if verify and not picture_is_test_window(node):
    say("the node does not show the test window: node not given")
    stop_session()
    sys.exit(2)

tmp = node_file + ".tmp"
with open(tmp, "w") as f:
    f.write("%d,window\n" % node)
os.rename(tmp, node_file)
say("node file written: %s" % node_file)


def quit_loop(*_):
    loop.quit()
    return False


signal.signal(signal.SIGTERM, lambda *_: GLib.idle_add(quit_loop))
signal.signal(signal.SIGINT, lambda *_: GLib.idle_add(quit_loop))
GLib.timeout_add(200, lambda: True)  # lets Python run its signal handlers
if "--stdin" in args:
    GLib.io_add_watch(0, GLib.IO_HUP | GLib.IO_IN, lambda fd, cond: (loop.quit(), False)[1] if not os.read(fd, 4096) else True)
loop.run()
stop_session()
try:
    os.remove(node_file)
except FileNotFoundError:
    pass
sys.exit(state["code"])
