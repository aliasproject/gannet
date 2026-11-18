#!/usr/bin/env python3
"""Stands in for xdg-desktop-portal's Wallpaper interface, on the e2e suite's private bus.
Each request's file, as the descriptor Gannet passes names it, goes on a line of LOG. It
answers 0 (set), or 2 (not set) for a file whose name has "fail" in it, as the real portal
does after its backend.

  python3 tests/wallpaper_portal.py LOG
"""
import os
import sys

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

PATH = "/org/freedesktop/portal/desktop"
XML = """
<node>
  <interface name="org.freedesktop.portal.Wallpaper">
    <method name="SetWallpaperFile">
      <arg type="s" direction="in"/><arg type="h" direction="in"/><arg type="a{sv}" direction="in"/>
      <arg type="o" direction="out"/>
    </method>
    <property name="version" type="u" access="read"/>
  </interface>
</node>
"""
log = sys.argv[1]


def on_call(conn, sender, _path, _iface, _method, params, invocation):
    fds = invocation.get_message().get_unix_fd_list().steal_fds()
    named = os.readlink(f"/proc/self/fd/{fds[params.unpack()[1]]}")
    for fd in fds:
        os.close(fd)
    token = params.unpack()[2].get("handle_token", "t")
    handle = f"{PATH}/request/{sender[1:].replace('.', '_')}/{token}"
    with open(log, "a") as f:
        print(named, file=f)
    invocation.return_value(GLib.Variant("(o)", (handle,)))
    code = 2 if "fail" in os.path.basename(named) else 0
    GLib.timeout_add(50, lambda: conn.emit_signal(sender, handle, "org.freedesktop.portal.Request", "Response", GLib.Variant("(ua{sv})", (code, {}))) and False)


def on_property(*_):
    return GLib.Variant("u", 1)


bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
bus.register_object(PATH, Gio.DBusNodeInfo.new_for_xml(XML).interfaces[0], on_call, on_property, None)
Gio.bus_own_name_on_connection(bus, "org.freedesktop.portal.Desktop", Gio.BusNameOwnerFlags.NONE, None, None)
GLib.MainLoop().run()
