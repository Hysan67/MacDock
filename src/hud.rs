//! macOS-style translucent HUD overlay for Volume and Brightness keys.
//!
//! Runs on its own thread with a click-through, non-activating layered window.
//! Volume updates arrive from `winhook` (on volume key presses); brightness changes are detected
//! by lightly polling the fast `\\.\LCD` ioctl (no WMI overhead; disabled on desktop monitors).

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, PixmapPaint, Rect, Stroke, Transform};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{self, Surface};
use crate::sysctl;

const WM_HUD_VOL: u32 = WM_APP + 40;
const WM_HUD_BRIGHT: u32 = WM_APP + 41;

const TIMER_HOLD: usize = 1;
const TIMER_FADE: usize = 2;
const TIMER_VOL_READ: usize = 3;

const BOX_SIZE: f32 = 172.0;
const MARGIN: f32 = 24.0;
const RADIUS: f32 = 22.0;
const SEGMENTS: usize = 16;
const HOLD_MS: u32 = 1150;
const FADE_SECS: f32 = 0.24;

static HWND_HUD: AtomicIsize = AtomicIsize::new(0);
static ENABLED: AtomicBool = AtomicBool::new(true);
static PAUSED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Volume { level: f32, muted: bool },
    Brightness { level: f32 },
}

struct Hud {
    hwnd: HWND,
    surface: Option<Surface>,
    pos: (i32, i32),
    size: (u32, u32),
    s: f32,
    shown: bool,
    fading: Option<Instant>,
    kind: Option<Kind>,
}

thread_local! {
    static STATE: std::cell::RefCell<Option<Hud>> = const { std::cell::RefCell::new(None) };
}

fn with(f: impl FnOnce(&mut Hud)) {
    STATE.with(|st| {
        if let Some(h) = st.borrow_mut().as_mut() {
            f(h);
        }
    });
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
}

/// Called from the keyboard hook when Volume Up / Down / Mute is pressed.
pub fn notify_volume() {
    if !ENABLED.load(Ordering::Relaxed) || PAUSED.load(Ordering::Relaxed) {
        return;
    }
    let h = HWND_HUD.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_HUD_VOL, WPARAM(0), LPARAM(0));
        }
    }
}

fn paint(rgb: (u8, u8, u8), a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(Color::from_rgba8(rgb.2, rgb.1, rgb.0, a)); // BGRA surface
    p.anti_alias = true;
    p
}

impl Hud {
    fn place(&mut self) {
        let (sw, sh, dpi) = crate::dock::primary_screen();
        let s = dpi as f32;
        self.s = s;
        let total = ((BOX_SIZE + 2.0 * MARGIN) * s).ceil() as u32;
        self.size = (total, total);
        let cx = ((sw as f32 * s - total as f32) / 2.0).round() as i32;
        let cy = ((sh as f32 * s * 0.72) - total as f32 / 2.0).round().max(40.0) as i32;
        self.pos = (cx, cy);
    }

    fn show(&mut self, kind: Kind) {
        self.kind = Some(kind);
        self.fading = None;
        self.place();
        self.render(255);
        unsafe {
            if !self.shown {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                self.shown = true;
            }
            let _ = KillTimer(self.hwnd, TIMER_FADE);
            SetTimer(self.hwnd, TIMER_HOLD, HOLD_MS, None);
        }
    }

    fn hide(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_HOLD);
            let _ = KillTimer(self.hwnd, TIMER_FADE);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.shown = false;
        self.fading = None;
    }

    fn render(&mut self, alpha: u8) {
        let Some(kind) = self.kind else { return };
        let (w, h) = self.size;
        if self.surface.as_ref().is_none_or(|sf| sf.w != w || sf.h != h) {
            self.surface = Surface::new(w.max(1), h.max(1));
        }
        let Some(sf) = self.surface.as_mut() else { return };
        let mut pm = sf.pixmap();
        pm.fill(Color::TRANSPARENT);

        let s = self.s;
        let m = MARGIN * s;
        let bw = BOX_SIZE * s;
        let r = RADIUS * s;

        // Soft drop shadow
        if let Some((sh, sx, sy)) = gfx::rrect_shadow(m, m + 4.0 * s, bw, bw, r, 16.0 * s, (0, 0, 0, 0x55)) {
            pm.draw_pixmap(sx.round() as i32, sy.round() as i32, sh.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }

        // Frosted dark glass body
        if let Some(bg) = gfx::rrect(m, m, bw, bw, r) {
            pm.fill_path(&bg, &paint((0x1E, 0x1E, 0x22), 0xD2), FillRule::Winding, Transform::identity(), None);
            let hair = Stroke { width: 1.0, ..Default::default() };
            pm.stroke_path(&bg, &paint((255, 255, 255), 0x30), &hair, Transform::identity(), None);
        }

        // Icon centered in the upper ~68% of the box
        let cx = m + bw / 2.0;
        let cy = m + bw * 0.42;
        match kind {
            Kind::Volume { level, muted } => draw_speaker(&mut pm, cx, cy, s, level, muted),
            Kind::Brightness { .. } => draw_sun(&mut pm, cx, cy, s),
        }

        // 16-segment macOS level bar at the bottom
        let level = match kind {
            Kind::Volume { level, muted } => if muted { 0.0 } else { level },
            Kind::Brightness { level } => level,
        };
        let filled = (level.clamp(0.0, 1.0) * SEGMENTS as f32).round() as usize;
        let bar_w = bw - 34.0 * s;
        let bar_h = 7.0 * s;
        let bar_x = m + 17.0 * s;
        let bar_y = m + bw - 26.0 * s;
        if let Some(track) = gfx::rrect(bar_x - 2.0 * s, bar_y - 2.0 * s, bar_w + 4.0 * s, bar_h + 4.0 * s, 3.0 * s) {
            pm.fill_path(&track, &paint((0, 0, 0), 0x50), FillRule::Winding, Transform::identity(), None);
        }
        let gap = 2.0 * s;
        let seg_w = (bar_w - gap * (SEGMENTS as f32 - 1.0)) / SEGMENTS as f32;
        for i in 0..SEGMENTS {
            let sx = bar_x + i as f32 * (seg_w + gap);
            let col = if i < filled { paint((255, 255, 255), 0xF0) } else { paint((255, 255, 255), 0x22) };
            if let Some(rect) = Rect::from_xywh(sx, bar_y, seg_w.max(1.0), bar_h) {
                pm.fill_rect(rect, &col, Transform::identity(), None);
            }
        }

        sf.present_alpha(self.hwnd, self.pos.0, self.pos.1, alpha);
    }
}

fn draw_sun(pm: &mut tiny_skia::PixmapMut, cx: f32, cy: f32, s: f32) {
    let col = paint((255, 255, 255), 0xEC);
    if let Some(c) = PathBuilder::from_circle(cx, cy, 13.0 * s) {
        pm.fill_path(&c, &col, FillRule::Winding, Transform::identity(), None);
    }
    let st = Stroke { width: 4.5 * s, line_cap: LineCap::Round, ..Default::default() };
    let (r0, r1) = (21.0 * s, 30.0 * s);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (dx, dy) = (a.cos(), a.sin());
        let mut pb = PathBuilder::new();
        pb.move_to(cx + dx * r0, cy + dy * r0);
        pb.line_to(cx + dx * r1, cy + dy * r1);
        if let Some(p) = pb.finish() {
            pm.stroke_path(&p, &col, &st, Transform::identity(), None);
        }
    }
}

fn draw_speaker(pm: &mut tiny_skia::PixmapMut, cx: f32, cy: f32, s: f32, level: f32, muted: bool) {
    let col = paint((255, 255, 255), 0xEC);
    let ox = cx - 8.0 * s;
    let mut pb = PathBuilder::new();
    pb.move_to(ox - 18.0 * s, cy - 9.0 * s);
    pb.line_to(ox - 7.0 * s, cy - 9.0 * s);
    pb.line_to(ox + 7.0 * s, cy - 22.0 * s);
    pb.line_to(ox + 7.0 * s, cy + 22.0 * s);
    pb.line_to(ox - 7.0 * s, cy + 9.0 * s);
    pb.line_to(ox - 18.0 * s, cy + 9.0 * s);
    pb.close();
    if let Some(p) = pb.finish() {
        pm.fill_path(&p, &col, FillRule::Winding, Transform::identity(), None);
    }

    if muted || level <= 0.001 {
        let st = Stroke { width: 4.2 * s, line_cap: LineCap::Round, ..Default::default() };
        let mut slash = PathBuilder::new();
        slash.move_to(cx - 24.0 * s, cy - 24.0 * s);
        slash.line_to(cx + 24.0 * s, cy + 24.0 * s);
        if let Some(p) = slash.finish() {
            pm.stroke_path(&p, &paint((255, 255, 255), 0xF0), &st, Transform::identity(), None);
        }
        return;
    }

    let waves = if level > 0.66 { 3 } else if level > 0.33 { 2 } else { 1 };
    let st = Stroke { width: 3.8 * s, line_cap: LineCap::Round, ..Default::default() };
    for i in 1..=3 {
        let r = 10.0 * s + i as f32 * 8.5 * s;
        let c = if i <= waves { paint((255, 255, 255), 0xEC) } else { paint((255, 255, 255), 0x38) };
        let mut arc = PathBuilder::new();
        let steps = 16;
        let a0 = -0.68f32;
        let a1 = 0.68f32;
        for j in 0..=steps {
            let a = a0 + (a1 - a0) * j as f32 / steps as f32;
            let (px, py) = (ox + 8.0 * s + r * a.cos(), cy + r * a.sin());
            if j == 0 {
                arc.move_to(px, py);
            } else {
                arc.line_to(px, py);
            }
        }
        if let Some(p) = arc.finish() {
            pm.stroke_path(&p, &c, &st, Transform::identity(), None);
        }
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HUD_VOL => {
            // Let Windows apply the volume key step first (~30 ms), then read the new volume.
            SetTimer(h, TIMER_VOL_READ, 30, None);
            return LRESULT(0);
        }
        WM_HUD_BRIGHT => {
            let pct = (wp.0 as u8).min(100) as f32 / 100.0;
            with(|hud| hud.show(Kind::Brightness { level: pct }));
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_VOL_READ => {
            let _ = KillTimer(h, TIMER_VOL_READ);
            if let Some((level, muted)) = sysctl::volume_state() {
                with(|hud| hud.show(Kind::Volume { level, muted }));
            }
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_HOLD => {
            let _ = KillTimer(h, TIMER_HOLD);
            with(|hud| hud.fading = Some(Instant::now()));
            SetTimer(h, TIMER_FADE, 16, None);
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_FADE => {
            with(|hud| {
                let Some(t0) = hud.fading else { return };
                let p = (t0.elapsed().as_secs_f32() / FADE_SECS).clamp(0.0, 1.0);
                if p >= 1.0 {
                    hud.hide();
                } else {
                    let a = ((1.0 - p) * 255.0).round() as u8;
                    gfx::set_alpha(hud.hwnd, a);
                }
            });
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        _ => {}
    }
    DefWindowProcW(h, msg, wp, lp)
}

pub fn start(enabled: bool) {
    set_enabled(enabled);
    std::thread::spawn(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("MacDockHud"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
            w!("MacDockHud"),
            w!("MacDock HUD"),
            WS_POPUP,
            0, 0, 1, 1,
            HWND::default(), HMENU::default(), hinst, None,
        ) else {
            return;
        };
        HWND_HUD.store(hwnd.0 as isize, Ordering::Relaxed);
        STATE.with(|st| {
            *st.borrow_mut() = Some(Hud {
                hwnd, surface: None, pos: (0, 0), size: (1, 1), s: 1.0, shown: false, fading: None, kind: None,
            });
        });

        // Background brightness watcher using fast \\.\LCD ioctl (exits immediately on desktop monitors).
        std::thread::spawn(move || {
            let Some(mut last) = sysctl::brightness_fast() else { return };
            loop {
                std::thread::sleep(Duration::from_millis(160));
                if !ENABLED.load(Ordering::Relaxed) || PAUSED.load(Ordering::Relaxed) {
                    if let Some(cur) = sysctl::brightness_fast() {
                        last = cur;
                    }
                    continue;
                }
                if let Some(cur) = sysctl::brightness_fast() {
                    if cur != last {
                        last = cur;
                        let target = HWND_HUD.load(Ordering::Relaxed);
                        if target != 0 {
                            let _ = PostMessageW(HWND(target as *mut _), WM_HUD_BRIGHT, WPARAM(cur as usize), LPARAM(0));
                        }
                    }
                }
            }
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}
