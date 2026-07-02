//! Claude Code hook イベントを受信する HTTP サーバ。
//!
//! 各 ccc インスタンスから `~/.ccc/bin/ccc-claude-code-hook` 経由で
//! POST されてくる hook event を受け取り、`InstanceManager` の状態に反映する。

pub mod events;
mod server;

pub use events::HookEventKind;
pub use server::{HookReceiver, ReceivedHook};
