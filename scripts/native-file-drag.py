"""Linux/X11: fuente GTK externa y arrastre real con XTest hacia Pulpo.

Usar bajo Xvfb en CI. No envía eventos al DOM ni usa CDP para el arrastre.
"""
import ctypes as C
import os
import sys
import threading
import time

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gio, GLib, Gtk

x = C.CDLL("libX11.so.6")
xt = C.CDLL("libXtst.so.6")
Display = C.c_void_p
Window = C.c_ulong
x.XOpenDisplay.argtypes = [C.c_char_p]
x.XOpenDisplay.restype = Display
x.XDefaultRootWindow.argtypes = [Display]
x.XDefaultRootWindow.restype = Window
x.XQueryTree.argtypes = [Display, Window, C.POINTER(Window), C.POINTER(Window), C.POINTER(C.POINTER(Window)), C.POINTER(C.c_uint)]
x.XFetchName.argtypes = [Display, Window, C.POINTER(C.c_void_p)]
x.XFree.argtypes = [C.c_void_p]
x.XGetGeometry.argtypes = [Display, Window, C.POINTER(Window), C.POINTER(C.c_int), C.POINTER(C.c_int), C.POINTER(C.c_uint), C.POINTER(C.c_uint), C.POINTER(C.c_uint), C.POINTER(C.c_uint)]
x.XTranslateCoordinates.argtypes = [Display, Window, Window, C.c_int, C.c_int, C.POINTER(C.c_int), C.POINTER(C.c_int), C.POINTER(Window)]
x.XFlush.argtypes = [Display]
x.XCloseDisplay.argtypes = [Display]
xt.XTestFakeMotionEvent.argtypes = [Display, C.c_int, C.c_int, C.c_int, C.c_ulong]
xt.XTestFakeButtonEvent.argtypes = [Display, C.c_uint, C.c_int, C.c_ulong]

result = 1
source_title = f"Pulpo file source {os.getpid()}"


def find(display, parent, title):
    name = C.c_void_p()
    if x.XFetchName(display, parent, C.byref(name)) and name.value:
        text = C.string_at(name).decode("utf-8", "replace")
        x.XFree(name)
        if text == title:
            return parent
    root, owner = Window(), Window()
    children = C.POINTER(Window)()
    count = C.c_uint()
    if x.XQueryTree(display, parent, C.byref(root), C.byref(owner), C.byref(children), C.byref(count)):
        ids = [children[i] for i in range(count.value)]
        if children:
            x.XFree(children)
        for child in ids:
            found = find(display, child, title)
            if found:
                return found
    return None


def bounds(display, window):
    root, child = Window(), Window()
    left, top, rx, ry = C.c_int(), C.c_int(), C.c_int(), C.c_int()
    width, height, border, depth = C.c_uint(), C.c_uint(), C.c_uint(), C.c_uint()
    assert x.XGetGeometry(display, window, C.byref(root), C.byref(left), C.byref(top), C.byref(width), C.byref(height), C.byref(border), C.byref(depth))
    assert x.XTranslateCoordinates(display, window, root, 0, 0, C.byref(rx), C.byref(ry), C.byref(child))
    return rx.value, ry.value, width.value, height.value


def drag(app):
    global result
    display = x.XOpenDisplay(None)
    try:
        assert display, "X11 display unavailable"
        root = x.XDefaultRootWindow(display)
        source = target = None
        for _ in range(50):
            source = find(display, root, source_title)
            target = find(display, root, "Pulpo (test)")
            if source and target:
                break
            time.sleep(0.1)
        assert source and target, "source or Pulpo window not found"
        time.sleep(1)  # Espera a que el gestor termine de colocar las ventanas.
        sx, sy, sw, sh = bounds(display, source)
        tx, ty, tw, th = bounds(display, target)
        start = (sx + sw // 2, sy + sh // 2)
        end = (tx + tw * 3 // 4, ty + th // 2)
        print(f"Native drag: {start} -> {end}; Pulpo bounds {(tx, ty, tw, th)}", flush=True)
        xt.XTestFakeMotionEvent(display, -1, *start, 0)
        x.XFlush(display)
        time.sleep(0.2)
        xt.XTestFakeButtonEvent(display, 1, 1, 0)
        x.XFlush(display)
        time.sleep(0.2)
        for step in range(1, 41):
            px = int(start[0] + (end[0] - start[0]) * step / 40)
            py = int(start[1] + (end[1] - start[1]) * step / 40)
            xt.XTestFakeMotionEvent(display, -1, px, py, 0)
            x.XFlush(display)
            time.sleep(0.04)
        time.sleep(0.3)
        xt.XTestFakeButtonEvent(display, 1, 0, 0)
        x.XFlush(display)
        time.sleep(0.5)
        result = 0
    except Exception as error:
        print(f"Native drag failed: {error}", file=sys.stderr)
    finally:
        if display:
            xt.XTestFakeButtonEvent(display, 1, 0, 0)
            x.XFlush(display)
            x.XCloseDisplay(display)
        GLib.idle_add(app.quit)


def activate(app):
    window = Gtk.ApplicationWindow(application=app, title=source_title)
    window.set_default_size(240, 100)
    window.set_child(Gtk.Label(label="Drag test files"))
    controller = Gtk.DragSource(actions=Gdk.DragAction.COPY)
    controller.connect("drag-begin", lambda *_: print("GTK drag started", flush=True))
    controller.connect("drag-cancel", lambda _source, _drag, reason: print(f"GTK drag cancelled: {reason}", flush=True))
    files = Gdk.FileList.new_from_list([Gio.File.new_for_path(path) for path in sys.argv[1:]])
    controller.set_content(Gdk.ContentProvider.new_for_value(files))
    window.add_controller(controller)
    window.present()
    threading.Thread(target=drag, args=(app,), daemon=True).start()


app = Gtk.Application(application_id="com.pulpo.filedragtest")
app.connect("activate", activate)
app.run([])
sys.exit(result)
