// GitHub 令牌获取：环境变量 -> 本地便携缓存 -> gh CLI -> OAuth 设备码流程 四级回退
// 缓存是 exe 同目录的明文 token.json：便携优先的取舍，文件夹整体拷走凭据跟着走，
// 目录读写权限由用户自行负责，不落任何用户主目录/应用数据目录

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{Context, ContextCompat, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const VSCODE_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const TOKEN_ENV_VAR: &str = "STT_GITHUB_TOKEN";
// 总超时在 expires_in 之上加缓冲，容忍本机与 GitHub 的时钟差
const POLL_GRACE: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const GH_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Env,
    Cache,
    Gh,
    Device,
}

impl std::fmt::Display for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            AuthMode::Env => "env",
            AuthMode::Cache => "cache",
            AuthMode::Gh => "gh",
            AuthMode::Device => "device",
        };
        f.write_str(name)
    }
}

pub struct GitHubToken {
    pub access_token: String,
    pub mode: AuthMode,
}

// 手写 Debug，默认 derive 会把令牌原文带进任何调试输出
impl std::fmt::Debug for GitHubToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubToken")
            .field("access_token", &mask_token(&self.access_token))
            .field("mode", &self.mode)
            .finish()
    }
}

pub struct DeviceSession {
    // device_code 属敏感值，保持私有，只在模块内部用于轮询
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

impl DeviceSession {
    pub fn device_code_masked(&self) -> String {
        mask_token(&self.device_code)
    }
}

// 手写 Debug，device_code 是敏感值，不能跟着 {item:?} 漏出去
impl std::fmt::Debug for DeviceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSession")
            .field("device_code", &self.device_code_masked())
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

pub(crate) enum Poll {
    Pending,
    SlowedDown(u64),
    Authorized(String),
    Failed(String),
}

impl std::fmt::Debug for Poll {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Poll::Pending => f.write_str("Pending"),
            Poll::SlowedDown(i) => write!(f, "SlowedDown({i})"),
            Poll::Authorized(t) => write!(f, "Authorized({})", mask_token(t)),
            Poll::Failed(r) => write!(f, "Failed({r})"),
        }
    }
}

pub fn mask_token(token: &str) -> String {
    let len = token.chars().count();
    if len <= 4 {
        return format!("****(len={len})");
    }
    let head: String = token.chars().take(4).collect();
    format!("{head}****(len={len})")
}

const TOKEN_CACHE_FILE: &str = "token.json";

#[derive(Serialize, Deserialize)]
struct TokenCache {
    access_token: String,
    saved_at: String,
}

// 缓存跟随 exe 目录，实现整目录可拷走的便携语义；不碰用户主目录与应用数据目录
fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().wrap_err("无法定位自身可执行文件路径")?;
    let dir = exe.parent().wrap_err("可执行文件路径缺少父目录")?;
    Ok(dir.to_path_buf())
}

// unix 秒转 UTC 时间戳，为缓存时间戳引入日期库不值得
fn format_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let clock = secs % 86400;
    // Hinnant civil_from_days：格里历 400 年一轮，3 月为纪元起点
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        clock / 3600,
        (clock / 60) % 60,
        clock % 60
    )
}

fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format_utc(secs)
}

// 缺文件、坏 JSON、字段缺失一律按无缓存处理，让回退链继续走下去
fn read_cache_at(dir: &Path) -> Option<String> {
    let path = dir.join(TOKEN_CACHE_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| tracing::debug!("没有可用的令牌缓存 {path:?}: {e}"))
        .ok()?;
    let cache: TokenCache = serde_json::from_str(&text)
        .map_err(|e| tracing::debug!("令牌缓存解析失败，忽略: {e}"))
        .ok()?;
    (!cache.access_token.is_empty()).then_some(cache.access_token)
}

// 先写临时名再改名，避免掉电或并发写坏缓存；坏缓存下次启动只会被静默跳过
fn write_cache_at(dir: &Path, token: &str) -> Result<()> {
    let path = dir.join(TOKEN_CACHE_FILE);
    let tmp = dir.join(format!("{TOKEN_CACHE_FILE}.tmp"));
    let body = TokenCache { access_token: token.to_string(), saved_at: now_utc() };
    let json = serde_json::to_string(&body).wrap_err("序列化令牌缓存失败")?;
    std::fs::write(&tmp, json.as_bytes()).wrap_err("写入令牌缓存临时文件失败")?;
    std::fs::rename(&tmp, &path)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .wrap_err("令牌缓存改名失败")?;
    Ok(())
}

fn read_cache() -> Option<String> {
    let dir = match exe_dir() {
        Ok(dir) => dir,
        Err(e) => {
            tracing::debug!("令牌缓存路径不可用: {e}");
            return None;
        }
    };
    read_cache_at(&dir)
}

// 缓存是优化不是依赖，写失败只记日志，不影响本次令牌使用
fn write_cache(token: &str) {
    let outcome = exe_dir().and_then(|dir| write_cache_at(&dir, token));
    match outcome {
        Ok(()) => tracing::debug!("GitHub 令牌已缓存到本地"),
        Err(e) => tracing::warn!("令牌缓存写入失败，本次不缓存: {e}"),
    }
}

pub async fn get_token() -> Result<GitHubToken> {
    if let Some(access_token) = env_token() {
        tracing::info!("使用环境变量 {TOKEN_ENV_VAR} 中的令牌");
        return Ok(GitHubToken { access_token, mode: AuthMode::Env });
    }
    // 缓存命中就不再问 gh 也不再走授权；env 是显式临时来源，不写缓存
    if let Some(access_token) = read_cache() {
        tracing::info!("使用本地缓存令牌 {}", mask_token(&access_token));
        return Ok(GitHubToken { access_token, mode: AuthMode::Cache });
    }
    // gh 是子进程，放 blocking 池避免卡住异步运行时
    let gh = tokio::task::spawn_blocking(run_gh_token)
        .await
        .wrap_err("gh 凭据检查被中断")?;
    match gh {
        Ok(access_token) => {
            // 也写一份缓存，下次 gh 凭据失效或 gh 不在 PATH 时还能顶着用
            write_cache(&access_token);
            tracing::info!("使用 gh CLI 凭据");
            Ok(GitHubToken { access_token, mode: AuthMode::Gh })
        }
        Err(e) => {
            // 只记失败原因，gh 的 stdout 可能含令牌，不进日志
            tracing::info!("gh auth token 不可用，改用设备码授权: {e}");
            let access_token = device_flow().await?;
            Ok(GitHubToken { access_token, mode: AuthMode::Device })
        }
    }
}

/// 快速诊断当前会走哪一级，只看本地状态，不发起设备码流程
pub fn describe_auth_source() -> String {
    if let Some(t) = env_token() {
        return format!("mode=env source={TOKEN_ENV_VAR} 已设置，长度 {}", t.chars().count());
    }
    if let Some(t) = read_cache() {
        return format!(
            "mode=cache source={} 已缓存，长度 {}",
            TOKEN_CACHE_FILE,
            t.chars().count()
        );
    }
    match run_gh_token() {
        Ok(t) => format!("mode=gh source=gh auth token 可用，长度 {}", t.chars().count()),
        Err(e) => format!(
            "mode=device source=将发起设备码授权 client_id={VSCODE_CLIENT_ID}；gh 不可用原因: {e}"
        ),
    }
}

pub(crate) fn env_token() -> Option<String> {
    token_from_env(TOKEN_ENV_VAR)
}

fn token_from_env(var: &str) -> Option<String> {
    let raw = std::env::var(var).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn run_gh_token() -> Result<String> {
    run_gh_command(&mut gh_probe_cmd(), GH_TIMEOUT)
}

fn run_gh_command(cmd: &mut Command, limit: Duration) -> Result<String> {
    cmd.stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped());
    let mut child = cmd.spawn().wrap_err("未找到 gh 命令或无法启动")?;
    let mut pipe = child.stdout.take().wrap_err("无法读取 gh 输出")?;
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait().wrap_err("查询 gh 进程状态失败")? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("gh auth token 超过 {} 秒未返回", limit.as_secs());
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let mut buf = Vec::new();
    // 子进程已退出，管道里的字节有限，不会挂
    let _ = pipe.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    parse_gh_output(status.success(), &text).ok_or_else(|| {
        color_eyre::eyre::Report::msg(format!(
            "gh auth token 退出码 {} 或输出为空",
            status.code().unwrap_or(-1)
        ))
    })
}

fn parse_gh_output(success: bool, stdout: &str) -> Option<String> {
    if !success {
        return None;
    }
    let trimmed = stdout.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn device_code_body(client_id: &str) -> String {
    format!("client_id={}&scope=", url_encode(client_id))
}

fn access_token_body(client_id: &str, device_code: &str) -> String {
    format!(
        "client_id={}&device_code={}&grant_type={}",
        url_encode(client_id),
        url_encode(device_code),
        url_encode(DEVICE_GRANT_TYPE)
    )
}

// 协议约定：设备码两个接口既可能回 JSON 也可能回 form-urlencoded，两种都要认
fn decode_json_or_form(text: &str) -> Result<Value> {
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(text).wrap_err("解析 GitHub JSON 响应失败");
    }
    let mut map = serde_json::Map::new();
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), Value::String(percent_decode(v)));
    }
    Ok(Value::Object(map))
}

fn field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn numeric(v: &Value, key: &str) -> Option<u64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_u64(),
        // form 形式的数字也在字符串里
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn next_interval(error: Option<&str>, interval: u64) -> u64 {
    // slow_down 按设备码规范每次加 5 秒
    if error == Some("slow_down") { interval + 5 } else { interval }
}

fn decide_poll(v: &Value, interval: u64) -> Poll {
    if let Some(tok) = field(v, "access_token")
        && !tok.is_empty()
    {
        return Poll::Authorized(tok.to_string());
    }
    let error = field(v, "error");
    match error {
        None => Poll::Failed("GitHub 返回的令牌响应不完整".to_string()),
        Some("authorization_pending") | Some("pending") => Poll::Pending,
        Some("slow_down") => Poll::SlowedDown(next_interval(error, interval)),
        Some(other) => {
            let desc = field(v, "error_description").unwrap_or("");
            Poll::Failed(format!("设备码授权失败: {other} {desc}").trim().to_string())
        }
    }
}

fn into_session(v: &Value) -> Result<DeviceSession> {
    if let Some(err) = field(v, "error") {
        let desc = field(v, "error_description").unwrap_or("");
        bail!("申请设备码被拒绝: {err} {desc}");
    }
    Ok(DeviceSession {
        device_code: field(v, "device_code").wrap_err("设备码响应缺少 device_code")?.to_string(),
        user_code: field(v, "user_code").wrap_err("设备码响应缺少 user_code")?.to_string(),
        verification_uri: field(v, "verification_uri")
            .wrap_err("设备码响应缺少 verification_uri")?
            .to_string(),
        expires_in: numeric(v, "expires_in").unwrap_or(900),
        interval: numeric(v, "interval").unwrap_or(5).max(1),
    })
}

pub(crate) async fn request_device_code() -> Result<DeviceSession> {
    let body = device_code_body(VSCODE_CLIENT_ID);
    let resp = post_form(DEVICE_CODE_URL, &body)
        .await
        .wrap_err("请求 GitHub 设备码失败")?;
    ensure!(
        resp.status == 200,
        "申请设备码返回 HTTP {}: {}",
        resp.status,
        redact_body(&resp.body)
    );
    let v = decode_json_or_form(&resp.body).wrap_err("设备码响应无法识别")?;
    into_session(&v)
}

pub(crate) async fn poll_device_token(session: &mut DeviceSession) -> Result<Poll> {
    let body = access_token_body(VSCODE_CLIENT_ID, &session.device_code);
    let resp = post_form(ACCESS_TOKEN_URL, &body)
        .await
        .wrap_err("轮询设备码授权结果失败")?;
    // 协议约定：pending / slow_down 之类的错误 GitHub 可能回 200 也可能回 400，两种都要继续解析
    ensure!(
        matches!(resp.status, 200 | 400),
        "轮询返回 HTTP {}: {}",
        resp.status,
        redact_body(&resp.body)
    );
    let v = decode_json_or_form(&resp.body).wrap_err("令牌响应无法识别")?;
    session.interval = next_interval(field(&v, "error"), session.interval);
    Ok(decide_poll(&v, session.interval))
}

pub(crate) async fn device_flow() -> Result<String> {
    let mut session = request_device_code().await?;
    println!("------------------------------------------------------------");
    println!("  请在浏览器打开: {}", session.verification_uri);
    println!("  输入验证码:     {}", session.user_code);
    println!("  有效期 {} 秒，请在浏览器里点确认授权", session.expires_in);
    println!("------------------------------------------------------------");
    tracing::info!("设备码已下发 {}，有效期 {} 秒", session.user_code, session.expires_in);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(session.expires_in) + POLL_GRACE;
    loop {
        if tokio::time::Instant::now() >= deadline {
            bail!("设备码等待授权超时，请重新发起");
        }
        match poll_device_token(&mut session).await? {
            Poll::Authorized(token) => {
                // 设备码拿到的令牌长期有效，授权一次后写缓存，下次启动不再重复浏览器授权
                write_cache(&token);
                tracing::info!("已取得 GitHub 令牌 {}", mask_token(&token));
                return Ok(token);
            }
            Poll::SlowedDown(interval) => {
                tracing::debug!("GitHub 要求放慢轮询，间隔改为 {interval} 秒");
            }
            Poll::Pending => {}
            Poll::Failed(reason) => bail!("{reason}"),
        }
        tokio::time::sleep(Duration::from_secs(session.interval)).await;
    }
}

async fn post_form(url: &str, body: &str) -> Result<Response> {
    let url = url.to_string();
    let body = body.to_string();
    // 阻塞式 TLS/HTTP 放独立线程，异步侧只管 sleep 与超时
    tokio::task::spawn_blocking(move || post_form_sync(&url, &body))
        .await
        .wrap_err("HTTP 请求线程被中断")?
}

fn post_form_sync(url: &str, body: &str) -> Result<Response> {
    let (host, port, path) = split_url(url)?;
    let request = build_request(&host, port, &path, body);
    let mut stream = open_tls_stream(&host, port)?;
    stream
        .write_all(request.as_bytes())
        .wrap_err("发送 GitHub 请求失败")?;
    stream.flush().wrap_err("刷新 GitHub 请求失败")?;
    let head = read_head(&mut stream)?;
    let headers = String::from_utf8_lossy(&head[..head.len() - 4]).replace("\r\n", "\n");
    let body_bytes = read_body(&mut stream, &headers)?;
    parse_status(&headers).map(|status| Response {
        status,
        body: String::from_utf8_lossy(&body_bytes).into_owned(),
    })
}

fn split_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url.strip_prefix("https://").wrap_err("只支持 https")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().wrap_err_with(|| format!("端口无效: {p}"))?),
        None => (authority, 443u16),
    };
    ensure!(!host.is_empty(), "URL 缺少主机名: {url}");
    Ok((host.to_string(), port, path.to_string()))
}

fn build_request(host: &str, port: u16, path: &str, body: &str) -> String {
    let authority = if port == 443 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    // 每次请求新建连接，Connection: close 让对端读完即断，不必处理 keep-alive
    format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         User-Agent: stt-input\r\n\
         Accept: application/json\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {body}",
        body.len()
    )
}

fn open_tls_stream(host: &str, port: u16) -> Result<native_tls::TlsStream<TcpStream>> {
    let target = format!("{host}:{port}");
    match proxy_for(host) {
        Some((ph, pp)) => via_proxy(ph, pp, &target, host),
        None => {
            let tcp = connect(&target)?;
            tls_connect(&tcp, host).wrap_err_with(|| format!("与 {host} 建立 TLS 失败"))
        }
    }
}

fn via_proxy(ph: String, pp: u16, target: &str, host: &str) -> Result<native_tls::TlsStream<TcpStream>> {
    let addr = format!("{ph}:{pp}");
    let mut tcp = connect(&addr).wrap_err_with(|| format!("连接代理 {addr} 失败"))?;
    let line = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    tcp.write_all(line.as_bytes())
        .wrap_err("向代理发送 CONNECT 失败")?;
    tcp.flush().wrap_err("刷新 CONNECT 请求失败")?;
    read_connect_ok(&mut tcp, target)?;
    // 隧道的起始偏移是 0，CONNECT 响应字节已被读走，TLS 接手同一个 socket
    tls_connect(&tcp, host).wrap_err_with(|| format!("经代理 {addr} 与 {host} 建立 TLS 失败"))
}

fn connect(target: &str) -> Result<TcpStream> {
    let addr = target
        .to_socket_addrs()
        .wrap_err_with(|| format!("解析 {target} 失败"))?
        .next()
        .wrap_err_with(|| format!("{target} 没有可用地址"))?;
    let tcp = TcpStream::connect(addr).wrap_err_with(|| format!("连接 {target} 超时"))?;
    let _ = tcp.set_read_timeout(Some(READ_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(CONNECT_TIMEOUT));
    Ok(tcp)
}

fn tls_connect(tcp: &TcpStream, host: &str) -> Result<native_tls::TlsStream<TcpStream>> {
    // CONNECT 隧道没消耗过请求字节，克隆一份交给 TLS
    let cloned = tcp.try_clone().wrap_err("复制隧道连接失败")?;
    let connector = native_tls::TlsConnector::new().wrap_err("初始化 TLS 失败")?;
    connector
        .connect(host, cloned)
        .wrap_err_with(|| format!("与 {host} 建立 TLS 失败"))
}

fn read_connect_ok(stream: &mut TcpStream, target: &str) -> Result<()> {
    let head = read_head(stream)?;
    let text = String::from_utf8_lossy(&head[..head.len() - 4]).replace("\r\n", "\n");
    let line = text.lines().next().unwrap_or("");
    let code = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    ensure!(code == 200, "代理拒绝 CONNECT {target}，返回: {line}");
    Ok(())
}

fn read_head(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).wrap_err("读取响应头失败")?;
        ensure!(n > 0, "对端在完成响应头前关闭了连接");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
        ensure!(head.len() <= 65536, "响应头超过 64KB 上限");
    }
}

fn read_body(stream: &mut impl Read, headers: &str) -> Result<Vec<u8>> {
    if header_value(headers, "transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        return read_chunked(stream);
    }
    match content_length(headers) {
        // 定长：一次读满
        Some(len) => {
            let mut body = vec![0u8; len as usize];
            stream.read_exact(&mut body).wrap_err("读取响应体失败")?;
            Ok(body)
        }
        // 既无长度也非分块，配合 Connection: close 读到 EOF
        None => {
            let mut body = Vec::new();
            stream.read_to_end(&mut body).wrap_err("读取响应体失败")?;
            Ok(body)
        }
    }
}

fn read_chunked(stream: &mut impl Read) -> Result<Vec<u8>> {
    const MAX_BODY: usize = 4 * 1024 * 1024;
    let mut body = Vec::new();
    loop {
        let line = read_line(stream)?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .wrap_err_with(|| format!("分块长度无法解析: {size_text}"))?;
        // 零长度块表示结束，trailer 与收尾空行留在缓冲里，连接随即关闭，不必消费
        if size == 0 {
            return Ok(body);
        }
        ensure!(body.len() + size <= MAX_BODY, "响应体超过 4MB 上限");
        let mut chunk = vec![0u8; size];
        stream.read_exact(&mut chunk).wrap_err("读取分块数据失败")?;
        body.extend(chunk);
        // 每块后面跟着一个 CRLF
        let mut crlf = [0u8; 2];
        stream.read_exact(&mut crlf).wrap_err("读取分块结束符失败")?;
        ensure!(&crlf == b"\r\n", "分块数据后缺少 CRLF");
    }
}

fn read_line(stream: &mut impl Read) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).wrap_err("读取行失败")?;
        ensure!(n > 0, "对端在完成一行前关闭了连接");
        buf.push(byte[0]);
        if buf.ends_with(b"\n") {
            return Ok(String::from_utf8_lossy(&buf).trim_end().to_string());
        }
        ensure!(buf.len() <= 8192, "单行长度超过 8KB 上限");
    }
}

struct Response {
    status: u16,
    body: String,
}

fn parse_status(headers: &str) -> Result<u16> {
    let line = headers.lines().next().unwrap_or("");
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .wrap_err_with(|| format!("无法解析 HTTP 状态行: {line}"))
}

fn header_value(block: &str, name: &str) -> Option<String> {
    for line in block.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn content_length(headers: &str) -> Option<u64> {
    header_value(headers, "content-length")?.parse().ok()
}

// 代理：读 HTTPS_PROXY / ALL_PROXY，命中 NO_PROXY 则直连。
// 约定：只支持 http 方案的代理，即 CONNECT 隧道，不带代理凭据
fn proxy_for(host: &str) -> Option<(String, u16)> {
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

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    if !bytes.contains(&b'%') && !bytes.contains(&b'+') {
        return input.to_string();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    // shortcut: 非法的截尾百分号转义按原字节保留，极端情况可能停在多字节字符中间
    String::from_utf8_lossy(&out).into_owned()
}

// 令牌只可能出现在响应体里，任何进错误信息的正文都先过一遍
fn redact_body(body: &str) -> String {
    const KEY: &str = "access_token";
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find(KEY) {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        // JSON 用冒号接值，form-urlencoded 用等号
        let Some(rel) = after[KEY.len()..].find([':', '=']) else {
            out.push_str(after);
            return out;
        };
        let sep_at = KEY.len() + rel;
        out.push_str(&after[..=sep_at]);
        let value = after[sep_at + 1..].trim_start();
        rest = match value.strip_prefix('"') {
            // JSON 字符串：到下一个引号为止
            Some(inner) => {
                out.push_str("\"***");
                let end = inner.find('"').unwrap_or(inner.len());
                &inner[end..]
            }
            // form 值或 null 等标量：到下一个分隔符为止
            None => {
                out.push_str("***");
                match value.find([',', '}', '&', '\r', '\n', ' ']) {
                    Some(end) => &value[end..],
                    None => "",
                }
            }
        };
    }
    out.push_str(rest);
    out
}

#[cfg(windows)]
fn gh_probe_cmd() -> Command {
    use std::os::windows::process::CommandExt;
    // 控制台子系统调用 gh 会闪出黑窗口，创建时禁掉
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token"]).creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(windows))]
fn gh_probe_cmd() -> Command {
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token"]);
    cmd
}

#[cfg(test)]
mod tests_env_serialized {
    use super::*;

    // 改进程级环境变量会和其他线程的 getenv 打架，所有动 env 的用例共用一把锁
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn new(keys: &[&str]) -> Self {
            Self {
                saved: keys.iter().map(|k| (k.to_string(), std::env::var(k).ok())).collect(),
                _lock: ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner()),
            }
        }
        fn set(&self, key: &str, value: &str) {
            unsafe { std::env::set_var(key, value) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.saved.drain(..) {
                match v {
                    Some(val) => unsafe { std::env::set_var(&k, val) },
                    None => unsafe { std::env::remove_var(&k) },
                }
            }
        }
    }

    #[test]
    fn env_hit_and_trimmed() {
        let var = "STT_AUTH_T_ENV1";
        let g = EnvGuard::new(&[var]);
        g.set(var, "  gho_envvalue  ");
        assert_eq!(token_from_env(var).as_deref(), Some("gho_envvalue"));
    }

    #[test]
    fn env_blank_and_missing_is_miss() {
        let var = "STT_AUTH_T_ENV2";
        let g = EnvGuard::new(&[var]);
        g.set(var, "   ");
        assert_eq!(token_from_env(var), None);
        assert_eq!(token_from_env("STT_AUTH_T_NOT_SET_AT_ALL"), None);
    }

    #[test]
    fn fallback_first_level_wins() {
        let g = EnvGuard::new(&[TOKEN_ENV_VAR]);
        g.set(TOKEN_ENV_VAR, "tok-from-env");
        assert_eq!(env_token().as_deref(), Some("tok-from-env"));
        // 第 1 级命中后不该再走 gh，gh 失败也不影响 env 结论
        assert_eq!(parse_gh_output(false, ""), None);
    }

    #[test]
    fn describe_reports_env_when_set() {
        let g = EnvGuard::new(&[TOKEN_ENV_VAR]);
        g.set(TOKEN_ENV_VAR, "gho_describe_test_value");
        let text = describe_auth_source();
        assert!(text.starts_with("mode=env"), "实际: {text}");
        assert!(!text.contains("gho_describe_test_value"), "诊断串不该带令牌原文: {text}");
    }

    #[test]
    fn describe_without_env_does_not_claim_env() {
        // 守卫只负责退出时还原环境
        let _g = EnvGuard::new(&[TOKEN_ENV_VAR]);
        unsafe { std::env::remove_var(TOKEN_ENV_VAR) };
        let text = describe_auth_source();
        // 本机 gh 凭据无效，预期落到 device；但绝不该报 env
        assert!(!text.starts_with("mode=env"), "实际: {text}");
    }

    #[test]
    fn proxy_env_parsing() {
        let g = EnvGuard::new(&["HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"]);
        g.set("ALL_PROXY", "");
        g.set("NO_PROXY", "");
        g.set("HTTPS_PROXY", "http://127.0.0.1:7897");
        assert_eq!(proxy_for("github.com"), Some(("127.0.0.1".to_string(), 7897)));
        g.set("NO_PROXY", "localhost,.example.com");
        assert_eq!(proxy_for("api.example.com"), None);
        assert_eq!(proxy_for("localhost"), None);
        assert_eq!(proxy_for("github.com"), Some(("127.0.0.1".to_string(), 7897)));
        g.set("HTTPS_PROXY", "");
        assert_eq!(proxy_for("github.com"), None);
    }

    #[test]
    fn no_proxy_match_and_proxy_authority_forms() {
        let g = EnvGuard::new(&["HTTPS_PROXY", "NO_PROXY"]);
        g.set("NO_PROXY", "*.internal");
        assert!(in_no_proxy("a.internal") && in_no_proxy("b.a.internal") && !in_no_proxy("internal.example"));
        g.set("NO_PROXY", "");
        g.set("HTTPS_PROXY", "user:pass@10.0.0.9:3128/");
        assert_eq!(proxy_for("github.com"), Some(("10.0.0.9".to_string(), 3128)));
        g.set("HTTPS_PROXY", "127.0.0.1:notaport");
        assert_eq!(proxy_for("github.com"), None);
        g.set("HTTPS_PROXY", ":7897");
        assert_eq!(proxy_for("github.com"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_output_parsing() {
        assert_eq!(parse_gh_output(true, "gho_abc\n").as_deref(), Some("gho_abc"));
        assert_eq!(parse_gh_output(true, "   "), None);
        assert_eq!(parse_gh_output(false, "gho_abc"), None);
    }

    #[test]
    fn missing_gh_binary_reports_error() {
        let mut cmd = Command::new("stt-auth-not-a-real-binary");
        cmd.args(["auth", "token"]);
        let err = run_gh_command(&mut cmd, Duration::from_millis(500)).unwrap_err();
        assert!(err.to_string().contains("gh"), "实际: {err}");
    }

    #[cfg(windows)]
    #[test]
    fn gh_probe_hits_hard_timeout() {
        // 借一个必然慢过阈值的命令验超时保护生效
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "30", "127.0.0.1"]);
        let err = run_gh_command(&mut cmd, Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("未返回"), "实际: {err}");
    }

    #[test]
    fn mask_never_leaks_whole_token() {
        let token = "gho_0123456789abcdefghij";
        let m = mask_token(token);
        assert!(!m.contains(token));
        assert!(m.starts_with("gho_"));
        assert!(m.contains("len=24"));
        assert_eq!(mask_token("abc"), "****(len=3)");
        assert_eq!(mask_token(""), "****(len=0)");
    }

    #[test]
    fn debug_masks_token_and_keeps_mode() {
        let token = "gho_SUPERSECRETLONGVALUE123";
        let item = GitHubToken { access_token: token.to_string(), mode: AuthMode::Device };
        let text = format!("{item:?}");
        assert!(!text.contains(token));
        assert!(text.contains("mode: Device"));
    }

    #[test]
    fn mode_display() {
        assert_eq!(AuthMode::Env.to_string(), "env");
        assert_eq!(AuthMode::Cache.to_string(), "cache");
        assert_eq!(AuthMode::Gh.to_string(), "gh");
        assert_eq!(AuthMode::Device.to_string(), "device");
    }

    #[test]
    fn device_request_bodies() {
        assert_eq!(
            device_code_body(VSCODE_CLIENT_ID),
            "client_id=Iv1.b507a08c87ecfe98&scope="
        );
        let b = access_token_body(VSCODE_CLIENT_ID, "dev/code+1");
        assert!(b.starts_with("client_id=Iv1.b507a08c87ecfe98&"));
        assert!(b.contains("device_code=dev%2Fcode%2B1"));
        assert!(b.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
    }

    #[test]
    fn url_encode_percent() {
        assert_eq!(url_encode("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(url_encode("Iv1.b507a08c87ecfe98"), "Iv1.b507a08c87ecfe98");
        assert_eq!(url_encode("中文"), "%E4%B8%AD%E6%96%87");
        assert_eq!(percent_decode("a%20b%2Fc+d"), "a b/c d");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn split_url_parts() {
        let (host, port, path) = split_url(DEVICE_CODE_URL).unwrap();
        assert_eq!((host.as_str(), port, path.as_str()), ("github.com", 443, "/login/device/code"));
        let (h2, p2, pa2) = split_url("https://example.com:8443/api/x").unwrap();
        assert_eq!((h2.as_str(), p2, pa2.as_str()), ("example.com", 8443, "/api/x"));
        assert!(split_url("http://github.com/x").is_err());
    }

    #[test]
    fn poll_pending_and_slowdown() {
        let pending = decode_json_or_form(r#"{"error":"authorization_pending"}"#).unwrap();
        assert!(matches!(decide_poll(&pending, 5), Poll::Pending));
        let slow = decode_json_or_form(r#"{"error":"slow_down"}"#).unwrap();
        assert!(matches!(decide_poll(&slow, 5), Poll::SlowedDown(10)));
        assert_eq!(next_interval(Some("slow_down"), 10), 15);
        assert_eq!(next_interval(Some("authorization_pending"), 10), 10);
    }

    #[test]
    fn poll_expired_and_denied_fail() {
        for code in ["expired_token", "access_denied"] {
            let v = decode_json_or_form(&format!(r#"{{"error":"{code}"}}"#)).unwrap();
            match decide_poll(&v, 5) {
                Poll::Failed(reason) => assert!(reason.contains(code), "实际: {reason}"),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn poll_authorized_and_incomplete() {
        let ok = decode_json_or_form(r#"{"access_token":"gho_x","token_type":"bearer"}"#).unwrap();
        match decide_poll(&ok, 5) {
            Poll::Authorized(t) => assert_eq!(t, "gho_x"),
            other => panic!("unexpected {other:?}"),
        }
        let empty = decode_json_or_form(r#"{"access_token":""}"#).unwrap();
        assert!(matches!(decide_poll(&empty, 5), Poll::Failed(_)));
    }

    #[test]
    fn session_from_json_and_form() {
        let json = r#"{"device_code":"dc_secret","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#;
        let v = decode_json_or_form(json).unwrap();
        let s = into_session(&v).unwrap();
        assert_eq!((s.user_code.as_str(), s.expires_in, s.interval), ("ABCD-1234", 900, 5));

        let form = "device_code=dc%2Fsecret&user_code=WXYZ-9876&verification_uri=https%3A%2F%2Fgithub.com%2Flogin%2Fdevice&expires_in=600&interval=7";
        let v2 = decode_json_or_form(form).unwrap();
        let s2 = into_session(&v2).unwrap();
        assert_eq!(s2.device_code, "dc/secret");
        assert_eq!((s2.user_code.as_str(), s2.expires_in, s2.interval), ("WXYZ-9876", 600, 7));
    }

    #[test]
    fn session_defaults_when_fields_missing() {
        let v = decode_json_or_form(
            r#"{"device_code":"d","user_code":"U","verification_uri":"https://github.com/login/device"}"#,
        )
        .unwrap();
        let s = into_session(&v).unwrap();
        assert_eq!((s.expires_in, s.interval), (900, 5));
    }

    #[test]
    fn session_error_from_github() {
        let v = decode_json_or_form(r#"{"error":"unsupported_grant_type"}"#).unwrap();
        let err = into_session(&v).unwrap_err();
        assert!(err.to_string().contains("unsupported_grant_type"));
    }

    #[test]
    fn session_missing_field_errors() {
        let v = decode_json_or_form(r#"{"user_code":"U","verification_uri":"V"}"#).unwrap();
        assert!(into_session(&v).is_err());
    }

    #[test]
    fn redact_masks_both_shapes() {
        let json = r#"{"access_token":"gho_LEAKME123456","scope":""}"#;
        let r = redact_body(json);
        assert!(!r.contains("gho_LEAKME123456"));
        assert!(r.contains("access_token\":\"***"));
        assert!(r.contains("\"scope\""));
        let form = "access_token=gho_LEAKME&token_type=bearer";
        let f = redact_body(form);
        assert!(!f.contains("gho_LEAKME"));
        assert!(f.contains("token_type=bearer"));
        assert_eq!(redact_body("no secrets here"), "no secrets here");
    }

    #[test]
    fn parse_status_reads_first_line() {
        assert_eq!(parse_status("HTTP/1.1 200 OK\r\nX: y").unwrap(), 200);
        assert_eq!(parse_status("HTTP/1.1 404 Not Found").unwrap(), 404);
        assert!(parse_status("garbage-line").is_err());
    }

    #[test]
    fn read_body_fixed_length() {
        let mut cur = std::io::Cursor::new(b"{\"ok\":true}\r\nrest".to_vec());
        let body = read_body(&mut cur, "HTTP/1.1 200 OK\nContent-Length: 13").unwrap();
        assert_eq!(body, b"{\"ok\":true}\r\n");
    }

    #[test]
    fn read_body_to_eof_without_length() {
        let mut cur = std::io::Cursor::new(b"a=1&b=2".to_vec());
        let body = read_body(&mut cur, "HTTP/1.1 200 OK").unwrap();
        assert_eq!(body, b"a=1&b=2");
    }

    // GitHub 对 device/code 实际回的是分块编码，这条覆盖真实路径
    #[test]
    fn read_body_chunked() {
        let raw = b"7\r\n{\"ok\":t\r\n4\r\nrue}\r\n0\r\n\r\n";
        let mut cur = std::io::Cursor::new(raw.to_vec());
        let body = read_body(&mut cur, "HTTP/1.1 200 OK\nTransfer-Encoding: chunked").unwrap();
        assert_eq!(body, b"{\"ok\":true}");

        let mut empty = std::io::Cursor::new(b"0\r\n\r\n".to_vec());
        assert_eq!(
            read_body(&mut empty, "Transfer-Encoding: chunked").unwrap(),
            Vec::<u8>::new()
        );
        let mut bad = std::io::Cursor::new(b"zz\r\nx".to_vec());
        assert!(read_body(&mut bad, "Transfer-Encoding: chunked").is_err());
    }

    #[test]
    fn chunked_trailer_stops_at_zero_size() {
        let raw = b"3\r\nabc\r\n0\r\nX-Trailer: 1\r\n\r\n";
        let mut cur = std::io::Cursor::new(raw.to_vec());
        let body = read_body(&mut cur, "Transfer-Encoding: chunked").unwrap();
        assert_eq!(body, b"abc");
    }

    #[test]
    fn read_head_stops_at_blank_line() {
        let raw = b"HTTP/1.1 200 OK\r\nA: b\r\n\r\npayload";
        let mut cur = std::io::Cursor::new(raw.to_vec());
        let head = read_head(&mut cur).unwrap();
        assert!(head.ends_with(b"\r\n\r\n"));
        assert_eq!(&head[..head.len() - 4], b"HTTP/1.1 200 OK\r\nA: b");
    }

    #[test]
    fn request_has_protocol_fields() {
        let req = build_request("github.com", 443, "/login/device/code", "client_id=x&scope=");
        assert!(req.starts_with("POST /login/device/code HTTP/1.1\r\n"));
        assert!(req.contains("Host: github.com\r\n"));
        assert!(req.contains("Accept: application/json\r\n"));
        assert!(req.contains("Content-Type: application/x-www-form-urlencoded\r\n"));
        assert!(req.contains("Content-Length: 18\r\n"));
        assert!(req.ends_with("\r\n\r\nclient_id=x&scope="));
        assert!(build_request("github.com", 8443, "/x", "").contains("Host: github.com:8443\r\n"));
    }

    #[test]
    fn host_of_shape_matches_tls_sni() {
        // 与 github.com 建 TLS 时 SNI 必须是裸主机名
        let (host, port, _) = split_url(ACCESS_TOKEN_URL).unwrap();
        assert_eq!((host.as_str(), port), ("github.com", 443));
    }
}

#[cfg(test)]
mod tests_cache {
    use super::*;

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    // 每个用例独占一个 temp 子目录，测完删掉，不碰真实 exe 目录
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new() -> Self {
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "stt-auth-cache-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default(),
                n
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cache_round_trip() {
        let tmp = TmpDir::new();
        assert_eq!(read_cache_at(&tmp.0), None);
        write_cache_at(&tmp.0, "gho_roundtrip_value").unwrap();
        assert_eq!(read_cache_at(&tmp.0).as_deref(), Some("gho_roundtrip_value"));
        // 原子写：不该留下临时文件
        assert!(!tmp.0.join(format!("{TOKEN_CACHE_FILE}.tmp")).exists());
        let raw = std::fs::read_to_string(tmp.0.join(TOKEN_CACHE_FILE)).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["access_token"], "gho_roundtrip_value");
        let saved = v["saved_at"].as_str().unwrap();
        assert_eq!(saved.len(), 20, "实际: {saved}");
        assert!(saved.ends_with('Z'), "实际: {saved}");
        // 覆盖写后读到的应是新值
        write_cache_at(&tmp.0, "gho_second").unwrap();
        assert_eq!(read_cache_at(&tmp.0).as_deref(), Some("gho_second"));
    }

    #[test]
    fn corrupt_cache_is_silently_ignored() {
        let tmp = TmpDir::new();
        let path = tmp.0.join(TOKEN_CACHE_FILE);
        for broken in [
            "",
            "not json at all",
            "{\"access_token\":",
            r#"{"saved_at":"2026-11-04T12:00:00Z"}"#,
            r#"{"access_token":"gho_x"}"#,
            r#"{"access_token":""}"#,
            r#"{"access_token":null,"saved_at":"x"}"#,
            "[1,2,3]",
        ] {
            std::fs::write(&path, broken).unwrap();
            assert_eq!(read_cache_at(&tmp.0), None, "输入: {broken}");
        }
        // 目录整体不存在也不该 panic
        let missing = tmp.0.join("nope");
        assert_eq!(read_cache_at(&missing), None);
    }

    #[test]
    fn write_cache_reports_unwritable_dir() {
        let missing = std::env::temp_dir().join(format!(
            "stt-auth-cache-missing-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        assert!(write_cache_at(&missing, "gho_x").is_err());
    }

    #[test]
    fn utc_timestamp_format() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(86400), "1970-01-02T00:00:00Z");
        // 闰日边界
        assert_eq!(format_utc(951782400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1793793600), "2026-11-04T12:00:00Z");
        assert_eq!(format_utc(1793750400), "2026-11-04T00:00:00Z");
    }

    #[test]
    fn debug_masks_cached_token() {
        let item = GitHubToken { access_token: "gho_CACHEDSECRETVALUE9999".to_string(), mode: AuthMode::Cache };
        let text = format!("{item:?}");
        assert!(!text.contains("gho_CACHEDSECRETVALUE9999"));
        assert!(text.contains("mode: Cache"));
    }
}
