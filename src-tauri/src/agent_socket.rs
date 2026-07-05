//! gpg agent forward の健全性チェック＋自動修復のアプリ側ラッパー。
//! コアロジックは共有 crate `ccc-sshkit::agent_socket` に移動した（ccc-ssh CLI と共用）。

use std::path::Path;

use crate::instance::debug_log;
use crate::instance::InstanceManager;
use tauri::State;

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

/// 新 master 起動前にリモート側の残骸 socket を明示的に unlink する。
/// sshd の StreamLocalBindUnlink 頼みでの沈黙 bind 失敗を回避する。詳細は
/// `ccc_sshkit::agent_socket::cleanup_stale_remote_sockets` の docstring 参照。
pub fn cleanup_stale_remote_sockets(host_alias: &str, log: Option<&Path>) {
    ccc_sshkit::agent_socket::cleanup_stale_remote_sockets(host_alias, &|msg| {
        debug_log::append(log, msg);
    });
}

/// UI 用: 60 秒監視ループが最後に判定した gpg forward 疎通状態を返す。
/// まだ判定していないホストや `no_forward`（unix socket forward が config に無い）は
/// `None` を返し、UI ではバッジを非表示にする。
#[tauri::command]
pub fn get_gpg_forward_status(
    host_alias: String,
    mgr: State<'_, InstanceManager>,
) -> Option<String> {
    use ccc_sshkit::agent_socket::ForwardHealth;
    match mgr.forward_status_snapshot(&host_alias)? {
        ForwardHealth::NoForward => None,
        other => Some(other.as_slug().to_string()),
    }
}
