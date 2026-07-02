//! transcript の raw JSON から「読み取れない thinking ブロック」を除去するフィルタ。
//!
//! 暗号化済み extended thinking（`{type:"thinking", thinking:"", signature:"..."}`）は
//! 表示も再利用もできないが、`signature` が 1〜2KB ある。これを除去することで
//! `messages.raw` のサイズを大きく削れる。
//!
//! 判定:
//! - `type == "redacted_thinking"`: 常に除去（内容が削除されたマーカー）
//! - `type == "thinking"` かつ `thinking` フィールドが空: 除去（暗号化のみ）
//! - `type == "thinking"` かつ `thinking` フィールドに本文あり: **残す**（open model 由来の平文）
//!
//! これにより「将来 open model を使った場合は思考本文を保持する」選択的記録になる。

use std::borrow::Cow;

use serde_json::Value;

/// raw JSON から空 thinking / redacted_thinking ブロックを除去した文字列を返す。
/// 変更が無ければ元の文字列をそのまま返す（アロケーションなし）。
pub fn strip_empty_thinking_blocks(raw: &str) -> Cow<'_, str> {
    let mut v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Cow::Borrowed(raw),
    };
    let Some(arr) = v
        .get_mut("message")
        .and_then(|m| m.get_mut("content"))
        .and_then(|c| c.as_array_mut())
    else {
        return Cow::Borrowed(raw);
    };
    let before = arr.len();
    arr.retain(|block| !is_empty_thinking(block));
    if arr.len() == before {
        return Cow::Borrowed(raw);
    }
    match serde_json::to_string(&v) {
        Ok(s) => Cow::Owned(s),
        Err(_) => Cow::Borrowed(raw),
    }
}

fn is_empty_thinking(block: &Value) -> bool {
    let ty = block.get("type").and_then(Value::as_str).unwrap_or("");
    if ty == "redacted_thinking" {
        return true;
    }
    if ty != "thinking" {
        return false;
    }
    let body = block
        .get("thinking")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    body.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_empty_thinking_and_keeps_others() {
        // 実物に近い構造: 空 thinking + redacted_thinking + tool_use を持つ assistant 行。
        let raw = r#"{"type":"assistant","sessionId":"s1","uuid":"u1","message":{"role":"assistant","content":[
            {"type":"thinking","thinking":"","signature":"AAAAlongbase64payload"},
            {"type":"redacted_thinking","data":"AAAA"},
            {"type":"text","text":"了解です"},
            {"type":"tool_use","name":"Bash","input":{"cmd":"ls"}}
        ]}}"#;
        let stripped = strip_empty_thinking_blocks(raw);
        assert!(
            stripped.len() < raw.len(),
            "空 thinking と redacted は除去される"
        );
        let v: Value = serde_json::from_str(&stripped).unwrap();
        let content = v["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
    }

    #[test]
    fn keeps_thinking_with_body() {
        // 本文がある thinking は残す（open model 由来想定）。
        let raw = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"まず方針を整理する..."},
            {"type":"text","text":"はい"}
        ]}}"#;
        let stripped = strip_empty_thinking_blocks(raw);
        // 変更なしなので Cow::Borrowed が返る。
        assert!(matches!(stripped, Cow::Borrowed(_)));
        assert_eq!(&*stripped, raw);
    }

    #[test]
    fn whitespace_only_thinking_is_treated_as_empty() {
        // 空白だけの thinking も除去する（境界ケース）。
        let raw = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"   \n  ","signature":"x"},
            {"type":"text","text":"はい"}
        ]}}"#;
        let stripped = strip_empty_thinking_blocks(raw);
        let v: Value = serde_json::from_str(&stripped).unwrap();
        let content = v["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn non_message_rows_are_unchanged() {
        // user 行や `message.content` を持たない system 行は素通り。
        let cases = [
            r#"{"type":"user","sessionId":"s1","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"system","subtype":"away_summary","content":"x"}"#,
        ];
        for c in cases {
            let s = strip_empty_thinking_blocks(c);
            assert!(matches!(s, Cow::Borrowed(_)));
            assert_eq!(&*s, c);
        }
    }

    #[test]
    fn broken_json_is_returned_as_is() {
        // 壊れた行も死なずそのまま返す（ingest は壊れ行を skip するので問題ない）。
        let raw = "not json{{";
        let s = strip_empty_thinking_blocks(raw);
        assert!(matches!(s, Cow::Borrowed(_)));
        assert_eq!(&*s, raw);
    }
}
