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
// 协议探针：实时率推流 hello-16k.wav，打印 partial 累计全文序列，验证流式信号
// 用法：cargo run --example stream_probe，token 走 auth 的 env/缓存/gh/设备码链路
#![allow(dead_code)]

#[path = "../src/auth.rs"]
mod auth;
#[path = "../src/stt.rs"]
mod stt;

use std::{path::PathBuf, time::Duration};

use color_eyre::Result;
use stt::TranscriptionClient;

const CHUNK: usize = 3200; // 100ms @ 16k mono s16le

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .init();
    let token = auth::get_token().await?.access_token;
    let pcm = {
        let file = std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tmp/hello-16k.wav"))?;
        file[44..].to_vec()
    };
    println!("推流 {} 字节 PCM，约 {:.1} 秒音频", pcm.len(), pcm.len() as f64 / 32000.0);

    let mut client = TranscriptionClient::connect(&token).await?;
    let turn = client.start_turn().await;
    let mut last = String::new();
    for (i, chunk) in pcm.chunks(CHUNK).enumerate() {
        client.push_audio(&turn, chunk).await;
        let (committed, text) = client.try_transcript(&turn);
        let full = format!("{committed}{text}");
        if full != last {
            println!("[{:>3}] committed={committed:?} text={text:?}", i + 1);
            last = full;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    client.end_turn(&turn).await;
    let text = client.await_final(&turn, Duration::from_secs(60)).await?;
    println!("final: {text:?}");
    client.close().await;
    Ok(())
}
