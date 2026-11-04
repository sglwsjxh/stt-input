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
// Ctrl+Space 热键检测：主循环轮询用，不起线程

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_RCONTROL};

/// 右 Ctrl 当前是否按下。GetAsyncKeyState 最高位为 1 表示按下，
/// 转 i16 后看符号即可；低位（曾被按下）不用，轮询场景要的是瞬时状态
pub fn hotkey_down() -> bool {
    let state = unsafe { GetAsyncKeyState(i32::from(VK_RCONTROL)) };
    (state as i16) < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_press_query_does_not_panic() {
        let _ = hotkey_down();
    }
}
