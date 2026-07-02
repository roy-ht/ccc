//! gpg agent forward の健全性チェック＋自動修復のアプリ側ラッパー。
//! コアロジックは共有 crate `ccc-sshkit::agent_socket` に移動した（ccc-ssh CLI と共用）。

use std::path::Path;

use crate::instance::debug_log;

/// インスタンス起動・再接続前に呼ぶ。ログはインスタンスの .debug.txt と stderr へ。
/// 修復に失敗してもインスタンス起動は止めない（即死は pane-died 検知が保全する）。
pub fn ensure_agent_forward(host_alias: &str, log: Option<&Path>) {
    // GUI 経由のインスタンス起動・再接続はゲート判定に任せる（force=false）。
    let healthy = ccc_sshkit::agent_socket::ensure_agent_forward(host_alias, false, &|msg| {
        debug_log::append(log, msg);
    });
    if !healthy {
        eprintln!("[ccc] {host_alias}: gpg agent forward 修復失敗");
    }
}
