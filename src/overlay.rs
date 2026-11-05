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
// 语音波形条：录音时置顶深灰底录音机风格滚动波形，纯 Win32 + GDI，零新增依赖

use std::cell::{Cell, OnceCell, RefCell};
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

const WIDTH: i32 = 213;
const HEIGHT: i32 = 25;
const PAD: i32 = 10;
const ALPHA: u8 = 200;
const TICK_MS: u32 = 30;
const TIMER_ID: usize = 1;
const CLASS_NAME: &str = "SttInputOverlay";

// 波形视觉参数
const BAR_W: i32 = 2; // 竖条宽，录音机细线风格
const GAP: i32 = 2; // 竖条间隙
const DOT_H: i32 = 2; // 圆点直径，方点近似
const MAX_H: i32 = 23; // 有声条最大可用高，留 1px 上下居中余量
const CURSOR_W: i32 = 3; // 右侧写入光标宽
const BG_COLOR: COLORREF = 0x0030_2D2D; // RGB(45,45,48) 深灰底
const FG_COLOR: COLORREF = 0x00F0_F0F0; // RGB(240,240,240) 波形白

/// 电平与录音状态经全局单例传给 wndproc：wndproc 签名带不了用户数据
static SHARED: OnceLock<(Arc<AtomicU8>, Arc<AtomicBool>)> = OnceLock::new();

thread_local! {
    static SHOWN: Cell<bool> = const { Cell::new(false) };
    static HIDE_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static BG_BRUSH: OnceCell<HBRUSH> = const { OnceCell::new() };
    static FG_BRUSH: OnceCell<HBRUSH> = const { OnceCell::new() };
    // 波形历史，队尾 = 最新 = 光标旁；录音结束后清空
    static HISTORY: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

/// 条数由窗口宽自动算出：两侧留边、扣掉光标位后按条宽加间隙的节拍排布
fn bar_count(width: i32) -> i32 {
    ((width - 2 * PAD - CURSOR_W - GAP) / (BAR_W + GAP)).max(0)
}

/// 电平采样成单根高度：静音记点，平方根映射让低音量也有可见振幅
fn sample_height(level: u8) -> i32 {
    if level == 0 {
        return DOT_H;
    }
    ((MAX_H as f64 * (level as f64 / 255.0).sqrt()) as i32).clamp(BAR_W, MAX_H)
}

/// 新样本入队尾，超出容量挤掉最旧一根，波形整体向左推进
fn push_sample(history: &mut Vec<i32>, sample: i32, cap: usize) {
    history.push(sample);
    if history.len() > cap {
        history.remove(0);
    }
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

/// 30ms 节拍：录音中采样电平入历史向左推一格，松开立即隐藏并清空历史
fn tick(hwnd: HWND) {
    unsafe {
        let (level, recording) = shared();
        if recording {
            if !SHOWN.with(|c| c.get()) {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                SHOWN.with(|c| c.set(true));
            }
            let sample = sample_height(level);
            HISTORY.with(|c| {
                let mut h = c.borrow_mut();
                push_sample(&mut h, sample, bar_count(WIDTH) as usize);
            });
            let dc = GetDC(hwnd);
            if !dc.is_null() {
                draw(dc, hwnd);
                ReleaseDC(hwnd, dc);
            }
        } else if SHOWN.with(|c| c.get()) {
            ShowWindow(hwnd, SW_HIDE);
            SHOWN.with(|c| c.set(false));
            HISTORY.with(|c| c.borrow_mut().clear());
        }
    }
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        draw(dc, hwnd);
        EndPaint(hwnd, &ps);
    }
}

/// 整窗填深灰后逐根画竖条；先画进离屏 DC 再整块贴回，避免逐条绘制时被看见中间态
fn draw(dc: HDC, hwnd: HWND) {
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
            render(dc, &rc, bg, fg);
            return;
        }
        let old = SelectObject(mem, bmp);
        render(mem, &rc, bg, fg);
        BitBlt(dc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
        // 位图还选在 mem 里时删不掉，先还原再回收
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
    }
}

/// 录音机视图：全宽圆点基线，历史波形右对齐向左铺开，最右全高写入光标
fn render(dc: HDC, rc: &RECT, bg: HBRUSH, fg: HBRUSH) {
    unsafe {
        FillRect(dc, rc, bg);
        let mid = (rc.bottom - rc.top) / 2;
        // 基线点与条位同余，波形区的静音点正好续上虚线
        let mut x = rc.left + DOT_H;
        while x + DOT_H <= rc.right - DOT_H {
            let dot = RECT {
                left: x,
                top: rc.top + mid - DOT_H / 2,
                right: x + DOT_H,
                bottom: rc.top + mid + (DOT_H + 1) / 2,
            };
            FillRect(dc, &dot, fg);
            x += DOT_H * 2;
        }
        let total = bar_count(rc.right - rc.left);
        let history = HISTORY.with(|c| c.borrow().clone());
        let n = history.len() as i32;
        for i in 0..total {
            // 历史右对齐：尾部最新贴光标，不足整窗时左侧留基线
            let idx = i - (total - n);
            let h = if idx >= 0 { history[idx as usize] } else { DOT_H };
            let x = rc.left + PAD + i * (BAR_W + GAP);
            let bar = RECT {
                left: x,
                top: rc.top + mid - h / 2,
                right: x + BAR_W,
                bottom: rc.top + mid + (h + 1) / 2,
            };
            FillRect(dc, &bar, fg);
        }
        let cursor = RECT {
            left: rc.right - PAD - CURSOR_W,
            top: rc.top,
            right: rc.right - PAD,
            bottom: rc.bottom,
        };
        FillRect(dc, &cursor, fg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_count_from_width() {
        assert_eq!(bar_count(WIDTH), 47);
        assert_eq!(bar_count(WIDTH - 1), 46);
        assert_eq!(bar_count(2 * PAD + CURSOR_W + GAP), 0);
        assert_eq!(bar_count(0), 0);
    }

    #[test]
    fn sample_height_boosts_quiet_levels() {
        assert_eq!(sample_height(0), DOT_H);
        assert_eq!(sample_height(255), MAX_H);
        // 平方根映射：电平 16（很轻的声音）也有约十分之一满高的可见振幅
        assert!(sample_height(16) > MAX_H / 10);
        assert!(sample_height(16) > sample_height(0));
        assert!(sample_height(64) > sample_height(16));
        assert!(sample_height(128) > sample_height(64));
        assert!(sample_height(192) > sample_height(128));
    }

    #[test]
    fn push_sample_scrolls_out_oldest() {
        let mut h = Vec::new();
        for i in 0..60 {
            push_sample(&mut h, i, 47);
        }
        assert_eq!(h.len(), 47);
        assert_eq!(h[0], 13);
        assert_eq!(h[46], 59);
    }

    #[test]
    fn push_sample_keeps_short_history() {
        let mut h = Vec::new();
        push_sample(&mut h, DOT_H, 47);
        push_sample(&mut h, MAX_H, 47);
        assert_eq!(h, vec![DOT_H, MAX_H]);
    }

    #[test]
    fn dot_pitch_matches_bar_pitch() {
        // 基线点距与波形区节拍一致，静音点才能和基线虚线连成一条
        assert_eq!(BAR_W + GAP, DOT_H * 2);
    }

    #[test]
    fn history_fits_between_pads_and_cursor() {
        // 条区 + 间隙 + 光标 + 右边距恰好填满窗口
        assert_eq!(PAD + bar_count(WIDTH) * (BAR_W + GAP) + GAP + CURSOR_W + PAD, WIDTH);
    }
}
