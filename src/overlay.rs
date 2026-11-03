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
// 语音波动条：录音时置顶黑底白条电平动画，纯 Win32 + GDI，零新增依赖

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, eyre};
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BLACK_BRUSH, BeginPaint, EndPaint, FillRect, GetDC, GetStockObject, HDC, PAINTSTRUCT,
    ReleaseDC, WHITE_BRUSH,
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
const PAD: i32 = 2;
const ALPHA: u8 = 200;
const TICK_MS: u32 = 30;
const TIMER_ID: usize = 1;
const HIDE_AFTER: Duration = Duration::from_millis(1000);
const CLASS_NAME: &str = "SttInputOverlay";

/// 电平与录音状态经全局单例传给 wndproc：wndproc 签名带不了用户数据
static SHARED: OnceLock<(Arc<AtomicU8>, Arc<AtomicBool>)> = OnceLock::new();

thread_local! {
    static SHOWN: Cell<bool> = const { Cell::new(false) };
    static HIDE_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static LAST_LEVEL: Cell<i32> = const { Cell::new(-1) };
}

/// 电平映射条宽，整数向下取整：0 得 0，255 得满宽
fn bar_width(level: u8, avail: i32) -> i32 {
    avail * level as i32 / 255
}

/// 活跃态：录音中且电平非零，否则进入隐藏倒计时
fn level_active(level: u8, recording: bool) -> bool {
    recording && level > 0
}

fn shared() -> (u8, bool) {
    match SHARED.get() {
        Some((l, v)) => (l.load(Ordering::Relaxed), v.load(Ordering::Relaxed)),
        None => (0, false),
    }
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
        rx.recv().map_err(|_| eyre!("波动条 UI 线程异常退出"))??;
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
            hbrBackground: GetStockObject(BLACK_BRUSH),
            lpszMenuName: null(),
            lpszClassName: name.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            let _ = tx.send(Err(eyre!("波动条注册窗口类失败，GetLastError={}", GetLastError())));
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
            let _ = tx.send(Err(eyre!("波动条建窗失败，GetLastError={}", GetLastError())));
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

/// 30ms 节拍：只在显隐切换或电平变化时动作，静帧不刷避免 GDI 闪烁
fn tick(hwnd: HWND) {
    unsafe {
        let (level, recording) = shared();
        if level_active(level, recording) {
            HIDE_AT.with(|c| c.set(None));
            if !SHOWN.with(|c| c.get()) {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                SHOWN.with(|c| c.set(true));
            }
            if LAST_LEVEL.with(|c| c.get()) != level as i32 {
                LAST_LEVEL.with(|c| c.set(level as i32));
                let dc = GetDC(hwnd);
                if !dc.is_null() {
                    draw(dc, hwnd, level);
                    ReleaseDC(hwnd, dc);
                }
            }
        } else if SHOWN.with(|c| c.get()) {
            let due = HIDE_AT.with(|c| match c.get() {
                None => {
                    c.set(Some(Instant::now()));
                    false
                }
                Some(t) => t.elapsed() >= HIDE_AFTER,
            });
            if due {
                ShowWindow(hwnd, SW_HIDE);
                SHOWN.with(|c| c.set(false));
                HIDE_AT.with(|c| c.set(None));
                LAST_LEVEL.with(|c| c.set(-1));
            }
        }
    }
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let (level, _) = shared();
        draw(dc, hwnd, level);
        LAST_LEVEL.with(|c| c.set(level as i32));
        EndPaint(hwnd, &ps);
    }
}

fn draw(dc: HDC, hwnd: HWND, level: u8) {
    unsafe {
        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc);
        FillRect(dc, &rc, GetStockObject(BLACK_BRUSH));
        let avail = (rc.right - rc.left - 2 * PAD).max(0);
        let w = bar_width(level, avail);
        if w > 0 {
            let bar = RECT {
                left: rc.left + PAD,
                top: rc.top + PAD,
                right: rc.left + PAD + w,
                bottom: rc.bottom - PAD,
            };
            FillRect(dc, &bar, GetStockObject(WHITE_BRUSH));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_width_maps_endpoints() {
        assert_eq!(bar_width(0, 320), 0);
        assert_eq!(bar_width(255, 320), 320);
    }

    #[test]
    fn bar_width_truncates_down() {
        assert_eq!(bar_width(128, 320), 160);
        assert_eq!(bar_width(127, 320), 159);
        assert_eq!(bar_width(128, 316), 158);
    }

    #[test]
    fn muted_or_stopped_is_inactive() {
        assert!(level_active(200, true));
        assert!(!level_active(0, true));
        assert!(!level_active(200, false));
        assert!(!level_active(0, false));
    }
}
