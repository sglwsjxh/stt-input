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
// 认证来源探针：打印当前会走哪一级，并真实发起一次设备码请求验证代理连通
// 独立编译整个 auth 模块，bin-only crate 的 example 拿不到 crate 内模块
#![allow(dead_code)]

#[path = "../src/auth.rs"]
mod auth;

use color_eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    println!("{}", auth::describe_auth_source());

    // 走完整 get_token 三级回退，只打印掩码后的结构
    if std::env::args().any(|a| a == "--get") {
        let item = auth::get_token().await?;
        println!("get_token 结果: {item:?}");
        return Ok(());
    }

    let probe_off = std::env::args().any(|a| a == "--no-probe");
    if probe_off {
        return Ok(());
    }
    // 只发 device/code，不轮询，不消耗浏览器授权
    let mut session = auth::request_device_code().await?;
    println!("代理与 TLS 通路正常，设备码申请成功");
    println!("  verification_uri: {}", session.verification_uri);
    println!("  user_code:        {}", session.user_code);
    println!("  expires_in:       {} 秒", session.expires_in);
    println!("  interval:         {} 秒", session.interval);
    println!("  device_code:      {}", session.device_code_masked());

    // 单次轮询验证 access_token 端点通路，未授权时预期 pending
    match auth::poll_device_token(&mut session).await? {
        auth::Poll::Pending => println!("  轮询通路正常: GitHub 回 authorization_pending"),
        auth::Poll::SlowedDown(i) => println!("  轮询通路正常: GitHub 要求放慢到 {i} 秒"),
        auth::Poll::Authorized(t) => println!("  已拿到令牌: {}", auth::mask_token(&t)),
        auth::Poll::Failed(r) => println!("  轮询返回失败: {r}"),
    }
    Ok(())
}
