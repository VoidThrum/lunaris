// System shortcuts for the stream view: asks the window system to give the
// window the key combinations it otherwise keeps for itself (Alt+Tab, the
// Super key, ...), so that they reach the remote computer.
//
// - X11: XGrabKeyboard on the window (as before; SDL does the same for
//   moonlight-qt's SDL_SetWindowKeyboardGrab, app/streaming/input/input.cpp
//   302-323).
// - Wayland: the keyboard-shortcuts-inhibit protocol
//   (zwp_keyboard_shortcuts_inhibit_manager_v1.inhibit_shortcuts(surface,
//   seat); wayland-protocols unstable/keyboard-shortcuts-inhibit/
//   keyboard-shortcuts-inhibit-unstable-v1.xml), which is what SDL's Wayland
//   backend sends for the same call. Qt has no support for it (6.10:
//   QWaylandWindow does not override setKeyboardGrabEnabled, and no Qt
//   library names the protocol), so it is spoken here on Qt's connection.
//   The compositor decides: KWin honours it while the window has the
//   keyboard, GNOME asks the user once and keeps Super+Escape as its own way
//   back (the protocol's text: the compositor "may offer some mechanism to
//   restore" its shortcuts).
// - Other systems: QWindow::setKeyboardGrabEnabled, which on Windows does
//   not take the Windows key or Alt+Tab (that needs a low-level keyboard
//   hook, not written).
//
// Nothing here links against libX11 or libwayland-client: both are looked up
// in the process (Qt's platform plugin has loaded the one in use), so the
// build needs no header or library of either.

#include "video_helper.h"

#include <QGuiApplication>
#include <QWindow>
#include <cstdint>
#include <iostream>
#include <string>

#if defined(Q_OS_LINUX)
#include <dlfcn.h>

// QNativeInterface::QWaylandApplication: Qt 6.5 and later, built with Wayland.
#if QT_VERSION >= QT_VERSION_CHECK(6, 5, 0)
#if QT_CONFIG(wayland)
#define LUNARIS_WAYLAND_GRAB 1
#endif
#endif

namespace {

// ---- X11 ----

typedef int (*XGrabKeyboardFn)(void*, unsigned long, int, int, int, unsigned long);
typedef int (*XUngrabKeyboardFn)(void*, unsigned long);
typedef int (*XFlushFn)(void*);

void* x11_library() {
    void* lib = dlopen("libX11.so.6", RTLD_LAZY | RTLD_NOLOAD);
    if (!lib) lib = dlopen("libX11.so", RTLD_LAZY | RTLD_NOLOAD);
    if (!lib) lib = dlopen("libX11.so.6", RTLD_LAZY);
    if (!lib) lib = dlopen("libX11.so", RTLD_LAZY);
    return lib;
}

// Returns false when X11 is not the window system in use.
bool x11_keyboard_grab(QWindow* window, bool grab) {
    auto* x11App = qApp->nativeInterface<QNativeInterface::QX11Application>();
    if (!x11App) return false;
    void* display = x11App->display();
    unsigned long x_window = (unsigned long)window->winId();
    if (!display || !x_window) return true;
    void* libX11 = x11_library();
    if (!libX11) return true;
    auto grab_fn = (XGrabKeyboardFn)dlsym(libX11, "XGrabKeyboard");
    auto ungrab_fn = (XUngrabKeyboardFn)dlsym(libX11, "XUngrabKeyboard");
    auto flush_fn = (XFlushFn)dlsym(libX11, "XFlush");
    if (!grab_fn || !ungrab_fn) return true;
    if (grab) {
        // owner_events, GrabModeAsync for pointer and keyboard, CurrentTime.
        int result = grab_fn(display, x_window, 1, 1, 1, 0);
        std::cerr << "Lunaris Client: XGrabKeyboard returned " << result << std::endl;
    } else {
        ungrab_fn(display, 0);
    }
    if (flush_fn) flush_fn(display);
    return true;
}

// ---- Wayland ----

#if defined(LUNARIS_WAYLAND_GRAB)

// The layout of libwayland's wl_message and wl_interface (wayland-util.h),
// which is part of its ABI.
struct WlInterface;
struct WlMessage {
    const char* name;
    const char* signature;
    const WlInterface** types;
};
struct WlInterface {
    const char* name;
    int version;
    int method_count;
    const WlMessage* methods;
    int event_count;
    const WlMessage* events;
};

struct WlProxy;
struct WlEventQueue;

struct WaylandApi {
    WlProxy* (*proxy_marshal_flags)(WlProxy*, uint32_t opcode, const WlInterface*, uint32_t version, uint32_t flags, ...);
    void* (*proxy_create_wrapper)(void*);
    void (*proxy_wrapper_destroy)(void*);
    int (*proxy_add_listener)(WlProxy*, void (**)(void), void*);
    uint32_t (*proxy_get_version)(WlProxy*);
    void (*proxy_set_queue)(WlProxy*, WlEventQueue*);
    WlEventQueue* (*display_create_queue)(void*);
    int (*display_roundtrip_queue)(void*, WlEventQueue*);
    int (*display_dispatch_queue_pending)(void*, WlEventQueue*);
    int (*display_flush)(void*);
    const WlInterface* registry_interface;
    const WlInterface* surface_interface;
    const WlInterface* seat_interface;
    bool ok = false;
};

WaylandApi& wayland_api() {
    static WaylandApi api;
    static bool loaded = false;
    if (loaded) return api;
    loaded = true;
    void* lib = dlopen("libwayland-client.so.0", RTLD_LAZY | RTLD_NOLOAD);
    if (!lib) lib = dlopen("libwayland-client.so.0", RTLD_LAZY);
    if (!lib) return api;
#define LUNARIS_WL_SYM(field, name) \
    api.field = reinterpret_cast<decltype(api.field)>(dlsym(lib, name)); \
    if (!api.field) return api;
    // wl_proxy_marshal_flags: libwayland 1.20 (2021) and later.
    LUNARIS_WL_SYM(proxy_marshal_flags, "wl_proxy_marshal_flags")
    LUNARIS_WL_SYM(proxy_create_wrapper, "wl_proxy_create_wrapper")
    LUNARIS_WL_SYM(proxy_wrapper_destroy, "wl_proxy_wrapper_destroy")
    LUNARIS_WL_SYM(proxy_add_listener, "wl_proxy_add_listener")
    LUNARIS_WL_SYM(proxy_get_version, "wl_proxy_get_version")
    LUNARIS_WL_SYM(proxy_set_queue, "wl_proxy_set_queue")
    LUNARIS_WL_SYM(display_create_queue, "wl_display_create_queue")
    LUNARIS_WL_SYM(display_roundtrip_queue, "wl_display_roundtrip_queue")
    LUNARIS_WL_SYM(display_dispatch_queue_pending, "wl_display_dispatch_queue_pending")
    LUNARIS_WL_SYM(display_flush, "wl_display_flush")
    LUNARIS_WL_SYM(registry_interface, "wl_registry_interface")
    LUNARIS_WL_SYM(surface_interface, "wl_surface_interface")
    LUNARIS_WL_SYM(seat_interface, "wl_seat_interface")
#undef LUNARIS_WL_SYM
    api.ok = true;
    return api;
}

// What wayland-scanner writes for keyboard-shortcuts-inhibit-unstable-v1.xml
// (two interfaces, version 1).
const WlInterface* g_noTypes[1] = {nullptr};
const WlInterface* g_inhibitTypes[3] = {nullptr, nullptr, nullptr};
const WlMessage g_inhibitorRequests[] = {{"destroy", "", g_noTypes}};
const WlMessage g_inhibitorEvents[] = {{"active", "", g_noTypes}, {"inactive", "", g_noTypes}};
const WlInterface g_inhibitorInterface = {
    "zwp_keyboard_shortcuts_inhibitor_v1", 1, 1, g_inhibitorRequests, 2, g_inhibitorEvents};
const WlMessage g_managerRequests[] = {{"destroy", "", g_noTypes}, {"inhibit_shortcuts", "noo", g_inhibitTypes}};
const WlInterface g_managerInterface = {
    "zwp_keyboard_shortcuts_inhibit_manager_v1", 1, 2, g_managerRequests, 0, nullptr};

const uint32_t WL_DISPLAY_GET_REGISTRY = 1;
const uint32_t WL_REGISTRY_BIND = 0;
const uint32_t MANAGER_INHIBIT_SHORTCUTS = 1;
const uint32_t INHIBITOR_DESTROY = 0;
const uint32_t WL_MARSHAL_FLAG_DESTROY = 1;

struct WaylandGrab {
    void* display = nullptr;
    WlEventQueue* queue = nullptr;
    WlProxy* manager = nullptr;
    uint32_t managerName = 0;
    bool looked = false;
    WlProxy* inhibitor = nullptr;
    // Whether the compositor says the shortcuts are with the window now.
    bool active = false;
};
WaylandGrab g_wayland;

void registry_global(void* data, WlProxy* registry, uint32_t name, const char* interface, uint32_t /*version*/) {
    auto* grab = static_cast<WaylandGrab*>(data);
    if (grab->manager || !interface || std::string(interface) != g_managerInterface.name) return;
    grab->managerName = name;
    grab->manager = wayland_api().proxy_marshal_flags(
        registry, WL_REGISTRY_BIND, &g_managerInterface, 1, 0,
        name, g_managerInterface.name, 1u, nullptr);
}
void registry_global_remove(void*, WlProxy*, uint32_t) {}
void (*g_registryListener[])(void) = {
    reinterpret_cast<void (*)(void)>(registry_global),
    reinterpret_cast<void (*)(void)>(registry_global_remove),
};

void inhibitor_active(void* data, WlProxy*) { static_cast<WaylandGrab*>(data)->active = true; }
void inhibitor_inactive(void* data, WlProxy*) { static_cast<WaylandGrab*>(data)->active = false; }
void (*g_inhibitorListener[])(void) = {
    reinterpret_cast<void (*)(void)>(inhibitor_active),
    reinterpret_cast<void (*)(void)>(inhibitor_inactive),
};

QNativeInterface::QWaylandApplication* wayland_app() {
    return qApp ? qApp->nativeInterface<QNativeInterface::QWaylandApplication>() : nullptr;
}

// Binds the compositor's manager, once, on a queue of our own so that Qt's
// dispatching is left alone. Null when the compositor does not have it.
WlProxy* wayland_manager() {
    WaylandGrab& grab = g_wayland;
    if (grab.looked) return grab.manager;
    auto* app = wayland_app();
    WaylandApi& api = wayland_api();
    if (!app || !api.ok || !app->display()) return nullptr;
    grab.looked = true;
    grab.display = app->display();
    g_inhibitTypes[0] = &g_inhibitorInterface;
    g_inhibitTypes[1] = api.surface_interface;
    g_inhibitTypes[2] = api.seat_interface;
    grab.queue = api.display_create_queue(grab.display);
    if (!grab.queue) return nullptr;
    void* wrapper = api.proxy_create_wrapper(grab.display);
    if (!wrapper) return nullptr;
    api.proxy_set_queue(static_cast<WlProxy*>(wrapper), grab.queue);
    WlProxy* registry = api.proxy_marshal_flags(
        static_cast<WlProxy*>(wrapper), WL_DISPLAY_GET_REGISTRY, api.registry_interface, 1, 0, nullptr);
    api.proxy_wrapper_destroy(wrapper);
    if (!registry) return nullptr;
    api.proxy_add_listener(registry, g_registryListener, &grab);
    api.display_roundtrip_queue(grab.display, grab.queue);
    // The registry stays (and its queue): destroying it would need
    // wl_proxy_destroy on an object that still gets events.
    return grab.manager;
}

void wayland_release() {
    WaylandGrab& grab = g_wayland;
    if (!grab.inhibitor) return;
    WaylandApi& api = wayland_api();
    api.proxy_marshal_flags(grab.inhibitor, INHIBITOR_DESTROY, nullptr,
                            api.proxy_get_version(grab.inhibitor), WL_MARSHAL_FLAG_DESTROY);
    grab.inhibitor = nullptr;
    grab.active = false;
    api.display_flush(grab.display);
}

// Returns false when Wayland is not the window system in use.
bool wayland_keyboard_grab(QWindow* window, bool grab) {
    auto* app = wayland_app();
    if (!app) return false;
    WaylandApi& api = wayland_api();
    WlProxy* manager = wayland_manager();
    if (g_wayland.queue) api.display_dispatch_queue_pending(g_wayland.display, g_wayland.queue);
    // One inhibitor at most: a second one for the same surface and seat is a
    // protocol error (already_inhibited) that ends the connection.
    wayland_release();
    if (!grab) return true;
    if (!manager) {
        std::cerr << "Lunaris Client: the compositor has no keyboard-shortcuts-inhibit protocol" << std::endl;
        return true;
    }
    // The window's wl_surface: QWaylandWindow::winId() (qtbase
    // src/plugins/platforms/wayland/qwaylandwindow.cpp, "return
    // reinterpret_cast<WId>(wlSurface())"). Only a shown window has one.
    if (!window->isVisible() || !window->handle()) return true;
    void* surface = reinterpret_cast<void*>(window->winId());
    void* seat = app->seat();
    if (!surface || !seat) return true;
    g_wayland.inhibitor = api.proxy_marshal_flags(
        manager, MANAGER_INHIBIT_SHORTCUTS, &g_inhibitorInterface,
        api.proxy_get_version(manager), 0, nullptr, surface, seat);
    if (g_wayland.inhibitor) {
        api.proxy_add_listener(g_wayland.inhibitor, g_inhibitorListener, &g_wayland);
    }
    api.display_flush(g_wayland.display);
    std::cerr << "Lunaris Client: asked the compositor for the system shortcuts" << std::endl;
    return true;
}

bool wayland_in_use() { return wayland_app() != nullptr; }
bool wayland_has_manager() { return wayland_manager() != nullptr; }

#else // no QWaylandApplication in this Qt

bool wayland_keyboard_grab(QWindow*, bool) { return false; }
bool wayland_in_use() { return false; }
bool wayland_has_manager() { return false; }

#endif // LUNARIS_WAYLAND_GRAB

} // namespace
#endif // Q_OS_LINUX

static QWindow* stream_window() {
    QWindow* window = QGuiApplication::focusWindow();
    if (!window) {
        const auto windows = QGuiApplication::topLevelWindows();
        for (QWindow* candidate : windows) {
            if (candidate->isVisible()) {
                window = candidate;
                break;
            }
        }
    }
    return window;
}

void set_keyboard_grab_helper(bool grab) {
    if (!qApp) return;
#if defined(Q_OS_LINUX)
    // Letting go needs no window (it may be hidden or gone by now).
    if (!grab && wayland_in_use()) {
        wayland_keyboard_grab(nullptr, false);
        return;
    }
#endif
    QWindow* window = stream_window();
    if (!window) return;
#if defined(Q_OS_LINUX)
    if (wayland_keyboard_grab(window, grab)) return;
    if (x11_keyboard_grab(window, grab)) return;
#endif
    window->setKeyboardGrabEnabled(grab);
}

// How the window system gives a window its shortcuts:
// 0: not at all (a Wayland compositor without the protocol).
// 1: the compositor does, for as long as it is asked and the window has the
//    keyboard; it may ask the user first (Wayland).
// 2: a keyboard grab, which has to be let go when another window is to get
//    the keys (X11).
// 3: the same grab from an X11 window in a Wayland session (XWayland): the
//    compositor decides whether it counts (GNOME: only for the programs its
//    settings list).
// 4: only Qt's own grab inside the app, which takes no system shortcut
//    (Windows, macOS: not written).
int system_keys_support_helper() {
    if (!qApp) return 0;
#if defined(Q_OS_LINUX)
    if (wayland_in_use()) return wayland_has_manager() ? 1 : 0;
    if (qApp->nativeInterface<QNativeInterface::QX11Application>()) {
        return qgetenv("XDG_SESSION_TYPE") == "wayland" ? 3 : 2;
    }
#endif
    return 4;
}
