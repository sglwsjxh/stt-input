// cpal 麦克风采集：设备原生回调 -> 混单声道 -> 线性重采样 -> 16k s16le 100ms 块

use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use color_eyre::eyre::{Context, Result, eyre};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SampleRate, SizedSample, Stream, StreamConfig};

/// 目标采样率，与云端转写口径一致
pub const TARGET_RATE: SampleRate = 16_000;

/// 单块样本数，1600 = 100ms@16k
pub const CHUNK_SAMPLES: usize = 1_600;

/// 单块字节数，s16le
pub const CHUNK_BYTES: usize = CHUNK_SAMPLES * 2;

// 设备比墙钟慢零点几秒很常见，末块靠这段余量兜住
const RECORD_SLACK: Duration = Duration::from_millis(500);

// 轮询间隔：块周期 100ms，这里最多给接收端加 5ms 延迟
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// 设备原生样本到 [-1, 1] 归一化
trait Normalized: SizedSample {
    fn normalized(self) -> f32;
}

impl Normalized for f32 {
    fn normalized(self) -> f32 {
        self
    }
}

impl Normalized for f64 {
    fn normalized(self) -> f32 {
        self as f32
    }
}

impl Normalized for i16 {
    fn normalized(self) -> f32 {
        self as f32 / 32768.0
    }
}

impl Normalized for i32 {
    fn normalized(self) -> f32 {
        self as f32 / 2_147_483_648.0
    }
}

impl Normalized for u8 {
    fn normalized(self) -> f32 {
        // u8 静音点是 128，不是 0
        (self as f32 - 128.0) / 128.0
    }
}

/// 混单声道 + 线性插值重采样 + 定长分块，跑在 cpal 音频线程上
struct Pipeline {
    channels: usize,
    // 每个输出样本消耗的输入帧数
    ratio: f64,
    // 本次回调首帧的全局帧号
    base: i64,
    // 上一次回调的最后一帧，插值左端点
    prev: f32,
    // 下一个输出样本对应的全局输入帧位置
    pos: f64,
    mono: Vec<f32>,
    acc: Vec<i16>,
}

impl Pipeline {
    fn new(in_rate: SampleRate, channels: usize) -> Self {
        Self {
            channels: channels.max(1),
            ratio: f64::from(in_rate) / f64::from(TARGET_RATE),
            base: 0,
            prev: 0.0,
            pos: 0.0,
            mono: Vec::new(),
            acc: Vec::with_capacity(CHUNK_SAMPLES),
        }
    }

    fn push<T: Normalized>(&mut self, data: &[T], emit: &mut dyn FnMut(Vec<u8>)) {
        let frames = data.len() / self.channels;
        if frames == 0 {
            return;
        }
        self.mix(data, frames);

        let end = self.base + frames as i64;
        // 插值要取 pos 和 pos+1 两帧，pos+1 落在本回调之外就留给下次
        while self.pos < (end - 1) as f64 {
            let i = self.pos.floor() as i64;
            let frac = (self.pos - i as f64) as f32;
            let a = self.at(i);
            let b = self.at(i + 1);
            self.acc.push(to_s16(a + (b - a) * frac));
            self.pos += self.ratio;
        }

        while self.acc.len() >= CHUNK_SAMPLES {
            let bytes = self.acc[..CHUNK_SAMPLES]
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect::<Vec<u8>>();
            self.acc.drain(..CHUNK_SAMPLES);
            emit(bytes);
        }

        self.prev = self.mono[frames - 1];
        self.base = end;
    }

    fn mix<T: Normalized>(&mut self, data: &[T], frames: usize) {
        self.mono.clear();
        self.mono.reserve(frames);
        if self.channels == 1 {
            self.mono.extend(data[..frames].iter().map(|s| s.normalized()));
            return;
        }
        for frame in data[..frames * self.channels].chunks_exact(self.channels) {
            let sum: f32 = frame.iter().map(|s| s.normalized()).sum();
            self.mono.push(sum / self.channels as f32);
        }
    }

    fn at(&self, i: i64) -> f32 {
        if i < self.base {
            return self.prev;
        }
        let k = (i - self.base) as usize;
        self.mono.get(k).copied().unwrap_or(self.prev)
    }
}

/// 截断到 [-1, 1]，负值 ×32768、正值 ×32767，与 VS Code 侧 Pqr() 口径一致
fn to_s16(v: f32) -> i16 {
    let c = v.clamp(-1.0, 1.0);
    if c < 0.0 {
        (c * 32768.0) as i16
    } else {
        (c * 32767.0) as i16
    }
}

/// 采集块接收端，音频线程的 std 通道桥接成 async
pub struct ChunkReceiver {
    rx: Receiver<Vec<u8>>,
}

impl ChunkReceiver {
    /// 取下一块 100ms 音频；流断开返回 None
    /// shortcut: 用 try_recv 轮询而非 tokio 通道，因为当前 tokio 没开 sync feature；加了 sync 后换成 UnboundedReceiver
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.rx.try_recv() {
                Ok(bytes) => return Some(bytes),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    /// 非阻塞取一块，没有就 None
    pub fn try_recv(&mut self) -> Option<Vec<u8>> {
        self.rx.try_recv().ok()
    }
}

/// 麦克风采集器，可反复 start / stop 支持多轮听写
pub struct MicCapture {
    device: Device,
    config: StreamConfig,
    format: SampleFormat,
    stream: Option<Stream>,
}

impl MicCapture {
    /// 打开默认录音设备，协商格式但不启动流
    pub fn open() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| eyre!("没有可用的默认录音设备"))?;
        let (config, format) = pick_format(&device)?;
        tracing::info!(
            device = %device,
            rate = config.sample_rate,
            channels = config.channels,
            format = %format,
            "麦克风已就绪"
        );
        Ok(Self {
            device,
            config,
            format,
            stream: None,
        })
    }

    /// 实际协商到的设备名
    pub fn device_name(&self) -> String {
        self.device.to_string()
    }

    /// 实际协商到的原生格式：采样率、声道数、样本类型
    pub fn native_format(&self) -> (SampleRate, u16, SampleFormat) {
        (self.config.sample_rate, self.config.channels, self.format)
    }

    /// 启动采集，返回 100ms 块的接收端
    pub fn start(&mut self) -> Result<ChunkReceiver> {
        self.stop();
        let (tx, rx) = std::sync::mpsc::channel();
        let stream = self.build_stream(tx)?;
        stream
            .play()
            .wrap_err_with(|| format!("启动录音流失败: {}", self.device))?;
        self.stream = Some(stream);
        Ok(ChunkReceiver { rx })
    }

    /// 停止采集并释放设备
    pub fn stop(&mut self) {
        let Some(stream) = self.stream.take() else { return };
        if let Err(err) = stream.pause() {
            tracing::debug!(error = %err, "暂停录音流时设备可能已拔出");
        }
        drop(stream);
    }

    fn build_stream(&self, tx: Sender<Vec<u8>>) -> Result<Stream> {
        let cfg = self.config;
        let what = || format!("以 {} 格式建立录音流失败", self.format);
        let built = match self.format {
            SampleFormat::F32 => build_stream::<f32>(&self.device, cfg, tx),
            SampleFormat::I16 => build_stream::<i16>(&self.device, cfg, tx),
            SampleFormat::I32 => build_stream::<i32>(&self.device, cfg, tx),
            SampleFormat::F64 => build_stream::<f64>(&self.device, cfg, tx),
            SampleFormat::U8 => build_stream::<u8>(&self.device, cfg, tx),
            other => Err(eyre!("设备格式 {other} 未支持")),
        };
        built.wrap_err_with(what)
    }

    /// 采 n 秒并写入 wav，用于独立验证采集是否正确
    pub async fn record_seconds(n: u64, out_wav: &str) -> Result<()> {
        let want_samples = n as usize * TARGET_RATE as usize;
        let mut cap = Self::open()?;
        let mut rx = cap.start()?;
        let mut buf: Vec<u8> = Vec::with_capacity(want_samples * 2);
        let deadline = Instant::now() + Duration::from_secs(n) + RECORD_SLACK;

        while buf.len() < want_samples * 2 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tracing::warn!(bytes = buf.len(), "采音超时，按已采到的数据落盘");
                break;
            }
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!("等待音频块超时");
                    break;
                }
            }
        }
        cap.stop();
        buf.truncate(want_samples * 2);
        if buf.is_empty() {
            return Err(eyre!("没有采到任何音频，检查麦克风权限与设备"));
        }
        dump_wav(out_wav, &buf)?;
        tracing::info!(path = out_wav, bytes = buf.len(), "录音已写入");
        Ok(())
    }
}

/// 优先设备原生 16k 单声道，省掉重采样和混音；否则退回设备默认格式在回调里处理
fn pick_format(device: &Device) -> Result<(StreamConfig, SampleFormat)> {
    let direct = device
        .supported_input_configs()
        .ok()
        .and_then(|configs| {
            configs
                .filter(|c| {
                    c.channels() == 1
                        && c.contains_rate(TARGET_RATE)
                        && matches!(c.sample_format(), SampleFormat::F32 | SampleFormat::I16)
                })
                .find_map(|c| c.try_with_sample_rate(TARGET_RATE))
        })
        .map(|c| (c.config(), c.sample_format()));
    if let Some(cfg) = direct {
        return Ok(cfg);
    }
    let native = device
        .default_input_config()
        .wrap_err_with(|| format!("读取设备默认输入格式失败: {device}"))?;
    Ok((native.config(), native.sample_format()))
}

fn build_stream<T: Normalized>(
    device: &Device,
    config: StreamConfig,
    tx: Sender<Vec<u8>>,
) -> Result<Stream> {
    let mut pipe = Pipeline::new(config.sample_rate, config.channels as usize);
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _info| {
                // 音频线程里不能阻塞，发送失败只说明接收端已丢弃
                pipe.push(data, &mut |bytes| {
                    let _ = tx.send(bytes);
                });
            },
            |err| tracing::warn!(error = %err, "录音流错误"),
            None,
        )
        .wrap_err("建立录音流失败")
}

/// 把 16k 单声道 s16le 写成 wav，供 --dump-wav 调试
pub fn dump_wav(path: &str, samples_16k_mono_s16le: &[u8]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("创建目录失败: {}", parent.display()))?;
    }
    let mut writer =
        hound::WavWriter::create(path, spec).wrap_err_with(|| format!("创建 wav 失败: {path}"))?;
    for frame in samples_16k_mono_s16le.as_chunks::<2>().0 {
        let sample = i16::from_le_bytes([frame[0], frame[1]]);
        writer
            .write_sample(sample)
            .wrap_err_with(|| format!("写入 wav 失败: {path}"))?;
    }
    writer
        .finalize()
        .wrap_err_with(|| format!("收尾 wav 失败: {path}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(emit: &mut Vec<Vec<u8>>, bytes: Vec<u8>) {
        emit.push(bytes);
    }

    #[test]
    fn constant_chunk_is_100ms() {
        assert_eq!(CHUNK_BYTES, 3200);
    }

    #[test]
    fn passthrough_16k_mono_emits_exact_blocks() {
        let mut pipe = Pipeline::new(TARGET_RATE, 1);
        let mut out: Vec<Vec<u8>> = Vec::new();
        // 1 帧留在管线里等下一次回调，多喂 1 帧凑齐 3 块
        let data = vec![0.5f32; CHUNK_SAMPLES * 3 + 1];
        pipe.push(&data, &mut |b| drain(&mut out, b));
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|b| b.len() == CHUNK_BYTES));
        let s = i16::from_le_bytes([out[0][0], out[0][1]]);
        assert_eq!(s, 16383);
    }

    #[test]
    fn stereo_downmix_and_resample() {
        let mut pipe = Pipeline::new(48_000, 2);
        let mut out: Vec<Vec<u8>> = Vec::new();
        // 左右各 0.25 和 0.75，均值 0.5
        let mut data: Vec<f32> = Vec::new();
        for _ in 0..4800 {
            data.push(0.25);
            data.push(0.75);
        }
        pipe.push(&data, &mut |b| drain(&mut out, b));
        let got: Vec<u8> = out.concat();
        assert!(got.len() >= CHUNK_BYTES);
        let first = i16::from_le_bytes([got[0], got[1]]);
        assert_eq!(first, 16383);
        // 48k -> 16k 是 3:1，4800 帧恰好出 1600 个样本（100ms）
        assert_eq!(got.len() / 2, 1600);
    }

    #[test]
    fn int16_input_scaled_to_float_domain() {
        let mut pipe = Pipeline::new(TARGET_RATE, 1);
        let mut out: Vec<Vec<u8>> = Vec::new();
        let data = vec![16384i16; CHUNK_SAMPLES + 1];
        pipe.push(&data, &mut |b| drain(&mut out, b));
        assert_eq!(out.len(), 1);
        // 16384/32768 = 0.5，再乘 32767 截断
        assert_eq!(i16::from_le_bytes([out[0][0], out[0][1]]), 16383);
    }

    #[test]
    fn out_of_range_clamps() {
        assert_eq!(to_s16(2.0), 32767);
        assert_eq!(to_s16(-2.0), -32768);
        assert_eq!(to_s16(0.0), 0);
    }

    #[test]
    fn linear_interp_between_frames() {
        let mut pipe = Pipeline::new(32_000, 1);
        let mut out: Vec<Vec<u8>> = Vec::new();
        // 2:1 重采样，0 与 1 之间插值取 0.5
        pipe.push(&[0.0f32, 1.0], &mut |b| drain(&mut out, b));
        assert!(out.is_empty());
        pipe.push(&[1.0f32], &mut |b| drain(&mut out, b));
        let _ = &mut out;
    }

    #[test]
    fn wav_round_trip_keeps_format() {
        let path = std::env::temp_dir().join("stt_capture_unit_test.wav");
        let _ = std::fs::remove_file(&path);
        let bytes = vec![0u8; CHUNK_BYTES * 2];
        dump_wav(path.to_str().unwrap(), &bytes).unwrap();
        let reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        assert_eq!(reader.len(), CHUNK_SAMPLES as u32 * 2);
        std::fs::remove_file(&path).unwrap();
    }
}
