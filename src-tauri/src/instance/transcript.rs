//! claude session transcript (JSONL) からの状態メッセージ抽出。
//!
//! claude code は `~/.claude/projects/<safe-cwd>/<session-id>.jsonl` 形式で
//! 1 メッセージ = 1 行の JSONL を append し続ける。各 hook event の payload に
//! `transcript_path` が含まれるため、それを末尾から逆順に走査して直近の
//! assistant text 内容を `status_message` として取り出す。
//!
//! 設計上の留意点:
//! - 長いセッションでは transcript が数 MB に達するため、`SeekFrom::End` で
//!   末尾だけを読む（先頭から全読みしない）
//! - `read_tail` は途中行を含み得るので、末尾切り出しが行われた場合は最初の
//!   1 行を捨てる
//! - `assistant` メッセージは `content` 配列内に `thinking` / `text` / `tool_use`
//!   が混在しうる。ここで欲しいのは `text` ブロックの先頭非空行
//! - claude が transcript を append している最中に hook が走る race は許容する。
//!   壊れた末尾行は serde_json の parse 失敗で自動的に skip される

use serde_json::Value;
use std::path::Path;

/// 末尾何バイトまで読むか。直近の assistant メッセージ 1〜数件分が入る大きさ。
pub(crate) const TAIL_BYTES: usize = 65_536;

/// 表示用 status_message の最大文字数（既存 narration 抽出と整合）。
const MAX_CHARS: usize = 240;

/// Esc 割り込み中断時に claude code が transcript へ残す user メッセージの先頭。
/// 後続が "]" のものと " for tool use]" のものの2バリアントがある。
pub const INTERRUPT_MARKER_PREFIX: &str = "[Request interrupted by user";

/// `transcript_path` を読み、最新の assistant text の先頭非空行を返す。
/// ファイル不存在 / 該当メッセージなしの場合は None。
pub fn extract_latest_narration(transcript_path: &Path) -> Option<String> {
    let (tail, was_truncated) = read_tail(transcript_path, TAIL_BYTES).ok()?;
    let lines: Vec<&str> = tail.lines().collect();
    let scan: &[&str] = if was_truncated && lines.len() > 1 {
        // 末尾切り出しが行われた場合、先頭1行は途中で切れている可能性が高いので捨てる
        &lines[1..]
    } else {
        &lines[..]
    };
    // 末尾から逆順に走査。`type == "user"` (tool_result または新規ユーザー入力) を
    // 跨いだ assistant text は「直近のターン/ツール往復より前」のものでスケイル。
    // フォールバックを使わせるため None を返す。
    for line in scan.iter().rev() {
        if is_user_boundary(line) {
            return None;
        }
        if let Some(text) = extract_assistant_text_from_line(line) {
            return Some(truncate_chars(&text, MAX_CHARS));
        }
    }
    None
}

/// transcript 末尾テキスト（JSONL 断片）が「ユーザー割り込みで中断された」状態を
/// 示すかどうか。ウォッチドッグ（`watchdog.rs`）が AgentBusy 固着の補正判定に使う。
///
/// 末尾から逆順に走査し、最初に現れた有意レコード（user / assistant）で判定する:
/// - user かつ中断マーカーを含む → true（中断直後。以降レコードは追記されない）
/// - user（tool_result / 新規プロンプト）or assistant → false（通常進行中 or 完了）
/// - その他の type（permission-mode 等）や壊れた行は skip して走査を続ける
///
/// 誤検知ゼロを優先し、判別できない場合は必ず false（busy 維持）に倒す。
pub fn tail_indicates_interrupt(tail: &str) -> bool {
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("user") => return user_line_is_interrupt(&v),
            Some("assistant") => return false,
            _ => continue,
        }
    }
    false
}

/// user レコードの content（string / blocks 配列の両形式）に中断マーカーが含まれるか。
fn user_line_is_interrupt(v: &Value) -> bool {
    let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
        return false;
    };
    let text_is_marker = |t: &str| t.trim_start().starts_with(INTERRUPT_MARKER_PREFIX);
    match content {
        Value::String(s) => text_is_marker(s),
        Value::Array(blocks) => blocks.iter().any(|b| {
            b.get("type").and_then(Value::as_str) == Some("text")
                && b.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(text_is_marker)
        }),
        _ => false,
    }
}

/// 行が JSONL の `type == "user"` メッセージ（ツール結果 / ユーザー入力）かどうか。
/// これに当たると以降の text はすでに「ターン/ツール境界より前」のスケイル narration。
fn is_user_boundary(line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    v.get("type").and_then(Value::as_str) == Some("user")
}

/// ファイル末尾 `max_bytes` を読む。返り値は (内容, ファイルが切り詰められたか)。
pub(crate) fn read_tail(path: &Path, max_bytes: usize) -> std::io::Result<(String, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    let was_truncated = len > max_bytes;
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start as u64))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok((String::from_utf8_lossy(&buf).into_owned(), was_truncated))
}

/// JSONL の 1 行を parse し、`type == "assistant"` であれば
/// content 配列を走査して text ブロックの先頭非空行を返す。
fn extract_assistant_text_from_line(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let content = v.get("message")?.get("content")?.as_array()?;
    for block in content {
        if block.get("type")?.as_str()? != "text" {
            continue;
        }
        let Some(text) = block.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(head) = first_non_empty_line(text) {
            return Some(head.to_string());
        }
    }
    None
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|s| !s.is_empty())
}

fn truncate_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let head: String = chars.into_iter().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    /// 各 JSON Value を 1 行ずつ書いた一時 JSONL ファイルを返す。
    fn write_jsonl(values: &[serde_json::Value]) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ccc-transcript-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for v in values {
            writeln!(f, "{v}").unwrap();
        }
        path
    }

    fn assistant_text(text: &str) -> serde_json::Value {
        json!({
            "type": "assistant",
            "message": { "content": [{"type": "text", "text": text}] }
        })
    }

    #[test]
    fn extracts_text_from_last_assistant() {
        // 末尾が assistant text のケース（Stop hook の典型）。
        let path = write_jsonl(&[
            json!({"type":"user","message":"hi"}),
            assistant_text("設計が固まりました。"),
        ]);
        let got = extract_latest_narration(&path);
        assert_eq!(got.as_deref(), Some("設計が固まりました。"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn picks_text_block_over_thinking_and_tool_use() {
        // assistant content に thinking → text → tool_use が並んでいる典型形
        let path = write_jsonl(&[json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "thinking", "thinking": "..."},
                    {"type": "text", "text": "次に SKILL.md を書きます。"},
                    {"type": "tool_use", "name": "Write", "input": {}}
                ]
            }
        })]);
        let got = extract_latest_narration(&path);
        assert_eq!(got.as_deref(), Some("次に SKILL.md を書きます。"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn returns_first_non_empty_line_of_multiline_text() {
        let path = write_jsonl(&[assistant_text("## ヘッダ\n\n本文1\n本文2")]);
        let got = extract_latest_narration(&path);
        assert_eq!(got.as_deref(), Some("## ヘッダ"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn skips_thinking_only_assistant_and_falls_back_to_earlier() {
        // 末尾の assistant が thinking のみで text を含まない場合、
        // それより前の assistant text を採用する。
        let path = write_jsonl(&[
            assistant_text("古いメッセージ"),
            json!({
                "type": "assistant",
                "message": { "content": [{"type": "thinking", "thinking": "考え中"}] }
            }),
        ]);
        let got = extract_latest_narration(&path);
        assert_eq!(got.as_deref(), Some("古いメッセージ"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn returns_text_when_tool_use_directly_follows_text() {
        // 典型: 「これから〜します」narration → tool_use の直後に PreToolUse 発火。
        // 間に user boundary が無いので narration はフレッシュ。
        let path = write_jsonl(&[
            assistant_text("これからファイルを読みます"),
            json!({
                "type": "assistant",
                "message": { "content": [{"type": "tool_use", "name": "Read", "input": {}}] }
            }),
        ]);
        let got = extract_latest_narration(&path);
        assert_eq!(got.as_deref(), Some("これからファイルを読みます"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn returns_none_when_tool_result_separates_text_from_end() {
        // text → tool_use → tool_result → tool_use の連鎖。
        // 末端 tool_use の PreToolUse 発火時、間に tool_result があるので narration はスケイル。
        let path = write_jsonl(&[
            assistant_text("最初のツールを呼びます"),
            json!({
                "type": "assistant",
                "message": { "content": [{"type": "tool_use", "name": "Read", "input": {}}] }
            }),
            json!({
                "type": "user",
                "message": { "content": [{"type": "tool_result", "content": "ok"}] }
            }),
            json!({
                "type": "assistant",
                "message": { "content": [{"type": "tool_use", "name": "Bash", "input": {}}] }
            }),
        ]);
        let got = extract_latest_narration(&path);
        assert_eq!(got, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn returns_none_when_post_tool_use_just_completed() {
        // PostToolUse 発火直後: 末端は user tool_result。narration はスケイル扱い。
        let path = write_jsonl(&[
            assistant_text("読みますね"),
            json!({
                "type": "assistant",
                "message": { "content": [{"type": "tool_use", "name": "Read", "input": {}}] }
            }),
            json!({
                "type": "user",
                "message": { "content": [{"type": "tool_result", "content": "ok"}] }
            }),
        ]);
        let got = extract_latest_narration(&path);
        assert_eq!(got, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn returns_none_when_new_user_prompt_after_previous_turn() {
        // 前ターンの末尾 narration → 新規 user prompt → 新ターン最初の tool_use。
        // 前ターン narration は new turn にとってスケイル。
        let path = write_jsonl(&[
            assistant_text("前ターンの結論"),
            json!({"type": "user", "message": {"role": "user", "content": "次の依頼"}}),
            json!({
                "type": "assistant",
                "message": { "content": [{"type": "thinking", "thinking": "考え中"}] }
            }),
            json!({
                "type": "assistant",
                "message": { "content": [{"type": "tool_use", "name": "Read", "input": {}}] }
            }),
        ]);
        let got = extract_latest_narration(&path);
        assert_eq!(got, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn returns_none_when_no_assistant_message() {
        let path = write_jsonl(&[
            json!({"type": "permission-mode", "permissionMode": "acceptEdits"}),
            json!({"type": "user", "message": "hi"}),
        ]);
        let got = extract_latest_narration(&path);
        assert_eq!(got, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn returns_none_for_missing_file() {
        let path =
            std::env::temp_dir().join(format!("ccc-no-such-file-{}.jsonl", uuid::Uuid::new_v4()));
        assert_eq!(extract_latest_narration(&path), None);
    }

    #[test]
    fn handles_broken_json_lines_at_tail() {
        // claude が transcript に append している最中の race を想定。
        // 末尾の壊れた行は skip され、ひとつ前の正常な assistant text が返る。
        let dir =
            std::env::temp_dir().join(format!("ccc-transcript-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", assistant_text("完全な行")).unwrap();
        // 途中で切れた壊れた行
        write!(f, "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"これは途中で")
            .unwrap();

        let got = extract_latest_narration(&path);
        assert_eq!(got.as_deref(), Some("完全な行"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncates_very_long_text() {
        let long = "あ".repeat(MAX_CHARS + 50);
        let path = write_jsonl(&[assistant_text(&long)]);
        let got = extract_latest_narration(&path).unwrap();
        assert!(got.ends_with('…'));
        assert!(got.chars().count() <= MAX_CHARS + 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ── tail_indicates_interrupt ─────────────────────────────────────────

    fn interrupt_user(text: &str) -> serde_json::Value {
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{"type": "text", "text": text}] }
        })
    }

    fn lines_of(values: &[serde_json::Value]) -> String {
        values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn interrupt_detected_when_last_record_is_marker() {
        let tail = lines_of(&[
            assistant_text("作業中です"),
            interrupt_user("[Request interrupted by user]"),
        ]);
        assert!(tail_indicates_interrupt(&tail));
    }

    #[test]
    fn interrupt_detected_for_tool_use_variant() {
        let tail = lines_of(&[
            assistant_text("ツールを呼びます"),
            interrupt_user("[Request interrupted by user for tool use]"),
        ]);
        assert!(tail_indicates_interrupt(&tail));
    }

    #[test]
    fn no_interrupt_when_last_record_is_assistant() {
        // 通常の busy（assistant が末尾）では補正しない
        let tail = lines_of(&[
            interrupt_user("[Request interrupted by user]"),
            assistant_text("再開後の応答"),
        ]);
        assert!(!tail_indicates_interrupt(&tail));
    }

    #[test]
    fn no_interrupt_for_normal_user_prompt() {
        let tail = lines_of(&[json!({
            "type": "user",
            "message": {"role": "user", "content": "次の依頼"}
        })]);
        assert!(!tail_indicates_interrupt(&tail));
    }

    #[test]
    fn no_interrupt_for_tool_result_record() {
        // ツール実行往復中（末尾が tool_result）は busy 継続
        let tail = lines_of(&[json!({
            "type": "user",
            "message": { "content": [{"type": "tool_result", "content": "ok"}] }
        })]);
        assert!(!tail_indicates_interrupt(&tail));
    }

    #[test]
    fn interrupt_check_skips_non_message_records_and_broken_lines() {
        let mut tail = lines_of(&[
            interrupt_user("[Request interrupted by user]"),
            json!({"type": "permission-mode", "permissionMode": "default"}),
        ]);
        tail.push_str("\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"ty");
        assert!(tail_indicates_interrupt(&tail));
    }

    #[test]
    fn no_interrupt_for_empty_tail() {
        assert!(!tail_indicates_interrupt(""));
    }

    // ── read_tail 単体テスト ──────────────────────────────────────────────


    #[test]
    fn read_tail_returns_whole_when_smaller_than_max() {
        let path = write_jsonl(&[json!({"a": 1}), json!({"b": 2})]);
        let (tail, truncated) = read_tail(&path, 1_000_000).unwrap();
        assert!(!truncated);
        assert!(tail.contains("\"a\":1"));
        assert!(tail.contains("\"b\":2"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn read_tail_truncates_when_larger() {
        // 1 行あたり ~40B × 100 行 = ~4KB をファイルとして書き、64B だけ読む。
        let values: Vec<serde_json::Value> = (0..100)
            .map(|i| json!({"line": format!("{i:0>30}")}))
            .collect();
        let path = write_jsonl(&values);
        let (tail, truncated) = read_tail(&path, 64).unwrap();
        assert!(truncated);
        assert!(tail.len() <= 64);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
