//! Menu-bar (system tray) residency.
//!
//! Everything here is implemented from Rust against the native APIs — the
//! vendored fenster C code is not modified. Where fenster's own behavior
//! fights the tray design (its macOS windowShouldClose: calls
//! `[NSApp terminate:]`, its event loop swallows left-clicks, and its Win32
//! WM_CLOSE destroys the window), we hook in at runtime instead: macOS
//! replaces the IMP of fenster's own runtime-registered window delegate,
//! Windows subclasses fenster's window proc with SetWindowLongPtrW.
//!
//! Linux has no unified tray API; there the tray is deferred and this module
//! is a no-op (`available()` returns false, Esc keeps quitting as before).

use crate::fenster::Fenster;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// Tray-menu command written by the platform backends: 1 = 打开窗口, 2 = 关闭程序.
static CMD: AtomicI32 = AtomicI32::new(0);
/// True while the main window is hidden and the app lives in the menu bar.
static HIDDEN: AtomicBool = AtomicBool::new(false);

/// False on Linux, where the tray is not implemented and Esc still quits.
pub fn available() -> bool {
    !cfg!(target_os = "linux")
}

pub fn is_hidden() -> bool {
    HIDDEN.load(Ordering::Relaxed)
}

/// Call once after `fenster.open()`: installs the menu-bar icon and hooks the
/// window-close path (IMP replacement on macOS, subclassing on Windows).
pub fn init(f: &Fenster) {
    imp::init(f.window_handle())
}

/// Per-frame pump: applies pending tray-menu commands ("打开窗口" is handled
/// inside; the window is shown directly). Returns true for 关闭程序, which the
/// caller turns into the normal graceful-quit path (UiEvent::Quit).
pub fn poll() -> bool {
    imp::poll()
}

/// Hide the window (Esc / close button); the app keeps running in the tray.
pub fn hide() {
    imp::hide()
}

/// Best-effort tray teardown (Windows deletes the icon; others are no-ops —
/// process exit removes the status item there anyway).
pub fn shutdown() {
    imp::shutdown()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{CMD, HIDDEN};
    use std::ffi::{c_char, c_void, CStr};
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    type Id = *mut c_void;
    type Sel = *const c_void;
    /// Objective-C BOOL: signed char on both arm64 and x86_64.
    type BOOL = i8;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSRect {
        origin: NSPoint,
        size: NSSize,
    }

    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra: usize) -> Id;
        fn objc_registerClassPair(cls: Id);
        fn class_addMethod(cls: Id, sel: Sel, imp: *const c_void, types: *const c_char) -> BOOL;
        fn class_replaceMethod(cls: Id, sel: Sel, imp: *const c_void, types: *const c_char) -> BOOL;
        fn sel_registerName(name: *const c_char) -> Sel;

        fn objc_msgSend();
        #[cfg(target_arch = "x86_64")]
        fn objc_msgSend_stret(); // struct returns > 16 bytes (System V x86_64)
    }

    // Typed objc_msgSend wrappers. Each re-casts the untyped extern to the
    // real signature so arguments land in their true ABI slots (CGFloat in a
    // float register, structs by value, correct return register) — the
    // Objective-C method ABI is the plain C ABI on arm64/x86_64.
    #[inline]
    unsafe fn send0<R>(rcvr: Id, sel: Sel) -> R {
        let msg: unsafe extern "C" fn(Id, Sel) -> R =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        unsafe { msg(rcvr, sel) }
    }
    #[inline]
    unsafe fn send1<A, R>(rcvr: Id, sel: Sel, a: A) -> R {
        let msg: unsafe extern "C" fn(Id, Sel, A) -> R =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        unsafe { msg(rcvr, sel, a) }
    }
    #[inline]
    unsafe fn send2<A, B, R>(rcvr: Id, sel: Sel, a: A, b: B) -> R {
        let msg: unsafe extern "C" fn(Id, Sel, A, B) -> R =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        unsafe { msg(rcvr, sel, a, b) }
    }
    #[inline]
    unsafe fn send3<A, B, C, R>(rcvr: Id, sel: Sel, a: A, b: B, c: C) -> R {
        let msg: unsafe extern "C" fn(Id, Sel, A, B, C) -> R =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        unsafe { msg(rcvr, sel, a, b, c) }
    }
    #[inline]
    #[cfg(target_arch = "x86_64")]
    unsafe fn send_stret<R>(rcvr: Id, sel: Sel) -> R {
        let msg: unsafe extern "C" fn(Id, Sel) -> R =
            unsafe { std::mem::transmute(objc_msgSend_stret as unsafe extern "C" fn()) };
        unsafe { msg(rcvr, sel) }
    }

    fn sel(name: &'static CStr) -> Sel {
        unsafe { sel_registerName(name.as_ptr()) }
    }

    unsafe fn cls(name: &'static CStr) -> Id {
        unsafe { objc_getClass(name.as_ptr()) }
    }

    type ActionImp = unsafe extern "C" fn(Id, Sel, Id);
    type ShouldCloseImp = unsafe extern "C" fn(Id, Sel, Id) -> BOOL;

    /// Turns a typed function pointer into a raw IMP.
    unsafe fn imp(f: ActionImp) -> *const c_void {
        unsafe { std::mem::transmute::<ActionImp, *const c_void>(f) }
    }

    const APP_ICON_PNG: &[u8] = include_bytes!("../assets/icon.iconset/icon_32x32.png");

    static WND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static ITEM: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static MENU: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static PREV_LEFT: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn tray_show(_self: Id, _cmd: Sel, _sender: Id) {
        CMD.store(1, Ordering::Relaxed);
    }

    unsafe extern "C" fn tray_quit(_self: Id, _cmd: Sel, _sender: Id) {
        CMD.store(2, Ordering::Relaxed);
    }

    /// Replacement for fenster's `windowShouldClose:` — the vendored version
    /// calls `[NSApp terminate:]`, killing the whole process on a close-button
    /// click. We hide the window instead and tell AppKit "not closed".
    unsafe extern "C" fn should_close(_self: Id, _cmd: Sel, window: Id) -> BOOL {
        unsafe {
            let nil: Id = std::ptr::null_mut();
            send1::<Id, ()>(window, sel(c"orderOut:"), nil);
            HIDDEN.store(true, Ordering::Relaxed);
            0 // NO
        }
    }

    unsafe fn nsstring(s: &'static CStr) -> Id {
        unsafe { send1(cls(c"NSString"), sel(c"stringWithUTF8String:"), s.as_ptr()) }
    }

    /// A tiny runtime class whose two action methods only set CMD; used as the
    /// menu items' target. (Everything here is intentionally never released —
    /// the tray lives for the whole process.)
    unsafe fn make_target() -> Id {
        unsafe {
            let c = objc_allocateClassPair(cls(c"NSObject"), c"RuozhiTrayTarget".as_ptr(), 0);
            class_addMethod(c, sel(c"trayShow:"), imp(tray_show), c"v@:@".as_ptr());
            class_addMethod(c, sel(c"trayQuit:"), imp(tray_quit), c"v@:@".as_ptr());
            objc_registerClassPair(c);
            send0(send0(c, sel(c"alloc")), sel(c"init"))
        }
    }

    unsafe fn add_item(menu: Id, target: Id, title: &'static CStr, action: &'static CStr) {
        unsafe {
            let item: Id = send3(
                send0(cls(c"NSMenuItem"), sel(c"alloc")),
                sel(c"initWithTitle:action:keyEquivalent:"),
                nsstring(title),
                sel(action),
                nsstring(c""), // no key equivalent
            );
            send1::<Id, ()>(item, sel(c"setTarget:"), target);
            send1::<Id, ()>(menu, sel(c"addItem:"), item);
        }
    }

    pub fn init(wnd: *mut c_void) {
        WND.store(wnd, Ordering::Relaxed);
        unsafe {
            let app: Id = send0(cls(c"NSApplication"), sel(c"sharedApplication"));
            // Accessory policy: menu-bar-only app, no Dock icon (fenster_open
            // sets Regular first; we downgrade right after).
            send1::<isize, ()>(app, sel(c"setActivationPolicy:"), 1isize);

            // Hook fenster's delegate. "FensterDelegate" is a constant string
            // inside c/fenster.h — keep in sync when upgrading that vendored
            // file.
            class_replaceMethod(
                cls(c"FensterDelegate"),
                sel(c"windowShouldClose:"),
                std::mem::transmute::<ShouldCloseImp, *const c_void>(should_close),
                c"c@:@".as_ptr(),
            );

            // Status item with the app icon.
            let bar: Id = send0(cls(c"NSStatusBar"), sel(c"systemStatusBar"));
            let item: Id = send1(bar, sel(c"statusItemWithLength:"), -1.0f64); // variable length
            ITEM.store(item, Ordering::Relaxed);
            let data: Id = send2(
                cls(c"NSData"),
                sel(c"dataWithBytes:length:"),
                APP_ICON_PNG.as_ptr(),
                APP_ICON_PNG.len(),
            );
            let img: Id = send1(send0(cls(c"NSImage"), sel(c"alloc")), sel(c"initWithData:"), data);
            send1::<NSSize, ()>(img, sel(c"setSize:"), NSSize { width: 18.0, height: 18.0 });
            let btn: Id = send0(item, sel(c"button"));
            send1::<Id, ()>(btn, sel(c"setImage:"), img);
            send1::<Id, ()>(btn, sel(c"setToolTip:"), nsstring(c"ruozhi"));

            let target: Id = make_target();
            let menu: Id = send1(
                send0(cls(c"NSMenu"), sel(c"alloc")),
                sel(c"initWithTitle:"),
                nsstring(c""),
            );
            add_item(menu, target, c"打开窗口", c"trayShow:");
            add_item(menu, target, c"关闭程序", c"trayQuit:");
            send1::<Id, ()>(item, sel(c"setMenu:"), menu); // right-click uses AppKit's own path
            MENU.store(menu, Ordering::Relaxed);
        }
    }

    pub fn poll() -> bool {
        if apply_cmd() {
            return true;
        }

        // Left-clicks never reach AppKit: fenster_loop dequeues every
        // LeftMouseDown and drops it without calling sendEvent:. So detect
        // clicks on the status item by polling the global mouse state
        // (+[NSEvent pressedMouseButtons] / mouseLocation) instead.
        let pressed = pressed_left();
        let was = PREV_LEFT.swap(pressed, Ordering::Relaxed);
        if pressed && !was && cursor_in_status_item() {
            unsafe { pop_up_menu() };
            if apply_cmd() {
                return true;
            }
        }
        false
    }

    /// Applies a pending menu choice; true means 关闭程序.
    fn apply_cmd() -> bool {
        match CMD.swap(0, Ordering::Relaxed) {
            1 => {
                show();
                false
            }
            2 => true,
            _ => false,
        }
    }

    unsafe fn pop_up_menu() {
        unsafe {
            let item = ITEM.load(Ordering::Relaxed);
            let menu = MENU.load(Ordering::Relaxed);
            if item.is_null() || menu.is_null() {
                return;
            }
            let btn: Id = send0(item, sel(c"button"));
            let nil: Id = std::ptr::null_mut();
            let loc = NSPoint { x: 0.0, y: 0.0 }; // button's bottom-left: menu hangs below
            // Blocks until the menu is dismissed (the 60fps loop pauses; audio
            // and protocol threads keep running).
            let _popped: BOOL =
                send3(menu, sel(c"popUpMenuPositioningItem:atLocation:inView:"), nil, loc, btn);
        }
    }

    fn pressed_left() -> bool {
        // +[NSEvent pressedMouseButtons]: bit 0 is the left button.
        let bits: usize = unsafe { send0(cls(c"NSEvent"), sel(c"pressedMouseButtons")) };
        bits & 1 != 0
    }

    fn cursor_in_status_item() -> bool {
        unsafe {
            let item = ITEM.load(Ordering::Relaxed);
            if item.is_null() {
                return false;
            }
            let btn: Id = send0(item, sel(c"button"));
            if btn.is_null() {
                return false;
            }
            let win: Id = send0(btn, sel(c"window"));
            if win.is_null() {
                return false;
            }
            let frame: NSRect = frame_of(win);
            let loc: NSPoint = send0(cls(c"NSEvent"), sel(c"mouseLocation"));
            loc.x >= frame.origin.x
                && loc.x <= frame.origin.x + frame.size.width
                && loc.y >= frame.origin.y
                && loc.y <= frame.origin.y + frame.size.height
        }
    }

    #[inline]
    unsafe fn frame_of(win: Id) -> NSRect {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            send_stret(win, sel(c"frame"))
        }
        #[cfg(not(target_arch = "x86_64"))]
        unsafe {
            send0(win, sel(c"frame"))
        }
    }

    pub fn show() {
        let wnd = WND.load(Ordering::Relaxed);
        if wnd.is_null() {
            return;
        }
        unsafe {
            let app: Id = send0(cls(c"NSApplication"), sel(c"sharedApplication"));
            send1::<i8, ()>(app, sel(c"activateIgnoringOtherApps:"), 1i8);
            send1::<Id, ()>(wnd, sel(c"makeKeyAndOrderFront:"), std::ptr::null_mut::<c_void>());
        }
        HIDDEN.store(false, Ordering::Relaxed);
    }

    pub fn hide() {
        let wnd = WND.load(Ordering::Relaxed);
        if wnd.is_null() {
            return;
        }
        unsafe {
            send1::<Id, ()>(wnd, sel(c"orderOut:"), std::ptr::null_mut::<c_void>());
        }
        HIDDEN.store(true, Ordering::Relaxed);
    }

    pub fn shutdown() {}
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{CMD, HIDDEN};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicIsize, AtomicPtr, AtomicU32, Ordering};

    type HWND = *mut c_void;
    type HICON = *mut c_void;
    type HMENU = *mut c_void;
    type WPARAM = usize;
    type LPARAM = isize;
    type LRESULT = isize;

    const WM_NULL: u32 = 0x0000;
    const WM_CLOSE: u32 = 0x0010;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_APP: u32 = 0x8000;
    /// Tray callback message: uCallbackMessage given to Shell_NotifyIconW.
    const TRAY_CALLBACK: u32 = WM_APP + 1;
    const GWLP_WNDPROC: i32 = -4;
    const SW_HIDE: i32 = 0;
    const SW_RESTORE: i32 = 9;
    const IMAGE_ICON: u32 = 1;
    const MF_STRING: u32 = 0x0000;
    const TPM_NONOTIFY: u32 = 0x0080;
    const TPM_RETURNCMD: u32 = 0x0100;
    const TPM_RIGHTBUTTON: u32 = 0x0002;
    const NIM_ADD: u32 = 0x0000;
    const NIM_DELETE: u32 = 0x0002;
    const NIF_MESSAGE: u32 = 0x0001;
    const NIF_ICON: u32 = 0x0002;
    const NIF_TIP: u32 = 0x0004;
    /// build.rs embeds assets/ruozhi.ico; winresource stores it as icon #1.
    const ICON_RESOURCE_ID: usize = 1;
    /// IDI_APPLICATION, as MAKEINTRESOURCE.
    const IDI_APPLICATION: usize = 32512;

    static WND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static OLD_WNDPROC: AtomicIsize = AtomicIsize::new(0);
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

    #[repr(C)]
    struct POINT {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct NOTIFYICONDATAW {
        cb_size: u32,
        hwnd: HWND,
        uid: u32,
        uflags: u32,
        callback_msg: u32,
        hicon: HICON,
        tip: [u16; 128],
        state: u32,
        state_mask: u32,
        info: [u16; 256],
        uversion: u32,
        info_title: [u16; 64],
        info_flags: u32,
        guid: [u32; 4],
    }

    unsafe extern "system" {
        fn Shell_NotifyIconW(msg: u32, data: *const NOTIFYICONDATAW) -> i32;
        fn LoadImageW(hinst: *mut c_void, name: *const u16, kind: u32, cx: i32, cy: i32, flags: u32) -> HICON;
        fn ExtractIconExW(file: *const u16, index: i32, large: *mut HICON, small: *mut HICON, count: u32) -> u32;
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn GetModuleFileNameW(hmod: *mut c_void, buf: *mut u16, len: u32) -> u32;
        fn CreatePopupMenu() -> HMENU;
        fn AppendMenuW(menu: HMENU, flags: u32, id: usize, text: *const u16) -> i32;
        fn TrackPopupMenu(menu: HMENU, flags: u32, x: i32, y: i32, rsvd: i32, hwnd: HWND, rsvd2: *mut c_void) -> i32;
        fn DestroyMenu(menu: HMENU) -> i32;
        fn GetCursorPos(pt: *mut POINT) -> i32;
        fn SetForegroundWindow(hwnd: HWND) -> i32;
        fn PostMessageW(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> i32;
        fn ShowWindow(hwnd: HWND, cmd: i32) -> i32;
        fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: isize) -> isize;
        fn CallWindowProcW(prev: isize, hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT;
        fn RegisterWindowMessageW(name: *const u16) -> u32;
    }

    type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

    /// Runs before fenster's own proc: hides on close instead of destroying,
    /// handles the tray callback message, forwards everything else unchanged.
    unsafe extern "system" fn tray_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                // X button / taskbar "Close window": fenster's proc would
                // DestroyWindow here (→ WM_DESTROY → app exit). Hide instead.
                WM_CLOSE => {
                    hide();
                    return 0;
                }
                // tray icon click (uCallbackMessage; lParam carries the mouse msg)
                TRAY_CALLBACK => {
                    let up = l as u32;
                    if up == WM_LBUTTONUP || up == WM_RBUTTONUP {
                        popup_menu(hwnd);
                    }
                    return 0;
                }
                // Explorer restarted: re-add the icon
                m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                    add_icon(hwnd);
                }
                _ => {}
            }
            CallWindowProcW(OLD_WNDPROC.load(Ordering::Relaxed), hwnd, msg, w, l)
        }
    }

    pub fn init(hwnd: *mut c_void) {
        WND.store(hwnd, Ordering::Relaxed);
        unsafe {
            let old = SetWindowLongPtrW(
                hwnd,
                GWLP_WNDPROC,
                (tray_wndproc as WndProc) as usize as isize,
            );
            OLD_WNDPROC.store(old, Ordering::Relaxed);
            TASKBAR_CREATED.store(
                RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
                Ordering::Relaxed,
            );
            add_icon(hwnd);
        }
    }

    unsafe fn add_icon(hwnd: HWND) {
        unsafe {
            let mut nid = std::mem::zeroed::<NOTIFYICONDATAW>();
            nid.cb_size = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hwnd = hwnd;
            nid.uid = 1;
            nid.uflags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            nid.callback_msg = TRAY_CALLBACK;
            nid.hicon = app_icon();
            set_wide(&mut nid.tip, "ruozhi");
            Shell_NotifyIconW(NIM_ADD, &nid);
        }
    }

    unsafe fn app_icon() -> HICON {
        unsafe {
            let exe = GetModuleHandleW(std::ptr::null());
            let h = LoadImageW(exe, ICON_RESOURCE_ID as *const u16, IMAGE_ICON, 16, 16, 0);
            if !h.is_null() {
                return h;
            }
            // fallback: first icon of our own exe (embedded by build.rs)
            let mut path = [0u16; 512];
            let n = GetModuleFileNameW(exe, path.as_mut_ptr(), path.len() as u32) as usize;
            if n > 0 && n < path.len() {
                let mut large = std::ptr::null_mut();
                let mut small = std::ptr::null_mut();
                if ExtractIconExW(path.as_ptr(), 0, &mut large, &mut small, 1) > 0
                    && !small.is_null()
                {
                    return small;
                }
            }
            LoadImageW(std::ptr::null_mut(), IDI_APPLICATION as *const u16, IMAGE_ICON, 16, 16, 0)
        }
    }

    unsafe fn popup_menu(hwnd: HWND) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            AppendMenuW(menu, MF_STRING, 1, wide("打开窗口").as_ptr());
            AppendMenuW(menu, MF_STRING, 2, wide("关闭程序").as_ptr());
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // Foreground dance so the menu also dismisses on outside clicks
            // (KB135788); TPM_RETURNCMD makes TrackPopupMenu return the id.
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                0,
                hwnd,
                std::ptr::null_mut(),
            );
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            match cmd {
                1 => CMD.store(1, Ordering::Relaxed), // 打开窗口
                2 => CMD.store(2, Ordering::Relaxed), // 关闭程序
                _ => {}
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Writes `s` into a NUL-terminated fixed-size UTF-16 buffer.
    fn set_wide(dst: &mut [u16], s: &str) {
        for (i, u) in s.encode_utf16().take(dst.len() - 1).enumerate() {
            dst[i] = u;
        }
    }

    pub fn poll() -> bool {
        match CMD.swap(0, Ordering::Relaxed) {
            1 => {
                show();
                false
            }
            2 => true,
            _ => false,
        }
    }

    pub fn show() {
        let hwnd = WND.load(Ordering::Relaxed);
        if hwnd.is_null() {
            return;
        }
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
        HIDDEN.store(false, Ordering::Relaxed);
    }

    pub fn hide() {
        let hwnd = WND.load(Ordering::Relaxed);
        if hwnd.is_null() {
            return;
        }
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
        HIDDEN.store(true, Ordering::Relaxed);
    }

    pub fn shutdown() {
        let hwnd = WND.load(Ordering::Relaxed);
        if hwnd.is_null() {
            return;
        }
        unsafe {
            let mut nid = std::mem::zeroed::<NOTIFYICONDATAW>();
            nid.cb_size = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hwnd = hwnd;
            nid.uid = 1;
            Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
}

#[cfg(target_os = "linux")]
#[allow(dead_code)] // API stays stable for when the Linux tray is implemented
mod imp {
    // Tray is deferred on Linux: no-ops keep the facade unchanged.
    pub fn init(_wnd: *mut std::ffi::c_void) {}
    pub fn poll() -> bool {
        false
    }
    pub fn show() {}
    pub fn hide() {}
    pub fn shutdown() {}
}
