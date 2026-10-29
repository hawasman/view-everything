//! The system tray icon and its menu (ADR-0012): Open, Sync now, the running Tasks and Quit.
//! GPUI has no tray API, so each platform talks to its own: a StatusNotifierItem over D-Bus on
//! Linux (`ksni`), `Shell_NotifyIconW` on Windows, `NSStatusItem` on macOS. Menu choices travel
//! as [`TrayAction`]s over a channel the app loop reads; the tray holds no GPUI types.

use async_channel::Sender;

/// What the user chose in the tray.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    /// Show the window (open one if none is).
    Open,
    /// Sync every online Volume.
    SyncNow,
    /// Start Live updates (paused on a login start).
    StartLive,
    Quit,
}

/// What the menu shows besides its fixed items.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayState {
    /// Labels of the running Tasks, read-only.
    pub tasks: Vec<String>,
    /// Live updates are paused: the menu offers Start.
    pub live_paused: bool,
}

/// A running tray icon. Dropping it removes the icon.
pub struct Tray(platform::Tray);

impl Tray {
    /// Puts the icon in the tray, or `None` when the desktop has none (no StatusNotifierWatcher
    /// on Linux, no shell notification area elsewhere).
    pub fn start(actions: Sender<TrayAction>, state: TrayState) -> Option<Tray> {
        platform::Tray::start(actions, state).map(Tray)
    }

    /// Shows `state` in the menu.
    pub fn update(&self, state: TrayState) {
        self.0.update(state);
    }
}

/// The menu's text for a Task, keeping it to one short line.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn task_text(label: &str) -> String {
    const MAX: usize = 60;
    let line = label.lines().next().unwrap_or_default();
    if line.chars().count() > MAX {
        format!("{}…", line.chars().take(MAX - 1).collect::<String>())
    } else {
        line.to_owned()
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{TrayAction, TrayState, task_text};
    use async_channel::Sender;
    use ksni::blocking::{Handle, TrayMethods};
    use ksni::menu::StandardItem;

    struct Item {
        actions: Sender<TrayAction>,
        state: TrayState,
    }

    impl Item {
        fn action(&self, label: &str, icon: &str, action: TrayAction) -> ksni::MenuItem<Self> {
            StandardItem {
                label: label.into(),
                icon_name: icon.into(),
                activate: Box::new(move |this: &mut Item| {
                    let _ = this.actions.send_blocking(action);
                }),
                ..Default::default()
            }
            .into()
        }
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            "view-everything".into()
        }

        fn title(&self) -> String {
            crate::app::APP_NAME.into()
        }

        fn icon_name(&self) -> String {
            "view-everything".into()
        }

        /// An AppImage carries its icon inside, where no icon theme looks.
        fn icon_theme_path(&self) -> String {
            std::env::var("APPDIR").map(|d| format!("{d}/usr/share/icons")).unwrap_or_default()
        }

        /// Used by the tray host when no icon theme has `view-everything`, as in a build run
        /// from the source tree.
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            [22, 32, 48]
                .into_iter()
                .filter_map(|size| {
                    crate::tray_icon::argb(size).map(|data| ksni::Icon {
                        width: size as i32,
                        height: size as i32,
                        data,
                    })
                })
                .collect()
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.actions.send_blocking(TrayAction::Open);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            let mut menu = vec![
                self.action("Open", "window-new", TrayAction::Open),
                self.action("Sync now", "view-refresh", TrayAction::SyncNow),
            ];
            if self.state.live_paused {
                menu.push(self.action("Live updates paused: Start", "media-playback-start", TrayAction::StartLive));
            }
            if !self.state.tasks.is_empty() {
                menu.push(ksni::MenuItem::Separator);
                for task in &self.state.tasks {
                    menu.push(
                        StandardItem {
                            // A lone underscore marks an access key in a label; double it to show it.
                            label: task_text(task).replace('_', "__"),
                            enabled: false,
                            ..Default::default()
                        }
                        .into(),
                    );
                }
            }
            menu.push(ksni::MenuItem::Separator);
            menu.push(self.action("Quit", "application-exit", TrayAction::Quit));
            menu
        }
    }

    pub struct Tray(Handle<Item>);

    impl Tray {
        /// `spawn` fails when no StatusNotifierWatcher owns its name on the session bus: that is
        /// how a desktop without tray support (GNOME without the AppIndicator extension) shows.
        pub fn start(actions: Sender<TrayAction>, state: TrayState) -> Option<Tray> {
            match (Item { actions, state }).spawn() {
                Ok(handle) => Some(Tray(handle)),
                Err(e) => {
                    eprintln!("No system tray: {e}");
                    None
                }
            }
        }

        pub fn update(&self, state: TrayState) {
            self.0.update(|item| item.state = state);
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            self.0.shutdown().wait();
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{TrayAction, TrayState, task_text};
    use async_channel::Sender;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
        GetCursorPos, GetMessageW, HWND_MESSAGE, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, RegisterClassW,
        SetForegroundWindow, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP,
        WM_RBUTTONUP, WNDCLASSW,
    };

    const CALLBACK: u32 = WM_APP + 1;
    const ID_OPEN: usize = 1;
    const ID_SYNC: usize = 2;
    const ID_LIVE: usize = 3;
    const ID_QUIT: usize = 4;

    /// What the window procedure reads; it has no other way to reach the app.
    struct Shared {
        actions: Sender<TrayAction>,
        state: Mutex<TrayState>,
    }

    static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    pub struct Tray {
        shared: Arc<Shared>,
        window: isize,
    }

    impl Tray {
        pub fn start(actions: Sender<TrayAction>, state: TrayState) -> Option<Tray> {
            let shared = Arc::new(Shared {
                actions,
                state: Mutex::new(state),
            });
            *SHARED.lock() = Some(shared.clone());
            let (ready, started) = std::sync::mpsc::channel::<Option<isize>>();
            std::thread::Builder::new().name("ve-tray".into()).spawn(move || run(ready)).ok()?;
            let window = started.recv().ok()??;
            Some(Tray { shared, window })
        }

        pub fn update(&self, state: TrayState) {
            *self.shared.state.lock() = state;
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            // SAFETY: posts a message to the window the tray thread owns; it removes the icon
            // and ends its loop on WM_CLOSE.
            unsafe { PostMessageW(self.window as HWND, WM_CLOSE, 0, 0) };
        }
    }

    /// The tray thread: a message-only window that owns the icon, and its message loop.
    fn run(ready: std::sync::mpsc::Sender<Option<isize>>) {
        // SAFETY: plain Win32 calls on this thread, with arguments built here and checked.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class = wide("ViewEverythingTray");
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(window_proc);
            wc.hInstance = instance;
            wc.lpszClassName = class.as_ptr();
            RegisterClassW(&wc);
            let window = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if window.is_null() {
                let _ = ready.send(None);
                return;
            }
            let mut data = icon_data(window);
            data.hIcon = app_icon();
            if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
                DestroyWindow(window);
                let _ = ready.send(None);
                return;
            }
            let _ = ready.send(Some(window as isize));
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe fn icon_data(window: HWND) -> NOTIFYICONDATAW {
        // SAFETY: NOTIFYICONDATAW is plain data; zero is a valid starting point.
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = window;
        data.uID = 1;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = CALLBACK;
        for (slot, unit) in data.szTip.iter_mut().zip(wide(crate::app::APP_NAME)) {
            *slot = unit;
        }
        data
    }

    /// The app icon rasterized to the small-icon size.
    unsafe fn app_icon() -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
        const SIZE: u32 = 32;
        let Some(rgba) = crate::tray_icon::rgba(SIZE) else {
            return std::ptr::null_mut();
        };
        // The icon wants BGRA; its AND mask is ignored for 32-bit colour but must be present.
        let bgra: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
        let mask = vec![0u8; (SIZE * SIZE / 8) as usize];
        // SAFETY: both buffers hold the SIZE x SIZE pixels the call reads.
        unsafe { CreateIcon(std::ptr::null_mut(), SIZE as i32, SIZE as i32, 1, 32, mask.as_ptr(), bgra.as_ptr()) }
    }

    unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let shared = SHARED.lock().clone();
        match message {
            CALLBACK => {
                // SAFETY: Win32 menu calls on the tray thread; the menu lives only here.
                unsafe {
                    match lparam as u32 {
                        WM_LBUTTONUP => {
                            if let Some(s) = &shared {
                                let _ = s.actions.send_blocking(TrayAction::Open);
                            }
                        }
                        WM_RBUTTONUP | WM_CONTEXTMENU => {
                            if let Some(s) = &shared {
                                popup(window, s);
                            }
                        }
                        _ => {}
                    }
                }
                0
            }
            WM_CLOSE => {
                // SAFETY: removes this window's own icon, then the window (which ends the loop).
                unsafe {
                    let data = icon_data(window);
                    Shell_NotifyIconW(NIM_DELETE, &data);
                    DestroyWindow(window);
                }
                0
            }
            WM_DESTROY => {
                // SAFETY: wakes this thread's loop to end it.
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
                0
            }
            // SAFETY: the default handling of a message this procedure does not take.
            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    unsafe fn popup(window: HWND, shared: &Shared) {
        let state = shared.state.lock().clone();
        // SAFETY: the menu is built, shown and destroyed here, with NUL-terminated strings that
        // outlive the calls.
        unsafe {
            let menu = CreatePopupMenu();
            let add = |flags: u32, id: usize, text: &str| {
                AppendMenuW(menu, flags, id, wide(text).as_ptr());
            };
            add(MF_STRING, ID_OPEN, "Open");
            add(MF_STRING, ID_SYNC, "Sync now");
            if state.live_paused {
                add(MF_STRING, ID_LIVE, "Live updates paused: Start");
            }
            if !state.tasks.is_empty() {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                for task in &state.tasks {
                    add(MF_STRING | MF_GRAYED, 0, &task_text(task));
                }
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            add(MF_STRING, ID_QUIT, "Quit");
            let mut at: POINT = std::mem::zeroed();
            GetCursorPos(&mut at);
            // Without this the menu would not close when the user clicks elsewhere.
            SetForegroundWindow(window);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | windows_sys::Win32::UI::WindowsAndMessaging::TPM_RETURNCMD,
                at.x,
                at.y,
                0,
                window,
                std::ptr::null(),
            );
            DestroyMenu(menu);
            let action = match chosen as usize {
                ID_OPEN => TrayAction::Open,
                ID_SYNC => TrayAction::SyncNow,
                ID_LIVE => TrayAction::StartLive,
                ID_QUIT => TrayAction::Quit,
                _ => return,
            };
            let _ = shared.actions.send_blocking(action);
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{TrayAction, TrayState, task_text};
    use async_channel::Sender;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
    use objc2_foundation::{NSData, NSString};
    use std::cell::RefCell;

    struct Ivars {
        actions: Sender<TrayAction>,
    }

    define_class!(
        // The menu items' target: each action sends its TrayAction.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "VETrayTarget"]
        #[ivars = Ivars]
        struct Target;

        impl Target {
            #[unsafe(method(open:))]
            fn open(&self, _sender: Option<&AnyObject>) {
                let _ = self.ivars().actions.send_blocking(TrayAction::Open);
            }

            #[unsafe(method(sync:))]
            fn sync(&self, _sender: Option<&AnyObject>) {
                let _ = self.ivars().actions.send_blocking(TrayAction::SyncNow);
            }

            #[unsafe(method(live:))]
            fn live(&self, _sender: Option<&AnyObject>) {
                let _ = self.ivars().actions.send_blocking(TrayAction::StartLive);
            }

            #[unsafe(method(quit:))]
            fn quit(&self, _sender: Option<&AnyObject>) {
                let _ = self.ivars().actions.send_blocking(TrayAction::Quit);
            }
        }
    );

    /// Lives on the main thread, where AppKit objects must be used (the app loop calls `update`
    /// there too).
    pub struct Tray {
        item: Retained<NSStatusItem>,
        target: Retained<Target>,
        mtm: MainThreadMarker,
        shown: RefCell<Option<TrayState>>,
    }

    impl Tray {
        pub fn start(actions: Sender<TrayAction>, state: TrayState) -> Option<Tray> {
            let mtm = MainThreadMarker::new()?;
            let target = Target::alloc(mtm).set_ivars(Ivars { actions });
            // SAFETY: NSObject's init on a freshly allocated instance of its subclass.
            let target: Retained<Target> = unsafe { msg_send![super(target), init] };
            let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            if let Some(button) = item.button(mtm) {
                if let Some(png) = crate::tray_icon::png(22) {
                    let data = NSData::with_bytes(&png);
                    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
                        image.setSize(objc2_foundation::NSSize::new(18.0, 18.0));
                        button.setImage(Some(&image));
                    }
                } else {
                    button.setTitle(&NSString::from_str("VE"));
                }
            }
            let tray = Tray {
                item,
                target,
                mtm,
                shown: RefCell::new(None),
            };
            tray.update(state);
            Some(tray)
        }

        pub fn update(&self, state: TrayState) {
            if self.shown.borrow().as_ref() == Some(&state) {
                return;
            }
            let menu = NSMenu::new(self.mtm);
            let add = |title: &str, action: Option<objc2::runtime::Sel>| {
                // SAFETY: the selector names a method Target implements; the item keeps the target.
                let item = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(self.mtm),
                        &NSString::from_str(title),
                        action,
                        &NSString::from_str(""),
                    )
                };
                if action.is_some() {
                    // SAFETY: as above.
                    unsafe { item.setTarget(Some(&self.target)) };
                } else {
                    item.setEnabled(false);
                }
                menu.addItem(&item);
            };
            add("Open", Some(sel!(open:)));
            add("Sync now", Some(sel!(sync:)));
            if state.live_paused {
                add("Live updates paused: Start", Some(sel!(live:)));
            }
            if !state.tasks.is_empty() {
                menu.addItem(&NSMenuItem::separatorItem(self.mtm));
                for task in &state.tasks {
                    add(&task_text(task), None);
                }
            }
            menu.addItem(&NSMenuItem::separatorItem(self.mtm));
            add("Quit", Some(sel!(quit:)));
            self.item.setMenu(Some(&menu));
            *self.shown.borrow_mut() = Some(state);
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
        }
    }
}
