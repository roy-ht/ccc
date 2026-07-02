use std::path::PathBuf;

// `~/.ccc[/dev]` の解決ロジックは ccc-sshkit に移動した（v0.10、ccc-ssh CLI と
// 必ず同じディレクトリを見るための単一の正）。既存参照のため再エクスポートする。
pub use ccc_sshkit::paths::{ccc_root, data_root};

/// `~/.ccc/settings.json` または dev 時 `~/.ccc/dev/settings.json`
pub fn settings_path() -> anyhow::Result<PathBuf> {
    Ok(data_root()?.join("settings.json"))
}

/// `~/.ccc/instances/` または dev 時 `~/.ccc/dev/instances/`
pub fn instances_dir() -> anyhow::Result<PathBuf> {
    Ok(data_root()?.join("instances"))
}

/// `~/.ccc/archive/` または dev 時 `~/.ccc/dev/archive/`（集約 DB の置き場）
pub fn archive_dir() -> anyhow::Result<PathBuf> {
    Ok(data_root()?.join("archive"))
}

/// 集約 DB のパス `~/.ccc[/dev]/archive/sessions.db`
pub fn archive_db_path() -> anyhow::Result<PathBuf> {
    Ok(archive_dir()?.join("sessions.db"))
}

/// `~/.ccc/agent_settings/`（dev/本番で共有）
pub fn agent_settings_dir() -> anyhow::Result<PathBuf> {
    Ok(ccc_root()?.join("agent_settings"))
}

/// `~/.ccc/agent_settings/claude/<profile>/`
pub fn claude_agent_settings_dir(profile: &str) -> anyhow::Result<PathBuf> {
    Ok(agent_settings_dir()?.join("claude").join(profile))
}

/// `~/.ccc/agent_settings/claude/default/`
pub fn default_claude_agent_settings_dir() -> anyhow::Result<PathBuf> {
    claude_agent_settings_dir("default")
}

/// アプリ起動時に必要なディレクトリ群を作成する。
pub fn ensure_layout() -> anyhow::Result<()> {
    std::fs::create_dir_all(data_root()?)?;
    std::fs::create_dir_all(instances_dir()?)?;
    std::fs::create_dir_all(archive_dir()?)?;
    std::fs::create_dir_all(default_claude_agent_settings_dir()?)?;
    Ok(())
}

/// ccc 本体バイナリ (`current_exe()`) と同ディレクトリにある同梱 sidecar のパスを返す。
///
/// Tauri は dev 実行時に `<name>-<target-triple>` 名でコピーし、
/// 配布バンドルでは suffix を剥がして `<name>` で配置する。両方をフォールバックで探索する。
fn sidecar_bin(name: &str) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("current_exe has no parent: {}", exe.display()))?;
    let plain = dir.join(name);
    if plain.exists() {
        return Ok(plain);
    }
    let target = env!("BUILD_TARGET");
    Ok(dir.join(format!("{name}-{target}")))
}

/// 同梱 sidecar `ccc-claude-auth`（認証取得）のパス。
pub fn ccc_claude_auth_bin() -> anyhow::Result<PathBuf> {
    sidecar_bin("ccc-claude-auth")
}

/// 同梱 CLI `ccc-sessions`（セッション集約の外部連携）のパス。
pub fn ccc_sessions_bin() -> anyhow::Result<PathBuf> {
    sidecar_bin("ccc-sessions")
}

/// 同梱 CLI `ccc-ssh`（ssh ラッパー）のパス。
pub fn ccc_ssh_bin() -> anyhow::Result<PathBuf> {
    sidecar_bin("ccc-ssh")
}
