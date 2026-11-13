"""A stand-in for a web browser in the end-to-end checks: a window that, dragged from, offers what
a browser offers for a picture on a page.

    python3 dragsource.py ADDRESS [PICTURE]

ADDRESS goes in a text/uri-list, as a browser gives a picture's web address. With PICTURE, a
PNG file, its bytes are offered as image/png too, as Firefox and its relatives do. It prints
"ready" once its window is up.
"""
import sys

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402


def main():
    address = sys.argv[1]
    picture = open(sys.argv[2], "rb").read() if len(sys.argv) > 2 else None
    app = Gtk.Application(application_id="com.aliasproject.GannetDragSource")

    def activate(app):
        window = Gtk.ApplicationWindow(application=app, title="drag source")
        area = Gtk.Label(label="drag me")
        area.set_hexpand(True)
        area.set_vexpand(True)
        source = Gtk.DragSource()
        source.set_actions(Gdk.DragAction.COPY | Gdk.DragAction.MOVE)

        def prepare(_source, _x, _y):
            offers = [Gdk.ContentProvider.new_for_bytes("text/uri-list", GLib.Bytes.new((address + "\r\n").encode()))]
            if picture is not None:
                offers.append(Gdk.ContentProvider.new_for_bytes("image/png", GLib.Bytes.new(picture)))
            return Gdk.ContentProvider.new_union(offers)

        source.connect("prepare", prepare)
        area.add_controller(source)
        window.set_child(area)
        window.connect("map", lambda *_: (print("ready", flush=True), None)[1])
        window.present()

    app.connect("activate", activate)
    app.run([])


if __name__ == "__main__":
    main()
