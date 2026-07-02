use std::path::{Path, PathBuf};

use super::types::InstanceInfo;
use crate::paths;

/// インスタンス用ディレクトリのパスを返す。
/// 例: `~/.ccc/instances/myproject-doexrslp/`
pub fn instance_dir_for(name: &str, hash: &str) -> anyhow::Result<PathBuf> {
    let safe_name = sanitize_name(name);
    Ok(paths::instances_dir()?.join(format!("{safe_name}-{hash}")))
}

/// ファイル名に使えない文字を `_` に置換する。
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// インスタンスディレクトリを作成する。
///
/// 旧方式では `claude_config/` も併せて作っていたが、新方式ではローカルの
/// `CLAUDE_CONFIG_DIR` は `~/.ccc/agent_settings/claude/<profile>/` を直接使い、
/// リモートはリモート側 `~/.ccc-tmp/<hash>/claude_config/` に scp 送信するため、
/// ここでは何も作らない。
pub fn ensure_instance_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// connection.json のパス
pub fn connection_path(dir: &Path) -> PathBuf {
    dir.join("connection.json")
}

/// connection.json を保存する。
pub fn save_connection(info: &InstanceInfo) -> anyhow::Result<()> {
    if info.instance_dir.as_os_str().is_empty() {
        return Ok(());
    }
    ensure_instance_dir(&info.instance_dir)?;
    let json = serde_json::to_string_pretty(info)?;
    std::fs::write(connection_path(&info.instance_dir), json)?;
    Ok(())
}

/// connection.json を読み込む。
pub fn load_connection(dir: &Path) -> anyhow::Result<InstanceInfo> {
    let path = connection_path(dir);
    let content = std::fs::read_to_string(&path)?;
    let info: InstanceInfo = serde_json::from_str(&content)?;
    Ok(info)
}

/// インスタンスディレクトリを完全削除する。
pub fn delete_instance_dir(dir: &Path) -> anyhow::Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// `~/.ccc/instances/` 配下のサブディレクトリ一覧を返す。
pub fn list_instance_dirs() -> anyhow::Result<Vec<PathBuf>> {
    let root = paths::instances_dir()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_name() {
        assert_eq!(sanitize_name("myproject"), "myproject");
        assert_eq!(sanitize_name("dev-host:foo"), "dev-host_foo");
        assert_eq!(sanitize_name("a/b\\c"), "a_b_c");
    }
}
