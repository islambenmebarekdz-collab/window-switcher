use crate::{
    app::{
        WM_USER_SWITCH_APPS, WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE,
        WM_USER_SWITCH_WINDOWS, WM_USER_SWITCH_WINDOWS_DONE,
    },
    config::{Hotkey, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID},
    foreground::IS_FOREGROUND_IN_BLACKLIST,
};

use anyhow::{anyhow, Result};
use indexmap::IndexSet;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::LazyLock;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{SCANCODE_LSHIFT, SCANCODE_RSHIFT},
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, PostMessageW, PostThreadMessageW,
            SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, LLKHF_UP,
            MSG, WH_KEYBOARD_LL, WM_QUIT,
        },
    },
};

static KEYBOARD_STATE: LazyLock<Mutex<Vec<HotKeyState>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// Target window handle (as isize) that hotkey messages are posted to.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static IS_SHIFT_PRESSED: AtomicBool = AtomicBool::new(false);
static IS_SWITCHING_APPS: AtomicBool = AtomicBool::new(false);
static PREVIOUS_KEYCODE: AtomicU32 = AtomicU32::new(0);
/// Owns the low-level keyboard hook, which lives on a thread of its own.
///
/// Windows delivers `WH_KEYBOARD_LL` callbacks through the message queue of the
/// thread that installed the hook, and drops the hook entirely if a callback
/// cannot complete within `LowLevelHooksTimeout` (300 ms by default). Sharing a
/// thread with the UI meant that any slow message - enumerating windows,
/// loading icons, activating a window, waiting on a screen reader - stopped the
/// queue from pumping, so the callback could not even be delivered and the hook
/// was silently removed. Every hotkey then stayed dead until the app restarted.
///
/// This thread does nothing but pump, so no amount of work elsewhere can starve
/// it; the hook only ever records state and posts a message.
#[derive(Debug)]
pub struct KeyboardListener {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KeyboardListener {
    pub fn init(hwnd: HWND, hotkeys: &[&Hotkey]) -> Result<Self> {
        WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);

        let keyboard_state = hotkeys
            .iter()
            .map(|hotkey| HotKeyState {
                hotkey: (*hotkey).clone(),
                is_modifier_pressed: false,
            })
            .collect();
        *KEYBOARD_STATE.lock() = keyboard_state;

        let (tx, rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let thread = std::thread::spawn(move || {
            // The hook handle is not Send, so it is created, used and released
            // entirely inside this thread.
            let hook = unsafe {
                match GetModuleHandleW(None) {
                    Ok(hinstance) => SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(keyboard_proc),
                        Some(hinstance.into()),
                        0,
                    ),
                    Err(err) => Err(err),
                }
            };
            let hook = match hook {
                Ok(hook) => {
                    let _ = tx.send(Ok(unsafe { GetCurrentThreadId() }));
                    hook
                }
                Err(err) => {
                    let _ = tx.send(Err(format!("Failed to set windows hook, {err}")));
                    return;
                }
            };

            let mut message = MSG::default();
            // Pump until Drop posts WM_QUIT. Nothing else runs here.
            while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }

            let _ = unsafe { UnhookWindowsHookEx(hook) };
        });

        let thread_id = rx
            .recv()
            .map_err(|_| anyhow!("Keyboard hook thread stopped before reporting"))?
            .map_err(|err| anyhow!(err))?;

        info!("keyboard listener start");
        Ok(Self {
            thread_id,
            thread: Some(thread),
        })
    }
}

impl Drop for KeyboardListener {
    fn drop(&mut self) {
        debug!("keyboard listener destroyed");
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug)]
struct HotKeyState {
    hotkey: Hotkey,
    is_modifier_pressed: bool,
}

/// Hand a hotkey action to the app window.
///
/// This must never block. A `WH_KEYBOARD_LL` hook that fails to return within
/// `LowLevelHooksTimeout` (300 ms by default, and the value is usually absent
/// from the registry) is silently removed by Windows, which kills every hotkey
/// until the app is restarted. Sending synchronously made that a real risk once
/// handling a hop grew to enumerating windows, loading icons, activating the
/// target window and notifying assistive technology - any of which can stall.
/// Posting keeps the hook instant, and since the reply was always discarded
/// nothing is lost; the queue still delivers the messages in order.
unsafe fn post_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    let _ = PostMessageW(Some(hwnd), msg, wparam, lparam);
}

unsafe extern "system" fn keyboard_proc(code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    let kbd_data: &KBDLLHOOKSTRUCT = &*(l_param.0 as *const _);
    // Deliberately no logging here. This runs for every keystroke on the
    // system, and writing a line to disk per key is slow enough to overrun
    // LowLevelHooksTimeout, at which point Windows removes the hook and every
    // hotkey stops working. Turning on debug logging used to kill the switcher
    // outright for exactly this reason.
    let mut is_modifier = false;
    let scan_code = kbd_data.scanCode;
    let is_key_pressed = || kbd_data.flags.0 & LLKHF_UP.0 == 0;
    if [SCANCODE_LSHIFT, SCANCODE_RSHIFT].contains(&scan_code) {
        IS_SHIFT_PRESSED.store(is_key_pressed(), Ordering::Relaxed);
    }
    let mut keyboard_state = KEYBOARD_STATE.lock();
    let mut send_done_hotkeys: IndexSet<u32> = IndexSet::new();
    let mut send_action_message: Option<(u32, isize, bool)> = None;

    for state in keyboard_state.iter_mut() {
        if state.hotkey.modifier.contains(&scan_code) {
            is_modifier = true;
            if is_key_pressed() {
                state.is_modifier_pressed = true;
            } else {
                state.is_modifier_pressed = false;
                if PREVIOUS_KEYCODE.load(Ordering::Relaxed) == state.hotkey.code {
                    send_done_hotkeys.insert(state.hotkey.id);
                }
            }
        }
    }
    if !is_modifier {
        for state in keyboard_state.iter_mut() {
            if is_key_pressed() && state.is_modifier_pressed {
                let id = state.hotkey.id;
                if scan_code == state.hotkey.code {
                    let reverse = if IS_SHIFT_PRESSED.load(Ordering::Relaxed) {
                        1
                    } else {
                        0
                    };
                    if id == SWITCH_APPS_HOTKEY_ID
                        || (id == SWITCH_WINDOWS_HOTKEY_ID
                            && !IS_FOREGROUND_IN_BLACKLIST.load(Ordering::Relaxed))
                    {
                        send_action_message = Some((id, reverse, false));
                        PREVIOUS_KEYCODE.store(scan_code, Ordering::Relaxed);
                        break;
                    };
                } else if id == SWITCH_APPS_HOTKEY_ID {
                    if scan_code == 0x01 {
                        // escape key
                        send_action_message = Some((id, 0, true));
                        PREVIOUS_KEYCODE.store(scan_code, Ordering::Relaxed);
                        break;
                    } else if [0x48, 0x4b, 0x4d, 0x50].contains(&scan_code)
                        && IS_SWITCHING_APPS.load(Ordering::Relaxed)
                    {
                        // arrow keys
                        let reverse = if scan_code == 0x48 || scan_code == 0x4b {
                            1
                        } else {
                            0
                        };
                        send_action_message = Some((id, reverse, false));
                        break;
                    }
                }
            }
        }
    }
    drop(keyboard_state);

    let window = HWND(WINDOW.load(Ordering::Relaxed) as _);

    for id in send_done_hotkeys {
        if id == SWITCH_APPS_HOTKEY_ID {
            post_message(window, WM_USER_SWITCH_APPS_DONE, WPARAM(0), LPARAM(0));
            IS_SWITCHING_APPS.store(false, Ordering::Relaxed);
        } else if id == SWITCH_WINDOWS_HOTKEY_ID {
            post_message(window, WM_USER_SWITCH_WINDOWS_DONE, WPARAM(0), LPARAM(0));
        }
    }

    if let Some((id, reverse, is_cancel)) = send_action_message {
        if id == SWITCH_APPS_HOTKEY_ID {
            if is_cancel {
                post_message(window, WM_USER_SWITCH_APPS_CANCEL, WPARAM(0), LPARAM(0));
                IS_SWITCHING_APPS.store(false, Ordering::Relaxed);
            } else {
                post_message(window, WM_USER_SWITCH_APPS, WPARAM(0), LPARAM(reverse));
                IS_SWITCHING_APPS.store(true, Ordering::Relaxed);
            }
            return LRESULT(1);
        } else if id == SWITCH_WINDOWS_HOTKEY_ID {
            post_message(window, WM_USER_SWITCH_WINDOWS, WPARAM(0), LPARAM(reverse));
            IS_SWITCHING_APPS.store(false, Ordering::Relaxed);
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, w_param, l_param)
}
