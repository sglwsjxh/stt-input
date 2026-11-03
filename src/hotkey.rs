// 空格键热键检测：主循环轮询用，不起线程

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SPACE};

/// 空格键当前是否按下。GetAsyncKeyState 最高位为 1 表示按下，
/// 转 i16 后看符号即可；低位（曾被按下）不用，轮询场景要的是瞬时状态
pub fn space_down() -> bool {
    let state = unsafe { GetAsyncKeyState(i32::from(VK_SPACE)) };
    (state as i16) < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_does_not_panic() {
        let _ = space_down();
    }
}
