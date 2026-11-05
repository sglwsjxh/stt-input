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

// release 构建为后台程序不弹控制台；debug 构建保留控制台方便开发
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(dead_code)] // 少量接口留给调试入口，暂不接线

#[path = "src/auth.rs"]
mod auth;
#[path = "src/autostart.rs"]
mod autostart;
#[path = "src/capture.rs"]
mod capture;
#[path = "src/hotkey.rs"]
mod hotkey;
#[path = "src/overlay.rs"]
mod overlay;
#[path = "src/paste.rs"]
mod paste;
#[path = "src/stt.rs"]
mod stt;
#[path = "src/tray.rs"]
mod tray;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONINFORMATION, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
};

const POLL: Duration = Duration::from_millis(20);
const FINAL_WAIT: Duration = Duration::from_secs(5);
const TURN_MAX: Duration = Duration::from_secs(60);
const RECONNECT_EVERY: Duration = Duration::from_secs(3);
// 低于此峰值视为静音（约 -43dB），避免底噪让波动条常亮
const MUTE_GATE: i16 = 200;
// 实例互斥：命名互斥量随进程存活，Windows 关闭进程时自动释放
const INSTANCE_MUTEX: &str = "Local\\stt-input-single-instance";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    // 互斥检查必须先于日志初始化，避免第二实例截断第一实例的日志
    ensure_single_instance()?;
    init_tracing(verbose)?;
    if color_eyre::install().is_err() {
        // 重复安装无害，通常只发生一次
    }
    if args.iter().any(|a| a == "--print-auth-mode") {
        tracing::info!("{}", auth::describe_auth_source());
        return Ok(());
    }
    if args.iter().any(|a| a == "--install-autostart") {
        return autostart::install();
    }
    if args.iter().any(|a| a == "--uninstall-autostart") {
        return autostart::uninstall();
    }
    if args.iter().any(|a| a == "--dump-wav") {
        let rt = tokio::runtime::Runtime::new()?;
        let result = capture::MicCapture::record_seconds(3, "tmp/cap.wav");
        rt.block_on(result)?;
        tracing::info!("录音自检完成: tmp/cap.wav");
        return Ok(());
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run(&args))
}

/// 实例互斥：已有一个实例在跑时弹窗提示并退出。
/// 用命名互斥量而不是 kill 旧进程——听写中途被杀会丢一轮转写
fn ensure_single_instance() -> Result<()> {
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide(INSTANCE_MUTEX).as_ptr()) };
    if handle.is_null() {
        return Err(eyre!("创建实例互斥量失败"));
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        alert("stt-input 已经在运行（系统托盘区图标，右键可退出）");
        std::process::exit(0);
    }
    // 句柄故意不关闭，持有到进程退出，互斥才有效
    Ok(())
}

/// 默认零输出；-v/--verbose 时把日志写到 exe 同目录 stt-input.log，每次启动覆盖
fn init_tracing(verbose: bool) -> Result<()> {
    if !verbose {
        return Ok(());
    }
    let path = exe_dir()?.join("stt-input.log");
    let file = std::fs::File::create(&path).wrap_err_with(|| format!("打开日志失败: {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(Mutex::new(file))
        .init();
    Ok(())
}

fn exe_dir() -> Result<PathBuf> {
    std::env::current_exe()
        .wrap_err("获取 exe 路径失败")?
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| eyre!("exe 路径没有父目录"))
}

/// GUI 子系统没有控制台，需要用户看到的提示用弹窗
fn alert(message: &str) {
    let title = wide("stt-input");
    let text = wide(message);
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

async fn run(args: &[String]) -> Result<()> {
    let mut paster = paste::WindowsPaster::new()?;
    if args.iter().any(|a| a == "--paste-with-shift") {
        paster.set_paste_with_shift(true);
    }
    // 波动条失败只降级不挡听写
    let overlay = match overlay::Overlay::spawn() {
        Ok(o) => Some(o),
        Err(error) => {
            tracing::warn!(error = %error, "波动条不可用");
            None
        }
    };
    // 托盘与波动条都是锦上添花，失败只降级不挡听写
    if let Err(error) = tray::spawn() {
        tracing::warn!(error = %error, "托盘不可用");
    }
    let mut mic = capture::MicCapture::open()?;
    tracing::info!(device = %mic.device_name(), "麦克风就绪");
    let mut rx = mic.start()?;
    let mut token = auth::get_token().await?.access_token;
    tracing::info!("正在连接云端转写服务…");
    let mut client: Option<stt::TranscriptionClient> = None;
    let mut last_retry = Instant::now() - RECONNECT_EVERY;
    tracing::info!("就绪：按住右Ctrl说话，松开自动粘贴，退出请结束进程");

    loop {
        // 空闲期丢弃音频块，保持通道不积压
        match tokio::time::timeout(POLL, rx.recv()).await {
            Ok(Some(_)) => {}
            Ok(None) => return Err(eyre!("音频流已断开，请重启")),
            Err(_) => {}
        }
        // 常驻连接断了就按节流自动重连，保证按键零等待；失败冷却 30s，避免刷爆 GitHub 设备码端点
        let alive = client.as_ref().is_some_and(stt::TranscriptionClient::is_connected);
        if !alive && last_retry.elapsed() >= RECONNECT_EVERY {
            match connect_with_reauth(&mut token).await {
                Ok(new) => {
                    client = Some(new);
                    last_retry = Instant::now();
                }
                Err(error) => {
                    tracing::warn!(error = %error, "重连失败，30 秒后自动重试");
                    last_retry = Instant::now() + Duration::from_secs(30) - RECONNECT_EVERY;
                }
            }
        }
        if hotkey::hotkey_down()
            && let Err(error) = do_turn(&mut client, &mut rx, &mut paster, &mut token, overlay.as_ref()).await
        {
            tracing::error!(error = %error, "本轮听写失败");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

async fn connect_with_reauth(token: &mut String) -> Result<stt::TranscriptionClient> {
    match stt::TranscriptionClient::connect(token).await {
        Ok(client) => Ok(client),
        Err(error) => {
            let text = error.to_string();
            if !text.contains("token 无效") {
                tracing::warn!(error = %text, "连接失败");
                return Err(error);
            }
            // GUI 无控制台，验证码通过弹窗展示；同时落日志留底
            alert("即将开始 GitHub 设备码授权，点击确定后请在浏览器完成操作（验证码见弹窗与日志）");
            *token = auth::device_flow_with(&|message| {
                alert(message);
                tracing::info!("{}", message);
            })
            .await?;
            stt::TranscriptionClient::connect(token).await
        }
    }
}

/// s16le 块的峰值电平映射到 0-255，低于噪声门限算静音
fn peak_level(block: &[u8]) -> u8 {
    let peak = block
        .as_chunks::<2>()
        .0
        .iter()
        .map(|f| i16::from_le_bytes(*f))
        .map(i16::unsigned_abs)
        .max()
        .unwrap_or(0);
    if peak <= MUTE_GATE as u16 {
        return 0;
    }
    let scaled = (peak as u32 * 255 / (i16::MAX as u32)) as u8;
    scaled.max(1)
}

async fn do_turn(
    client: &mut Option<stt::TranscriptionClient>,
    rx: &mut capture::ChunkReceiver,
    paster: &mut paste::WindowsPaster,
    token: &mut String,
    overlay: Option<&overlay::Overlay>,
) -> Result<()> {
    // 常驻连接断了就在按下瞬间现场重连；音频块在 std 通道缓冲，开头语音不丢
    let alive = client.as_ref().is_some_and(stt::TranscriptionClient::is_connected);
    if !alive {
        *client = Some(connect_with_reauth(token).await?);
    }
    let c = client.as_mut().expect("连接已就绪");
    // 建连期间就松手的误触不进识别；连接保留给下一轮
    if !hotkey::hotkey_down() {
        return Ok(());
    }
    let turn = c.start_turn().await;
    // 排干建连窗口里缓冲的块，开头语音不丢
    while let Some(block) = rx.try_recv() {
        c.push_audio(&turn, &block).await;
    }
    tracing::info!("开始录音");
    if let Some(o) = overlay {
        o.set_visible(true);
    }
    let started = Instant::now();
    // 流式状态：provisional = 当前段已上屏文本；清空帧把当前段作废后撤回重打
    let mut provisional = String::new();
    let mut revoked = false;
    loop {
        match tokio::time::timeout(POLL, rx.recv()).await {
            Ok(Some(block)) => {
                if let Some(o) = overlay {
                    o.set_level(peak_level(&block));
                }
                c.push_audio(&turn, &block).await;
                let (_, text) = c.try_transcript(&turn);
                match stream_step(&mut provisional, &mut revoked, &text) {
                    StreamStep::Idle => {}
                    StreamStep::Append(chunk) => {
                        if let Err(e) = paster.paste(&chunk) {
                            tracing::warn!("流式粘贴失败: {e}");
                        }
                    }
                    StreamStep::Rewrite { backspaces, text } => {
                        // 段落被服务端推翻：删旧段粘新段，仿 Win+H 的撤回重打
                        if let Err(e) = paster.erase(backspaces) {
                            tracing::warn!("流式撤回失败: {e}");
                        }
                        if let Err(e) = paster.paste(&text) {
                            tracing::warn!("流式粘贴失败: {e}");
                        }
                    }
                }
            }
            Ok(None) => {
                if let Some(o) = overlay {
                    o.set_visible(false);
                }
                return Err(eyre!("音频流已断开，请重启"));
            }
            Err(_) => {}
        }
        if !hotkey::hotkey_down() {
            break;
        }
        if started.elapsed() > TURN_MAX {
            tracing::info!("超过 60 秒，自动截断");
            break;
        }
    }
    if let Some(o) = overlay {
        o.set_visible(false);
    }
    c.end_turn(&turn).await;
    let text = c.await_final(&turn, FINAL_WAIT).await.wrap_err("识别失败")?;
    if provisional.is_empty() && !revoked && text.trim().is_empty() {
        tracing::info!("未识别到内容");
        return Ok(());
    }
    // final 为权威全文：当前段被作废或与 final 不一致就撤回残影重打，一致则续粘尾巴
    let stale = revoked || !text.starts_with(provisional.as_str());
    if stale {
        let chars = provisional.chars().count();
        if chars > 0 && let Err(e) = paster.erase(chars) {
            tracing::warn!("final 撤回残影失败: {e}");
        }
    }
    let from = if stale { 0 } else { provisional.len() };
    if text.len() > from && text.is_char_boundary(from) {
        let tail = &text[from..];
        if !tail.trim().is_empty() {
            paster.paste(tail)?;
        }
    }
    tracing::info!(%text, "已粘贴");
    Ok(())
}

/// 流式一步决策：清空帧作废当前段，段落被推翻则撤回重打，段内续写只粘增量
fn stream_step(provisional: &mut String, revoked: &mut bool, text: &str) -> StreamStep {
    // 清空帧：当前段作废标记，延迟到下一段内容到来时一并撤回，减少键入
    if text.is_empty() {
        if !provisional.is_empty() {
            *revoked = true;
        }
        return StreamStep::Idle;
    }
    if *revoked || !text.starts_with(provisional.as_str()) {
        let step = StreamStep::Rewrite {
            backspaces: provisional.chars().count(),
            text: text.to_string(),
        };
        *provisional = text.to_string();
        *revoked = false;
        return step;
    }
    if text.len() > provisional.len() {
        let chunk = text[provisional.len()..].to_string();
        *provisional = text.to_string();
        return StreamStep::Append(chunk);
    }
    StreamStep::Idle
}

enum StreamStep {
    Idle,
    Append(String),
    Rewrite { backspaces: usize, text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    // 走一遍探针实测的帧序列：首段 → 清空作废 → 服务端重写的新段 → final 收尾
    #[test]
    fn stream_step_follows_probe_sequence() {
        let mut provisional = String::new();
        let mut revoked = false;
        // 首段 partial：粘全文
        let step = stream_step(&mut provisional, &mut revoked, "测试听写一二。");
        assert!(matches!(step, StreamStep::Append(ref c) if c == "测试听写一二。"));
        assert_eq!(provisional, "测试听写一二。");
        // 清空帧：只作废不撤回
        assert!(matches!(stream_step(&mut provisional, &mut revoked, ""), StreamStep::Idle));
        assert!(revoked);
        // 重写的新段：撤回 7 个字符（中文按字符不按字节），粘新段全文
        let step = stream_step(&mut provisional, &mut revoked, "测试听写，123，Hello World，语音识别。");
        assert!(
            matches!(step, StreamStep::Rewrite { backspaces: 7, ref text }
                if text == "测试听写，123，Hello World，语音识别。"),
            "step 不匹配"
        );
        assert_eq!(provisional, "测试听写，123，Hello World，语音识别。");
        assert!(!revoked);
        // 再次清空后 final 与残影不一致：收尾判定走撤回重打
        assert!(matches!(stream_step(&mut provisional, &mut revoked, ""), StreamStep::Idle));
        assert!(revoked);
        let final_text = "测试听写，123，Hello World，语音识别测试。";
        let stale = revoked || !final_text.starts_with(provisional.as_str());
        assert!(stale, "被作废的段必须走撤回重打");
    }

    #[test]
    fn stream_step_appends_within_paragraph() {
        let mut provisional = String::new();
        let mut revoked = false;
        assert!(matches!(stream_step(&mut provisional, &mut revoked, "你好"), StreamStep::Append(_)));
        let step = stream_step(&mut provisional, &mut revoked, "你好世界");
        assert!(matches!(step, StreamStep::Append(ref c) if c == "世界"));
        assert_eq!(provisional, "你好世界");
        // 无增长的重复帧是 Idle
        assert!(matches!(stream_step(&mut provisional, &mut revoked, "你好世界"), StreamStep::Idle));
        // final 与已上屏一致：收尾判定走续粘尾巴
        let stale = revoked || !"你好世界。".starts_with(provisional.as_str());
        assert!(!stale);
    }

    #[test]
    fn peak_level_maps_scale() {
        let silence = [0u8, 0, 0, 0];
        assert_eq!(peak_level(&silence), 0);
        let mut loud = Vec::new();
        for _ in 0..4 {
            loud.extend_from_slice(&i16::MAX.to_le_bytes());
        }
        assert_eq!(peak_level(&loud), 255);
        let mut gated = Vec::new();
        for _ in 0..4 {
            gated.extend_from_slice(&150i16.to_le_bytes());
        }
        // 150 低于噪声门限 200，视为静音
        assert_eq!(peak_level(&gated), 0);
        let mut mid = Vec::new();
        for _ in 0..4 {
            mid.extend_from_slice(&3000i16.to_le_bytes());
        }
        // 3000 * 255 / 32767 = 23
        assert_eq!(peak_level(&mid), 23);
    }
}
