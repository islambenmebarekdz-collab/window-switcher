use crate::utils::get_window_exe;
use anyhow::{bail, Result};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
        WindowsAndMessaging::{
            EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
        },
    },
};

pub static IS_FOREGROUND_IN_BLACKLIST: AtomicBool = AtomicBool::new(false);

/// The lowercased executable blacklist. Held behind an RwLock (not a
/// write-once cell) so reloading the config can replace it at runtime.
static BLACKLIST: LazyLock<RwLock<HashSet<String>>> = LazyLock::new(|| RwLock::new(HashSet::new()));

#[derive(Debug)]
pub struct ForegroundWatcher {
    hook: HWINEVENTHOOK,
}

impl ForegroundWatcher {
    pub fn init(blacklist: &HashSet<String>) -> Result<Self> {
        // Always refresh the shared blacklist and reset the cached flag so a
        // reload that changes or clears the blacklist takes effect immediately.
        *BLACKLIST.write() = blacklist.iter().map(|v| v.to_lowercase()).collect();
        IS_FOREGROUND_IN_BLACKLIST.store(false, Ordering::Relaxed);

        if blacklist.is_empty() {
            return Ok(Self {
                hook: HWINEVENTHOOK::default(),
            });
        }

        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.is_invalid() {
            bail!("Failed to watch foreground");
        }

        info!("foreground watcher start");

        Ok(Self { hook })
    }
}

impl Drop for ForegroundWatcher {
    fn drop(&mut self) {
        debug!("foreground watcher destroyed");
        if !self.hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.hook);
            }
        }
    }
}

unsafe extern "system" fn win_event_proc(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _dw_event_thread: u32,
    _dwms_event_time: u32,
) {
    let exe = match get_window_exe(hwnd) {
        Some(v) => v.to_lowercase(),
        None => return,
    };
    let is_in_blacklist = BLACKLIST.read().contains(&exe);
    IS_FOREGROUND_IN_BLACKLIST.store(is_in_blacklist, Ordering::Relaxed);
    debug!("foreground {exe} {is_in_blacklist}");
}
