// 控制台听写：按住空格键说话，松开后把 MAI 云端识别文本粘贴进前台应用
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

use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};

const POLL: Duration = Duration::from_millis(20);
const FINAL_WAIT: Duration = Duration::from_secs(5);
const TURN_MAX: Duration = Duration::from_secs(60);
const RECONNECT_EVERY: Duration = Duration::from_secs(3);
// 低于此峰值视为静音（约 -43dB），避免底噪让波动条常亮
const MUTE_GATE: i16 = 200;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--print-auth-mode") {
        println!("{}", auth::describe_auth_source());
        return Ok(());
    }
    if args.iter().any(|a| a == "--install-autostart") {
        return autostart::install();
    }
    if args.iter().any(|a| a == "--uninstall-autostart") {
        return autostart::uninstall();
    }
    if args.iter().any(|a| a == "--dump-wav") {
        capture::MicCapture::record_seconds(3, "tmp/cap.wav").await?;
        println!("录音自检完成: tmp/cap.wav");
        return Ok(());
    }
    run(&args).await
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
    let mut mic = capture::MicCapture::open()?;
    println!("[听写] 麦克风: {}", mic.device_name());
    let mut rx = mic.start()?;
    let mut token = auth::get_token().await?.access_token;
    println!("[听写] 正在连接云端转写服务…");
    let mut client: Option<stt::TranscriptionClient> = None;
    let mut last_retry = Instant::now() - RECONNECT_EVERY;
    println!("[听写] 就绪：按住空格键说话，松开自动粘贴，Ctrl+C 退出");

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
        if hotkey::space_down()
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
            println!("[听写] token 失效，请按提示完成 GitHub 浏览器授权");
            *token = auth::device_flow().await?;
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
    if !hotkey::space_down() {
        return Ok(());
    }
    let turn = c.start_turn().await;
    // 排干建连窗口里缓冲的块，开头语音不丢
    while let Some(block) = rx.try_recv() {
        c.push_audio(&turn, &block).await;
    }
    println!("[听写] ● 请说话");
    if let Some(o) = overlay {
        o.set_visible(true);
    }
    let started = Instant::now();
    loop {
        match tokio::time::timeout(POLL, rx.recv()).await {
            Ok(Some(block)) => {
                if let Some(o) = overlay {
                    o.set_level(peak_level(&block));
                }
                c.push_audio(&turn, &block).await;
            }
            Ok(None) => {
                if let Some(o) = overlay {
                    o.set_visible(false);
                }
                return Err(eyre!("音频流已断开，请重启"));
            }
            Err(_) => {}
        }
        if !hotkey::space_down() {
            break;
        }
        if started.elapsed() > TURN_MAX {
            println!("[听写] 超过 60 秒，自动截断");
            break;
        }
    }
    if let Some(o) = overlay {
        o.set_visible(false);
    }
    c.end_turn(&turn).await;
    let text = c.await_final(&turn, FINAL_WAIT).await.wrap_err("识别失败")?;
    if text.trim().is_empty() {
        println!("[听写] 未识别到内容");
        return Ok(());
    }
    paster.paste(&text)?;
    println!("[听写] ✔ {text}");
    Ok(())
}
