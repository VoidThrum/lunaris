#!/bin/sh
# Renders the "Add computer" screens off screen (no window is opened) and
# checks what the pages do: qmltestrunner with a stub bridge. The pictures
# go to out/ beside this file, or to the directory given.
#   client-qml/tests/offscreen/run.sh [directory for the pictures]
set -e
here="$(cd "$(dirname "$0")" && pwd)"
runner="${QMLTESTRUNNER:-$(command -v qmltestrunner || echo /usr/lib/qt6/bin/qmltestrunner)}"
mkdir -p "$here/out"
# Off screen in every way: no window system, and no session bus (so that
# main.qml's tray icon has nowhere to appear).
export QT_QPA_PLATFORM=offscreen QT_QUICK_BACKEND=software QT_QUICK_CONTROLS_STYLE=Basic
export DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent
"$runner" -input "$here/tst_computers.qml"
"$runner" -import "$here/stubs" -input "$here/tst_source_session.qml"
"$runner" -import "$here/stubs" -input "$here/tst_pointer_in_picture.qml"
if [ -n "$1" ]; then
    mkdir -p "$1"
    mv "$here"/out/*.png "$1"/
fi
