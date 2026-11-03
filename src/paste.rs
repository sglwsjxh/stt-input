// 文本注入：剪贴板快照、写入转写文本、合成 Ctrl+V、延迟恢复
// 移植自 hex 的 WindowsPaster（github.com/anomalyco/hex，MIT 许可）

use std::mem::size_of;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use arboard::SetExtWindows;
use color_eyre::eyre::{Result, WrapErr, eyre};
use windows_sys::Win32::System::DataExchange::{CountClipboardFormats, GetClipboardSequenceNumber};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyExW, SendInput, VK_CONTROL, VK_LWIN, VK_MENU,
    VK_RWIN, VK_SHIFT, VK_V,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

const CLIPBOARD_RESTORE_DELAY: Duration = Duration::from_millis(500);

// 远程桌面客户端把剪贴板变更异步转发到远端，远端只在粘贴那一刻取文本：
// 立刻发 Ctrl+V 可能比新内容先到，500ms 就恢复会抢在对端取用之前，所以两头都多等
const REMOTE_DESKTOP_STEMS: &[&str] = &["mstsc", "msrdc"];
const REMOTE_DESKTOP_PASTE_DELAY: Duration = Duration::from_millis(400);
const REMOTE_DESKTOP_RESTORE_DELAY: Duration = Duration::from_secs(3);

// 标记注入事件，供外部键盘钩子识别来源；本 crate 内仅作为 extra-info 约定值
const INJECTED: usize = 0x5354_5421;

fn is_remote_desktop(stem: Option<&str>) -> bool {
    stem.is_some_and(|stem| REMOTE_DESKTOP_STEMS.contains(&stem))
}

enum ClipboardSnapshot {
    Empty,
    Files(Vec<PathBuf>),
    Html { html: String, text: Option<String> },
    Image(arboard::ImageData<'static>),
    Text(String),
}

impl ClipboardSnapshot {
    fn read(clipboard: &mut arboard::Clipboard) -> Result<Self> {
        if let Ok(html) = clipboard.get().html() {
            return Ok(Self::Html {
                html,
                text: clipboard.get_text().ok(),
            });
        }
        if let Ok(text) = clipboard.get_text() {
            return Ok(Self::Text(text));
        }
        if let Ok(files) = clipboard.get().file_list()
            && !files.is_empty()
        {
            return Ok(Self::Files(files));
        }
        if let Ok(image) = clipboard.get().image() {
            return Ok(Self::Image(image));
        }
        Self::from_format_count(unsafe { CountClipboardFormats() })
    }

    /// 无任何可识别格式时按格式数区分：0 是真空剪贴板，否则是保留不了的私有格式
    fn from_format_count(format_count: i32) -> Result<Self> {
        if format_count == 0 {
            return Ok(Self::Empty);
        }
        Err(eyre!("当前剪贴板格式无法安全保留"))
    }

    fn restore(self, clipboard: &mut arboard::Clipboard) -> Result<()> {
        match self {
            Self::Empty => clipboard.clear()?,
            Self::Files(files) => clipboard.set().file_list(&files)?,
            Self::Html { html, text } => clipboard.set().html(html, text)?,
            Self::Image(image) => clipboard.set_image(image)?,
            Self::Text(text) => clipboard.set_text(text)?,
        }
        Ok(())
    }
}

pub struct WindowsPaster {
    clipboard: arboard::Clipboard,
    clipboard_restore: Arc<Mutex<ClipboardRestore>>,
    // 终端类目标（VS Code 等）要 Ctrl+Shift+V 才是粘贴
    paste_with_shift: bool,
}

#[derive(Default)]
struct ClipboardRestore {
    generation: u64,
    original: Option<ClipboardSnapshot>,
    last_sequence: Option<u32>,
}

impl ClipboardRestore {
    fn register(
        &mut self,
        previous: Option<ClipboardSnapshot>,
        previous_sequence: u32,
        inserted_sequence: u32,
    ) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        // 连续粘贴时只有第一次的前值才是真正的原始剪贴板
        if self.last_sequence != Some(previous_sequence) {
            self.original = previous;
        }
        self.last_sequence = Some(inserted_sequence);
        self.generation
    }
}

impl WindowsPaster {
    pub fn new() -> Result<Self> {
        Ok(Self {
            clipboard: arboard::Clipboard::new()
                .wrap_err("无法打开 Windows 剪贴板")?,
            clipboard_restore: Arc::new(Mutex::new(ClipboardRestore::default())),
            paste_with_shift: false,
        })
    }

    pub fn set_paste_with_shift(&mut self, shift: bool) {
        self.paste_with_shift = shift;
    }

    pub fn paste(&mut self, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Err(eyre!("拒绝粘贴空白文本"));
        }
        let mut restore = self
            .clipboard_restore
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let original_sequence = clipboard_sequence();
        // 快照不了的私有格式不拦粘贴：转写文本照样写入，只是事后不恢复
        let snapshot = match ClipboardSnapshot::read(&mut self.clipboard) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                tracing::warn!(%error, "本次粘贴后不恢复原剪贴板");
                None
            }
        };
        wait_for_conflicting_modifiers();
        if clipboard_sequence() != original_sequence {
            return Err(eyre!("准备转写文本期间剪贴板已被其他程序修改"));
        }
        // 标记排除，避免转写文本进入 Win+V 历史和云同步
        self.clipboard
            .set()
            .exclude_from_history()
            .exclude_from_cloud()
            .text(text.to_string())
            .wrap_err("无法把转写文本写入剪贴板")?;
        let transcript_sequence = clipboard_sequence();
        let generation = restore.register(snapshot, original_sequence, transcript_sequence);
        drop(restore);

        let remote_desktop = is_remote_desktop(foreground_process_stem().as_deref());
        if remote_desktop {
            thread::sleep(REMOTE_DESKTOP_PASTE_DELAY);
        }
        if let Err(error) = send_paste_shortcut(self.paste_with_shift) {
            restore_clipboard_generation(&self.clipboard_restore, generation);
            return Err(error);
        }
        let clipboard_restore = self.clipboard_restore.clone();
        let restore_delay = if remote_desktop {
            REMOTE_DESKTOP_RESTORE_DELAY
        } else {
            CLIPBOARD_RESTORE_DELAY
        };
        thread::spawn(move || {
            thread::sleep(restore_delay);
            restore_clipboard_generation(&clipboard_restore, generation);
        });
        Ok(())
    }
}

fn clipboard_sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

fn restore_clipboard_generation(clipboard_restore: &Arc<Mutex<ClipboardRestore>>, generation: u64) {
    let mut restore = clipboard_restore
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // 更新的粘贴已经接管，旧代次不恢复，防并发覆盖
    if restore.generation != generation {
        return;
    }
    // 剪贴板被第三方改过，原快照已无意义，丢弃
    if restore.last_sequence != Some(clipboard_sequence()) {
        restore.original = None;
        restore.last_sequence = None;
        return;
    }
    let Some(snapshot) = restore.original.take() else {
        restore.last_sequence = None;
        return;
    };
    restore.last_sequence = None;
    let result = arboard::Clipboard::new()
        .wrap_err("无法重新打开 Windows 剪贴板")
        .and_then(|mut clipboard| snapshot.restore(&mut clipboard));
    if let Err(error) = result {
        tracing::warn!(%error, "无法恢复原剪贴板");
    }
}

/// 等 Shift/Alt/Win 全部松开：按着它们会把合成的 Ctrl+V 变成别的快捷键。
/// 用户按着 Ctrl 无害，不必等
fn wait_for_conflicting_modifiers() {
    while [VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
        .into_iter()
        .any(key_down)
    {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Ctrl+`key`（shift 为 Ctrl+Shift+`key`）。用户物理按住的 Ctrl 不补发按下与
/// 松开，否则系统认为它已松开；Shift 在 Ctrl 之内按下、先于 Ctrl 松开；
/// 每个键带扫描码，与物理按键一致
fn control_chord(
    key: u16,
    shift: bool,
    control_held: bool,
    scan: impl Fn(u16) -> u16,
) -> Vec<INPUT> {
    let input = |key, flags| keyboard_input(key, scan(key), flags);
    let mut inputs = Vec::with_capacity(6);
    if !control_held {
        inputs.push(input(VK_CONTROL, 0));
    }
    if shift {
        inputs.push(input(VK_SHIFT, 0));
    }
    inputs.push(input(key, 0));
    inputs.push(input(key, KEYEVENTF_KEYUP));
    if shift {
        inputs.push(input(VK_SHIFT, KEYEVENTF_KEYUP));
    }
    if !control_held {
        inputs.push(input(VK_CONTROL, KEYEVENTF_KEYUP));
    }
    inputs
}

/// `key` 在前景窗口键盘布局下的扫描码。Chromium 系靠它推导 KeyboardEvent.code，
/// VS Code 终端按 code 匹配快捷键：缺扫描码时 Ctrl+V 会被当作 ^V 送进终端程序
fn foreground_scan_code(key: u16) -> u16 {
    // SAFETY: 普通 Win32 查询，进程 ID 传空指针是允许的
    unsafe {
        let thread = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
        MapVirtualKeyExW(u32::from(key), MAPVK_VK_TO_VSC, GetKeyboardLayout(thread)) as u16
    }
}

fn key_down(key: u16) -> bool {
    unsafe { GetAsyncKeyState(i32::from(key)) < 0 }
}

fn send_paste_shortcut(shift: bool) -> Result<()> {
    let inputs = control_chord(VK_V, shift, key_down(VK_CONTROL), foreground_scan_code);
    let sent = send_inputs(&inputs);
    if sent != inputs.len() as u32 {
        // 只松开本次合成按下的键，绝不松开用户按住的键
        let cleanup = inputs
            .iter()
            .filter(|input| unsafe { input.Anonymous.ki.dwFlags } & KEYEVENTF_KEYUP != 0)
            .copied()
            .collect::<Vec<_>>();
        send_inputs(&cleanup);
        return Err(eyre!(
            "Windows 只接受了 {sent}/{} 个合成粘贴事件，提权窗口可能拒绝注入输入",
            inputs.len()
        ));
    }
    Ok(())
}

fn send_inputs(inputs: &[INPUT]) -> u32 {
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    }
}

fn keyboard_input(key: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED,
            },
        },
    }
}

/// 前景窗口所属进程的可执行文件名（小写、不含扩展名），取不到返回 None
fn foreground_process_stem() -> Option<String> {
    let path = window_process_path(unsafe { GetForegroundWindow() })?;
    Some(path.file_stem()?.to_string_lossy().to_lowercase())
}

fn window_process_path(window: windows_sys::Win32::Foundation::HWND) -> Option<PathBuf> {
    use windows_sys::Win32::Foundation::CloseHandle;

    if window.is_null() {
        return None;
    }
    unsafe {
        let mut pid = 0_u32;
        GetWindowThreadProcessId(window, &mut pid);
        if pid == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buffer = [0_u16; 1024];
        let mut length = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length);
        CloseHandle(process);
        if ok == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buffer[..length as usize]).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_clipboard_reads_as_empty() {
        assert!(matches!(
            ClipboardSnapshot::from_format_count(0),
            Ok(ClipboardSnapshot::Empty)
        ));
    }

    #[test]
    fn unrecognized_formats_are_not_empty() {
        assert!(ClipboardSnapshot::from_format_count(3).is_err());
    }

    #[test]
    fn rapid_pastes_keep_the_original_clipboard_for_the_latest_restore() {
        let mut restore = ClipboardRestore::default();

        let first = restore.register(Some(ClipboardSnapshot::Text("original".into())), 10, 11);
        let second = restore.register(Some(ClipboardSnapshot::Text("first paste".into())), 11, 12);

        assert_ne!(first, second);
        assert_eq!(restore.generation, 2);
        assert_eq!(restore.last_sequence, Some(12));
        assert!(matches!(
            restore.original.as_ref(),
            Some(ClipboardSnapshot::Text(text)) if text == "original"
        ));
    }

    #[test]
    fn an_unreadable_clipboard_still_pastes_and_restores_nothing() {
        let mut restore = ClipboardRestore::default();
        restore.register(Some(ClipboardSnapshot::Text("old".into())), 1, 2);

        restore.register(None, 10, 11);

        assert_eq!(restore.last_sequence, Some(11));
        assert!(restore.original.is_none());
    }

    #[test]
    fn remote_desktop_clients_get_the_slower_paste() {
        assert!(is_remote_desktop(Some("mstsc")));
        assert!(is_remote_desktop(Some("msrdc")));
        assert!(!is_remote_desktop(Some("notepad")));
        assert!(!is_remote_desktop(None));
    }

    /// 美式布局下各键的扫描码
    fn scan(key: u16) -> u16 {
        match key {
            VK_CONTROL => 0x1D,
            VK_SHIFT => 0x2A,
            VK_V => 0x2F,
            _ => 0,
        }
    }

    #[test]
    fn paste_chords_carry_each_keys_scan_code() {
        let inputs = control_chord(VK_V, true, false, scan);
        assert!(inputs.iter().all(|input| {
            let ki = unsafe { input.Anonymous.ki };
            ki.wScan == scan(ki.wVk) && ki.wScan != 0
        }));
    }

    fn keys(inputs: &[INPUT]) -> Vec<(u16, bool)> {
        inputs
            .iter()
            .map(|input| {
                let ki = unsafe { input.Anonymous.ki };
                (ki.wVk, ki.dwFlags & KEYEVENTF_KEYUP != 0)
            })
            .collect()
    }

    #[test]
    fn paste_chords_press_shift_inside_control_and_leave_a_held_control_alone() {
        assert_eq!(
            keys(&control_chord(VK_V, false, false, scan)),
            [
                (VK_CONTROL, false),
                (VK_V, false),
                (VK_V, true),
                (VK_CONTROL, true)
            ]
        );
        assert_eq!(
            keys(&control_chord(VK_V, false, true, scan)),
            [(VK_V, false), (VK_V, true)]
        );
        assert_eq!(
            keys(&control_chord(VK_V, true, false, scan)),
            [
                (VK_CONTROL, false),
                (VK_SHIFT, false),
                (VK_V, false),
                (VK_V, true),
                (VK_SHIFT, true),
                (VK_CONTROL, true)
            ]
        );
        assert_eq!(
            keys(&control_chord(VK_V, true, true, scan)),
            [
                (VK_SHIFT, false),
                (VK_V, false),
                (VK_V, true),
                (VK_SHIFT, true)
            ]
        );
    }
}
