use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type InstanceId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InstanceKind {
    Local,
    Remote,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    Connecting,
    Running,
    AgentIdle,
    AgentBusy,
    AgentWaitingInput,
    Disconnected,
    Terminated,
}

/// 指示待ち時の選択肢ボタン1つ分
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptOption {
    /// インスタンスに送るキー文字列（例: "1"）
    pub key: String,
    /// ボタン表示テキスト（例: "Yes"）
    pub label: String,
}

/// 指示待ち（agent_waiting_input）時の詳細
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingPrompt {
    /// permission 系プロンプト：Yes / Yes-don't-ask / No 等のボタン化
    Permission {
        description: String,
        options: Vec<PromptOption>,
    },
    /// plan モードの選択待ち：表示のみ、ボタンなし
    Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub id: InstanceId,
    pub kind: InstanceKind,
    pub name: String,
    pub status: InstanceStatus,
    /// SSH ホストエイリアス（reconnect 用。ローカルは None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_alias: Option<String>,
    /// 作業ディレクトリ（reconnect 用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// 起動コマンド（reconnect 用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// tmux セッション名（リモートインスタンスの永続化用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_session: Option<String>,
    /// インスタンスハッシュ（8文字、ディレクトリ名末尾に使用）
    #[serde(default)]
    pub instance_hash: String,
    /// インスタンス情報の永続化ディレクトリ (~/.ccc/instances/<name>-<hash>/)
    #[serde(default)]
    pub instance_dir: PathBuf,
    /// 使用する agent_settings プロファイル名（v0.2 では常に "default"）
    #[serde(default = "default_agent_profile")]
    pub agent_profile: String,
    /// 状態メッセージ（一覧画面の1行表示用）。永続化対象外。
    #[serde(default, skip_serializing)]
    pub status_message: Option<String>,
    /// 指示待ち時のプロンプト情報（permission / plan）。永続化対象外。
    #[serde(default, skip_serializing)]
    pub pending_prompt: Option<PendingPrompt>,
    /// 現在ライブで動いている claude code セッションの session_id。hook 受信で更新する。
    /// Sessions タブで「現在開いているセッション」マーカーに使う。
    /// 永続化することで、ccc 再起動後の reattach 直後でも「前回の現セッション」を
    /// 即座にサイドバーへ復元できる（同じ tmux に reattach するので session_id は
    /// そのまま続行する想定）。新セッションへ切り替わったら次の hook で上書きされる。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_session_id: Option<String>,
    /// 現在ライブのセッションの「タイトル」。Sessions タブ要約と同じく aiTitle 優先、
    /// 無ければ最初のユーザー入力。同一接続席で複数セッションを立てたとき、サイドバーで
    /// 区別するために使う。`current_session_id` が変わった hook 受信時に再抽出する。
    /// 永続化することで再起動直後の hook 受信前でも前回の値が即時表示される。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    /// 直近の PTY サイズ (rows, cols)。reconnect / restore 時にこのサイズで再 resize する。
    /// 永続化することで、ccc 再起動後の reattach も「前回の正しいサイズ」で立ち上げられる。
    /// これがないと PTY は default 80x24 で起動し、tmux pane も 80x24 に強制 resize されて
    /// その後フロントの fit() で SIGWINCH が伝播するまで描画が破綻する
    /// （リモートは ssh 越しに SIGWINCH が遅延する分、視覚的に長く残る）。
    #[serde(default)]
    pub last_size: Option<(u16, u16)>,
    /// 直近 hook payload の transcript_path（リモートはリモート側の実パス）。
    /// ウォッチドッグが割り込み中断の確認に使う。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transcript_path: Option<String>,
}

fn default_agent_profile() -> String {
    "default".to_string()
}

/// インスタンス状態変更イベント
#[derive(Debug, Clone, Serialize)]
pub struct StatusChangedPayload {
    pub id: InstanceId,
    pub status: InstanceStatus,
    pub status_message: Option<String>,
    pub pending_prompt: Option<PendingPrompt>,
    /// 現在ライブのセッション（Sessions タブのマーカー用）。
    pub current_session_id: Option<String>,
    /// 現在ライブのセッションタイトル（最初のユーザープロンプト）。
    pub session_title: Option<String>,
}

/// フロントエンドへ送るターミナル出力イベント
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputPayload {
    pub instance_id: InstanceId,
    pub data: Vec<u8>,
}

/// gpg agent forward 疎通状態イベント（60 秒監視ループから emit）。
/// `status` は `ForwardHealth::as_slug()`（`healthy` / `broken` / `unreachable` /
/// `no_gpg` / `no_forward`）。UI はホスト単位でバッジ表示する。
#[derive(Debug, Clone, Serialize)]
pub struct GpgForwardStatusPayload {
    pub host_alias: String,
    pub status: String,
    /// unix epoch 秒（判定タイムスタンプ）
    pub checked_at: i64,
    /// 直近判定で不調 → 自動 heal をトリガーしたか（UI 表示用）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_heal_triggered: bool,
}
