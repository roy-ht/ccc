use serde::Serialize;
use tauri::{ipc::Channel, State};

use crate::instance::types::OutputPayload;
use crate::instance::{InstanceId, InstanceInfo, InstanceManager};
use crate::settings::{self, AppSettings};
use crate::ssh_config::SshHost;

// ─── ローカルインスタンス ────────────────────────────────────────────────────

#[tauri::command]
pub async fn create_local_instance(
    command: String,
    directory: Option<String>,
    copy_auth_from: Option<String>,
    agent_profile: Option<String>,
    name: Option<String>,
    mgr: State<'_, InstanceManager>,
) -> Result<InstanceId, String> {
    mgr.create_local(
        &command,
        directory.as_deref(),
        copy_auth_from.as_deref(),
        agent_profile.as_deref(),
        name.as_deref(),
    )
    .map_err(|e: anyhow::Error| e.to_string())
}

// ─── リモートインスタンス ────────────────────────────────────────────────────

#[tauri::command]
pub async fn list_ssh_hosts(mgr: State<'_, InstanceManager>) -> Result<Vec<SshHost>, String> {
    mgr.list_ssh_hosts()
        .map_err(|e: anyhow::Error| e.to_string())
}

#[tauri::command]
pub async fn create_remote_instance(
    host: String,
    command: String,
    directory: Option<String>,
    copy_auth_from: Option<String>,
    agent_profile: Option<String>,
    name: Option<String>,
    mgr: State<'_, InstanceManager>,
) -> Result<InstanceId, String> {
    mgr.create_remote(
        &host,
        &command,
        directory.as_deref(),
        copy_auth_from.as_deref(),
        agent_profile.as_deref(),
        name.as_deref(),
    )
    .await
    .map_err(|e: anyhow::Error| e.to_string())
}

// ─── 共通操作 ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn write_to_instance(
    id: InstanceId,
    data: Vec<u8>,
    mgr: State<'_, InstanceManager>,
) -> Result<(), String> {
    mgr.write(&id, &data)
        .await
        .map_err(|e: anyhow::Error| e.to_string())
}

#[tauri::command]
pub async fn resize_instance(
    id: InstanceId,
    rows: u16,
    cols: u16,
    mgr: State<'_, InstanceManager>,
) -> Result<(), String> {
    mgr.resize(&id, rows, cols)
        .await
        .map_err(|e: anyhow::Error| e.to_string())
}

#[tauri::command]
pub async fn close_instance(id: InstanceId, mgr: State<'_, InstanceManager>) -> Result<(), String> {
    mgr.close(&id);
    Ok(())
}

#[tauri::command]
pub async fn list_instances(mgr: State<'_, InstanceManager>) -> Result<Vec<InstanceInfo>, String> {
    Ok(mgr.list())
}

#[tauri::command]
pub async fn subscribe_instance_output(
    id: InstanceId,
    channel: Channel<OutputPayload>,
    mgr: State<'_, InstanceManager>,
) -> Result<(), String> {
    mgr.subscribe(id, channel)
        .map_err(|e: anyhow::Error| e.to_string())
}

#[tauri::command]
pub async fn reconnect_instance(
    id: InstanceId,
    mgr: State<'_, InstanceManager>,
) -> Result<(), String> {
    mgr.reconnect(&id)
        .await
        .map_err(|e: anyhow::Error| e.to_string())
}

/// 切断済み / 終了済みインスタンスと同じ設定 (kind / host / dir / command /
/// profile / name) で新しいインスタンスを作り直す。
///
/// 古いインスタンスは新規作成成功後にクローズ・ディレクトリ削除する。
/// 戻り値は新しい instance id（フロントは active 切り替えに使う）。
#[tauri::command]
pub async fn recreate_instance(
    id: InstanceId,
    mgr: State<'_, InstanceManager>,
) -> Result<InstanceId, String> {
    mgr.recreate(&id)
        .await
        .map_err(|e: anyhow::Error| e.to_string())
}

// ─── Terminal タブ（shell）操作 ───────────────────────────────────────────────

/// Terminal タブを初めて開いたタイミングで呼ぶ。
/// 既に shell PTY が起動済みなら no-op。失敗時はフロントにメッセージを返す。
#[tauri::command]
pub async fn ensure_shell_started(
    id: InstanceId,
    mgr: State<'_, InstanceManager>,
) -> Result<(), String> {
    mgr.ensure_shell_started(&id).await
}

#[tauri::command]
pub async fn write_to_shell(
    id: InstanceId,
    data: Vec<u8>,
    mgr: State<'_, InstanceManager>,
) -> Result<(), String> {
    mgr.write_shell(&id, &data)
        .await
        .map_err(|e: anyhow::Error| e.to_string())
}

#[tauri::command]
pub async fn resize_shell(
    id: InstanceId,
    rows: u16,
    cols: u16,
    mgr: State<'_, InstanceManager>,
) -> Result<(), String> {
    mgr.resize_shell(&id, rows, cols)
        .await
        .map_err(|e: anyhow::Error| e.to_string())
}

#[tauri::command]
pub async fn subscribe_shell_output(
    id: InstanceId,
    channel: Channel<OutputPayload>,
    mgr: State<'_, InstanceManager>,
) -> Result<(), String> {
    mgr.subscribe_shell(id, channel)
        .map_err(|e: anyhow::Error| e.to_string())
}

#[tauri::command]
pub async fn close_shell(id: InstanceId, mgr: State<'_, InstanceManager>) -> Result<(), String> {
    mgr.close_shell(&id);
    Ok(())
}

#[tauri::command]
pub async fn show_main_window(window: tauri::WebviewWindow) -> Result<(), String> {
    window.show().map_err(|e| e.to_string())
}

/// 保存済みインスタンスを tmux reattach で復元し、復元完了時点の一覧を返す。
///
/// フロントは戻り値をそのまま採用するだけで良く、`instances-restored` イベントの
/// 登録タイミングに依存しない（listen() 登録が遅れて event を取り逃すと
/// 一覧が空のまま固定される race を回避する）。
#[tauri::command]
pub async fn restore_instances(
    mgr: State<'_, InstanceManager>,
) -> Result<Vec<InstanceInfo>, String> {
    mgr.restore().await;
    Ok(mgr.list())
}

// ─── 設定 ────────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn load_settings() -> Result<AppSettings, String> {
    settings::load().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_settings(settings: AppSettings) -> Result<(), String> {
    settings::save(&settings).map_err(|e| e.to_string())
}

/// `~/.ccc/agent_settings/claude/` 配下のディレクトリ名 (= 利用可能なプロファイル一覧)
/// を返す。何も見つからなければ `["default"]` を返す。
#[tauri::command]
pub fn list_claude_profiles() -> Vec<String> {
    let dir = match crate::paths::agent_settings_dir() {
        Ok(p) => p.join("claude"),
        Err(_) => return vec!["default".to_string()],
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(_) => return vec!["default".to_string()],
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    if names.is_empty() {
        return vec!["default".to_string()];
    }
    names.sort();
    if let Some(idx) = names.iter().position(|n| n == "default") {
        // default は先頭に
        let d = names.remove(idx);
        names.insert(0, d);
    }
    names
}

#[tauri::command]
pub fn list_system_fonts() -> Vec<String> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let families: std::collections::BTreeSet<String> = db
        .faces()
        .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
        .collect();
    families.into_iter().collect()
}

/// 指定ファミリー・ウェイトのフォントファイルのバイト列を返す。
///
/// WKWebView の canvas2d はユーザーインストールフォント (~/Library/Fonts) の
/// 解決が信頼できない（フォールバック付き指定では常に失敗し、単独指定でも
/// 失敗することがある。CSS/DOM 経由は常に正常）。xterm v6 は文字幅計測も
/// グリフ描画も canvas で行うため、フロント (terminalFont.ts) がこのバイト列を
/// FontFace (Web フォント) として document に登録し直して使う。document 登録
/// フォントは canvas でも確実に解決される。
#[tauri::command]
pub fn read_font_face(family: String, weight: u16) -> Result<tauri::ipc::Response, String> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(&family)],
        weight: fontdb::Weight(weight),
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    let id = db
        .query(&query)
        .ok_or_else(|| format!("フォントが見つかりません: {family}"))?;
    let face = db
        .face(id)
        .ok_or_else(|| format!("フォント face を取得できません: {family}"))?;
    // FontFace API はコレクション (.ttc) 内の face index を指定できないため、
    // 先頭以外の face は誤ったフォントを登録してしまう。単一フォントのみ対応。
    if face.index != 0 {
        return Err(format!(
            "フォントコレクション (face index {}) は非対応: {family}",
            face.index
        ));
    }
    match &face.source {
        fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => std::fs::read(path)
            .map(tauri::ipc::Response::new)
            .map_err(|e| format!("フォントファイルの読み込みに失敗: {e}")),
        fontdb::Source::Binary(data) => {
            Ok(tauri::ipc::Response::new(data.as_ref().as_ref().to_vec()))
        }
    }
}

// ─── 認証 ────────────────────────────────────────────────────────────────────
//
// v0.2 で認証は agent_settings プロファイル単位で共有する方式に変更したため、
// 「インスタンス間で認証情報をコピー/クリアする」という概念は廃止された。
// フロントの既存呼び出しを壊さないために以下のコマンドは残してあるが、
// list_auth_sources は常に空リスト、コピー/クリアは no-op として振る舞う。
// UI 撤去後にこれらのコマンドも削除する予定。

#[derive(Debug, Clone, Serialize)]
pub struct AuthInfo {
    pub id: InstanceId,
    pub name: String,
    pub has_credentials: bool,
    pub copied_from: Option<String>,
}

#[tauri::command]
pub fn list_auth_sources(_mgr: State<'_, InstanceManager>) -> Vec<AuthInfo> {
    Vec::new()
}

#[tauri::command]
pub fn copy_auth_from_instance(
    _source_id: InstanceId,
    _target_id: InstanceId,
    _mgr: State<'_, InstanceManager>,
) -> Result<bool, String> {
    Ok(false)
}

#[tauri::command]
pub fn clear_instance_auth(
    _id: InstanceId,
    _mgr: State<'_, InstanceManager>,
) -> Result<(), String> {
    Ok(())
}
