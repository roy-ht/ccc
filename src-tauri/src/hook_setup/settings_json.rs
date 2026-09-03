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
//!
//! 正規化の際、同一 event に複数の ccc エントリが並んでいたら 1 件に畳む。
//! command 表記を変更したバージョン（バイナリ直叩き → wrapper）で
//! 「旧エントリを検出できず新エントリを追加」→「後続バージョンが両方を
//! 同じ command に正規化」という経路で完全同一のエントリが 2 件残った実績が
//! あるため、merge のたびに自己修復させる。

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
/// `ccc-hook.sh` / `ccc-claude-code-hook`）が見つかった場合は command を
/// `hook_cmd` に正規化し、2 件目以降の ccc エントリは取り除いて 1 件に畳む。
/// これにより旧形式からの自動マイグレーションと、過去バージョンが作った
/// 重複登録の自己修復を同時に行う。
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

        if normalize_ccc_entries(entries, event, hook_cmd) {
            changed = true;
        }

        if !contains_ccc_hook(entries) {
            entries.push(make_ccc_entry(event, hook_cmd));
            changed = true;
        }
    }

    changed
}

/// 1 event 分のエントリ列について、ccc 由来エントリを 1 件に畳みつつ
/// command を `hook_cmd` に揃える。ccc エントリが無い場合や既に整っている
/// 場合は false を返す（変更なし）。
///
/// 残すエントリは「その event で ccc が本来書く matcher と一致するもの」を
/// 優先し、無ければ先頭の ccc エントリ。残さない側からは ccc の hook だけを
/// 取り除くので、同じエントリに同居している他用途 hook は保護される。
fn normalize_ccc_entries(entries: &mut Vec<Value>, event: &str, hook_cmd: &str) -> bool {
    let ccc_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry_has_ccc_hook(entry))
        .map(|(i, _)| i)
        .collect();
    if ccc_indices.is_empty() {
        return false;
    }

    let canonical = canonical_matcher(event);
    let keep = ccc_indices
        .iter()
        .copied()
        .find(|&i| entry_matcher(&entries[i]) == canonical)
        .unwrap_or(ccc_indices[0]);

    let mut changed = false;
    for i in ccc_indices {
        changed |= if i == keep {
            retain_single_ccc_hook(&mut entries[i], hook_cmd)
        } else {
            strip_ccc_hooks(&mut entries[i])
        };
    }

    // ccc hook を剥がした結果、実行するものが無くなったエントリは捨てる。
    let before = entries.len();
    entries.retain(|entry| !has_empty_hooks(entry));
    changed || entries.len() != before
}

/// 残す ccc エントリ側の処理: 最初の ccc hook を `hook_cmd` に正規化し、
/// 同一エントリ内に重複している 2 件目以降の ccc hook を取り除く。
fn retain_single_ccc_hook(entry: &mut Value, hook_cmd: &str) -> bool {
    let Some(hooks) = entry.get_mut("hooks").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    let mut seen = false;
    hooks.retain_mut(|h| {
        if !hook_is_ccc(h) {
            return true;
        }
        if seen {
            changed = true;
            return false;
        }
        seen = true;
        if h.get("command").and_then(|v| v.as_str()) != Some(hook_cmd) {
            h["command"] = Value::String(hook_cmd.to_string());
            changed = true;
        }
        true
    });
    changed
}

/// 畳まれる側のエントリから ccc hook だけを取り除く。
fn strip_ccc_hooks(entry: &mut Value) -> bool {
    let Some(hooks) = entry.get_mut("hooks").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let before = hooks.len();
    hooks.retain(|h| !hook_is_ccc(h));
    hooks.len() != before
}

fn hook_is_ccc(hook: &Value) -> bool {
    hook.get("command")
        .and_then(|v| v.as_str())
        .map(command_is_ccc_hook)
        .unwrap_or(false)
}

fn entry_has_ccc_hook(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|v| v.as_array())
        .map(|hooks| hooks.iter().any(hook_is_ccc))
        .unwrap_or(false)
}

fn entry_matcher(entry: &Value) -> Option<&str> {
    entry.get("matcher").and_then(|v| v.as_str())
}

/// `make_ccc_entry` が書き込む matcher。畳むときにどのエントリを残すかの判定に使う。
fn canonical_matcher(event: &str) -> Option<&'static str> {
    match event {
        "PreToolUse" | "PostToolUse" => Some("*"),
        _ => None,
    }
}

/// hooks が空配列になったエントリ（= 何も実行しない残骸）かどうか。
/// hooks キー自体が無い/配列でないエントリは判断できないので残す。
fn has_empty_hooks(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|v| v.as_array())
        .map(|hooks| hooks.is_empty())
        .unwrap_or(false)
}

/// ccc の hook 定義が既に含まれているかチェック。
/// command の basename が `ccc-claude-code-hook` であるエントリを ccc 由来とみなす。
fn contains_ccc_hook(entries: &[Value]) -> bool {
    entries.iter().any(entry_has_ccc_hook)
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
    fn merge_collapses_identical_duplicate_entries() {
        // 過去バージョンが作った「完全同一の 2 エントリ」を 1 件に畳む。
        let mut v = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "*", "hooks": [{ "type": "command", "command": TEST_CMD }] },
                    { "matcher": "*", "hooks": [{ "type": "command", "command": TEST_CMD }] }
                ]
            }
        });
        let changed = merge_into(&mut v, TEST_CMD);
        assert!(changed, "重複解消は変更として報告される");
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(!merge_into(&mut v, TEST_CMD), "畳んだ後は冪等");
    }

    #[test]
    fn merge_collapses_legacy_and_current_entries() {
        // 「旧 command のエントリ + 現行 command のエントリ」が並んでいたら、
        // 正規化してから 1 件に畳む（両方を同じ command に書き換えて放置しない）。
        let mut v = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "~/.ccc/bin/ccc-claude-code-hook" }] },
                    { "hooks": [{ "type": "command", "command": TEST_CMD }] }
                ]
            }
        });
        let changed = merge_into(&mut v, TEST_CMD);
        assert!(changed);
        let arr = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["hooks"][0]["command"].as_str(), Some(TEST_CMD));
    }

    #[test]
    fn merge_keeps_canonical_matcher_entry_when_collapsing() {
        // matcher 違いの ccc エントリが混ざっていたら、ccc が本来書く
        // matcher (`*`) のエントリを残す。
        let mut v = json!({
            "hooks": {
                "PostToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": TEST_CMD }] },
                    { "matcher": "*", "hooks": [{ "type": "command", "command": TEST_CMD }] }
                ]
            }
        });
        merge_into(&mut v, TEST_CMD);
        let arr = v["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["matcher"], "*");
    }

    #[test]
    fn merge_collapse_preserves_other_hooks_in_same_entry() {
        // 畳む側のエントリに他用途 hook が同居していたら、そちらは残す。
        let mut v = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": TEST_CMD }] },
                    { "hooks": [
                        { "type": "command", "command": TEST_CMD },
                        { "type": "command", "command": "claude-notify" }
                    ]}
                ]
            }
        });
        merge_into(&mut v, TEST_CMD);
        let arr = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(arr[1]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(
            arr[1]["hooks"][0]["command"].as_str(),
            Some("claude-notify"),
            "他用途 hook は残る"
        );
    }

    #[test]
    fn merge_collapses_duplicates_inside_single_entry() {
        // 同一エントリ内に ccc hook が 2 つ並ぶケースも 1 つに畳む。
        let mut v = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [
                        { "type": "command", "command": TEST_CMD },
                        { "type": "command", "command": TEST_CMD }
                    ]}
                ]
            }
        });
        let changed = merge_into(&mut v, TEST_CMD);
        assert!(changed);
        let arr = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["hooks"].as_array().unwrap().len(), 1);
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
