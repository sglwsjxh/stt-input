// 开机自启动：HKCU Run 注册表键，随 exe 路径走，无安装服务
use std::path::Path;

use color_eyre::eyre::{Result, WrapErr, eyre};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegDeleteValueW, RegQueryValueExW, RegSetValueExW,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "stt-input";

fn exe_command() -> Result<String> {
    let path = std::env::current_exe().wrap_err("获取 exe 路径失败")?;
    let quoted = format!("\"{}\"", path.display());
    Ok(quoted)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_run_key(write: bool) -> Result<HKEY> {
    let mut hkey: HKEY = std::ptr::null_mut();
    let access = if write { KEY_SET_VALUE | KEY_QUERY_VALUE } else { KEY_QUERY_VALUE };
    let err = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            std::ptr::null(),
            0,
            access,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        )
    };
    if err != ERROR_SUCCESS {
        return Err(eyre!("打开注册表 Run 键失败: error {err}"));
    }
    Ok(hkey)
}

fn close_key(hkey: HKEY) {
    unsafe { RegCloseKey(hkey) };
}

/// 注册自启动，值指向当前 exe（带引号防路径空格）
pub fn install() -> Result<()> {
    let cmd = exe_command()?;
    let data = wide(&cmd);
    let hkey = open_run_key(true)?;
    let err = unsafe {
        RegSetValueExW(
            hkey,
            wide(VALUE_NAME).as_ptr(),
            0,
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    close_key(hkey);
    if err != ERROR_SUCCESS {
        return Err(eyre!("写入自启动值失败: error {err}"));
    }
    println!("[自启动] 已注册：{cmd}");
    Ok(())
}

/// 移除自启动注册值，不存在时静默成功
pub fn uninstall() -> Result<()> {
    let hkey = open_run_key(true)?;
    let err = unsafe { RegDeleteValueW(hkey, wide(VALUE_NAME).as_ptr()) };
    close_key(hkey);
    match err {
        ERROR_SUCCESS => println!("[自启动] 已移除"),
        ERROR_FILE_NOT_FOUND => println!("[自启动] 本就未注册"),
        e => return Err(eyre!("删除自启动值失败: error {e}")),
    }
    Ok(())
}

/// 检查当前是否已注册自启动
pub fn is_installed() -> Result<bool> {
    let hkey = open_run_key(false)?;
    let mut size = 0u32;
    let err = unsafe {
        RegQueryValueExW(
            hkey,
            wide(VALUE_NAME).as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    close_key(hkey);
    match err {
        ERROR_SUCCESS | ERROR_MORE_DATA => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        e => Err(eyre!("查询自启动值失败: error {e}")),
    }
}

/// 自启动目标与当前 exe 是否一致（exe 挪窝后需要重装）
pub fn is_current() -> Result<bool> {
    let hkey = open_run_key(false)?;
    let mut size = 0u32;
    let err = unsafe {
        RegQueryValueExW(
            hkey,
            wide(VALUE_NAME).as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if err != ERROR_SUCCESS && err != ERROR_MORE_DATA {
        close_key(hkey);
        return Err(eyre!("查询自启动值失败: error {err}"));
    }
    let mut buf = vec![0u16; (size as usize / 2).max(1)];
    let err = unsafe {
        RegQueryValueExW(
            hkey,
            wide(VALUE_NAME).as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    close_key(hkey);
    if err != ERROR_SUCCESS {
        return Err(eyre!("读取自启动值失败: error {err}"));
    }
    let stored = String::from_utf16_lossy(&buf);
    let stored = stored.trim_end_matches('\0').trim_matches('"');
    let current = std::env::current_exe().wrap_err("获取 exe 路径失败")?;
    let same = Path::new(stored) == current.as_path();
    Ok(same)
}
