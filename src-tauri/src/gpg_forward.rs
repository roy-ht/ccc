//! gpg agent forward（relay 方式）の Tauri コマンド層。
//!
//! コアロジックは共有 crate `ccc-sshkit`（`gpg_config` / `gpg_uplink`）にあり、
//! `ccc-ssh gpg` サブコマンドと同じものを呼ぶ。GUI と CLI で挙動が割れないよう、
//! ここは薄いラッパーに留める。

use ccc_sshkit::gpg_config::{self, GpgConfig};
use ccc_sshkit::gpg_uplink::{self, EnsureOutcome, UplinkPhase};
use serde::Serialize;

/// 設定画面に出す 1 ホスト分の行。
#[derive(Debug, Clone, Serialize)]
pub struct GpgForwardRow {
    pub host_alias: String,
    /// `gpg.json` で有効になっているか
    pub enabled: bool,
    /// uplink デーモンが常駐しているか（flock で判定）
    pub running: bool,
    /// `healthy` / `broken` / `unreachable` / `no_forward`
    pub health: String,
    /// `connecting` / `connected` / `retrying` / `auth_failed` / `stopped`
    /// （uplink が一度も動いていなければ `None`）
    pub phase: Option<String>,
    pub generation: u64,
    pub reconnects: u64,
    /// 最終疎通の unix 秒（未疎通は `None`）
    pub last_ok_epoch: Option<u64>,
    pub remote_socket: Option<String>,
    pub local_socket: Option<String>,
    /// ローカル gpg-agent へ繋げるか（`ok` / `down` / `unknown`）
    pub local_agent: Option<String>,
    pub last_error: Option<String>,
}

fn phase_slug(phase: UplinkPhase) -> &'static str {
    match phase {
        UplinkPhase::Connecting => "connecting",
        UplinkPhase::Connected => "connected",
        UplinkPhase::Retrying => "retrying",
        UplinkPhase::AuthFailed => "auth_failed",
        UplinkPhase::Stopped => "stopped",
    }
}

fn build_row(config: &GpgConfig, alias: &str) -> GpgForwardRow {
    let enabled = config.is_enabled(alias);
    let running = gpg_uplink::is_running(alias);
    // 停止中なら状態ファイルは前回の残骸。表示に使うのは socket パス程度に留める
    let state = gpg_uplink::read_state(alias);
    GpgForwardRow {
        host_alias: alias.to_string(),
        enabled,
        running,
        health: gpg_uplink::health(alias).as_slug().to_string(),
        phase: running
            .then(|| state.as_ref().map(|s| phase_slug(s.state).to_string()))
            .flatten(),
        generation: state.as_ref().map(|s| s.generation).unwrap_or(0),
        reconnects: state.as_ref().map(|s| s.reconnects).unwrap_or(0),
        last_ok_epoch: state
            .as_ref()
            .map(|s| s.last_ok_epoch)
            .filter(|epoch| *epoch > 0),
        remote_socket: state.as_ref().and_then(|s| s.remote_socket.clone()),
        local_socket: state.as_ref().map(|s| s.local_socket.clone()),
        local_agent: state.as_ref().map(|s| s.local_agent.clone()),
        last_error: state.as_ref().and_then(|s| s.last_error.clone()),
    }
}

/// 設定済みホストの一覧（`gpg.json` のキー順 = alias 昇順）。
///
/// `ssh -G` は走らせない（ホスト数ぶんプロセスが増えるため）。状態は
/// uplink が書いた状態ファイルと flock だけで判定するのでコストはほぼゼロ。
#[tauri::command]
pub async fn gpg_forward_list() -> Result<Vec<GpgForwardRow>, String> {
    tokio::task::spawn_blocking(move || {
        let config = gpg_config::load().map_err(|e| e.to_string())?;
        Ok(config
            .hosts
            .keys()
            .map(|alias| build_row(&config, alias))
            .collect())
    })
    .await
    .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}

/// 有効・無効を切り替える。無効化時は動作中の uplink も畳む（設定と実態を揃える）。
#[tauri::command]
pub async fn gpg_forward_set_enabled(
    host_alias: String,
    enabled: bool,
) -> Result<GpgForwardRow, String> {
    tokio::task::spawn_blocking(move || {
        let mut config = gpg_config::load().map_err(|e| e.to_string())?;
        config.set_enabled(&host_alias, enabled);
        gpg_config::save(&config).map_err(|e| e.to_string())?;

        if !enabled && gpg_uplink::is_running(&host_alias) {
            gpg_uplink::stop_uplink(&host_alias, &log).map_err(|e| e.to_string())?;
        }
        Ok(build_row(&config, &host_alias))
    })
    .await
    .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}

/// エントリを台帳から消す（uplink が動いていれば止める）。
#[tauri::command]
pub async fn gpg_forward_remove(host_alias: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        if gpg_uplink::is_running(&host_alias) {
            gpg_uplink::stop_uplink(&host_alias, &log).map_err(|e| e.to_string())?;
        }
        let mut config = gpg_config::load().map_err(|e| e.to_string())?;
        config.hosts.remove(&host_alias);
        gpg_config::save(&config).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}

/// uplink を起動する（既に居れば何もしない）。
#[tauri::command]
pub async fn gpg_forward_up(host_alias: String) -> Result<GpgForwardRow, String> {
    let launcher = crate::paths::ccc_ssh_bin().map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        match gpg_uplink::ensure_uplink(&host_alias, &launcher, &log) {
            Ok(EnsureOutcome::Disabled) => {
                return Err(format!(
                    "{host_alias} は無効です（先に有効化してください）"
                ))
            }
            Ok(EnsureOutcome::StartFailed) => {
                return Err(format!(
                    "{host_alias}: uplink の起動を確認できませんでした（~/.ccc/run/ のログを確認してください）"
                ))
            }
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
        let config = gpg_config::load().map_err(|e| e.to_string())?;
        Ok(build_row(&config, &host_alias))
    })
    .await
    .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}

/// uplink を停止する。
#[tauri::command]
pub async fn gpg_forward_down(host_alias: String) -> Result<GpgForwardRow, String> {
    tokio::task::spawn_blocking(move || {
        gpg_uplink::stop_uplink(&host_alias, &log).map_err(|e| e.to_string())?;
        let config = gpg_config::load().map_err(|e| e.to_string())?;
        Ok(build_row(&config, &host_alias))
    })
    .await
    .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}

/// 停止 → 起動で張り直す（`ccc-ssh heal` の gpg 部分と同じ）。
#[tauri::command]
pub async fn gpg_forward_restart(host_alias: String) -> Result<GpgForwardRow, String> {
    let launcher = crate::paths::ccc_ssh_bin().map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        if gpg_uplink::is_running(&host_alias) {
            gpg_uplink::stop_uplink(&host_alias, &log).map_err(|e| e.to_string())?;
        }
        gpg_uplink::ensure_uplink(&host_alias, &launcher, &log).map_err(|e| e.to_string())?;
        let config = gpg_config::load().map_err(|e| e.to_string())?;
        Ok(build_row(&config, &host_alias))
    })
    .await
    .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}

/// `ssh -G` に unix socket の RemoteForward が残っていないかを調べる。
///
/// 残っていると素の `ssh` で接続した瞬間に relay の socket が上書きされる
/// （specs/v0.14 §11.2 の層 1）。移行漏れの検出用で、ホストを指定したときだけ走らせる。
#[tauri::command]
pub async fn gpg_forward_stale_config(host_alias: String) -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(move || {
        gpg_uplink::stale_config_forwards(&host_alias).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("spawn_blocking 失敗: {e}"))?
}

fn log(msg: &str) {
    eprintln!("[ccc] {msg}");
}
