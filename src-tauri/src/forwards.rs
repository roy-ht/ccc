//! SSH port forwarding 管理の Tauri コマンド層。
//! コアロジック（台帳・mux 操作・リプレイ）は共有 crate `ccc-sshkit` に移動した（ccc-ssh CLI と共用）。

pub use ccc_sshkit::forwards::*;

use crate::instance::InstanceManager;
use tauri::State;

/// 一覧取得。タブ表示の都度呼ばれるため、ここで世代チェック＋リプレイも行う（検知トリガー③）。
#[tauri::command]
pub async fn forwards_list(
    host_alias: String,
    mgr: State<'_, InstanceManager>,
) -> Result<Vec<ForwardRow>, String> {
    let hook_port = mgr.hook_port();
    tokio::task::spawn_blocking(move || {
        sync_ledger(&host_alias);
        list(&host_alias, hook_port)
    })
    .await
    .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}

#[tauri::command]
pub async fn forwards_add(host_alias: String, spec: ForwardSpec) -> Result<(), String> {
    tokio::task::spawn_blocking(move || add(&host_alias, spec))
        .await
        .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}

#[tauri::command]
pub async fn forwards_remove(host_alias: String, spec: ForwardSpec) -> Result<(), String> {
    tokio::task::spawn_blocking(move || remove(&host_alias, spec))
        .await
        .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}
