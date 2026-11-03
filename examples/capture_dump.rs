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
