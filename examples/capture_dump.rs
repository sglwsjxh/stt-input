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
// 采集自检：录 3 秒写 tmp/cap.wav，验证设备链路与 wav 参数
// bin-only crate 的 example 拿不到 crate 内模块，用 path 引入
#![allow(dead_code)]

#[path = "../src/capture.rs"]
mod capture;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    capture::MicCapture::record_seconds(3, "tmp/cap.wav").await?;
    println!("录音完成: tmp/cap.wav");
    Ok(())
}
