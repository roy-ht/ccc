//! Claude Code のユーザー設定ファイル (`.claude.json`) の選択的マージ。
//!
//! # 背景
//!
//! このファイルは「マシン非依存のアカウント設定」と「マシン依存の状態」を
//! 1 つに混ぜて持っている。v0.10.3 まではリモート同期時に rsync で丸ごと
//! 上書きしていたため、リモート側の状態が毎回巻き戻っていた。
//!
//! 特に `projects` は**絶対パスをキーとするオブジェクト**で、
//! `hasTrustDialogAccepted` や `allowedTools` を保持している。ローカルの
//! `/Users/...` 起点のエントリで上書きすると、リモートでは trust ダイアログと
//! 権限プロンプトが毎回復活し、ローカル側のパスがゴミとして混入する。
//!
//! # 方針
//!
//! **allowlist 方式**。[`SYNCED_KEYS`] に列挙したトップレベルキーだけを
//! ローカル値で上書きし、それ以外はリモートの値をそのまま残す。
//! 判断のつかない新しいキーは自動的に「移送しない」側へ倒れるため、
//! Claude Code のバージョンアップでキーが増えてもリモート状態を壊さない。

use serde_json::{Map, Value};

/// ローカル → リモートへ移送するトップレベルキー。
///
/// 収録基準は「アカウントの同定に必要」または「起動時の対話を抑止する」もののみ。
/// 迷ったら入れない（リモート側の値を尊重する）。
///
/// 意図的に**除外**している代表例と理由:
///
/// - `projects` — 絶対パスがキー。移送するとリモートの trust 承諾 / 許可済み
///   ツール / 入力履歴が消える。このモジュールを作った最大の理由
/// - `machineID` — マシン識別子。移送すると 2 台が同じ ID を名乗る
/// - `installMethod` / `autoUpdates` / `autoUpdatesProtectedForNative` —
///   インストール形態依存 (macOS native と Linux npm では異なる)
/// - `numStartups` / `firstStartTime` / `skillUsage` / `pluginUsage` /
///   `promptQueueUseCount` — マシンごとの利用統計
/// - `*Cache` 系 (`overageCreditGrantCache`, `modelAccessCache`,
///   `clientDataCacheSlots` 等) — 古いキャッシュを押し付けることになる
/// - `githubRepoPaths` / `deepLinkTerminal` — パス・端末依存
/// - `migrationVersion` / `official Marketplace*` — リモート側の Claude Code が
///   自分で判断すべき状態。移送すると必要な移行処理がスキップされうる
pub const SYNCED_KEYS: &[&str] = &[
    // アカウント同定。資格情報と対で必要。
    "oauthAccount",
    "userID",
    "claudeCodeFirstTokenDate",
    // 初回起動時のオンボーディング対話を抑止する。
    "hasCompletedOnboarding",
    "lastOnboardingVersion",
    // 既読状態。リモートで一度見た通知・Tips を再表示させない。
    "tipsHistory",
    "tipLifetimeShownCounts",
    "seenNotifications",
    "lastReleaseNotesSeen",
    "hasSeenAutoModeEntryWarning",
    // 組織属性。
    "penguinModeOrgEnabled",
];

/// マージ結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    /// リモートへ書き戻すべき JSON。
    pub merged: Value,
    /// 実際に値が変わったキー（ログ用）。
    pub updated_keys: Vec<&'static str>,
}

/// リモートの JSON にローカルの移送対象キーを重ねる。
///
/// 実際に変更が生じた場合のみ `Some` を返す。無変更なら `None` を返し、
/// 呼び出し側は書き込み自体をスキップできる（走行中の Claude が持つファイルを
/// 不必要に触らないため）。
///
/// `remote` がオブジェクトでない場合（ファイル不在で `null` を渡した場合を含む）は
/// 空オブジェクトから組み立てる。この場合、移送対象キーだけを持つ最小の
/// `.claude.json` が生成され、残りは Claude Code 自身が埋める。
pub fn merge(local: &Value, remote: &Value) -> Option<MergeResult> {
    let local_obj = local.as_object()?;
    let mut merged: Map<String, Value> = remote.as_object().cloned().unwrap_or_default();
    let mut updated_keys = Vec::new();

    for key in SYNCED_KEYS {
        let Some(local_value) = local_obj.get(*key) else {
            // ローカルに無いキーでリモートを消さない。
            continue;
        };
        if merged.get(*key) == Some(local_value) {
            continue;
        }
        merged.insert((*key).to_string(), local_value.clone());
        updated_keys.push(*key);
    }

    if updated_keys.is_empty() {
        return None;
    }
    Some(MergeResult {
        merged: Value::Object(merged),
        updated_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_remote_projects() {
        // 最重要のケース: リモートのプロジェクト状態を絶対に壊さない
        let local = json!({
            "oauthAccount": {"accountUuid": "u1"},
            "projects": {
                "/Users/user/work": {"hasTrustDialogAccepted": true, "allowedTools": ["Bash"]}
            }
        });
        let remote = json!({
            "projects": {
                "/home/user/work": {"hasTrustDialogAccepted": true, "allowedTools": ["Read"]}
            }
        });
        let r = merge(&local, &remote).unwrap();
        assert_eq!(
            r.merged["projects"],
            json!({"/home/user/work": {"hasTrustDialogAccepted": true, "allowedTools": ["Read"]}}),
            "リモートの projects が変化している"
        );
        assert_eq!(r.merged["oauthAccount"], json!({"accountUuid": "u1"}));
        assert_eq!(r.updated_keys, vec!["oauthAccount"]);
    }

    #[test]
    fn machine_specific_keys_are_never_transferred() {
        let local = json!({
            "machineID": "local-machine",
            "installMethod": "native",
            "autoUpdates": false,
            "numStartups": 999,
            "modelAccessCache": ["local"],
            "githubRepoPaths": {"/Users/user/repo": "x"},
            "hasCompletedOnboarding": true,
        });
        let remote = json!({
            "machineID": "remote-machine",
            "installMethod": "npm",
            "autoUpdates": true,
            "numStartups": 3,
            "modelAccessCache": ["remote"],
        });
        let r = merge(&local, &remote).unwrap();
        assert_eq!(r.merged["machineID"], "remote-machine");
        assert_eq!(r.merged["installMethod"], "npm");
        assert_eq!(r.merged["autoUpdates"], true);
        assert_eq!(r.merged["numStartups"], 3);
        assert_eq!(r.merged["modelAccessCache"], json!(["remote"]));
        assert!(r.merged.get("githubRepoPaths").is_none());
        // 移送対象は反映される
        assert_eq!(r.merged["hasCompletedOnboarding"], true);
        assert_eq!(r.updated_keys, vec!["hasCompletedOnboarding"]);
    }

    #[test]
    fn unknown_future_keys_default_to_remote() {
        // Claude Code の更新で増えた未知キーはリモート側の値を尊重する
        let local = json!({"someBrandNewSetting": "local", "userID": "u"});
        let remote = json!({"someBrandNewSetting": "remote"});
        let r = merge(&local, &remote).unwrap();
        assert_eq!(r.merged["someBrandNewSetting"], "remote");
    }

    #[test]
    fn returns_none_when_nothing_changes() {
        let local = json!({"userID": "u1", "numStartups": 1});
        let remote = json!({"userID": "u1", "numStartups": 500});
        assert_eq!(merge(&local, &remote), None);
    }

    #[test]
    fn seeds_minimal_object_when_remote_is_absent() {
        let local = json!({
            "oauthAccount": {"accountUuid": "u1"},
            "hasCompletedOnboarding": true,
            "lastOnboardingVersion": "1.2.3",
            "machineID": "local",
            "projects": {"/Users/user/x": {}},
        });
        let r = merge(&local, &Value::Null).unwrap();
        let obj = r.merged.as_object().unwrap();
        assert_eq!(obj.len(), 3, "移送対象キーだけが入るべき: {obj:?}");
        assert!(obj.contains_key("oauthAccount"));
        assert!(obj.contains_key("hasCompletedOnboarding"));
        assert!(obj.contains_key("lastOnboardingVersion"));
        assert!(!obj.contains_key("machineID"));
        assert!(!obj.contains_key("projects"));
    }

    #[test]
    fn missing_local_key_does_not_delete_remote_value() {
        let local = json!({"userID": "u1"});
        let remote = json!({"userID": "u0", "hasCompletedOnboarding": true});
        let r = merge(&local, &remote).unwrap();
        assert_eq!(r.merged["hasCompletedOnboarding"], true);
        assert_eq!(r.merged["userID"], "u1");
    }

    #[test]
    fn non_object_local_is_ignored() {
        assert_eq!(merge(&json!("garbage"), &json!({})), None);
        assert_eq!(merge(&Value::Null, &json!({})), None);
    }

    #[test]
    fn non_object_remote_is_replaced_by_fresh_object() {
        let local = json!({"userID": "u1"});
        let r = merge(&local, &json!("garbage")).unwrap();
        assert_eq!(r.merged, json!({"userID": "u1"}));
    }

    #[test]
    fn projects_is_not_in_the_allowlist() {
        // 回帰防止: 誤って projects を allowlist に足さないこと
        assert!(!SYNCED_KEYS.contains(&"projects"));
        assert!(!SYNCED_KEYS.contains(&"machineID"));
    }
}
