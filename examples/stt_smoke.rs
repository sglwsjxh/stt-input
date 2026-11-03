// 云端转写冒烟：真实 token + 真实端点 + hello-16k.wav 实时率推流
// 用法：设 STT_GITHUB_TOKEN 后 cargo run --example stt_smoke
#![allow(dead_code)]

#[path = "../src/stt.rs"]
mod stt;

use std::{path::PathBuf, time::Duration};

use color_eyre::Result;
use stt::TranscriptionClient;

const CHUNK: usize = 3200; // 100ms @ 16k mono s16le

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let token = std::env::var("STT_GITHUB_TOKEN").expect("请先设置 STT_GITHUB_TOKEN");
    let wav = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tmp/hello-16k.wav");
    let file = std::fs::read(&wav)?;
    // 标准 44 字节 RIFF 头直接跳过，ffmpeg/TTS 产物即此形状
    let pcm = &file[44..];
    println!("推流 {} 字节 PCM，约 {:.1} 秒音频", pcm.len(), pcm.len() as f64 / 32000.0);

    let mut client = TranscriptionClient::connect(&token).await?;
    let turn = client.start_turn().await;
    for (i, chunk) in pcm.chunks(CHUNK).enumerate() {
        client.push_audio(&turn, chunk).await;
        println!("  chunk {:>3} 已发送", i + 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    client.end_turn(&turn).await;
    let text = client.await_final(&turn, Duration::from_secs(60)).await?;
    println!("await_final 结果: {text}");
    client.close().await;
    Ok(())
}
