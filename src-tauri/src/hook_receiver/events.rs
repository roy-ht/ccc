use serde::Deserialize;
use serde_json::Value;

/// `ccc-claude-code-hook` から POST される共通エンベロープ。
#[derive(Debug, Clone, Deserialize)]
pub struct HookEvent {
    pub instance_id: String,
    pub hook_event: String,
    /// 送信側マシンの時刻（epoch マイクロ秒）。並行 POST の順序逆転検出に使う。
    /// 旧バイナリからのイベントには無い（None = 順序チェックなしで適用）。
    #[serde(default)]
    pub sent_at_us: Option<u64>,
    pub payload: Value,
}

/// hook_event_name の判別。サーバ層で簡単にmatchできるよう列挙。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEventKind {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Notification,
    PermissionRequest,
    Stop,
    StopFailure,
    /// tmux の pane-died フック発（コマンド本体が終了しペインが dead になった）。
    /// Claude Code の hook ではなく ccc 自前のイベント。
    PaneDied,
    Other,
}

impl HookEventKind {
    pub fn from_str(s: &str) -> Self {
        match s {
            "SessionStart" => Self::SessionStart,
            "SessionEnd" => Self::SessionEnd,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "Notification" => Self::Notification,
            "PermissionRequest" => Self::PermissionRequest,
            "Stop" => Self::Stop,
            "StopFailure" => Self::StopFailure,
            "PaneDied" => Self::PaneDied,
            _ => Self::Other,
        }
    }

    /// イベント名（archive の events 記録用）。
    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::Notification => "Notification",
            Self::PermissionRequest => "PermissionRequest",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::PaneDied => "PaneDied",
            Self::Other => "Other",
        }
    }
}
