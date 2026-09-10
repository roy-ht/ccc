//! gpg agent forward の状態を UI へ渡すアプリ側ラッパー。
//!
//! v0.14 で relay 方式へ移行し、「チェック → 修復」のロジックは無くなった
//! （復旧は uplink デーモンが自分で行う）。ここに残るのは UI 向けの読み取りだけ。
//! コアは `ccc_sshkit::gpg_uplink`。

use crate::instance::InstanceManager;
use tauri::State;

/// UI 用: 60 秒監視ループが最後に読み取った gpg relay の状態を返す。
/// まだ判定していないホストや `no_forward`（gpg.json で無効）は `None` を返し、
/// UI ではバッジを非表示にする。
#[tauri::command]
pub fn get_gpg_forward_status(
    host_alias: String,
    mgr: State<'_, InstanceManager>,
) -> Option<String> {
    use ccc_sshkit::gpg_uplink::ForwardHealth;
    match mgr.forward_status_snapshot(&host_alias)? {
        ForwardHealth::NoForward => None,
        other => Some(other.as_slug().to_string()),
    }
}
