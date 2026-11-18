#!/usr/bin/env python3
"""A stand-in media player on the session bus for the stranger workflow's
nowplaying screens: an MPRIS player that plays one track with a cover, with
Shuffle on and LoopStatus Playlist, and keeps what a client sets.

    mpris_player.py <cover.png>

Runs until killed. Needs PyGObject (Gio).
"""

import sys

from gi.repository import Gio, GLib

BUS_NAME = "org.mpris.MediaPlayer2.strangerplayer"
PATH = "/org/mpris/MediaPlayer2"
INTERFACES = """
<node>
  <interface name="org.mpris.MediaPlayer2">
    <method name="Raise"/>
    <method name="Quit"/>
    <property name="Identity" type="s" access="read"/>
    <property name="CanQuit" type="b" access="read"/>
    <property name="CanRaise" type="b" access="read"/>
    <property name="HasTrackList" type="b" access="read"/>
    <property name="SupportedUriSchemes" type="as" access="read"/>
    <property name="SupportedMimeTypes" type="as" access="read"/>
  </interface>
  <interface name="org.mpris.MediaPlayer2.Player">
    <method name="Next"/>
    <method name="Previous"/>
    <method name="Pause"/>
    <method name="PlayPause"/>
    <method name="Stop"/>
    <method name="Play"/>
    <property name="PlaybackStatus" type="s" access="read"/>
    <property name="LoopStatus" type="s" access="readwrite"/>
    <property name="Shuffle" type="b" access="readwrite"/>
    <property name="Rate" type="d" access="read"/>
    <property name="Metadata" type="a{sv}" access="read"/>
    <property name="Volume" type="d" access="read"/>
    <property name="Position" type="x" access="read"/>
    <property name="CanGoNext" type="b" access="read"/>
    <property name="CanGoPrevious" type="b" access="read"/>
    <property name="CanPlay" type="b" access="read"/>
    <property name="CanPause" type="b" access="read"/>
    <property name="CanSeek" type="b" access="read"/>
    <property name="CanControl" type="b" access="read"/>
  </interface>
</node>
"""


def main(argv):
    if len(argv) != 1:
        print(__doc__, file=sys.stderr)
        return 2
    cover = Gio.File.new_for_path(argv[0]).get_uri()
    values = {
        "Identity": GLib.Variant("s", "Stranger Player"),
        "CanQuit": GLib.Variant("b", False),
        "CanRaise": GLib.Variant("b", False),
        "HasTrackList": GLib.Variant("b", False),
        "SupportedUriSchemes": GLib.Variant("as", []),
        "SupportedMimeTypes": GLib.Variant("as", []),
        "PlaybackStatus": GLib.Variant("s", "Playing"),
        "LoopStatus": GLib.Variant("s", "Playlist"),
        "Shuffle": GLib.Variant("b", True),
        "Rate": GLib.Variant("d", 1.0),
        "Metadata": GLib.Variant("a{sv}", {
            "mpris:trackid": GLib.Variant("o", "/org/mpris/MediaPlayer2/Track/1"),
            "mpris:length": GLib.Variant("x", 240_000_000),
            "mpris:artUrl": GLib.Variant("s", cover),
            "xesam:title": GLib.Variant("s", "Stranger Song"),
            "xesam:artist": GLib.Variant("as", ["Stranger Band"]),
            "xesam:album": GLib.Variant("s", "Stranger Album"),
        }),
        "Volume": GLib.Variant("d", 1.0),
        "Position": GLib.Variant("x", 0),
        "CanGoNext": GLib.Variant("b", True),
        "CanGoPrevious": GLib.Variant("b", True),
        "CanPlay": GLib.Variant("b", True),
        "CanPause": GLib.Variant("b", True),
        "CanSeek": GLib.Variant("b", False),
        "CanControl": GLib.Variant("b", True),
    }
    connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)

    def call(_connection, _sender, _path, _interface, _method, _parameters, invocation):
        invocation.return_value(None)

    def get(_connection, _sender, _path, _interface, name):
        return values[name]

    def set_(_connection, _sender, path, interface, name, value):
        values[name] = value
        connection.emit_signal(None, path, "org.freedesktop.DBus.Properties", "PropertiesChanged",
                               GLib.Variant("(sa{sv}as)", (interface, {name: value}, [])))
        print(f"set {name} = {value}", flush=True)
        return True

    # PyGObject binds g_dbus_connection_register_object_with_closures as register_object.
    for interface in Gio.DBusNodeInfo.new_for_xml(INTERFACES).interfaces:
        connection.register_object(PATH, interface, call, get, set_)
    Gio.bus_own_name_on_connection(connection, BUS_NAME, Gio.BusNameOwnerFlags.NONE, None, None)
    print(f"playing on {BUS_NAME}, cover {cover}", flush=True)
    GLib.MainLoop().run()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
