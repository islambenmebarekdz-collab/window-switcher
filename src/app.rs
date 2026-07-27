use crate::config::{edit_config_file, load_config, Config};
use crate::foreground::ForegroundWatcher;
use crate::keyboard::KeyboardListener;
use crate::mru;
use crate::painter::GdiAAPainter;
use crate::startup::Startup;
use crate::trayicon::TrayIcon;
use crate::uia::{notify, raise_selection_events, ListProvider, Selection};
use crate::utils::{
    announce, check_error, get_app_icon, get_foreground_window, get_window_user_data,
    is_iconic_window, is_running_as_admin, list_windows, minimize_window, set_foreground_window,
    set_window_user_data,
};

use anyhow::{anyhow, Result};
use indexmap::IndexSet;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::core::{w, ComObject, PCWSTR};
use windows::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
    System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
    System::LibraryLoader::GetModuleHandleW,
    UI::Accessibility::{IRawElementProviderSimple, UiaReturnRawElementProvider, UiaRootObjectId},
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyIcon, DispatchMessageW, GetMessageW,
        GetWindowLongPtrW, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW,
        RegisterWindowMessageW, SetWindowLongPtrW, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
        CW_USEDEFAULT, GWL_STYLE, HICON, HTCLIENT, IDC_ARROW, MSG, WINDOW_STYLE, WM_COMMAND,
        WM_ERASEBKGND, WM_GETOBJECT, WM_LBUTTONUP, WM_NCHITTEST, WM_RBUTTONUP, WNDCLASSW,
        WS_CAPTION, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    },
};

pub const NAME: PCWSTR = w!("Window Switcher");
pub const WM_USER_TRAYICON: u32 = 6000;
pub const WM_USER_REGISTER_TRAYICON: u32 = 6001;
pub const WM_USER_SWITCH_APPS: u32 = 6010;
pub const WM_USER_SWITCH_APPS_DONE: u32 = 6011;
pub const WM_USER_SWITCH_APPS_CANCEL: u32 = 6012;
pub const WM_USER_SWITCH_WINDOWS: u32 = 6020;
pub const WM_USER_SWITCH_WINDOWS_DONE: u32 = 6021;
pub const IDM_EXIT: u32 = 1;
pub const IDM_STARTUP: u32 = 2;
pub const IDM_CONFIGURE: u32 = 3;
pub const WM_POWERBROADCAST: u32 = 0x0218;

pub fn start(config: &Config) -> Result<()> {
    info!("start config={config:?}");
    App::start(config)
}

/// Listen to this message to recreate the tray icon since the taskbar has been recreated.
static WM_TASKBARCREATED: AtomicU32 = AtomicU32::new(0);

pub struct App {
    hwnd: HWND,
    is_admin: bool,
    trayicon: Option<TrayIcon>,
    startup: Startup,
    config: Config,
    switch_windows_state: SwitchWindowsState,
    switch_apps_state: Option<SwitchAppsState>,
    /// Our own most-recently-used order of app keys (front = most recent).
    /// Kept independent of the live Z-order, which our per-hop activations
    /// pollute, so the switcher stays ordered by genuine usage.
    app_mru: Vec<String>,
    /// Most-recently-used order of individual window handles (front = most
    /// recent), stored as isize. Used to pick which window of a multi-window
    /// app to activate, so switching to an app restores the window you last
    /// used rather than the oldest one. Like `app_mru`, it is updated only on
    /// genuine focus captures and commits, never on intermediate hops.
    window_mru: Vec<isize>,
    /// Windows that were minimized and which we restored purely to preview them
    /// while cycling. Any that the user does not settle on are minimized again,
    /// so tabbing past a minimized app no longer un-minimizes it for good.
    restored_windows: Vec<isize>,
    cached_icons: HashMap<String, HICON>,
    /// Describes the overlay to assistive technology as a list of windows.
    uia: ComObject<ListProvider>,
    painter: GdiAAPainter,
    original_foreground_hwnd: Option<HWND>,
    keyboard_listener: Option<KeyboardListener>,
    foreground_watcher: Option<ForegroundWatcher>,
}

impl App {
    pub fn start(config: &Config) -> Result<()> {
        // UI Automation delivers provider events to clients through COM, so a
        // thread that never entered an apartment can raise events all day and
        // every one of them is dropped on the floor - no error, no delivery.
        // Screen readers therefore heard nothing from our accessible list.
        // Single-threaded is the right apartment for the thread that owns the
        // window and pumps its messages.
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if hr.is_err() {
                error!("Failed to initialize COM, {hr:?}");
            }
        }

        let hwnd = Self::create_window()?;
        let painter = GdiAAPainter::new(hwnd)?;

        let foreground_watcher = ForegroundWatcher::init(&config.switch_windows_blacklist)?;
        let keyboard_listener = KeyboardListener::init(hwnd, &config.to_hotkeys())?;

        let trayicon = match config.trayicon {
            true => Some(TrayIcon::create()),
            false => None,
        };

        let is_admin = is_running_as_admin()?;
        debug!("is_admin {is_admin}");

        let startup = Startup::init(is_admin)?;

        let mut app = App {
            hwnd,
            is_admin,
            trayicon,
            startup,
            config: config.clone(),
            switch_windows_state: SwitchWindowsState {
                cache: None,
                modifier_released: true,
            },
            switch_apps_state: None,
            app_mru: Vec::new(),
            window_mru: Vec::new(),
            restored_windows: Vec::new(),
            cached_icons: Default::default(),
            uia: ComObject::new(ListProvider::new(hwnd)),
            painter,
            original_foreground_hwnd: None,
            keyboard_listener: Some(keyboard_listener),
            foreground_watcher: Some(foreground_watcher),
        };

        app.set_trayicon();

        let app_ptr = Box::into_raw(Box::new(app)) as _;
        check_error(|| set_window_user_data(hwnd, app_ptr))
            .map_err(|err| anyhow!("Failed to set window ptr, {err}"))?;

        Self::eventloop()
    }

    fn eventloop() -> Result<()> {
        let mut message = MSG::default();
        loop {
            let ret = unsafe { GetMessageW(&mut message, None, 0, 0) };
            match ret.0 {
                -1 => {
                    unsafe { GetLastError() }.ok()?;
                }
                0 => break,
                _ => unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                },
            }
        }

        Ok(())
    }

    fn create_window() -> Result<HWND> {
        WM_TASKBARCREATED.store(
            unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            Ordering::Relaxed,
        );

        let hinstance = unsafe { GetModuleHandleW(None) }
            .map_err(|err| anyhow!("Failed to get current module handle, {err}"))?;

        let hcursor = unsafe { LoadCursorW(None, IDC_ARROW) }
            .map_err(|err| anyhow!("Failed to load arrow cursor, {err}"))?;

        let window_class = WNDCLASSW {
            hCursor: hcursor,
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: NAME,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(App::window_proc),
            ..Default::default()
        };

        let atom = check_error(|| unsafe { RegisterClassW(&window_class) })
            .map_err(|err| anyhow!("Failed to register class, {err}"))?;

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(atom as _),
                NAME,
                WINDOW_STYLE(0),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .map_err(|err| anyhow!("Failed to create windows, {err}"))?;

        // hide caption
        let mut style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
        style &= !WS_CAPTION.0;
        unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, style as _) };

        Ok(hwnd)
    }

    fn set_trayicon(&mut self) {
        if let Some(trayicon) = self.trayicon.as_mut() {
            match trayicon.register(self.hwnd) {
                Ok(()) => info!("trayicon registered"),
                Err(err) => {
                    if !trayicon.exist() {
                        error!("{err}, retrying in 3 second");
                        let hwnd = self.hwnd.0 as isize;
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            let _ = unsafe {
                                PostMessageW(
                                    Some(HWND(hwnd as _)),
                                    WM_USER_REGISTER_TRAYICON,
                                    WPARAM(0),
                                    LPARAM(0),
                                )
                            };
                        });
                    }
                }
            }
        }
    }

    /// Re-read the config file and re-apply any settings that affect running
    /// behaviour, without restarting. Called after the user edits the config.
    fn reload_config(&mut self) {
        let config = match load_config() {
            Ok(config) => config,
            Err(err) => {
                alert!("{err}");
                return;
            }
        };
        if config == self.config {
            return;
        }
        self.config = config;

        // Reinstall the hooks whose behaviour depends on the config: the
        // hotkeys and the foreground blacklist. Other settings (icon overrides,
        // ignore_minimal, current-desktop) are read from `self.config` on each
        // switch, so replacing it above is enough for them.
        self.keyboard_listener = None;
        match KeyboardListener::init(self.hwnd, &self.config.to_hotkeys()) {
            Ok(listener) => self.keyboard_listener = Some(listener),
            Err(err) => alert!("Failed to apply hotkeys: {err}"),
        }
        self.foreground_watcher = None;
        if let Ok(watcher) = ForegroundWatcher::init(&self.config.switch_windows_blacklist) {
            self.foreground_watcher = Some(watcher);
        }
        info!("config reloaded");
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match Self::handle_message(hwnd, msg, wparam, lparam) {
            Ok(ret) => ret,
            Err(err) => {
                error!("{err}");
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }

    fn handle_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Result<LRESULT> {
        match msg {
            WM_USER_TRAYICON => {
                let app = get_app(hwnd)?;
                if let Some(trayicon) = app.trayicon.as_mut() {
                    let keycode = lparam.0 as u32;
                    if keycode == WM_LBUTTONUP || keycode == WM_RBUTTONUP {
                        trayicon.show(app.startup.is_enable)?;
                    }
                }
                return Ok(LRESULT(0));
            }
            WM_USER_SWITCH_APPS => {
                debug!("message WM_USER_SWITCH_APPS");
                let app = get_app(hwnd)?;
                let reverse = lparam.0 == 1;
                let opening = app.switch_apps_state.is_none();
                app.switch_apps(reverse)?;
                if let Some(state) = &app.switch_apps_state {
                    app.painter.paint(state);
                }
                if opening && app.config.switch_apps_accessible_list {
                    // Now that the overlay is on screen it can take focus, so
                    // the screen reader reads our list instead of the app that
                    // happened to be in front.
                    app.focus_overlay();
                }
            }
            WM_USER_SWITCH_APPS_DONE => {
                debug!("message WM_USER_SWITCH_APPS_DONE");
                let app = get_app(hwnd)?;
                app.do_switch_app();
            }
            WM_USER_SWITCH_APPS_CANCEL => {
                debug!("message WM_USER_SWITCH_APPS_CANCEL");
                let app = get_app(hwnd)?;
                app.cancel_switch_app(true);
            }
            WM_USER_SWITCH_WINDOWS => {
                debug!("message WM_USER_SWITCH_WINDOWS");
                let app = get_app(hwnd)?;
                let reverse = lparam.0 == 1;
                let hwnd = app
                    .switch_apps_state
                    .as_ref()
                    .and_then(|state| state.apps.get(state.index).map(|(_, id)| *id))
                    .unwrap_or_else(get_foreground_window);
                app.switch_windows(hwnd, reverse)?;
                // Keep the window just activated by switch_windows; only close
                // the switch-apps overlay without restoring the original window.
                app.cancel_switch_app(false);
            }
            WM_USER_SWITCH_WINDOWS_DONE => {
                debug!("message WM_USER_SWITCH_WINDOWS_DONE");
                let app = get_app(hwnd)?;
                app.switch_windows_state.modifier_released = true;
            }
            WM_GETOBJECT => {
                // How assistive technology reaches our provider: UIA sends this
                // with UiaRootObjectId and we hand back the list root.
                if lparam.0 as i32 == UiaRootObjectId {
                    let app = get_app(hwnd)?;
                    let provider: IRawElementProviderSimple = app.uia.to_interface();
                    return Ok(unsafe {
                        UiaReturnRawElementProvider(hwnd, wparam, lparam, &provider)
                    });
                }
            }
            WM_NCHITTEST => {
                return Ok(LRESULT(HTCLIENT as _));
            }
            WM_LBUTTONUP => {
                let app = get_app(hwnd)?;
                app.click();
            }
            WM_COMMAND => {
                let value = wparam.0 as u32;
                let kind = ((value >> 16) & 0xffff) as u16;
                let id = value & 0xffff;
                if kind == 0 {
                    match id {
                        IDM_EXIT => {
                            if let Ok(app) = get_app(hwnd) {
                                unsafe { drop(Box::from_raw(app)) }
                            }
                            unsafe { PostQuitMessage(0) }
                        }
                        IDM_STARTUP => {
                            let app = get_app(hwnd)?;
                            app.startup.toggle()?;
                        }
                        IDM_CONFIGURE => match edit_config_file() {
                            Ok(_) => {
                                if let Ok(app) = get_app(hwnd) {
                                    app.reload_config();
                                }
                            }
                            Err(err) => alert!("{err}"),
                        },
                        _ => {}
                    }
                }
            }
            WM_POWERBROADCAST => {
                let event = wparam.0 as u32;
                if event == 0x0012 || event == 0x0007 {
                    // PBT_APMRESUMEAUTOMATIC or PBT_APMRESUMESUSPEND
                    debug!(
                        "System resume from sleep/standby detected. Re-installing keyboard hook..."
                    );
                    if let Ok(app) = get_app(hwnd) {
                        // Drop old listener and watcher first
                        app.keyboard_listener = None;
                        app.foreground_watcher = None;
                        // Re-create new listener and watcher
                        if let Ok(listener) = KeyboardListener::init(hwnd, &app.config.to_hotkeys())
                        {
                            app.keyboard_listener = Some(listener);
                            info!("Keyboard hook re-installed successfully after resume.");
                        }
                        if let Ok(watcher) =
                            ForegroundWatcher::init(&app.config.switch_windows_blacklist)
                        {
                            app.foreground_watcher = Some(watcher);
                            info!("Foreground watcher re-installed successfully after resume.");
                        }
                    }
                }
            }
            WM_ERASEBKGND => {
                return Ok(LRESULT(0));
            }
            _ if msg == WM_USER_REGISTER_TRAYICON
                || msg == WM_TASKBARCREATED.load(Ordering::Relaxed) =>
            {
                let app = get_app(hwnd)?;
                app.set_trayicon();
            }
            _ => {}
        }
        Ok(unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
    }

    fn switch_windows(&mut self, hwnd: HWND, reverse: bool) -> Result<bool> {
        let windows = list_windows(
            self.config.switch_windows_ignore_minimal,
            self.config.switch_windows_only_current_desktop(),
            self.is_admin,
        )?;
        debug!(
            "switch windows: hwnd:{hwnd:?} reverse:{reverse} state:{:?}",
            self.switch_windows_state
        );
        let module_path = match windows
            .iter()
            .find(|(_, v)| v.iter().any(|(id, _)| *id == hwnd))
            .map(|(k, _)| k.clone())
        {
            Some(v) => v,
            None => return Ok(false),
        };
        match windows.get(&module_path) {
            None => Ok(false),
            Some(windows) => {
                let windows_len = windows.len();
                if windows_len == 1 {
                    return Ok(false);
                }
                let current_id = windows[0].0;
                let mut index = 1;
                let mut state_id = current_id;
                let mut state_windows = vec![];
                if windows_len > 2 {
                    if let Some((cache_module_path, cache_id, cache_index, cache_windows)) =
                        self.switch_windows_state.cache.as_ref()
                    {
                        if cache_module_path == &module_path {
                            if self.switch_windows_state.modifier_released {
                                if *cache_id != current_id {
                                    if let Some((i, _)) =
                                        windows.iter().enumerate().find(|(_, (v, _))| v == cache_id)
                                    {
                                        index = i;
                                    }
                                }
                            } else {
                                state_id = *cache_id;
                                let mut windows_set: IndexSet<isize> =
                                    windows.iter().map(|(v, _)| v.0 as _).collect();
                                for id in cache_windows {
                                    if windows_set.contains(id) {
                                        state_windows.push(*id);
                                        windows_set.swap_remove(id);
                                    }
                                }
                                state_windows.extend(windows_set);
                                index = if reverse {
                                    if *cache_index == 0 {
                                        windows_len - 1
                                    } else {
                                        cache_index - 1
                                    }
                                } else if *cache_index >= windows_len - 1 {
                                    0
                                } else {
                                    cache_index + 1
                                };
                            }
                        }
                    }
                }
                if state_windows.is_empty() {
                    state_windows = windows.iter().map(|(v, _)| v.0 as _).collect();
                }
                let hwnd = HWND(state_windows[index] as _);
                self.switch_windows_state = SwitchWindowsState {
                    cache: Some((module_path.clone(), state_id, index, state_windows)),
                    modifier_released: false,
                };
                // Record the window we just activated as the app's most-recent
                // one immediately, so switching away and back restores it without
                // waiting for the next switch-apps session to observe the focus.
                mru::promote(&mut self.window_mru, hwnd.0 as isize);
                set_foreground_window(hwnd);

                Ok(true)
            }
        }
    }

    fn switch_apps(&mut self, reverse: bool) -> Result<()> {
        debug!(
            "switch apps: reverse:{reverse}, state:{:?}",
            self.switch_apps_state
        );

        // Remember where the user started so ESC can take them back.
        if self.switch_apps_state.is_none() {
            self.original_foreground_hwnd = Some(get_foreground_window());
        }

        // Read before borrowing the state mutably below.
        let announce = self.config.switch_apps_announce;
        let accessible_list = self.config.switch_apps_accessible_list;

        if let Some(state) = self.switch_apps_state.as_mut() {
            if reverse {
                if state.index == 0 {
                    state.index = state.apps.len() - 1;
                } else {
                    state.index -= 1;
                }
            } else if state.index == state.apps.len() - 1 {
                state.index = 0;
            } else {
                state.index += 1;
            };
            debug!("switch apps: new index:{}", state.index);

            // Actually activate the candidate window on every hop. The OS then
            // emits genuine focus events, so screen readers (NVDA, Narrator...)
            // announce the switch naturally - no synthetic events needed.
            let target = state.apps.get(state.index).map(|(_, h)| *h);
            let announcement = Self::announcement_for(state, announce);
            let index = state.index;
            // Keep the accessible list in step with the selection so anything
            // inspecting it sees the truth.
            self.uia.set_selected_index(index);
            if accessible_list {
                // The overlay itself holds focus in this mode, so it owns the
                // announcement and the candidate is only activated on commit.
                raise_selection_events(&self.uia);
                self.announce_only(announcement);
            } else if let Some(hwnd) = target {
                // Real focus is about to move to the target window, and that is
                // what the screen reader will read. Claiming focus for our own
                // item as well would put two focus claims in flight and make it
                // read a mixture of the two, so the events stay silent here.
                self.activate_candidate(hwnd, announcement);
            }
            return Ok(());
        }
        let windows = list_windows(
            self.config.switch_apps_ignore_minimal,
            self.config.switch_apps_only_current_desktop(),
            self.is_admin,
        )?;
        if windows.is_empty() {
            return Ok(());
        }

        // Order the apps by our own MRU list rather than the live Z-order.
        // The Z-order is unreliable here because activating each candidate
        // while cycling (needed for screen-reader announcements) permanently
        // reshuffles it. Instead we promote whatever is genuinely focused
        // right now to the front, then fall back to the order we last showed,
        // then append any apps we have never seen.
        let foreground = get_foreground_window();
        let foreground_key = windows
            .iter()
            .find(|(_, hwnds)| hwnds.iter().any(|(id, _)| *id == foreground))
            .map(|(key, _)| key.clone());
        if let Some(key) = foreground_key {
            mru::promote(&mut self.app_mru, key);
        }
        let present_keys: Vec<String> = windows.keys().cloned().collect();

        // Release cached icons for apps that have since closed, so a long-running
        // session does not accumulate icon handles for apps that are long gone.
        self.cached_icons.retain(|key, icon| {
            if present_keys.contains(key) {
                true
            } else {
                unsafe {
                    let _ = DestroyIcon(*icon);
                }
                false
            }
        });

        let ordered = mru::order_by_mru(&self.app_mru, &present_keys);
        // Persist the freshly computed order (also prunes closed apps).
        self.app_mru = ordered.clone();

        // Record the genuinely-focused window as its app's most-recent one, then
        // drop closed windows. Like `app_mru`, `window_mru` is updated only here
        // and on commit, never on intermediate hops, so our own per-hop
        // activations cannot make it forget the window you last used.
        let foreground_id = foreground.0 as isize;
        let all_window_ids: Vec<isize> = windows
            .values()
            .flat_map(|hwnds| hwnds.iter().map(|(h, _)| h.0 as isize))
            .collect();
        mru::retain_present(&mut self.window_mru, &all_window_ids);
        if all_window_ids.contains(&foreground_id) {
            mru::promote(&mut self.window_mru, foreground_id);
        }

        let mut apps = vec![];
        let mut keys = vec![];
        let mut names = vec![];
        for key in ordered {
            let hwnds = &windows[&key];
            let module_hwnd = self.pick_app_window(hwnds);
            let module_hicon = *self.cached_icons.entry(key.clone()).or_insert_with(|| {
                get_app_icon(&self.config.switch_apps_override_icons, &key, module_hwnd)
            });
            let name = hwnds
                .iter()
                .find(|(h, _)| *h == module_hwnd)
                .map(|(_, title)| title.clone())
                .unwrap_or_default();
            apps.push((module_hicon, module_hwnd));
            keys.push(key);
            names.push(name);
        }

        let index = if apps.len() == 1 {
            0
        } else if reverse {
            apps.len() - 1
        } else {
            1
        };

        let state = SwitchAppsState {
            apps,
            keys,
            names,
            index,
        };

        // Describe the whole list to assistive technology up front, so it can be
        // explored (and, in accessible-list mode, announced) as a real list.
        self.uia.set_selection(Selection {
            names: state.names.clone(),
            rects: self.painter.item_rects(state.apps.len()),
            selected: state.index,
        });

        let target = state.apps.get(state.index).map(|(_, h)| *h);
        let announcement = Self::announcement_for(&state, announce);
        self.switch_apps_state = Some(state);

        if accessible_list {
            // Real Alt-Tab architecture: the overlay takes focus and the
            // candidates are left alone until the user commits. Nothing is
            // activated, restored or reordered while merely looking around.
            // Focus is taken by the caller once the overlay is actually on
            // screen, since a hidden window cannot become foreground.
            self.announce_only(announcement);
        } else if let Some(hwnd) = target {
            // Actually activate the first candidate so screen readers announce
            // it naturally through the genuine focus change. No UI Automation
            // focus event here: the real one is authoritative.
            self.activate_candidate(hwnd, announcement);
        }
        debug!("switch apps, new state:{:?}", self.switch_apps_state);
        Ok(())
    }

    fn click(&mut self) {
        if let Some(state) = self.switch_apps_state.as_mut() {
            if let Some(i) = self.painter.find_clicked_app_index(state) {
                state.index = i;
                self.do_switch_app();
            }
        }
    }

    /// Build the text a screen reader should speak for the current selection,
    /// or `None` when announcements are turned off.
    fn announcement_for(state: &SwitchAppsState, enabled: bool) -> Option<String> {
        if !enabled {
            return None;
        }
        let name = state.names.get(state.index)?;
        Some(format!(
            "{name}, {} of {}",
            state.index + 1,
            state.apps.len()
        ))
    }

    /// Activate a window while cycling, remembering it if we had to un-minimize
    /// it so it can be put back should the user move on, and optionally telling
    /// the screen reader where the selection now is.
    fn activate_candidate(&mut self, hwnd: HWND, announcement: Option<String>) {
        if set_foreground_window(hwnd) {
            let id = hwnd.0 as isize;
            if !self.restored_windows.contains(&id) {
                self.restored_windows.push(id);
            }
        }
        if let Some(text) = announcement {
            announce(self.hwnd, &text);
        }
    }

    /// Give the overlay real keyboard focus and point assistive technology at
    /// the selected item. Only used in accessible-list mode.
    fn focus_overlay(&mut self) {
        set_foreground_window(self.hwnd);
        raise_selection_events(&self.uia);
    }

    /// Speak `announcement` without touching any window. Used in
    /// accessible-list mode, where nothing is activated while cycling.
    fn announce_only(&self, announcement: Option<String>) {
        if let Some(text) = announcement {
            notify(&self.uia, &text);
        }
    }

    /// Re-minimize every window we restored just to preview it, except `keep`.
    fn undo_previews(&mut self, keep: Option<isize>) {
        for id in std::mem::take(&mut self.restored_windows) {
            if Some(id) != keep {
                minimize_window(HWND(id as _));
            }
        }
    }

    /// Pick which window of an app to activate: the one the user most recently
    /// used (per `window_mru`), falling back to the top of the current Z-order.
    /// The fallback prefers a non-minimized window over the app's oldest one.
    fn pick_app_window(&self, hwnds: &[(HWND, String)]) -> HWND {
        let ids: Vec<isize> = hwnds.iter().map(|(h, _)| h.0 as isize).collect();
        if let Some(i) = mru::most_recent_index(&self.window_mru, &ids) {
            return hwnds[i].0;
        }
        if is_iconic_window(hwnds[0].0) {
            if let Some((h, _)) = hwnds.iter().find(|(h, _)| !is_iconic_window(*h)) {
                return *h;
            }
        }
        hwnds[0].0
    }

    fn do_switch_app(&mut self) {
        if let Some(state) = self.switch_apps_state.take() {
            self.original_foreground_hwnd = None;
            // The list is gone; stop advertising stale items.
            self.uia.clear();
            // The committed app is now the most-recently-used one.
            if let Some(key) = state.keys.get(state.index) {
                mru::promote(&mut self.app_mru, key.clone());
            }
            let chosen = state.apps.get(state.index).map(|(_, id)| *id);
            // Put back every window we un-minimized just to preview it, keeping
            // only the one the user settled on.
            self.undo_previews(chosen.map(|h| h.0 as isize));
            if let Some(id) = chosen {
                // Remember this as the app's most-recently-used window so a later
                // switch back to it restores this window, not the oldest one.
                mru::promote(&mut self.window_mru, id.0 as isize);
                set_foreground_window(id);
            }
            self.painter.unpaint(state);
        }
    }

    fn cancel_switch_app(&mut self, restore_original: bool) {
        if let Some(state) = self.switch_apps_state.take() {
            // On explicit cancel (ESC), take the user back where they started;
            // the resulting genuine focus event makes screen readers announce it.
            // Nothing was chosen, so every previewed window goes back to the
            // taskbar - except the one we are returning to.
            self.uia.clear();
            let orig = self.original_foreground_hwnd.take();
            // Keep whichever window the user ends up on: the original when they
            // cancelled, or the one switch-windows just activated when it took
            // over the session.
            let keep = if restore_original {
                orig
            } else {
                Some(get_foreground_window())
            };
            self.undo_previews(keep.map(|h| h.0 as isize));
            if restore_original {
                if let Some(orig) = orig {
                    set_foreground_window(orig);
                }
            }
            self.painter.unpaint(state);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        for (_, icon) in self.cached_icons.drain() {
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }
}

fn get_app(hwnd: HWND) -> Result<&'static mut App> {
    unsafe {
        let ptr = check_error(|| get_window_user_data(hwnd))
            .map_err(|err| anyhow!("Failed to get window ptr, {err}"))?;
        let tx: &mut App = &mut *(ptr as *mut App);
        Ok(tx)
    }
}

#[derive(Debug)]
struct SwitchWindowsState {
    cache: Option<(String, HWND, usize, Vec<isize>)>,
    modifier_released: bool,
}

#[derive(Debug)]
pub struct SwitchAppsState {
    pub apps: Vec<(HICON, HWND)>,
    /// App key parallel to `apps`, used to maintain the MRU order.
    pub keys: Vec<String>,
    /// Window title parallel to `apps`, spoken when announcements are enabled.
    pub names: Vec<String>,
    pub index: usize,
}
