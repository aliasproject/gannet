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
  fields               the window's text fields: name, description, text, caret, selection,
                       states, and each character's box
  focus_field NAME     moves the keyboard to a field through its Component interface
  set_text NAME TEXT   replaces a field's text through its EditableText interface
  insert NAME AT TEXT  inserts text at a character's place
  delete NAME A B      deletes the characters from A to B
  caret NAME AT        moves the caret through the Text interface
  select NAME A B      selects from A to B
  word NAME AT         the word at a character's place, as Orca reads one: [text, start, end]
  at X Y               the name of the window's part at a point, as pointing at it finds
  events               the events heard since last asked
A field is found by its name, or the start of its name.
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
    heard = {"type": e.type, "detail1": e.detail1, "detail2": e.detail2, "source": src}
    if e.type.startswith("object:text-changed"):
        heard["text"] = e.any_data
    events.append(heard)


listener = Atspi.EventListener.new(on_event)
for t in ["object:state-changed:focused", "object:state-changed:selected", "object:state-changed:showing", "object:selection-changed", "object:children-changed", "object:property-change:accessible-name", "object:active-descendant-changed", "window:activate", "object:text-changed", "object:text-caret-moved", "object:text-selection-changed", "object:property-change:accessible-description"]:
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


def entries():
    frame = gannet().get_child_at_index(0)
    return [k for k in children(frame) if k.get_role() == Atspi.Role.ENTRY]


def field(name):
    found = [k for k in entries() if k.get_name() == name] or [k for k in entries() if k.get_name().startswith(name)]
    if not found:
        raise LookupError(f"no field {name!r} among {[k.get_name() for k in entries()]}")
    return found[0]


def describe(f):
    n = Atspi.Text.get_character_count(f)
    states = f.get_state_set()
    boxes = [Atspi.Text.get_character_extents(f, i, Atspi.CoordType.WINDOW) for i in range(n)]
    selections = [Atspi.Text.get_selection(f, i) for i in range(Atspi.Text.get_n_selections(f))]
    ext = f.get_extents(Atspi.CoordType.WINDOW)
    return {
        "name": f.get_name(),
        "description": f.get_description(),
        "role": f.get_role_name(),
        "text": Atspi.Text.get_text(f, 0, -1),
        "count": n,
        "caret": Atspi.Text.get_caret_offset(f),
        "selections": [[r.start_offset, r.end_offset] for r in selections],
        "editable": states.contains(Atspi.StateType.EDITABLE),
        "single_line": states.contains(Atspi.StateType.SINGLE_LINE),
        "focused": states.contains(Atspi.StateType.FOCUSED),
        "rect": [ext.x, ext.y, ext.width, ext.height],
        "boxes": [[b.x, b.y, b.width, b.height] for b in boxes],
        "interfaces": f.get_interfaces(),
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
    if cmd == "at":
        x, y = arg.split(" ")
        o = gannet().get_child_at_index(0).get_accessible_at_point(int(x), int(y), Atspi.CoordType.WINDOW)
        return o.get_name() if o else None
    if cmd == "fields":
        return [describe(f) for f in entries()]
    if cmd == "focus_field":
        return field(arg).grab_focus()
    if cmd == "set_text":
        name, _, text = arg.partition(" ")
        return Atspi.EditableText.set_text_contents(field(name), text)
    if cmd == "insert":
        name, at, text = arg.split(" ", 2)
        return Atspi.EditableText.insert_text(field(name), int(at), text, len(text.encode()))
    if cmd == "delete":
        name, a, b = arg.split(" ")
        return Atspi.EditableText.delete_text(field(name), int(a), int(b))
    if cmd == "caret":
        name, at = arg.split(" ")
        return Atspi.Text.set_caret_offset(field(name), int(at))
    if cmd == "select":
        name, a, b = arg.split(" ")
        return Atspi.Text.set_selection(field(name), 0, int(a), int(b))
    if cmd == "word":
        name, at = arg.split(" ")
        r = Atspi.Text.get_string_at_offset(field(name), int(at), Atspi.TextGranularity.WORD)
        return [r.content, r.start_offset, r.end_offset]
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
