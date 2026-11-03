<h1 align="center">stt-input</h1>

<p align="center">
  <a href="README.md">English</a>
  |
  <a href="README-zh.md">简体中文</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue" alt="GPL-3.0-or-later License" /></a>
</p>

基于 MAI STT 服务的语音输入法

按住空格键说话，松开即把识别文本粘贴进当前焦点应用，客户端协议依据 MIT 许可的 VS Code 开源源码重新实现，无需安装 VS Code

## 工作原理

- 按住空格键：从默认录音设备采集 16 kHz 单声道音频
- 音频经 WebSocket 流式推送到 MAI 转写端点
- 使用 GitHub OAuth 令牌鉴权，与 VS Code 语音模式一致
- 松开空格键：识别终稿写入剪贴板并以 Ctrl+V 粘贴
- 录音期间屏幕上方显示一个黑色电平条，随语音起伏

## 环境要求

- Windows 10/11
- 编译需要 Rust 工具链
- 一个 GitHub 账号
- 麦克风
- 可访问外网，支持可选的 `HTTPS_PROXY` 代理

## 构建

```powershell
cargo build --release
target\release\stt-input.exe
```

## 使用

按住空格键说话，松开自动粘贴

| 参数 | 作用 |
| --- | --- |
| 无参数 | 进入听写主循环 |
| `--install-autostart` | 注册开机自启（HKCU Run 键） |
| `--uninstall-autostart` | 移除开机自启 |
| `--print-auth-mode` | 显示当前生效的凭据来源 |
| `--dump-wav` | 录 3 秒音频到 `tmp/cap.wav` 用于麦克风自检 |
| `--paste-with-shift` | 以 Ctrl+Shift+V 粘贴（终端类应用） |

## 鉴权

令牌按以下顺序解析：

1. 环境变量 `STT_GITHUB_TOKEN`
2. exe 同目录的 `token.json`（便携缓存）
3. GitHub CLI 凭据库中的 `gh auth token`
4. GitHub 设备码授权——程序打印验证码，打开 <https://github.com/login/device> 输入即可

gh 或设备码获取的令牌会缓存到 exe 同目录的 `token.json`，下次启动免登录，删除该文件可强制重新登录。文件随目录走，不会写入 `~` 或 AppData

## 项目结构

```
main.rs            主入口
src/auth.rs        GitHub 令牌：env → token.json → gh → 设备码
src/stt.rs         MAI 转写 WebSocket 客户端（代理 + TLS）
src/capture.rs     麦克风采集，16 kHz 单声道 s16le，100 ms 分块
src/paste.rs       剪贴板注入，移植自 Hex
src/hotkey.rs      空格键状态轮询
src/overlay.rs     语音电平条窗口
src/autostart.rs   注册表开机自启
```

## 限制与已知问题

- 仅支持 Windows：注入、热键、电平条、自启动均调用 Win32 API
- 空格是常用打字键：打字时按键会触发听写
- 使用系统默认录音设备，Voicemeeter 用户需把麦克风路由进默认录音端点（用 `--dump-wav` 验证）
- 终端类应用通常需要 `--paste-with-shift`
- `token.json` 以明文存放在 exe 旁，是便携优先的取舍
- MAI 端点归 Microsoft 所有：个人复用 VS Code 听写配额可能随时被限流或限制

## 致谢

- [Hex](https://github.com/anomalyco/hex)（MIT）——Windows 粘贴引擎移植自其 `WindowsPaster`
- [VS Code](https://github.com/microsoft/vscode)（MIT）——听写客户端协议依据其开源源码重新实现

## 许可证

[GPL-3.0-or-later](LICENSE)
