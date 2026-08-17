use dashmap::DashMap;
use std::sync::{Arc, Mutex};
use tauri::Emitter;
use tokio::sync::{broadcast, mpsc};

use super::screen_monitor::ScreenMonitor;
use super::storage;
use super::types::{InstanceId, InstanceInfo, InstanceStatus};

/// 転送タスクの共通コンテキスト
pub struct RelayContext {
    pub instance_id: InstanceId,
    pub infos: Arc<DashMap<InstanceId, InstanceInfo>>,
    pub app_handle: Option<tauri::AppHandle>,
    /// シャドウスクリーン（v0.12 画面状態検出）。relay がバイトを供給し、
    /// 評価はグローバルタスク（`manager::spawn_screen_evaluator`）が行う。
    pub monitor: Arc<Mutex<ScreenMonitor>>,
    /// この relay の世代。PTY を張り直すたびに加算される。
    pub epoch: u64,
    /// インスタンスごとの現行世代。`epoch` と一致しない relay は「旧世代」。
    pub epochs: Arc<DashMap<InstanceId, u64>>,
    /// 切断判定の記録先。
    pub log_path: Option<std::path::PathBuf>,
}

/// mpsc → broadcast の転送タスクを起動する。
/// emit_disconnect が true の場合、raw_rx 枯渇時に切断イベントを発行する。
///
/// 状態 (`InstanceStatus` / `status_message` / `pending_prompt`) の更新は
/// hook event 経由 (`hook_dispatch`) を主とし、ここでは出力の broadcast と
/// シャドウスクリーンへの供給のみ行う（状態判定はしない）。
///
/// # 世代ガード
///
/// reconnect などで PTY を張り直すと、旧 PTY の relay は「あとから」終了する。
/// 世代を確認せずに切断を反映すると、**既に復旧している現行接続の status を
/// Disconnected に踏み潰す**。こうなるとフロントは入力を握り潰し、permission の
/// 選択肢ボタンも消えるうえ、hook が来るまで自己修復しない（＝ユーザー入力待ちの
/// 状態では永久に復帰しない）。旧世代の relay は切断を報告せず静かに終了する。
pub fn spawn_relay(
    ctx: RelayContext,
    mut raw_rx: mpsc::Receiver<Vec<u8>>,
    bcast_tx: broadcast::Sender<Vec<u8>>,
    emit_disconnect: bool,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(data) = raw_rx.recv().await {
            if let Ok(mut m) = ctx.monitor.lock() {
                m.process(&data);
            }
            let _ = bcast_tx.send(data);
        }

        // raw_rx が枯渇 = この PTY の出力が尽きた
        let current = ctx.epochs.get(&ctx.instance_id).map(|e| *e);
        if !is_current_generation(ctx.epoch, current) {
            super::debug_log::append(
                ctx.log_path.as_deref(),
                &format!(
                    "[relay] 旧世代 PTY の終了を検知 (epoch={}, 現行={current:?}) — 切断は反映しない",
                    ctx.epoch
                ),
            );
            return;
        }

        if emit_disconnect {
            super::debug_log::append(
                ctx.log_path.as_deref(),
                &format!(
                    "[relay] 現行 PTY (epoch={}) の出力が尽きたため Disconnected へ遷移",
                    ctx.epoch
                ),
            );
            if let Some(mut info) = ctx.infos.get_mut(&ctx.instance_id) {
                if info.status != InstanceStatus::Terminated {
                    info.status = InstanceStatus::Disconnected;
                    let _ = storage::save_connection(&info);
                    if let Some(ref handle) = ctx.app_handle {
                        let _ = handle.emit("instance-disconnected", &ctx.instance_id);
                    }
                }
            }
        } else {
            // ローカルインスタンス: プロセス終了時にインスタンスを自動クローズ
            let is_active = ctx
                .infos
                .get(&ctx.instance_id)
                .map(|i| i.status != InstanceStatus::Terminated)
                .unwrap_or(false);
            if is_active {
                if let Some(ref handle) = ctx.app_handle {
                    let _ = handle.emit("instance-terminated", &ctx.instance_id);
                }
            }
        }
    });
}

/// この relay が現行世代かどうか（世代ガードの判定本体・テスト対象）。
///
/// `current` が `None` になるのはインスタンスが既に削除された場合。
/// その場合も「現行ではない」として扱い、削除済みインスタンスの status を
/// 復活させない。
fn is_current_generation(epoch: u64, current: Option<u64>) -> bool {
    current == Some(epoch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_generation_reports_disconnect() {
        assert!(is_current_generation(3, Some(3)));
    }

    #[test]
    fn stale_generation_is_silenced() {
        // 本命の回帰防止: reconnect 後に旧 PTY が遅れて死んでも、
        // 復旧済みの現行接続を Disconnected に踏み潰さない。
        assert!(!is_current_generation(1, Some(2)));
        assert!(!is_current_generation(1, Some(9)));
    }

    #[test]
    fn removed_instance_is_silenced() {
        assert!(!is_current_generation(1, None));
    }
}
