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
// 系统托盘图标：独立线程建隐藏消息窗口，右键菜单退出

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Sender};
use std::thread;

use color_eyre::eyre::{Result, eyre};
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE, LoadImageW,
    MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_DESTROY,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows_sys::core::GUID;

// Cargo.toml 未启用 Win32_System_LibraryLoader feature，直链 kernel32 取实例句柄
unsafe extern "system" {
    fn GetModuleHandleW(lpmodulename: *const u16) -> *mut c_void;
}

// Cargo.toml 未启用 Win32_UI_Shell feature，Shell_NotifyIconW 直链 shell32；
// NOTIFYICONDATAW 按 windows-sys x64 定义复刻，字段顺序与对齐一致，cbSize 才能被 shell 接受
unsafe extern "system" {
    fn Shell_NotifyIconW(dwmessage: u32, lpdata: *const NotifyIconDataW) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NotifyIconDataW {
    cb_size: u32,
    hwnd: HWND,
    uid: u32,
    u_flags: u32,
    u_callback_message: u32,
    h_icon: HICON,
    sz_tip: [u16; 128],
    dw_state: u32,
    dw_state_mask: u32,
    sz_info: [u16; 256],
    // 与 uTimeout 同偏移的匿名联合成员，v0 语义下闲置
    u_version: u32,
    sz_info_title: [u16; 64],
    dw_info_flags: u32,
    guid_item: GUID,
    h_balloon_icon: HICON,
}

impl Default for NotifyIconDataW {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

const CLASS_NAME: &str = "SttInputTray";
// WM_APP 起才是应用私有区间，避开 WM_USER 与 shell 内部消息的撞车区
const WM_TRAY: u32 = WM_APP + 1;
const TRAY_UID: u32 = 1;
const IDM_EXIT: usize = 0x1001;
// 嵌入资源 ICON 的 ID，与 build.rs embed-resource 一致；MAKEINTRESOURCEW 宏就是把整数当字符串指针
const ICON_RESOURCE_ID: usize = 1;
const NIF_MESSAGE: u32 = 1;
const NIF_ICON: u32 = 2;
const NIF_TIP: u32 = 4;
const NIM_ADD: u32 = 0;
const NIM_DELETE: u32 = 2;

/// 起托盘线程建图标，经 mpsc 同步等待创建结果再返回
pub fn spawn() -> Result<()> {
    let (tx, rx) = mpsc::channel::<Result<()>>();
    thread::spawn(move || tray_thread(tx));
    rx.recv().map_err(|_| eyre!("托盘线程异常退出"))??;
    Ok(())
}

fn tray_thread(tx: Sender<Result<()>>) {
    if unsafe { build_tray(&tx) } {
        pump_messages();
    }
}

/// 注册类、建隐藏窗、NIM_ADD；成败经 tx 回报，返回 false 表示不进消息循环。
/// 版本用默认 v0：lParam 直接就是鼠标消息，免 NIM_SETVERSION，消息解析最简单可靠
unsafe fn build_tray(tx: &Sender<Result<()>>) -> bool {
    unsafe {
        let name = wide(CLASS_NAME);
        let title = wide("stt-input");
        let hinstance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: name.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            let _ = tx.send(Err(eyre!("托盘注册窗口类失败，GetLastError={}", GetLastError())));
            return false;
        }
        // 不 ShowWindow，WS_OVERLAPPEDWINDOW 建出来就是隐藏的消息载体
        let hwnd = CreateWindowExW(
            0,
            name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        );
        if hwnd.is_null() {
            let _ = tx.send(Err(eyre!("托盘建窗失败，GetLastError={}", GetLastError())));
            return false;
        }
        let mut nid = NotifyIconDataW {
            cb_size: size_of::<NotifyIconDataW>() as u32,
            hwnd,
            uid: TRAY_UID,
            u_flags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            u_callback_message: WM_TRAY,
            h_icon: load_icon(),
            ..Default::default()
        };
        for (dst, src) in nid.sz_tip.iter_mut().zip("stt-input".encode_utf16()) {
            *dst = src;
        }
        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            let _ = tx.send(Err(eyre!("托盘图标添加失败，GetLastError={}", GetLastError())));
            return false;
        }
        tracing::info!("托盘已启动");
        let _ = tx.send(Ok(()));
        true
    }
}

/// 优先取自身嵌入的 ICON 资源 1，失败退回系统应用图标
unsafe fn load_icon() -> HICON {
    unsafe {
        let hmod = GetModuleHandleW(null());
        let mut icon =
            LoadImageW(hmod, ICON_RESOURCE_ID as *const u16, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE);
        if icon.is_null() {
            icon = LoadImageW(null_mut(), IDI_APPLICATION, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE);
        }
        icon
    }
}

fn remove_icon(hwnd: HWND) {
    let nid = NotifyIconDataW {
        cb_size: size_of::<NotifyIconDataW>() as u32,
        hwnd,
        uid: TRAY_UID,
        ..Default::default()
    };
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &nid);
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
            WM_TRAY => {
                // v0 语义：lParam 低位即鼠标消息本身
                match lparam as u32 {
                    WM_RBUTTONUP => show_menu(hwnd),
                    WM_LBUTTONUP => {}
                    _ => {}
                }
                0
            }
            WM_DESTROY => {
                remove_icon(hwnd);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn show_menu(hwnd: HWND) {
    unsafe {
        // 不先置前台，隐藏窗的弹出菜单点了不消失也不响应，TrackPopupMenu 的老规矩
        SetForegroundWindow(hwnd);
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let text = wide("退出");
        AppendMenuW(menu, MF_STRING, IDM_EXIT, text.as_ptr());
        let mut pt = POINT::default();
        GetCursorPos(&mut pt);
        // TPM_RETURNCMD 同步返回被点菜单项 ID，0 是没选
        let cmd =
            TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
        DestroyMenu(menu);
        // 补一条空消息，让 shell 把菜单立即收掉而不是悬到下次点击
        PostMessageW(hwnd, WM_NULL, 0, 0);
        if cmd == IDM_EXIT as i32 {
            remove_icon(hwnd);
            // 后台程序进程级退出是干净路径，剪贴板恢复线程由 OS 回收
            std::process::exit(0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
