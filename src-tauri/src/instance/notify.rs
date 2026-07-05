//! `instance-status-changed` の emit ヘルパー。
//!
//! 状態補正の経路（hook_dispatch / watchdog / screen_monitor / manager）が
//! それぞれ payload を組み立てていた重複を一箇所に集約する。

use tauri::Emitter;

use super::types::{GpgForwardStatusPayload, InstanceInfo, StatusChangedPayload};

/// `info` の現在値から payload を組み立ててフロントへ通知する。
/// 呼び出し側は info の更新と `storage::save_connection` を済ませてから呼ぶこと。
pub(crate) fn emit_status_changed(app_handle: &Option<tauri::AppHandle>, info: &InstanceInfo) {
    let Some(handle) = app_handle else {
        return;
    };
    let _ = handle.emit(
        "instance-status-changed",
        StatusChangedPayload {
            id: info.id.clone(),
            status: info.status.clone(),
            status_message: info.status_message.clone(),
            pending_prompt: info.pending_prompt.clone(),
            current_session_id: info.current_session_id.clone(),
            session_title: info.session_title.clone(),
        },
    );
}

/// gpg agent forward の疎通状態を UI へ通知する。60 秒監視ループから、
/// 状態遷移時（healthy↔broken など）と初回検知時に呼ぶ。
pub(crate) fn emit_gpg_forward_status(
    app_handle: &Option<tauri::AppHandle>,
    payload: GpgForwardStatusPayload,
) {
    let Some(handle) = app_handle else {
        return;
    };
    let _ = handle.emit("gpg-forward-status-changed", payload);
}
