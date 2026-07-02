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
}

/// mpsc → broadcast の転送タスクを起動する。
/// emit_disconnect が true の場合、raw_rx 枯渇時に切断イベントを発行する。
///
/// 状態 (`InstanceStatus` / `status_message` / `pending_prompt`) の更新は
/// hook event 経由 (`hook_dispatch`) を主とし、ここでは出力の broadcast と
/// シャドウスクリーンへの供給のみ行う（状態判定はしない）。
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

        // raw_rx が枯渇 = 接続が切断された
        if emit_disconnect {
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
