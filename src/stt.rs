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
// MAI 云端转写 WebSocket 客户端
//
// 传输层用同步 tungstenite + 专属工作线程：本 crate 未直接依赖 futures-util，tokio 的 sync/net 组合下
// WebSocketStream 的 Sink/Stream 无法导入；故用 std mpsc 对接 spawn_blocking 的 async 门面。
// 代理与 TLS 同 auth.rs 规则（CONNECT 隧道），其函数为私有，此处做最小本地复制

use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use color_eyre::eyre::{Context, Result, ensure, eyre};
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::{
    ClientHandshake, Error as WsError, HandshakeError,
    client::IntoClientRequest,
    protocol::{
        Message, WebSocket,
        frame::{CloseFrame, coding::CloseCode},
    },
};
use uuid::Uuid;

const ENDPOINT_HOST: &str = "falcon-caas.mai.microsoft.com";
const ENDPOINT_PATH: &str = "/voice-code/api/v1/realtime/transcription";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const INIT_TIMEOUT: Duration = Duration::from_secs(4);
const PING_INTERVAL: Duration = Duration::from_secs(25);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
// 工作线程靠读超时切片轮询指令通道；帧解码跨切片续读，不丢数据
const READ_SLICE: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(20);

// 实测协议约定：主动 close 用 4000，假 token 握手成功后被 4001 踢
const CLOSE_NORMAL: u16 = 4000;
const CLOSE_UNAUTHORIZED: u16 = 4001;

enum Cmd {
    Text(String),
    Close(u16, String),
}

enum Evt {
    SessionInit,
    Transcription {
        turn_id: String,
        status: String,
        text: String,
        // 已定稿前缀，partial 帧中途即有值；final 帧恒等于 text
        committed: String,
    },
    Error {
        code: i64,
        detail: String,
        terminal: bool,
    },
    Dead(String),
}

// 服务端下行帧，未声明字段忽略；audio_codec 实测可选，不解析
#[derive(Deserialize)]
struct ServerMsg {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    turn_id: String,
    // committed 恒存在，partial 为空串，按实测形状声明为 String 防类型漂移
    #[allow(dead_code)]
    #[serde(default)]
    committed: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    code: i64,
    #[serde(default)]
    terminal: bool,
}

// 明文与 TLS 两种隧道统一成 Read+Write 交给同步 WebSocket
enum Stream {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Stream {
    fn tcp_mut(&mut self) -> &mut TcpStream {
        match self {
            Stream::Plain(t) => t,
            Stream::Tls(t) => t.get_mut(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(t) => t.read(buf),
            Stream::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(t) => t.write(buf),
            Stream::Tls(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(t) => t.flush(),
            Stream::Tls(t) => t.flush(),
        }
    }
}

pub struct TranscriptionClient {
    cmd_tx: Sender<Cmd>,
    evt_rx: Receiver<Evt>,
    // 连接存活标志：worker 任何退出路径置 false，供空闲期判断是否需要重连
    connected: Arc<AtomicBool>,
    // 流式轮询挑走转写帧后，非转写帧暂存于此，保证 await_final 不漏 error/dead
    pending: VecDeque<Evt>,
}

impl TranscriptionClient {
    // token URL 编码进 query，无鉴权头；代理与 NO_PROXY 规则同 auth.rs
    pub async fn connect(token: &str) -> Result<Self> {
        let url = format!("wss://{ENDPOINT_HOST}{ENDPOINT_PATH}?token={}", url_encode(token));
        Self::open(url, false).await
    }

    // bypass_proxy 供单测直连本机假服务器，其余路径与生产一致
    async fn open(url: String, bypass_proxy: bool) -> Result<Self> {
        let mut client = tokio::task::spawn_blocking(move || establish(&url, bypass_proxy))
            .await
            .map_err(|e| eyre!("连接任务失败: {e}"))??;
        client.send_text(json!({"type":"start_session"}).to_string());
        client.wait_session_init(INIT_TIMEOUT).await?;
        Ok(client)
    }

    pub async fn start_turn(&mut self) -> String {
        let turn_id = Uuid::new_v4().to_string();
        self.send_text(json!({"type":"ptt_start","turn_id":turn_id}).to_string());
        turn_id
    }

    pub async fn push_audio(&mut self, turn_id: &str, pcm_s16le: &[u8]) {
        let audio = STANDARD.encode(pcm_s16le);
        self.send_text(json!({"type":"ptt_audio_chunk","turn_id":turn_id,"audio":audio}).to_string());
    }

    pub async fn end_turn(&mut self, turn_id: &str) {
        self.send_text(json!({"type":"ptt_end","turn_id":turn_id}).to_string());
    }

    // text 是累积全文非增量；超时返回最后一个 partial；从未收到本 turn 帧则 Err；terminal 错误立即 Err
    pub async fn await_final(&mut self, turn_id: &str, timeout: Duration) -> Result<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last: Option<String> = None;
        // 流式轮询暂存的帧先消化，final 与终端错误不被漏掉
        for evt in std::mem::take(&mut self.pending) {
            if let Some(r) = absorb(evt, turn_id, &mut last) {
                return r;
            }
        }
        loop {
            match self.evt_rx.try_recv() {
                Ok(evt) => {
                    if let Some(r) = absorb(evt, turn_id, &mut last) {
                        return r;
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    for evt in std::mem::take(&mut self.pending) {
                        if let Some(r) = absorb(evt, turn_id, &mut last) {
                            return r;
                        }
                    }
                    return Err(eyre!("工作线程已退出"));
                }
                Err(TryRecvError::Empty) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return last.map(Ok).unwrap_or_else(|| {
                    Err(eyre!("等待 final 超时且未收到本 turn 的任何转写帧"))
                });
            }
            tokio::time::sleep(POLL).await;
        }
    }

    // 非阻塞捞本 turn 已定稿前缀，多条积压取最新；软错误与非本 turn 帧同 await_final 规则消化
    pub fn try_committed(&mut self, turn_id: &str) -> String {
        let mut committed = String::new();
        loop {
            let evt = match self.pending.pop_front() {
                Some(e) => e,
                None => match self.evt_rx.try_recv() {
                    Ok(e) => e,
                    Err(_) => return committed,
                },
            };
            // final、终端错误、死亡帧停手放回队头保持原序，其余帧消化跳过
            let sticky = match &evt {
                Evt::Transcription { turn_id: t, status, .. } => {
                    t == turn_id && status == "final"
                }
                Evt::Error { terminal: true, .. } => true,
                Evt::Dead(_) => true,
                _ => false,
            };
            if sticky {
                self.pending.push_front(evt);
                return committed;
            }
            if let Evt::Transcription {
                turn_id: t,
                committed: c,
                ..
            } = evt
                && t == turn_id
            {
                committed = c;
            }
        }
    }

    pub async fn close(&mut self) {
        let _ = self
            .cmd_tx
            .send(Cmd::Close(CLOSE_NORMAL, "client close".to_string()));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            match self.evt_rx.try_recv() {
                Err(TryRecvError::Disconnected) => return,
                Ok(Evt::Dead(_)) => return,
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    // 连接是否仍然存活：空闲期据此决定要不要重连
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn wait_session_init(&mut self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.evt_rx.try_recv() {
                Ok(Evt::SessionInit) => return Ok(()),
                // 会话建立前的任何错误都视为致命
                Ok(Evt::Error { code, detail, .. }) => {
                    return Err(eyre!("服务端错误 code={code} {detail}"));
                }
                Ok(Evt::Dead(reason)) => return Err(eyre!(reason)),
                Ok(_) => {}
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return Err(eyre!("工作线程已退出")),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(eyre!("等待 session_init 超时"));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn send_text(&mut self, text: String) {
        // 通道断开说明工作线程已退出，错误由后续 await 统一暴露
        let _ = self.cmd_tx.send(Cmd::Text(text));
    }
}

// 单帧归约：final 返回全文，终端错误或连接死亡返回 Err，其余只推进 last
fn absorb(evt: Evt, turn_id: &str, last: &mut Option<String>) -> Option<Result<String>> {
    match evt {
        Evt::Transcription {
            turn_id: t,
            status,
            text,
            committed: _,
        } if t == turn_id => {
            if status == "final" {
                Some(Ok(text))
            } else {
                *last = Some(text);
                None
            }
        }
        Evt::Error {
            code,
            detail,
            terminal: true,
        } => Some(Err(eyre!("服务端错误 code={code} {detail}"))),
        Evt::Dead(reason) => Some(Err(eyre!(reason))),
        _ => None,
    }
}

fn establish(url: &str, bypass_proxy: bool) -> Result<TranscriptionClient> {
    let (secure, host, port) = split_ws_url(url)?;
    let stream = if secure {
        let base = match proxy_for(&host, bypass_proxy) {
            Some((ph, pp)) => via_proxy(&ph, pp, &host, port)?,
            None => connect_plain(&host, port)?,
        };
        let connector = native_tls::TlsConnector::new().map_err(|e| eyre!("初始化 TLS 失败: {e}"))?;
        let tls = connector
            .connect(&host, base)
            .map_err(|e| eyre!("与 {host} 建立 TLS 失败: {e}"))?;
        Stream::Tls(Box::new(tls))
    } else {
        Stream::Plain(connect_plain(&host, port)?)
    };
    let mut ws = ws_handshake(url, stream)?;
    // 握手后把读超时缩短为轮询片，写超时保持 8s
    ws.get_mut()
        .tcp_mut()
        .set_read_timeout(Some(READ_SLICE))
        .wrap_err("设置读超时失败")?;

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel();
    let connected = Arc::new(AtomicBool::new(false));
    let worker_flag = Arc::clone(&connected);
    thread::spawn(move || worker_loop(ws, cmd_rx, evt_tx, worker_flag));
    Ok(TranscriptionClient {
        cmd_tx,
        evt_rx,
        connected,
        pending: VecDeque::new(),
    })
}

fn ws_handshake(url: &str, stream: Stream) -> Result<WebSocket<Stream>> {
    let request = url
        .into_client_request()
        .map_err(|e| eyre!("构造握手请求失败: {e}"))?;
    let started =
        ClientHandshake::start(stream, request, None).map_err(|e| eyre!("WS 握手启动失败: {e}"))?;
    match started.handshake() {
        Ok((ws, _)) => Ok(ws),
        Err(HandshakeError::Failure(e)) => Err(eyre!("WS 握手失败: {e}")),
        Err(HandshakeError::Interrupted(_)) => Err(eyre!("WS 握手未完成")),
    }
}

// 工作线程一退出就把存活标志落下，覆盖全部 return 与 panic 路径，不必逐条重复写
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

fn worker_loop(
    mut ws: WebSocket<Stream>,
    cmd_rx: Receiver<Cmd>,
    evt_tx: Sender<Evt>,
    connected: Arc<AtomicBool>,
) {
    let _alive = AliveGuard(Arc::clone(&connected));
    // revision 表按 turn 记账，未登记的 turn 首帧默认从 0 起
    let mut revisions: HashMap<String, u64> = HashMap::new();
    let mut last_ping = Instant::now();
    let mut ping_at: Option<Instant> = None;

    loop {
        // 排干所有待发命令再读：一轮内可追赶多条音频块，避免每 100ms 只发一条
        let mut dropped = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(Cmd::Text(t)) => {
                    if let Err(e) = ws.send(Message::text(t)) {
                        let _ = evt_tx.send(Evt::Dead(format!("发送失败: {e}")));
                        return;
                    }
                }
                Ok(Cmd::Close(code, reason)) => {
                    graceful_close(&mut ws, code, &reason);
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    dropped = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if dropped {
            graceful_close(&mut ws, CLOSE_NORMAL, "client dropped");
            return;
        }

        if last_ping.elapsed() >= PING_INTERVAL {
            last_ping = Instant::now();
            ping_at = Some(last_ping);
            if let Err(e) = ws.send(Message::text(json!({"type":"ping"}).to_string())) {
                let _ = evt_tx.send(Evt::Dead(format!("发送 ping 失败: {e}")));
                return;
            }
        }
        // pong 超时按约定用 4000 主动断开
        if let Some(at) = ping_at
            && at.elapsed() >= PONG_TIMEOUT
        {
            tracing::warn!("pong 超时，主动关闭连接");
            graceful_close(&mut ws, CLOSE_NORMAL, "pong timeout");
            return;
        }

        match ws.read() {
            Ok(Message::Text(text)) => {
                let Ok(msg) = serde_json::from_str::<ServerMsg>(text.as_str()) else {
                    continue;
                };
                match msg.kind.as_str() {
                    "session_init" => {
                        // 先落存活标志再发事件：客户端收到事件时标志必已为 true，无跨线程竞态
                        connected.store(true, Ordering::Relaxed);
                        if evt_tx.send(Evt::SessionInit).is_err() {
                            return;
                        }
                    }
                    "pong" => ping_at = None,
                    "transcription" => {
                        let last = revisions.entry(msg.turn_id.clone()).or_insert(0);
                        // 服务端可能乱序补发旧 revision，<= 已见值丢弃
                        if msg.revision <= *last {
                            continue;
                        }
                        let evt = Evt::Transcription {
                            turn_id: msg.turn_id.clone(),
                            status: msg.status.clone(),
                            text: msg.text.clone(),
                            committed: msg.committed.clone(),
                        };
                        if evt_tx.send(evt).is_err() {
                            return;
                        }
                        *last = msg.revision;
                    }
                    "error" => {
                        let terminal = msg.terminal;
                        let evt = Evt::Error {
                            code: msg.code,
                            detail: msg.detail.clone(),
                            terminal,
                        };
                        if evt_tx.send(evt).is_err() || terminal {
                            return;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(frame)) => {
                let (code, reason) = match &frame {
                    Some(f) => (u16::from(f.code), f.reason.as_str().to_string()),
                    None => (u16::from(CloseCode::Abnormal), String::new()),
                };
                let _ = evt_tx.send(Evt::Dead(close_message(code, &reason)));
                return;
            }
            Ok(_) => {}
            Err(ref e) if is_io_timeout(e) => {}
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => {
                let _ = evt_tx.send(Evt::Dead("连接已关闭".to_string()));
                return;
            }
            Err(e) => {
                let _ = evt_tx.send(Evt::Dead(format!("连接异常: {e}")));
                return;
            }
        }
    }
}

// 主动关闭：排队 close 帧后短暂驱动读循环把回帧刷出，随后整体 Drop
fn graceful_close(ws: &mut WebSocket<Stream>, code: u16, reason: &str) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    };
    let _ = ws.close(Some(frame));
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match ws.read() {
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => break,
            Err(ref e) if is_io_timeout(e) => {}
            _ => break,
        }
    }
}

fn close_message(code: u16, reason: &str) -> String {
    // 实测：假 token 在握手成功后被 4001 踢下，必须映射成明确的 token 错误
    let head = if code == CLOSE_UNAUTHORIZED {
        "GitHub token 无效或未登录 Voice Mode"
    } else {
        "服务端关闭连接"
    };
    if reason.is_empty() {
        format!("{head}（{code}）")
    } else {
        format!("{head}（{code}: {reason}）")
    }
}

fn is_io_timeout(e: &WsError) -> bool {
    matches!(e, WsError::Io(io_err) if matches!(io_err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock))
}

fn split_ws_url(url: &str) -> Result<(bool, String, u16)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| eyre!("URL 缺少协议"))?;
    let secure = match scheme {
        "wss" => true,
        "ws" => false,
        s => return Err(eyre!("不支持的协议 {s}")),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    // 端口可选，ws 默认 80、wss 默认 443；不支持 IPv6 字面量
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p))
            if !h.contains(':')
                && !p.is_empty()
                && p.chars().all(|c| c.is_ascii_digit()) =>
        {
            (h.to_string(), p.parse::<u16>().wrap_err("端口无效")?)
        }
        _ => (authority.to_string(), if secure { 443 } else { 80 }),
    };
    ensure!(!host.is_empty(), "URL 缺少主机名");
    Ok((secure, host, port))
}

fn resolve_addr(host: &str, port: u16) -> Result<SocketAddr> {
    let mut addrs = (host, port)
        .to_socket_addrs()
        .wrap_err_with(|| format!("解析 {host}:{port} 失败"))?;
    addrs
        .next()
        .ok_or_else(|| eyre!("{host}:{port} 没有可用地址"))
}

fn connect_plain(host: &str, port: u16) -> Result<TcpStream> {
    let addr = resolve_addr(host, port)?;
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .wrap_err_with(|| format!("连接 {host}:{port} 超时"))?;
    let _ = tcp.set_read_timeout(Some(CONNECT_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(CONNECT_TIMEOUT));
    Ok(tcp)
}

fn via_proxy(ph: &str, pp: u16, host: &str, port: u16) -> Result<TcpStream> {
    let target = format!("{host}:{port}");
    let addr = resolve_addr(ph, pp).wrap_err_with(|| format!("解析代理 {ph}:{pp} 失败"))?;
    let mut tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .wrap_err_with(|| format!("连接代理 {ph}:{pp} 超时"))?;
    let _ = tcp.set_read_timeout(Some(CONNECT_TIMEOUT));
    let req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    tcp.write_all(req.as_bytes())
        .wrap_err("向代理发送 CONNECT 失败")?;
    tcp.flush().wrap_err("刷新 CONNECT 请求失败")?;
    let status = read_line(&mut tcp)?;
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    ensure!(code == 200, "代理拒绝 CONNECT {target}: {status}");
    // 剩余响应头逐行丢弃直到空行，无正文，TLS 接手同一 socket
    loop {
        let line = read_line(&mut tcp)?;
        if line.is_empty() {
            break;
        }
    }
    Ok(tcp)
}

fn read_line(stream: &mut impl Read) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).wrap_err("读取代理响应失败")?;
        ensure!(n > 0, "代理在完成响应前关闭了连接");
        buf.push(byte[0]);
        if buf.ends_with(b"\n") {
            return Ok(String::from_utf8_lossy(&buf).trim_end().to_string());
        }
        ensure!(buf.len() <= 8192, "代理响应单行超过 8KB 上限");
    }
}

// 代理判定与 auth.rs 同规则：只支持 http 方案的 CONNECT 代理，NO_PROXY 命中直连
fn proxy_for(host: &str, bypass: bool) -> Option<(String, u16)> {
    if bypass {
        return None;
    }
    let raw = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .into_iter()
        .find_map(|k| std::env::var(k).ok())?;
    let raw = raw.trim();
    if raw.is_empty() || in_no_proxy(host) {
        return None;
    }
    let without_scheme = raw.split_once("://").map(|(_, r)| r).unwrap_or(raw);
    let authority = without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(without_scheme);
    let (h, p) = authority.rsplit_once(':')?;
    if h.is_empty() {
        return None;
    }
    let port = p.parse::<u16>().ok()?;
    (port > 0).then(|| (h.to_string(), port))
}

fn in_no_proxy(host: &str) -> bool {
    let list = ["NO_PROXY", "no_proxy"]
        .into_iter()
        .find_map(|k| std::env::var(k).ok())
        .unwrap_or_default();
    let host = host.to_ascii_lowercase();
    list.split(',').map(str::trim).any(|entry| {
        if entry == "*" {
            return true;
        }
        // 前导 * 或 . 都表示后缀匹配，裸域名同时匹配自身
        let suffix = entry.trim_start_matches('*').trim_start_matches('.');
        !suffix.is_empty() && (host == suffix || host.ends_with(&format!(".{suffix}")))
    })
}

fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tokio_tungstenite::tungstenite;

    fn fake_server<F>(scenario: F) -> SocketAddr
    where
        F: FnOnce(&mut WebSocket<TcpStream>) + Send + 'static,
    {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定假服务器失败");
        let addr = l.local_addr().unwrap();
        thread::spawn(move || {
            let Ok((tcp, _)) = l.accept() else { return };
            let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
            if let Ok(mut ws) = tungstenite::accept(tcp) {
                scenario(&mut ws);
            }
        });
        addr
    }

    fn send_json(ws: &mut WebSocket<TcpStream>, v: Value) {
        let _ = ws.send(Message::text(v.to_string()));
    }

    // 帧形与实测样本一致：committed 恒存在，partial 中途即携带已定稿前缀
    fn transcription(
        turn: &str,
        text: &str,
        status: &str,
        revision: u64,
        committed: &str,
    ) -> Value {
        json!({
            "type":"transcription","text":text,"status":status,
            "committed":committed,
            "turn_id":turn,"revision":revision
        })
    }

    fn client_url(addr: SocketAddr) -> String {
        format!("ws://{addr}{ENDPOINT_PATH}?token=fake")
    }

    // 跳过无关指令帧，返回首个 type==want 的帧；读超时返回 Null
    fn next_cmd_text(ws: &mut WebSocket<TcpStream>, want: &str) -> Value {
        loop {
            let Ok(m) = ws.read() else { return Value::Null };
            let Ok(t) = m.to_text() else { continue };
            let Ok(v) = serde_json::from_str::<Value>(t) else {
                continue;
            };
            if v["type"].as_str() == Some(want) {
                return v;
            }
        }
    }

    #[tokio::test]
    async fn replays_three_frames_final_with_audio_count() -> Result<()> {
        let addr = fake_server(|ws| {
            send_json(
                ws,
                json!({"type":"session_init","session_id":"test","audio_codec":"wav"}),
            );
            let start = next_cmd_text(ws, "ptt_start");
            let turn = start["turn_id"].as_str().unwrap_or("").to_string();
            let mut chunks = 0usize;
            let mut b64_len = 0usize;
            loop {
                let Ok(m) = ws.read() else { break };
                let Ok(t) = m.to_text() else { continue };
                let Ok(v) = serde_json::from_str::<Value>(t) else {
                    continue;
                };
                match v["type"].as_str().unwrap_or("") {
                    "ptt_audio_chunk" => {
                        chunks += 1;
                        b64_len += v["audio"].as_str().unwrap_or("").len();
                    }
                    "ptt_end" => break,
                    _ => {}
                }
            }
            send_json(ws, transcription(&turn, "测试听写一二。", "partial", 1, "测试听写。"));
            send_json(ws, transcription(&turn, "语音识别测。", "partial", 2, "语音识别。"));
            // 乱序补发旧 revision，客户端应丢弃
            send_json(ws, transcription(&turn, "STALE", "partial", 1, ""));
            let final_text = format!("FINAL chunks={chunks} b64={b64_len}");
            send_json(ws, transcription(&turn, &final_text, "final", 3, &final_text));
            thread::sleep(Duration::from_secs(2));
        });
        let mut c = TranscriptionClient::open(client_url(addr), true).await?;
        let turn = c.start_turn().await;
        c.push_audio(&turn, &[0u8; 3200]).await;
        c.push_audio(&turn, &[7u8; 3200]).await;
        c.end_turn(&turn).await;
        // 3200 字节 s16le，base64 各 4268 字符（含填充）
        let out = c.await_final(&turn, Duration::from_secs(5)).await?;
        assert_eq!(out, "FINAL chunks=2 b64=8536");
        Ok(())
    }

    #[tokio::test]
    async fn stale_revision_dropped_timeout_returns_last_partial() -> Result<()> {
        let addr = fake_server(|ws| {
            send_json(ws, json!({"type":"session_init","session_id":"t"}));
            let start = next_cmd_text(ws, "ptt_start");
            let turn = start["turn_id"].as_str().unwrap_or("").to_string();
            send_json(ws, transcription(&turn, "P2", "partial", 2, "P2"));
            // 若 <= 已见 revision 未被丢弃，await_final 会拿到 P1-STALE
            send_json(ws, transcription(&turn, "P1-STALE", "partial", 1, ""));
            thread::sleep(Duration::from_secs(3));
        });
        let mut c = TranscriptionClient::open(client_url(addr), true).await?;
        let turn = c.start_turn().await;
        let out = c.await_final(&turn, Duration::from_millis(800)).await?;
        assert_eq!(out, "P2");
        Ok(())
    }

    #[tokio::test]
    async fn timeout_without_frames_is_error() -> Result<()> {
        let addr = fake_server(|ws| {
            send_json(ws, json!({"type":"session_init","session_id":"t"}));
            thread::sleep(Duration::from_secs(3));
        });
        let mut c = TranscriptionClient::open(client_url(addr), true).await?;
        let turn = c.start_turn().await;
        let err = c
            .await_final(&turn, Duration::from_millis(400))
            .await
            .expect_err("应有错误");
        assert!(err.to_string().contains("超时"), "错误内容: {err}");
        Ok(())
    }

    #[tokio::test]
    async fn try_committed_streams_prefix_and_spares_final() -> Result<()> {
        let addr = fake_server(|ws| {
            send_json(ws, json!({"type":"session_init","session_id":"t"}));
            let start = next_cmd_text(ws, "ptt_start");
            let turn = start["turn_id"].as_str().unwrap_or("").to_string();
            send_json(ws, transcription(&turn, "你好", "partial", 1, "你好"));
            send_json(ws, transcription(&turn, "你好世界", "partial", 2, "你好世界"));
            // 软错误帧应被暂存绕过，不影响流式轮询与 final
            send_json(ws, json!({"type":"error","code":0,"detail":"soft","turn_id":&turn}));
            send_json(ws, transcription(&turn, "你好世界。", "final", 3, "你好世界。"));
            thread::sleep(Duration::from_secs(2));
        });
        let mut c = TranscriptionClient::open(client_url(addr), true).await?;
        let turn = c.start_turn().await;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = String::new();
        while got != "你好世界" && Instant::now() < deadline {
            let latest = c.try_committed(&turn);
            if !latest.is_empty() {
                got = latest;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(got, "你好世界");
        c.end_turn(&turn).await;
        // final 被 try_committed 暂存而非吞掉，await_final 仍能拿到
        let out = c.await_final(&turn, Duration::from_secs(5)).await?;
        assert_eq!(out, "你好世界。");
        Ok(())
    }

    #[tokio::test]
    async fn closed_connection_survives_stream_polling() -> Result<()> {
        let addr = fake_server(|ws| {
            send_json(ws, json!({"type":"session_init","session_id":"t"}));
            let start = next_cmd_text(ws, "ptt_start");
            let turn = start["turn_id"].as_str().unwrap_or("").to_string();
            send_json(ws, transcription(&turn, "P", "partial", 1, "P"));
            let _ = ws.close(None);
            thread::sleep(Duration::from_millis(300));
        });
        let mut c = TranscriptionClient::open(client_url(addr), true).await?;
        let turn = c.start_turn().await;
        // 捞走 partial；worker 收 Close 发出的 Dead 帧被暂存
        let deadline = Instant::now() + Duration::from_secs(2);
        while c.try_committed(&turn).is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        c.end_turn(&turn).await;
        let started = Instant::now();
        let err = c
            .await_final(&turn, Duration::from_secs(5))
            .await
            .expect_err("断连后应报错");
        // 暂存的 Dead 帧让 await_final 立即失败，而不是耗尽 5 秒超时
        assert!(started.elapsed() < Duration::from_secs(4), "应快速失败");
        assert!(!err.to_string().contains("超时"), "错误内容: {err}");
        Ok(())
    }

    #[tokio::test]
    async fn fake_token_close_4001_maps_to_token_error() -> Result<()> {
        let addr = fake_server(|ws| {
            let frame = CloseFrame {
                code: CloseCode::from(CLOSE_UNAUTHORIZED),
                reason: "Sign in to GitHub to use Voice Mode".to_string().into(),
            };
            let _ = ws.close(Some(frame));
            thread::sleep(Duration::from_millis(500));
        });
        let Err(err) = TranscriptionClient::open(client_url(addr), true).await else {
            return Err(eyre!("假 token 应失败"));
        };
        let text = format!("{err:?}");
        assert!(text.contains("token 无效"), "错误内容: {text}");
        Ok(())
    }

    #[tokio::test]
    async fn connected_flag_true_in_session_false_after_server_close() -> Result<()> {
        let addr = fake_server(|ws| {
            send_json(ws, json!({"type":"session_init","session_id":"t"}));
            let _ = next_cmd_text(ws, "ptt_start");
            let _ = ws.close(None);
            thread::sleep(Duration::from_millis(500));
        });
        let mut c = TranscriptionClient::open(client_url(addr), true).await?;
        assert!(c.is_connected(), "session_init 后应为已连接");
        let turn = c.start_turn().await;
        let err = c
            .await_final(&turn, Duration::from_secs(2))
            .await
            .expect_err("服务端关闭应报错");
        assert!(err.to_string().contains("服务端关闭连接"), "错误内容: {err}");
        // Dead 事件先发于 worker 收尾，标志要等线程退出才落下，轮询到超时为止
        for _ in 0..50 {
            if !c.is_connected() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!c.is_connected(), "Dead 后应为已断开");
        Ok(())
    }

    #[test]
    fn url_split_and_proxy_rules() {
        let (secure, host, port) = split_ws_url("wss://example.com/api/x?a=b").unwrap();
        assert!(secure && host == "example.com" && port == 443);
        let (secure, host, port) = split_ws_url("ws://127.0.0.1:54321/api").unwrap();
        assert!(!secure && host == "127.0.0.1" && port == 54321);
        assert!(split_ws_url("http://example.com").is_err());

        let g = EnvGuard::new(&["HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"]);
        g.set("NO_PROXY", "");
        g.set("HTTPS_PROXY", "http://127.0.0.1:7897");
        assert_eq!(
            proxy_for("example.com", false),
            Some(("127.0.0.1".to_string(), 7897))
        );
        assert_eq!(proxy_for("example.com", true), None);
        g.set("NO_PROXY", "localhost,127.0.0.1");
        assert_eq!(proxy_for("127.0.0.1", false), None);
    }

    // 环境变量是进程级状态，仅此测试读写；异步测试全部 bypass 不碰代理
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&'static str]) -> Self {
            let saved = keys
                .iter()
                .map(|k| (*k, std::env::var(k).ok()))
                .collect();
            Self { saved }
        }

        fn set(&self, key: &str, value: &str) {
            unsafe { std::env::set_var(key, value) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }
}
