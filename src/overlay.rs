// stt-input - push-to-talk console dictation
// Copyright (C) 2026 清木殇 (sglwsjxh)
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
// 语音波形条：录音时置顶深灰底白色对称波形动画，纯 Win32 + GDI，零新增依赖

use std::cell::{Cell, OnceCell};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;

use color_eyre::eyre::{Result, eyre};
use windows_sys::Win32::Foundation::{COLORREF, GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, BeginPaint, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, GetDC, HBRUSH, HDC, PAINTSTRUCT, ReleaseDC, SRCCOPY,
    SelectObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
    GetSystemMetrics, KillTimer, LWA_ALPHA, MSG, PostQuitMessage, RegisterClassW,
    SetLayeredWindowAttributes, SetTimer, ShowWindow, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE,
    SW_SHOWNOACTIVATE, TranslateMessage, WM_DESTROY, WM_PAINT, WM_TIMER, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WNDCLASSW,
};

// Cargo.toml 未启用 Win32_System_LibraryLoader feature，直链 kernel32 取实例句柄
unsafe extern "system" {
    fn GetModuleHandleW(lpmodulename: *const u16) -> *mut c_void;
}

const WIDTH: i32 = 320;
const HEIGHT: i32 = 14;
const PAD: i32 = 10;
const ALPHA: u8 = 200;
const TICK_MS: u32 = 30;
const TIMER_ID: usize = 1;
const CLASS_NAME: &str = "SttInputOverlay";

// 波形视觉参数
const BAR_W: i32 = 5; // 竖条宽
const GAP: i32 = 2; // 竖条间隙
const DOT_H: i32 = 2; // 静音小点高
const MAX_H: i32 = 12; // 有声条最大可用高，留 1px 上下居中余量
const CENTER_MIN: i32 = 5; // 有声时至少活跃的中间根数
const SPREAD_MIN: i32 = 6; // 包络宽度下限，防止窄窗下波形塌成一根
const BG_COLOR: COLORREF = 0x0030_2D2D; // RGB(45,45,48) 深灰底
const FG_COLOR: COLORREF = 0x00F0_F0F0; // RGB(240,240,240) 波形白

/// 电平与录音状态经全局单例传给 wndproc：wndproc 签名带不了用户数据
static SHARED: OnceLock<(Arc<AtomicU8>, Arc<AtomicBool>)> = OnceLock::new();

thread_local! {
    static SHOWN: Cell<bool> = const { Cell::new(false) };
    static HIDE_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static FRAME: Cell<u32> = const { Cell::new(0) };
    static BG_BRUSH: OnceCell<HBRUSH> = const { OnceCell::new() };
    static FG_BRUSH: OnceCell<HBRUSH> = const { OnceCell::new() };
}

/// 条数由窗口宽自动算出：两侧留边后按条宽加间隙的节拍排布
fn bar_count(width: i32) -> i32 {
    ((width - 2 * PAD) / (BAR_W + GAP)).max(0)
}

/// 活跃根数：电平线性映射后向上取整，至少中间 CENTER_MIN 根，不超过总根数
fn active_count(level: u8, total: i32) -> i32 {
    let n = (level as i32 * total + 254) / 255;
    n.max(CENTER_MIN).min(total.max(0))
}

/// 钟形包络，中心为 1，离中心越远衰减越快
fn envelope(i: i32, center: i32, spread: f64) -> f64 {
    let d = (i - center) as f64 / spread;
    (-(d * d)).exp()
}

// 边缘淡出：最外侧条压到满高的 1/6，向内 6 根线性恢复，波形不顶着窗口边缘生硬收尾
const EDGE_FADE: i32 = 6;
const EDGE_MIN: f64 = 1.0 / 6.0;

fn edge_damp(i: i32, total: i32) -> f64 {
    let edge = i.min(total - 1 - i);
    let f = (edge as f64 / EDGE_FADE as f64).min(1.0);
    EDGE_MIN + (1.0 - EDGE_MIN) * f
}

/// 无状态 hash 抖动，逐帧变化，免引随机数 crate；值域 0.85~1.15
fn jitter(i: i32, frame: u32) -> f64 {
    let h = (i as u32).wrapping_mul(73856093) ^ frame.wrapping_mul(19349663);
    0.85 + (h as f64 * 0.6180339887).fract() * 0.3
}

/// 单根高度：静音或落在活跃区外画小点，否则最大高 × 包络 × 抖动，点条不抖
fn bar_height(i: i32, level: u8, total: i32, frame: u32) -> i32 {
    if level == 0 || total <= 0 {
        return DOT_H;
    }
    let center = total / 2;
    let active = active_count(level, total);
    // 对称波形按奇数根取，active 以内算活跃，外面衰减成点
    if (i - center).abs() * 2 + 1 > active {
        return DOT_H;
    }
    let spread = (active / 3).max(SPREAD_MIN) as f64;
    let h = MAX_H as f64 * envelope(i, center, spread) * edge_damp(i, total) * jitter(i, frame);
    (h.round() as i32).clamp(DOT_H, MAX_H)
}

fn shared() -> (u8, bool) {
    match SHARED.get() {
        Some((l, v)) => (l.load(Ordering::Relaxed), v.load(Ordering::Relaxed)),
        None => (0, false),
    }
}

/// 画刷在 UI 线程只建一次，两个 GDI 句柄常驻不回收
fn brushes() -> (HBRUSH, HBRUSH) {
    let bg = BG_BRUSH.with(|c| *c.get_or_init(|| unsafe { CreateSolidBrush(BG_COLOR) }));
    let fg = FG_BRUSH.with(|c| *c.get_or_init(|| unsafe { CreateSolidBrush(FG_COLOR) }));
    (bg, fg)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub struct Overlay {
    level: Arc<AtomicU8>,
    visible: Arc<AtomicBool>,
}

impl Overlay {
    /// 起 UI 线程建窗，经 mpsc 同步等待建窗结果再返回
    pub fn spawn() -> Result<Self> {
        let level = Arc::new(AtomicU8::new(0));
        let visible = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<()>>();
        let (l2, v2) = (level.clone(), visible.clone());
        thread::spawn(move || ui_thread(l2, v2, tx));
        rx.recv().map_err(|_| eyre!("波形条 UI 线程异常退出"))??;
        Ok(Self { level, visible })
    }

    pub fn set_level(&self, v: u8) {
        self.level.store(v, Ordering::Relaxed);
    }

    pub fn set_visible(&self, on: bool) {
        self.visible.store(on, Ordering::Relaxed);
    }
}

fn ui_thread(level: Arc<AtomicU8>, visible: Arc<AtomicBool>, tx: Sender<Result<()>>) {
    if unsafe { build_window(level, visible, &tx) } {
        pump_messages();
    }
}

/// 注册类、建窗、挂定时器；成败经 tx 回报，返回 false 表示不进消息循环
unsafe fn build_window(
    level: Arc<AtomicU8>,
    visible: Arc<AtomicBool>,
    tx: &Sender<Result<()>>,
) -> bool {
    unsafe {
        let _ = SHARED.set((level, visible));
        let name = wide(CLASS_NAME);
        let title = wide("语音波动条");
        let hinstance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: null_mut(),
            hCursor: null_mut(),
            // 类背景直接用深灰画刷，免得首帧前闪黑
            hbrBackground: brushes().0,
            lpszMenuName: null(),
            lpszClassName: name.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            let _ = tx.send(Err(eyre!("波形条注册窗口类失败，GetLastError={}", GetLastError())));
            return false;
        }
        let x = (GetSystemMetrics(SM_CXSCREEN) - WIDTH) / 2;
        let y = GetSystemMetrics(SM_CYSCREEN) / 5;
        // TRANSPARENT 点击穿透需与 LAYERED 同挂；NOACTIVATE 不抢焦点，TOOLWINDOW 不进任务栏和 Alt-Tab
        let ex =
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
        let hwnd = CreateWindowExW(
            ex,
            name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            x,
            y,
            WIDTH,
            HEIGHT,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        );
        if hwnd.is_null() {
            let _ = tx.send(Err(eyre!("波形条建窗失败，GetLastError={}", GetLastError())));
            return false;
        }
        SetLayeredWindowAttributes(hwnd, 0, ALPHA, LWA_ALPHA);
        SetTimer(hwnd, TIMER_ID, TICK_MS, None);
        let _ = tx.send(Ok(()));
        true
    }
}

fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        // GetMessageW：0 是 WM_QUIT，-1 是出错，两者都结束循环
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                tick(hwnd);
                0
            }
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, TIMER_ID);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 30ms 节拍：录音中始终显示（有声画波形、无声画点阵），松开立即隐藏
fn tick(hwnd: HWND) {
    unsafe {
        let (level, recording) = shared();
        if recording {
            if !SHOWN.with(|c| c.get()) {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                SHOWN.with(|c| c.set(true));
            }
            let frame = FRAME.with(|c| {
                c.set(c.get().wrapping_add(1));
                c.get()
            });
            let dc = GetDC(hwnd);
            if !dc.is_null() {
                draw(dc, hwnd, level, frame);
                ReleaseDC(hwnd, dc);
            }
        } else if SHOWN.with(|c| c.get()) {
            ShowWindow(hwnd, SW_HIDE);
            SHOWN.with(|c| c.set(false));
        }
    }
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let (level, _) = shared();
        draw(dc, hwnd, level, FRAME.with(|c| c.get()));
        EndPaint(hwnd, &ps);
    }
}

/// 整窗填深灰后逐根画竖条；先画进离屏 DC 再整块贴回，避免逐条绘制时被看见中间态
fn draw(dc: HDC, hwnd: HWND, level: u8, frame: u32) {
    unsafe {
        let (bg, fg) = brushes();
        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc);
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        let mem = CreateCompatibleDC(dc);
        let bmp = if mem.is_null() { null_mut() } else { CreateCompatibleBitmap(dc, w, h) };
        if mem.is_null() || bmp.is_null() {
            if !mem.is_null() {
                DeleteDC(mem);
            }
            render(dc, &rc, level, frame, bg, fg);
            return;
        }
        let old = SelectObject(mem, bmp);
        render(mem, &rc, level, frame, bg, fg);
        BitBlt(dc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
        // 位图还选在 mem 里时删不掉，先还原再回收
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
    }
}

fn render(dc: HDC, rc: &RECT, level: u8, frame: u32, bg: HBRUSH, fg: HBRUSH) {
    unsafe {
        FillRect(dc, rc, bg);
        let total = bar_count(rc.right - rc.left);
        let mid = (rc.bottom - rc.top) / 2;
        for i in 0..total {
            let h = bar_height(i, level, total, frame);
            let x = rc.left + PAD + i * (BAR_W + GAP);
            let bar = RECT {
                left: x,
                top: rc.top + mid - h / 2,
                right: x + BAR_W,
                bottom: rc.top + mid + (h + 1) / 2,
            };
            FillRect(dc, &bar, fg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_count_from_width() {
        assert_eq!(bar_count(WIDTH), 42);
        assert_eq!(bar_count(WIDTH + BAR_W + GAP - 1), 43);
        assert_eq!(bar_count(WIDTH + BAR_W + GAP), 43);
        assert_eq!(bar_count(2 * PAD), 0);
        assert_eq!(bar_count(0), 0);
    }

    #[test]
    fn active_count_floor_and_ceil() {
        // 电平 0 仍给中心下限，但绘制端在 bar_height 里判成静音全画点
        assert_eq!(active_count(0, 42), CENTER_MIN);
        assert_eq!(active_count(1, 42), CENTER_MIN);
        assert_eq!(active_count(128, 42), 22);
        assert_eq!(active_count(255, 42), 42);
    }

    #[test]
    fn active_count_never_exceeds_total() {
        assert_eq!(active_count(255, 3), 3);
        assert_eq!(active_count(255, 0), 0);
    }

    #[test]
    fn envelope_peaks_at_center_and_falls() {
        let c = 39;
        assert!((envelope(c, c, 6.0) - 1.0).abs() < 1e-12);
        let near = envelope(c + 1, c, 6.0);
        let mid = envelope(c + 4, c, 6.0);
        let far = envelope(c + 9, c, 6.0);
        assert!(near < 1.0 && mid < near && far < mid);
    }

    #[test]
    fn envelope_spreads_symmetrically() {
        let c = 39;
        assert!((envelope(c - 5, c, 8.0) - envelope(c + 5, c, 8.0)).abs() < 1e-12);
    }

    #[test]
    fn jitter_stays_in_band() {
        for i in 0..42 {
            for frame in 0..200 {
                let j = jitter(i, frame);
                assert!((0.85..1.15).contains(&j), "i={i} frame={frame} j={j}");
            }
        }
    }

    #[test]
    fn jitter_changes_across_frames() {
        let mut distinct = 0;
        for f in 1..30 {
            if jitter(20, f) != jitter(20, f - 1) {
                distinct += 1;
            }
        }
        assert!(distinct > 20, "逐帧波动太弱: {distinct}");
    }

    #[test]
    fn silent_level_draws_dots_everywhere() {
        for i in 0..42 {
            assert_eq!(bar_height(i, 0, 42, 5), DOT_H);
        }
    }

    #[test]
    fn full_level_bars_stay_in_range_and_center_taller() {
        for i in 0..42 {
            let h = bar_height(i, 255, 42, 7);
            assert!((DOT_H..=MAX_H).contains(&h), "i={i} h={h}");
        }
        // 中心最高，最外侧衰减到点高
        assert!(bar_height(21, 255, 42, 7) > DOT_H);
        assert!(bar_height(21, 255, 42, 7) > bar_height(0, 255, 42, 7));
    }

    #[test]
    fn side_bars_decay_to_dots() {
        // 电平 20 -> 活跃 5 根（CENTER_MIN 兜底），中心两侧第 3 根起衰减成点
        assert!(bar_height(21, 20, 42, 3) > DOT_H);
        assert!(bar_height(21 + 2, 20, 42, 3) > DOT_H);
        assert_eq!(bar_height(21 + 3, 20, 42, 3), DOT_H);
        assert_eq!(bar_height(21 - 3, 20, 42, 3), DOT_H);
    }

    #[test]
    fn heights_never_overflow_window() {
        for level in [1u8, 64, 128, 200, 255] {
            for i in 0..42 {
                assert!(bar_height(i, level, 42, 31) <= MAX_H);
            }
        }
    }

    #[test]
    fn edge_damp_pins_outer_bar_to_one_sixth() {
        let min = EDGE_MIN;
        assert!((edge_damp(0, 42) - min).abs() < 1e-12);
        assert!((edge_damp(41, 42) - min).abs() < 1e-12);
        assert!((edge_damp(6, 42) - 1.0).abs() < 1e-12);
        assert!(edge_damp(3, 42) > min && edge_damp(3, 42) < 1.0);
    }
}
