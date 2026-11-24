#!/usr/bin/env python3
"""A scripted screen reader for the e2e suite: libatspi, as Orca uses it, on the private
accessibility bus in AT_SPI_BUS_ADDRESS. It reads one command a line from stdin and answers
each with a line of JSON, and keeps the events it hears for the "events" command.

  apps                 the names of the apps the registry lists
  tree                 Gannet's window: the frame, the places, the rows and what has focus
  keep N               holds on to row N of the file list, as a screen reader would
  kept                 what the kept row says now: its name, or the error asking gave
  select_row N         adds row N to the selection through the list's Selection interface
  deselect_row N       takes row N out of the selection, the same way
  focus_row N          moves the keyboard to row N through its Component interface
  focus_place N        moves to place N in the sidebar through its Component interface
  select_place N       selects place N in the sidebar, which opens it
  press_back           does the Back button's action
  events               the events heard since last asked, an announcement with what it said

For the other windows (the preview, Open and Save dialogs, Open With), a window is the one of
Gannet's whose name has the first of a JSON list's strings in it, and a part is found in it by
its name, the rest of the list being the names on the way down:

  dump ["WINDOW"]               the window's parts: role, name, description, states, children
  do ["WINDOW", "NAME"...]      does that part's action
  grab ["WINDOW", "NAME"...]    moves to that part through its Component interface
  select ["WINDOW", "NAME"..., N]  selects child N of that list through its Selection interface

A menu has no name, so its items are found under "".
"""
import json
import os
import sys
import warnings

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, GLib  # noqa: E402

assert os.environ.get("AT_SPI_BUS_ADDRESS", "").startswith("unix:path=/tmp/gne2e/"), "only ever the suite's private bus"
# get_action_name is deprecated for get_name, which an Accessible already has.
warnings.filterwarnings("ignore", category=DeprecationWarning)
Atspi.init()
events = []
kept = {}


def on_event(e):
    try:
        src = e.source.get_name()
    except GLib.Error as ex:
        src = f"? {ex.message}"
    heard = {"type": e.type, "detail1": e.detail1, "source": src}
    if e.type == "object:announcement":
        heard["said"] = e.any_data
    events.append(heard)


listener = Atspi.EventListener.new(on_event)
for t in ["object:state-changed:focused", "object:state-changed:selected", "object:state-changed:showing", "object:state-changed:checked", "object:state-changed:sensitive", "object:selection-changed", "object:children-changed", "object:property-change:accessible-name", "object:property-change:accessible-description", "object:active-descendant-changed", "window:activate", "object:announcement"]:
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
        "places_showing": [p.get_name() for p in children(places) if p.get_state_set().contains(Atspi.StateType.SHOWING)],
        "rows": [r.get_name() for r in rows],
        "selected": [r.get_name() for r in rows if r.get_state_set().contains(Atspi.StateType.SELECTED)],
        "selected_at": [i for i, r in enumerate(rows) if r.get_state_set().contains(Atspi.StateType.SELECTED)],
        "focus_at": [i for i, r in enumerate(rows) if focused(r)],
        "focus": focus,
        "descriptions": [r.get_description() for r in rows],
        "manages_descendants": files.get_state_set().contains(Atspi.StateType.MANAGES_DESCENDANTS),
    }


STATES = {"focused": Atspi.StateType.FOCUSED, "selected": Atspi.StateType.SELECTED, "enabled": Atspi.StateType.SENSITIVE, "showing": Atspi.StateType.SHOWING, "checked": Atspi.StateType.CHECKED, "editable": Atspi.StateType.EDITABLE, "active": Atspi.StateType.ACTIVE}
STATES |= {"default": Atspi.StateType.IS_DEFAULT, "modal": Atspi.StateType.MODAL}


def window(sub):
    d = Atspi.get_desktop(0)
    for i in range(d.get_child_count()):
        a = d.get_child_at_index(i)
        if a is not None and a.get_name() == "Gannet" and a.get_child_count() > 0:
            w = a.get_child_at_index(0)
            if sub in w.get_name():
                return w
    raise LookupError(f"no window of Gannet's named like {sub!r}")


def part(names):
    o = window(names[0])
    for name in names[1:]:
        found = [c for c in children(o) if c.get_name() == name]
        if not found:
            raise LookupError(f"no {name!r} in {o.get_name()!r}")
        o = found[0]
    return o


def dump(o, depth=0):
    states = o.get_state_set()
    out = {"role": o.get_role_name(), "name": o.get_name(), "description": o.get_description(), "states": [k for k, v in STATES.items() if states.contains(v)]}
    if o.get_role_name() not in ("list item", "label", "heading", "button", "check box", "entry"):
        out["children"] = [dump(c, depth + 1) for c in children(o)]
    try:
        out["action"] = o.get_action_name(0) if o.get_n_actions() > 0 else None
    except GLib.Error:
        out["action"] = None
    return out


def run(cmd, arg):
    if cmd == "dump":
        return dump(window(json.loads(arg)[0]))
    if cmd == "do":
        return part(json.loads(arg)).do_action(0)
    if cmd == "grab":
        return part(json.loads(arg)).grab_focus()
    if cmd == "select":
        a = json.loads(arg)
        return part(a[:-1]).select_child(a[-1])
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
    if cmd == "deselect_row":
        return parts()[3].deselect_child(int(arg))
    if cmd == "focus_place":
        return parts()[2].get_child_at_index(int(arg)).grab_focus()
    if cmd == "focus_row":
        return parts()[3].get_child_at_index(int(arg)).grab_focus()
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
