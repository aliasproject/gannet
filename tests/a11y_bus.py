#!/usr/bin/env python3
"""Stands in for at-spi-bus-launcher on the e2e suite's private session bus: says where the
accessibility bus is (another private bus the suite started) and whether a screen reader is
on. IsEnabled starts off; the suite turns it on and off as Orca would, by setting it.

  python3 tests/a11y_bus.py A11Y_BUS_ADDRESS
"""
import sys

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

PATH = "/org/a11y/bus"
XML = """
<node>
  <interface name="org.a11y.Bus">
    <method name="GetAddress"><arg type="s" direction="out"/></method>
  </interface>
  <interface name="org.a11y.Status">
    <property name="IsEnabled" type="b" access="readwrite"/>
    <property name="ScreenReaderEnabled" type="b" access="readwrite"/>
  </interface>
</node>
"""
address = sys.argv[1]
state = {"IsEnabled": False, "ScreenReaderEnabled": False}
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)


def on_call(_conn, _sender, _path, _iface, _method, _params, invocation):
    invocation.return_value(GLib.Variant("(s)", (address,)))


def on_get(_conn, _sender, _path, _iface, name):
    return GLib.Variant("b", state[name])


def on_set(conn, _sender, path, iface, name, value):
    state[name] = value.unpack()
    conn.emit_signal(None, path, "org.freedesktop.DBus.Properties", "PropertiesChanged", GLib.Variant("(sa{sv}as)", (iface, {name: value}, [])))
    return True


node = Gio.DBusNodeInfo.new_for_xml(XML)
bus.register_object(PATH, node.interfaces[0], on_call, None, None)
bus.register_object(PATH, node.interfaces[1], None, on_get, on_set)
Gio.bus_own_name_on_connection(bus, "org.a11y.Bus", Gio.BusNameOwnerFlags.NONE, None, None)
GLib.MainLoop().run()
