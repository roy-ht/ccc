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

/// ホスト横断の一覧。
///
/// インスタンスが起動していないホストの forward も見えるようにするための入口。
/// forward の実体はホスト単位の台帳（`~/.ccc/forwards/<alias>.json`）なので、
/// インスタンスの生死とは無関係に列挙できる。
///
/// 走査対象は既定で「台帳を持つホスト ∪ 現在 ccc が抱えているホスト」。
/// `include_config_hosts = true` のときだけ `~/.ssh/config` の全 Host も加える
/// （config 定義の LocalForward まで含めた完全な衝突検出用。ホストごとに
/// `ssh -G` が走るため既定では行わない）。
#[tauri::command]
pub async fn forwards_list_all(
    include_config_hosts: bool,
    mgr: State<'_, InstanceManager>,
) -> Result<Vec<GlobalForwardRow>, String> {
    let hook_port = mgr.hook_port();
    let connected: Vec<String> = mgr
        .list()
        .into_iter()
        .filter_map(|i| i.host_alias)
        .collect();
    let config_hosts: Vec<String> = if include_config_hosts {
        mgr.list_ssh_hosts()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|h| h.alias)
            .collect()
    } else {
        Vec::new()
    };

    tokio::task::spawn_blocking(move || {
        let mut hosts: std::collections::BTreeSet<String> = ledger_hosts().into_iter().collect();
        hosts.extend(connected);
        hosts.extend(config_hosts);
        let hosts: Vec<String> = hosts.into_iter().collect();
        Ok(list_all(&hosts, hook_port))
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
