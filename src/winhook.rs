//! Low-level keyboard hook for Windows key overrides:
//! - Single tap on Left/Right Win key -> opens MacDock Apps / Launchpad (suppressing Windows Start Menu).
//! - Win+S or Win+Q (without Ctrl/Alt/Shift, so Win+Shift+S Snipping Tool stays intact) -> opens Spotlight.
//! - Volume Up / Down / Mute keys -> notifies the macOS-style HUD while letting Windows adjust volume normally.
//!
//! Runs on its own dedicated thread so it always answers within Windows' low-level hook timeout.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the main dock window when the Win key is tapped alone.
pub const WM_WIN_APPS: u32 = WM_APP + 30;
/// Posted to the main dock window when Win+S or Win+Q is pressed.
pub const WM_WIN_SPOTLIGHT: u32 = WM_APP + 31;

static TARGET: AtomicIsize = AtomicIsize::new(0);
static WIN_APPS: AtomicBool = AtomicBool::new(true);
static WIN_SPOTLIGHT: AtomicBool = AtomicBool::new(true);
static MEDIA_HUD: AtomicBool = AtomicBool::new(true);
static PAUSED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static WIN_DOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static WIN_COMBO: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn set_options(win_apps: bool, win_spotlight: bool, media_hud: bool) {
    WIN_APPS.store(win_apps, Ordering::Relaxed);
    WIN_SPOTLIGHT.store(win_spotlight, Ordering::Relaxed);
    MEDIA_HUD.store(media_hud, Ordering::Relaxed);
}

pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
}

fn down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(vk.0 as i32) as u16) & 0x8000 != 0 }
}

/// Inject an unassigned virtual key (0xE8) so Explorer sees a key between Win-down and Win-up
/// and does not pop open the Windows Start Menu when the Win key is released.
fn suppress_start_menu() {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), dwFlags: KEYBD_EVENT_FLAGS(0), ..Default::default() },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), dwFlags: KEYEVENTF_KEYUP, ..Default::default() },
            },
        },
    ];
    unsafe {
        let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

unsafe extern "system" fn kb_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wp, lp);
    }
    let kb = &*(lp.0 as *const KBDLLHOOKSTRUCT);
    if kb.flags.0 & LLKHF_INJECTED.0 != 0 {
        return CallNextHookEx(None, code, wp, lp);
    }
    let msg = wp.0 as u32;
    let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    let vk = kb.vkCode;

    // Volume keys: let Windows handle the actual volume change, and trigger our macOS HUD.
    if is_down
        && MEDIA_HUD.load(Ordering::Relaxed)
        && matches!(vk, x if x == VK_VOLUME_UP.0 as u32 || x == VK_VOLUME_DOWN.0 as u32 || x == VK_VOLUME_MUTE.0 as u32)
    {
        crate::hud::notify_volume();
        return CallNextHookEx(None, code, wp, lp);
    }

    if PAUSED.load(Ordering::Relaxed) {
        WIN_DOWN.set(false);
        WIN_COMBO.set(false);
        return CallNextHookEx(None, code, wp, lp);
    }

    let is_win = vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32;
    if is_win {
        if is_down {
            if !WIN_DOWN.get() {
                WIN_DOWN.set(true);
                WIN_COMBO.set(false);
            }
        } else if is_up && WIN_DOWN.get() {
            let combo = WIN_COMBO.get();
            WIN_DOWN.set(false);
            WIN_COMBO.set(false);
            if !combo && WIN_APPS.load(Ordering::Relaxed) {
                suppress_start_menu();
                let h = TARGET.load(Ordering::Relaxed);
                if h != 0 {
                    let _ = PostMessageW(HWND(h as *mut _), WM_WIN_APPS, WPARAM(0), LPARAM(0));
                }
            }
        }
        return CallNextHookEx(None, code, wp, lp);
    }

    if WIN_DOWN.get() && is_down {
        WIN_COMBO.set(true);
        // Win+S or Win+Q (with no Shift/Ctrl/Alt) -> Spotlight instead of Windows Search.
        if WIN_SPOTLIGHT.load(Ordering::Relaxed)
            && (vk == b'S' as u32 || vk == b'Q' as u32)
            && !down(VK_SHIFT)
            && !down(VK_CONTROL)
            && !down(VK_MENU)
        {
            suppress_start_menu();
            let h = TARGET.load(Ordering::Relaxed);
            if h != 0 {
                let _ = PostMessageW(HWND(h as *mut _), WM_WIN_SPOTLIGHT, WPARAM(0), LPARAM(0));
            }
            return LRESULT(1);
        }
    }

    CallNextHookEx(None, code, wp, lp)
}

pub fn start(target: HWND, win_apps: bool, win_spotlight: bool, media_hud: bool) {
    TARGET.store(target.0 as isize, Ordering::Relaxed);
    set_options(win_apps, win_spotlight, media_hud);
    std::thread::spawn(|| unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), hinst, 0) else { return };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    });
}
