//! Claude Code hook を ccc に接続するためのセットアップ処理。
//!
//! - `~/.ccc/bin/ccc-claude-code-hook` のバイナリ配信
//! - `~/.ccc/bin/ccc-hook.sh` のラッパースクリプト生成
//!   (`CCC_HOOK_ENDPOINT` / `CCC_SESSION_TOKEN` を ccc 起動ごとに更新)
//! - 各プロファイル (`~/.ccc/agent_settings/claude/<profile>/settings.json`) への
//!   hook 定義の冪等 merge (`command` は wrapper のパスを指す)

pub mod binary;
pub mod settings_json;
pub mod wrapper;

/// settings.json に書き込む hook `command` のパス文字列。
///
/// 同じ `agent_settings/claude/<profile>/settings.json` がローカル機と
/// リモート機の両方で読まれるため、ホスト毎のホームディレクトリで解決される
/// チルダ表記を使う。Claude Code は hook command をシェル経由で起動するため
/// チルダはランタイムに展開される。
///
/// 実体はラッパースクリプトで、内部で `ccc-claude-code-hook` バイナリを
/// `CCC_HOOK_ENDPOINT` / `CCC_SESSION_TOKEN` 付きで exec する。ccc 再起動で
/// endpoint が変わってもこのパスは不変なので、tmux 環境変数の焼き込み問題を
/// 回避できる（詳細は `wrapper.rs` を参照）。
pub const HOOK_BINARY_COMMAND: &str = "~/.ccc/bin/ccc-hook.sh";

/// `~/.ccc/bin` ディレクトリの絶対パスを返す（ローカル機上のファイル配置用）。
pub fn hook_bin_dir() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::paths::ccc_root()?.join("bin"))
}

/// `~/.ccc/agent_settings/claude/` 直下の全プロファイルディレクトリへ
/// hook 定義の冪等 merge をかける。
///
/// アプリ起動時に呼び、リストア経由のインスタンス（`local_claude_config_dir`
/// を通らない）でも hook が確実に登録されるようにする。
///
/// 戻り値: 実際に書き換えが発生したプロファイル名のリスト。
pub fn merge_settings_for_all_profiles() -> anyhow::Result<Vec<String>> {
    let claude_root = crate::paths::agent_settings_dir()?.join("claude");
    let mut updated = Vec::new();
    let read_dir = match std::fs::read_dir(&claude_root) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(updated),
        Err(e) => return Err(e.into()),
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match settings_json::merge_into_config_dir(&path) {
            Ok(true) => {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    updated.push(name.to_string());
                }
            }
            Ok(false) => {}
            Err(e) => eprintln!(
                "[ccc] {} の hook merge に失敗: {e}",
                path.join("settings.json").display()
            ),
        }
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_binary_command_uses_tilde() {
        // ローカル/リモート両ホストで解決可能であるため、絶対パスではなく
        // チルダ表記を使う。
        assert!(HOOK_BINARY_COMMAND.starts_with("~/"));
        assert!(HOOK_BINARY_COMMAND.ends_with(wrapper::WRAPPER_SCRIPT_NAME));
    }
}
