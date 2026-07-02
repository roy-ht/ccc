//! インスタンス用 `CLAUDE_CONFIG_DIR/settings.json` への hook 定義の冪等 merge。
//!
//! ccc は各インスタンスごとに `~/.ccc/agent_settings/claude/<profile>/` を
//! `CLAUDE_CONFIG_DIR` として注入するため、hook 定義もそのディレクトリ内の
//! `settings.json` にマージする。
//!
//! Claude Code の hook 仕様では、同一 event に対して複数の matcher と複数の
//! hook を並列に登録できる。ccc が登録するエントリは command の basename が
//! `ccc-hook.sh` (現行) または `ccc-claude-code-hook` (旧形式) のものを
//! 一意マーカーとして判定する。新規 merge では現行値 `HOOK_BINARY_COMMAND`
//! (= wrapper script) に正規化するため、旧バイナリ直叩き形式から
//! ラッパー方式への自動マイグレーションも兼ねる。

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;

use super::wrapper;
use super::HOOK_BINARY_COMMAND;

/// ccc が登録対象とする hook event の一覧。
pub const HOOKED_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "PermissionRequest",
    "Stop",
    "StopFailure",
];

/// settings.json を読み込む（存在しない場合は空オブジェクト）。
pub fn read_settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Default::default()));
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("settings.json の読み込みに失敗: {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let v: Value = serde_json::from_str(&raw)
        .with_context(|| format!("settings.json のJSONパースに失敗: {}", path.display()))?;
    Ok(v)
}

/// 指定された `CLAUDE_CONFIG_DIR` 配下の `settings.json` に ccc の hook 定義を
/// 冪等にマージする。
///
/// - `config_dir` 自体は呼び出し側が事前に作成している前提
/// - 既存の他用途 hook は破壊しない（並列追加）
/// - 既に ccc の hook が登録済みなら何もせず変更フラグ false を返す
pub fn merge_into_config_dir(config_dir: &Path) -> Result<bool> {
    let path = config_dir.join("settings.json");

    let mut current = read_settings(&path)?;
    let changed = merge_into(&mut current, HOOK_BINARY_COMMAND);
    if !changed {
        return Ok(false);
    }

    let pretty = serde_json::to_string_pretty(&current)?;
    std::fs::write(&path, pretty)
        .with_context(|| format!("settings.json への書き込みに失敗: {}", path.display()))?;
    Ok(true)
}

/// 既存 settings.json (Value) に hook 定義を冪等マージする。
/// 何か変更があれば true を返す。
///
/// 各 event について、ccc 由来の既存エントリ（command の basename が
/// `ccc-claude-code-hook`）が見つかった場合は command を `hook_cmd` に正規化する。
/// これにより旧形式 `~/.ccc/bin/...` から絶対パスへの自動マイグレーションを行う。
pub fn merge_into(root: &mut Value, hook_cmd: &str) -> bool {
    if !root.is_object() {
        *root = Value::Object(Default::default());
    }
    let obj = root.as_object_mut().expect("root is object");

    let hooks_entry = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks_entry.is_object() {
        *hooks_entry = json!({});
    }
    let hooks = hooks_entry.as_object_mut().expect("hooks is object");

    let mut changed = false;
    for event in HOOKED_EVENTS {
        let arr = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !arr.is_array() {
            *arr = Value::Array(Vec::new());
        }
        let entries = arr.as_array_mut().expect("entries is array");

        if normalize_ccc_hook_commands(entries, hook_cmd) {
            changed = true;
            continue;
        }

        if !contains_ccc_hook(entries) {
            entries.push(make_ccc_entry(event, hook_cmd));
            changed = true;
        }
    }

    changed
}

/// 既存の ccc 由来エントリの command を `hook_cmd` に揃える。
/// 既に揃っているか ccc エントリが無い場合は false を返す（変更なし）。
fn normalize_ccc_hook_commands(entries: &mut [Value], hook_cmd: &str) -> bool {
    let mut changed = false;
    let mut found_ccc = false;
    for entry in entries.iter_mut() {
        let Some(hooks) = entry.get_mut("hooks").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for h in hooks {
            let Some(cmd) = h.get("command").and_then(|v| v.as_str()) else {
                continue;
            };
            if command_is_ccc_hook(cmd) {
                found_ccc = true;
                if cmd != hook_cmd {
                    h["command"] = Value::String(hook_cmd.to_string());
                    changed = true;
                }
            }
        }
    }
    // ccc エントリが存在しなければ false を返し、上位で新規追加に進ませる。
    found_ccc && changed
}

/// ccc の hook 定義が既に含まれているかチェック。
/// command の basename が `ccc-claude-code-hook` であるエントリを ccc 由来とみなす。
fn contains_ccc_hook(entries: &[Value]) -> bool {
    entries.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|v| v.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|v| v.as_str())
                        .map(command_is_ccc_hook)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

fn command_is_ccc_hook(cmd: &str) -> bool {
    // 旧バイナリ直叩き (`ccc-claude-code-hook`) と現行ラッパー (`ccc-hook.sh`)
    // のどちらも ccc 由来として認識し、merge_into で現行コマンドに上書きする。
    wrapper::path_is_ccc_hook(cmd)
}

fn make_ccc_entry(event: &str, hook_cmd: &str) -> Value {
    let mut entry = json!({
        "hooks": [{
            "type": "command",
            "command": hook_cmd
        }]
    });
    // ツール毎に発火する event は matcher を入れておく
    if matches!(event, "PreToolUse" | "PostToolUse") {
        entry["matcher"] = Value::String("*".to_string());
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CMD: &str = "/Users/test/.ccc/bin/ccc-claude-code-hook";

    #[test]
    fn merge_into_empty() {
        let mut v = json!({});
        let changed = merge_into(&mut v, TEST_CMD);
        assert!(changed);
        let hooks = v.get("hooks").unwrap().as_object().unwrap();
        assert_eq!(hooks.len(), HOOKED_EVENTS.len());
        for event in HOOKED_EVENTS {
            let arr = hooks.get(*event).unwrap().as_array().unwrap();
            assert_eq!(arr.len(), 1);
        }
    }

    #[test]
    fn merge_into_idempotent() {
        let mut v = json!({});
        merge_into(&mut v, TEST_CMD);
        let changed = merge_into(&mut v, TEST_CMD);
        assert!(!changed, "二回目の merge では変更なしになるべき");
    }

    #[test]
    fn merge_migrates_existing_ccc_entry_command() {
        // 既存の ccc 由来エントリは basename 一致で検出し、command を最新形式に
        // 書き換える（旧絶対パスやチルダ表記の混在を正規化する）。
        let mut v = json!({
            "hooks": {
                "Stop": [
                    {
                        "hooks": [{ "type": "command", "command": "/old/abs/ccc-claude-code-hook" }]
                    }
                ]
            }
        });
        let changed = merge_into(&mut v, TEST_CMD);
        assert!(changed);
        let stop_arr = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop_arr.len(), 1, "重複追加せず1件のまま");
        assert_eq!(
            stop_arr[0]["hooks"][0]["command"].as_str(),
            Some(TEST_CMD),
            "command が新形式に正規化される"
        );
    }

    #[test]
    fn merge_skips_when_already_absolute() {
        // 既に絶対パスで登録されていれば変更なし。
        let mut v = json!({});
        merge_into(&mut v, TEST_CMD);
        let changed = merge_into(&mut v, TEST_CMD);
        assert!(!changed);
    }

    #[test]
    fn merge_preserves_existing_hooks() {
        let mut v = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{ "type": "command", "command": "/path/to/other-hook" }]
                    }
                ]
            }
        });
        merge_into(&mut v, TEST_CMD);

        let arr = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "既存 hook と ccc hook が並列に存在");
        assert!(arr.iter().any(|e| e["matcher"] == "Bash"));
        assert!(arr
            .iter()
            .any(|e| { e["hooks"][0]["command"].as_str() == Some(TEST_CMD) }));
    }

    #[test]
    fn merge_handles_non_object_hooks_field() {
        let mut v = json!({ "hooks": "broken" });
        let changed = merge_into(&mut v, TEST_CMD);
        assert!(changed);
        assert!(v["hooks"].is_object());
    }

    #[test]
    fn matcher_only_for_tool_events() {
        let entry_pre = make_ccc_entry("PreToolUse", TEST_CMD);
        assert_eq!(entry_pre["matcher"], "*");
        let entry_stop = make_ccc_entry("Stop", TEST_CMD);
        assert!(entry_stop.get("matcher").is_none());
    }

    #[test]
    fn command_is_ccc_hook_recognizes_basename() {
        // 現行ラッパー
        assert!(command_is_ccc_hook("/Users/me/.ccc/bin/ccc-hook.sh"));
        assert!(command_is_ccc_hook("~/.ccc/bin/ccc-hook.sh"));
        assert!(command_is_ccc_hook("ccc-hook.sh"));
        // 旧バイナリ直叩き（マイグレーション対象として認識）
        assert!(command_is_ccc_hook(
            "/Users/me/.ccc/bin/ccc-claude-code-hook"
        ));
        assert!(command_is_ccc_hook("~/.ccc/bin/ccc-claude-code-hook"));
        assert!(command_is_ccc_hook("ccc-claude-code-hook"));
        assert!(!command_is_ccc_hook("/usr/bin/other-hook"));
        assert!(!command_is_ccc_hook(""));
    }

    #[test]
    fn merge_migrates_legacy_binary_command_to_wrapper() {
        // 旧形式 (バイナリ直叩き) で登録されたエントリは ccc 由来として
        // 検出され、現行のラッパースクリプトパスに置き換えられる。
        let mut v = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": "~/.ccc/bin/ccc-claude-code-hook"
                    }]
                }]
            }
        });
        let new_cmd = "/Users/test/.ccc/bin/ccc-hook.sh";
        let changed = merge_into(&mut v, new_cmd);
        assert!(changed);
        let stop_arr = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop_arr.len(), 1, "重複追加せず1件のまま");
        assert_eq!(
            stop_arr[0]["hooks"][0]["command"].as_str(),
            Some(new_cmd),
            "command が wrapper パスに正規化される"
        );
    }
}
