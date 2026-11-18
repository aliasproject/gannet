#!/usr/bin/env python3
"""A scripted screen reader for the e2e suite: libatspi, as Orca uses it, on the private
accessibility bus in AT_SPI_BUS_ADDRESS. It reads one command a line from stdin and answers
each with a line of JSON, and keeps the events it hears for the "events" command.

  apps                 the names of the apps the registry lists
  tree                 Gannet's window: the frame, the places, the rows and what has focus
  keep N               holds on to row N of the file list, as a screen reader would
  kept                 what the kept row says now: its name, or the error asking gave
  select_row N         selects row N through the list's Selection interface
  select_place N       selects place N in the sidebar, which opens it
  press_back           does the Back button's action
  events               the events heard since last asked
"""
import json
import os
import sys

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, GLib  # noqa: E402

assert os.environ.get("AT_SPI_BUS_ADDRESS", "").startswith("unix:path=/tmp/gne2e/"), "only ever the suite's private bus"
Atspi.init()
events = []
kept = {}


def on_event(e):
    try:
        src = e.source.get_name()
    except GLib.Error as ex:
        src = f"? {ex.message}"
    events.append({"type": e.type, "detail1": e.detail1, "source": src})


listener = Atspi.EventListener.new(on_event)
for t in ["object:state-changed:focused", "object:state-changed:selected", "object:selection-changed", "object:children-changed", "object:property-change:accessible-name", "object:active-descendant-changed", "window:activate"]:
    listener.register(t)


def apps():
    # The registry can briefly list an app whose connection has just closed: it shows as nothing.
    d = Atspi.get_desktop(0)
    return [a.get_name() for a in (d.get_child_at_index(i) for i in range(d.get_child_count())) if a is not None]


def gannet():
    d = Atspi.get_desktop(0)
    for i in range(d.get_child_count()):
        a = d.get_child_at_index(i)
        if a is not None and a.get_name() == "Gannet":
            return a
    raise LookupError("Gannet isn't registered")


def parts():
    frame = gannet().get_child_at_index(0)
    kids = [frame.get_child_at_index(i) for i in range(frame.get_child_count())]
    by_name = {k.get_name(): k for k in kids}
    return frame, by_name["Back"], by_name["Places"], by_name["Files"]


def children(o):
    return [o.get_child_at_index(i) for i in range(o.get_child_count())]


def focused(o):
    return o.get_state_set().contains(Atspi.StateType.FOCUSED)


def tree():
    frame, back, places, files = parts()
    rows = children(files)
    focus = [r.get_name() for r in rows if focused(r)] or (["Files"] if focused(files) else [])
    return {
        "frame": frame.get_name(),
        "back_enabled": back.get_state_set().contains(Atspi.StateType.SENSITIVE),
        "places": [p.get_name() for p in children(places)],
        "rows": [r.get_name() for r in rows],
        "selected": [r.get_name() for r in rows if r.get_state_set().contains(Atspi.StateType.SELECTED)],
        "focus": focus,
        "manages_descendants": files.get_state_set().contains(Atspi.StateType.MANAGES_DESCENDANTS),
    }


def run(cmd, arg):
    if cmd == "apps":
        return apps()
    if cmd == "tree":
        return tree()
    if cmd == "keep":
        kept["row"] = parts()[3].get_child_at_index(int(arg))
        return kept["row"].get_name()
    if cmd == "kept":
        try:
            # A role is never cached, so this asks the app whether the object is still there.
            kept["row"].clear_cache()
            return {"role": kept["row"].get_role_name(), "name": kept["row"].get_name()}
        except GLib.Error as e:
            return {"error": e.message}
    if cmd == "select_row":
        return parts()[3].select_child(int(arg))
    if cmd == "select_place":
        return parts()[2].select_child(int(arg))
    if cmd == "press_back":
        return parts()[1].do_action(0)
    if cmd == "events":
        out = list(events)
        events.clear()
        return out
    raise ValueError(cmd)


def on_line(channel, _cond):
    line = channel.readline()
    if not line:
        loop.quit()
        return False
    cmd, _, arg = line.strip().partition(" ")
    try:
        out = {"ok": run(cmd, arg)}
    except Exception as e:  # noqa: BLE001 - every failure goes back to the suite
        out = {"error": f"{type(e).__name__}: {e}"}
    print(json.dumps(out), flush=True)
    return True


loop = GLib.MainLoop()
GLib.io_add_watch(GLib.IOChannel.unix_new(sys.stdin.fileno()), GLib.PRIORITY_DEFAULT, GLib.IOCondition.IN | GLib.IOCondition.HUP, on_line)
loop.run()
