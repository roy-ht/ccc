//! `HookReceiver` から流れてきた hook event を `InstanceInfo` に反映する。

use dashmap::DashMap;
use serde_json::Value;
use tauri::Emitter;

use crate::hook_receiver::HookEventKind;

use super::storage;
use super::transcript;
use super::types::{
    InstanceId, InstanceInfo, InstanceStatus, PendingPrompt, PromptOption, StatusChangedPayload,
};

/// payload の `transcript_path` を読み、直近の assistant text の先頭行を返す。
fn narration_from_payload(payload: &Value) -> Option<String> {
    let path_str = payload.get("transcript_path").and_then(|v| v.as_str())?;
    transcript::extract_latest_narration(std::path::Path::new(path_str))
}

/// 並行 POST の順序逆転検出。インスタンスごとに最後に適用したイベントの
/// 送信時刻（epoch マイクロ秒）を保持し、それより古いイベントなら true（stale）。
///
/// - hook 1件 = 1プロセス = 1 HTTP POST で axum は並行処理するため、
///   ほぼ同時の2イベント（Stop と次の UserPromptSubmit 等）はリトライ遅延などで
///   到着順が入れ替わり得る。送信側マシンの時刻は1インスタンスにつき単一
///   ソースなので比較に使える。
/// - `sent_at_us` が None（旧バイナリ）のイベントはチェックせず適用し、
///   最終適用時刻も更新しない。
pub fn is_stale_event(
    last_applied: &DashMap<InstanceId, u64>,
    instance_id: &str,
    sent_at_us: Option<u64>,
) -> bool {
    let Some(ts) = sent_at_us else {
        return false;
    };
    match last_applied.entry(instance_id.to_string()) {
        dashmap::mapref::entry::Entry::Occupied(mut e) => {
            if ts < *e.get() {
                true
            } else {
                e.insert(ts);
                false
            }
        }
        dashmap::mapref::entry::Entry::Vacant(v) => {
            v.insert(ts);
            false
        }
    }
}

/// hook 受信1件分の状態反映。
///
/// hook event の種類に応じて `InstanceStatus` / `status_message` /
/// `pending_prompt` を更新し、フロントへ通知する。
pub fn apply(
    infos: &DashMap<InstanceId, InstanceInfo>,
    app_handle: &Option<tauri::AppHandle>,
    instance_id: &str,
    kind: HookEventKind,
    payload: &Value,
) {
    let Some(update) = derive_update(kind, payload) else {
        return;
    };

    let Some(mut info) = infos.get_mut(instance_id) else {
        return;
    };

    if matches!(info.status, InstanceStatus::Terminated) {
        return;
    }

    if let Some(status) = update.status.clone() {
        info.status = status;
    }
    info.status_message = update.message.clone();
    info.pending_prompt = update.prompt.clone();
    // payload に session_id があればライブセッションとして控える（Sessions タブの
    // マーカー用）。無い hook では既存値を保持する。
    // session_id が切り替わったら session_title をクリアし、archive_service の
    // ingest 完了時に DB 由来の新しいタイトルへ置き換わる。
    if let Some(sid) = payload.get("session_id").and_then(Value::as_str) {
        let new_sid = Some(sid.to_string());
        if info.current_session_id != new_sid {
            info.session_title = None;
        }
        info.current_session_id = new_sid;
    }
    // transcript_path はウォッチドッグの割り込み確認に使う。無い hook では既存値を保持。
    if let Some(tp) = payload.get("transcript_path").and_then(Value::as_str) {
        info.last_transcript_path = Some(tp.to_string());
    }

    let _ = storage::save_connection(&info);

    if let Some(ref handle) = app_handle {
        let _ = handle.emit(
            "instance-status-changed",
            StatusChangedPayload {
                id: instance_id.to_string(),
                status: info.status.clone(),
                status_message: update.message,
                pending_prompt: update.prompt,
                current_session_id: info.current_session_id.clone(),
                session_title: info.session_title.clone(),
            },
        );
    }
}

struct Update {
    /// `None` の場合は status は変更せず、message/prompt のみ更新
    status: Option<InstanceStatus>,
    message: Option<String>,
    prompt: Option<PendingPrompt>,
}

fn derive_update(kind: HookEventKind, payload: &Value) -> Option<Update> {
    match kind {
        HookEventKind::SessionStart => Some(Update {
            status: Some(InstanceStatus::Running),
            message: None,
            prompt: None,
        }),

        HookEventKind::UserPromptSubmit => Some(Update {
            status: Some(InstanceStatus::AgentBusy),
            message: None,
            prompt: None,
        }),

        HookEventKind::PreToolUse => {
            let tool = payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if tool == "ExitPlanMode" {
                let plan = payload
                    .get("tool_input")
                    .and_then(|v| v.get("plan"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let message = if plan.is_empty() {
                    "plan 承認待ち".to_string()
                } else {
                    format!("plan 承認待ち: {}", truncate(&plan, 80))
                };
                Some(Update {
                    status: Some(InstanceStatus::AgentWaitingInput),
                    message: Some(message),
                    prompt: Some(PendingPrompt::Plan),
                })
            } else {
                // narration が取れればそれを優先、無ければツール名+入力で代替。
                let fallback = tool_action_message(payload, tool, "実行中");
                Some(Update {
                    status: Some(InstanceStatus::AgentBusy),
                    message: narration_from_payload(payload).or(fallback),
                    prompt: None,
                })
            }
        }

        HookEventKind::PostToolUse => {
            // status は変えず、narration があれば優先、無ければ "<tool> 完了 [: 入力]"
            let tool = payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let fallback = tool_action_message(payload, tool, "完了");
            Some(Update {
                status: None,
                message: narration_from_payload(payload).or(fallback),
                prompt: None,
            })
        }

        HookEventKind::Notification => {
            let ntype = payload
                .get("notification_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let msg = payload
                .get("message")
                .and_then(|v| v.as_str())
                .map(String::from);
            match ntype {
                "permission_prompt" => Some(Update {
                    status: Some(InstanceStatus::AgentWaitingInput),
                    message: msg.clone(),
                    prompt: Some(PendingPrompt::Permission {
                        description: msg.unwrap_or_default(),
                        options: default_permission_options(),
                    }),
                }),
                "idle_prompt" => Some(Update {
                    status: Some(InstanceStatus::AgentIdle),
                    message: msg,
                    prompt: None,
                }),
                _ => Some(Update {
                    status: None,
                    message: msg,
                    prompt: None,
                }),
            }
        }

        HookEventKind::PermissionRequest => {
            let tool_name = payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let tool_input = payload
                .get("tool_input")
                .map(format_tool_input)
                .unwrap_or_default();
            let desc = if tool_input.is_empty() {
                tool_name.to_string()
            } else {
                format!("{tool_name}: {tool_input}")
            };
            Some(Update {
                status: Some(InstanceStatus::AgentWaitingInput),
                message: Some(format!("{tool_name} の許可待ち")),
                prompt: Some(PendingPrompt::Permission {
                    description: desc,
                    options: default_permission_options(),
                }),
            })
        }

        HookEventKind::Stop => Some(Update {
            status: Some(InstanceStatus::AgentIdle),
            message: narration_from_payload(payload),
            prompt: None,
        }),

        HookEventKind::StopFailure => {
            let err = payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            Some(Update {
                status: Some(InstanceStatus::AgentIdle),
                message: Some(format!("エラー: {err}")),
                prompt: None,
            })
        }

        // SessionEnd は claude code セッション終了の合図に過ぎず、tmux/PTY は
        // 生存している。`/resume` で同一インスタンス内に新セッションを張る
        // ケースを誤って Terminated に落とさないよう no-op とする。
        // 実際の「インスタンス終了」は PTY 側 (relay.rs) で検知する。
        HookEventKind::SessionEnd => None,

        // tmux pane-died フック発: コマンド本体（claude 等）が終了した。
        // remain-on-exit によりセッションと出力は保全されているので、
        // ユーザーは Agent タブで最後のエラー出力を確認できる。
        HookEventKind::PaneDied => {
            let status = payload
                .get("exit_status")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown");
            Some(Update {
                status: Some(InstanceStatus::Terminated),
                message: Some(format!(
                    "コマンドが終了しました (exit {status})。出力は Agent タブで確認できます"
                )),
                prompt: None,
            })
        }

        HookEventKind::Other => None,
    }
}

/// permission UI のデフォルト選択肢。
/// 実際にどの番号がどの選択肢にマップされるかは Claude Code 側の実装依存だが、
/// 一般的な「Yes / Yes & don't ask / No」の3択を暫定とする。
fn default_permission_options() -> Vec<PromptOption> {
    vec![
        PromptOption {
            key: "1".to_string(),
            label: "Yes".to_string(),
        },
        PromptOption {
            key: "2".to_string(),
            label: "Yes, don't ask again".to_string(),
        },
        PromptOption {
            key: "3".to_string(),
            label: "No".to_string(),
        },
    ]
}

fn format_tool_input(v: &Value) -> String {
    if let Some(obj) = v.as_object() {
        for key in &["command", "file_path", "url", "pattern", "path"] {
            if let Some(s) = obj.get(*key).and_then(|x| x.as_str()) {
                return truncate(s, 120);
            }
        }
    }
    truncate(&v.to_string(), 120)
}

/// PreToolUse / PostToolUse の status_message フォールバック。
/// `tool` が空なら None、`tool_input` から有意な要約が取れれば
/// `"<tool> <suffix>: <input>"`、取れなければ `"<tool> <suffix>"` を返す。
fn tool_action_message(payload: &Value, tool: &str, suffix: &str) -> Option<String> {
    if tool.is_empty() {
        return None;
    }
    let summary = payload.get("tool_input").and_then(summarize_tool_input);
    Some(match summary {
        Some(s) => format!("{tool} {suffix}: {s}"),
        None => format!("{tool} {suffix}"),
    })
}

/// `tool_input` から status_message 向けの短い要約を取り出す。
/// 主要ツールの代表フィールドのみを採用し、見つからなければ None。
/// （`format_tool_input` は permission UI 向けで JSON dump フォールバックがあるため別関数。）
fn summarize_tool_input(v: &Value) -> Option<String> {
    let obj = v.as_object()?;
    for key in &[
        "command",
        "file_path",
        "path",
        "url",
        "pattern",
        "query",
        "description",
        "prompt",
        "notebook_path",
    ] {
        if let Some(s) = obj.get(*key).and_then(|x| x.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(truncate(trimmed, 80));
            }
        }
    }
    None
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_action_message_with_file_path() {
        let payload = json!({"tool_input": {"file_path": "src/foo.rs"}});
        let got = tool_action_message(&payload, "Read", "実行中");
        assert_eq!(got.as_deref(), Some("Read 実行中: src/foo.rs"));
    }

    #[test]
    fn tool_action_message_with_bash_command() {
        let payload = json!({"tool_input": {"command": "cargo test --lib"}});
        let got = tool_action_message(&payload, "Bash", "実行中");
        assert_eq!(got.as_deref(), Some("Bash 実行中: cargo test --lib"));
    }

    #[test]
    fn tool_action_message_with_agent_description() {
        let payload = json!({"tool_input": {"description": "Investigate flaky test"}});
        let got = tool_action_message(&payload, "Agent", "実行中");
        assert_eq!(got.as_deref(), Some("Agent 実行中: Investigate flaky test"));
    }

    #[test]
    fn tool_action_message_falls_back_when_no_known_key() {
        let payload = json!({"tool_input": {"obscure": 42}});
        let got = tool_action_message(&payload, "MysteryTool", "完了");
        assert_eq!(got.as_deref(), Some("MysteryTool 完了"));
    }

    #[test]
    fn tool_action_message_returns_none_for_empty_tool() {
        let payload = json!({"tool_input": {"command": "ls"}});
        assert!(tool_action_message(&payload, "", "実行中").is_none());
    }

    #[test]
    fn summarize_tool_input_skips_blank_strings() {
        let v = json!({"file_path": "   ", "command": "echo hi"});
        assert_eq!(summarize_tool_input(&v).as_deref(), Some("echo hi"));
    }

    #[test]
    fn stale_event_is_dropped_after_newer_one_applied() {
        let map = DashMap::new();
        assert!(!is_stale_event(&map, "i1", Some(200)));
        // 遅延到着した古いイベント（送信時刻 100 < 適用済み 200）は stale
        assert!(is_stale_event(&map, "i1", Some(100)));
        // さらに新しいイベントは適用される
        assert!(!is_stale_event(&map, "i1", Some(300)));
    }

    #[test]
    fn same_timestamp_event_is_not_stale() {
        let map = DashMap::new();
        assert!(!is_stale_event(&map, "i1", Some(100)));
        assert!(!is_stale_event(&map, "i1", Some(100)));
    }

    #[test]
    fn event_without_sent_at_is_always_applied() {
        let map = DashMap::new();
        assert!(!is_stale_event(&map, "i1", Some(200)));
        // 旧バイナリからのイベント（None）は常に適用、最終適用時刻も維持
        assert!(!is_stale_event(&map, "i1", None));
        assert!(is_stale_event(&map, "i1", Some(100)));
    }

    #[test]
    fn staleness_is_tracked_per_instance() {
        let map = DashMap::new();
        assert!(!is_stale_event(&map, "i1", Some(200)));
        // 別インスタンスは独立して判定される
        assert!(!is_stale_event(&map, "i2", Some(100)));
    }

    #[test]
    fn pane_died_maps_to_terminated_with_exit_status() {
        let payload = json!({"exit_status": "127"});
        let update = derive_update(HookEventKind::PaneDied, &payload).unwrap();
        assert_eq!(update.status, Some(InstanceStatus::Terminated));
        assert!(update.message.unwrap().contains("exit 127"));
    }

    #[test]
    fn pane_died_with_empty_status_falls_back_to_unknown() {
        let payload = json!({"exit_status": ""});
        let update = derive_update(HookEventKind::PaneDied, &payload).unwrap();
        assert!(update.message.unwrap().contains("exit unknown"));
    }

    #[test]
    fn summarize_tool_input_truncates_long_values() {
        let long = "x".repeat(200);
        let v = json!({"command": long});
        let got = summarize_tool_input(&v).unwrap();
        assert!(got.ends_with('…'));
        assert!(got.chars().count() <= 81);
    }
}
