//! `ccc-claude-code-hook` バイナリの配信。
//!
//! - ローカル: ホストアーキ用バイナリを `~/.ccc/bin/ccc-claude-code-hook` にコピー
//! - リモート: `uname -sm` でアーキ判定 → 該当バイナリを scp で送信
//!
//! バージョン管理は `--version` 出力を突き合わせて行う（不一致なら再配信）。

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::hook_bin_dir;

/// 期待される hook バイナリのバージョン。
/// `ccc-claude-code-hook` crate の version と同期。
pub const EXPECTED_VERSION: &str = "0.3.0";

/// プラットフォーム識別子。`ccc-claude-code-hook` の `--platform` 出力と整合させる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    DarwinArm64,
    LinuxArm64,
    LinuxAmd64,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::DarwinArm64 => "darwin-arm64",
            Platform::LinuxArm64 => "linux-arm64",
            Platform::LinuxAmd64 => "linux-amd64",
        }
    }

    /// `uname -sm` 出力（"Darwin arm64" / "Linux x86_64" 等）から判定。
    pub fn from_uname(uname_sm: &str) -> Result<Self> {
        let parts: Vec<&str> = uname_sm.split_whitespace().collect();
        let (os, arch) = match parts.as_slice() {
            [os, arch, ..] => (*os, *arch),
            _ => return Err(anyhow!("uname -sm の形式が不正: {uname_sm:?}")),
        };
        match (os, arch) {
            ("Darwin", "arm64") => Ok(Platform::DarwinArm64),
            ("Linux", "aarch64") | ("Linux", "arm64") => Ok(Platform::LinuxArm64),
            ("Linux", "x86_64") | ("Linux", "amd64") => Ok(Platform::LinuxAmd64),
            _ => Err(anyhow!("未対応のプラットフォーム: {os} {arch}")),
        }
    }

    /// 現在のホスト用 Platform。
    pub fn host() -> Result<Self> {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Ok(Platform::DarwinArm64)
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Ok(Platform::LinuxArm64)
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Ok(Platform::LinuxAmd64)
        } else {
            Err(anyhow!("現在のホストOSは未対応です"))
        }
    }
}

/// 同梱バイナリの探索先一覧（先頭から順に試す）。
///
/// 配布バンドル (`tauri.conf.json` の `bundle.resources` 経由) では
/// macOS の場合 `ccc.app/Contents/Resources/binaries/...` に配置される。
/// 開発時は `cargo build` の出力ディレクトリと、`just prepare-hook(-all)`
/// で生成される `<repo>/src-tauri/binaries/...` をフォールバックとして探す。
fn bundled_binary_search_paths(platform: Platform) -> Result<Vec<PathBuf>> {
    let exe = std::env::current_exe()?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| anyhow!("current_exe has no parent"))?;
    let mut candidates = Vec::new();

    // 1) macOS .app バンドルの Resources 配下 (本番配布の正規位置)
    //    exe_dir = `ccc.app/Contents/MacOS` → ../Resources/binaries/...
    if let Some(contents_dir) = exe_dir.parent() {
        candidates.push(
            contents_dir
                .join("Resources")
                .join("binaries")
                .join("ccc-claude-code-hook")
                .join(platform.as_str())
                .join("ccc-claude-code-hook"),
        );
    }

    // 2) exe_dir 直下の binaries/ (Linux/Windows 配布や、手動配置の応急対応用)
    candidates.push(
        exe_dir
            .join("binaries")
            .join("ccc-claude-code-hook")
            .join(platform.as_str())
            .join("ccc-claude-code-hook"),
    );

    // 3) 開発時の prepare-hook 配置先 (`<repo>/src-tauri/binaries/ccc-claude-code-hook/<platform>/`)
    //    exe_dir = `<repo>/src-tauri/target/<profile>` → 親の親 = `<repo>/src-tauri/`
    if let Some(src_tauri_dir) = exe_dir.parent().and_then(|p| p.parent()) {
        candidates.push(
            src_tauri_dir
                .join("binaries")
                .join("ccc-claude-code-hook")
                .join(platform.as_str())
                .join("ccc-claude-code-hook"),
        );
    }

    // 4) ホスト用は cargo build の出力場所もフォールバックとして見る
    if platform == Platform::host()? {
        candidates.push(exe_dir.join("ccc-claude-code-hook"));
        if let Some(target_dir) = exe_dir.parent() {
            candidates.push(target_dir.join("debug").join("ccc-claude-code-hook"));
            candidates.push(target_dir.join("release").join("ccc-claude-code-hook"));
        }
    }

    Ok(candidates)
}

/// 同梱バイナリのうち、最初に見つかった実在パスを返す。
pub fn bundled_binary(platform: Platform) -> Result<PathBuf> {
    for cand in bundled_binary_search_paths(platform)? {
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(anyhow!(
        "{} 用の同梱バイナリが見つかりません（src-tauri/binaries/ccc-claude-code-hook/{}/ に配置してください）",
        platform.as_str(),
        platform.as_str()
    ))
}

/// 既存の `~/.ccc/bin/ccc-claude-code-hook` の `--version` 出力を取得。
/// バイナリが存在しない／実行失敗なら None。
fn local_installed_version(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let out = Command::new(path).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // `clap` のデフォルト形式は "ccc-claude-code-hook X.Y.Z"
    s.split_whitespace().last().map(String::from)
}

/// ローカル `~/.ccc/bin/ccc-claude-code-hook` を最新バイナリで上書きする。
/// バージョンが既に最新なら何もせず Ok(false) を返す。
pub fn install_local() -> Result<bool> {
    let target_dir = hook_bin_dir()?;
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("ディレクトリ作成失敗: {}", target_dir.display()))?;
    let target_path = target_dir.join("ccc-claude-code-hook");

    if local_installed_version(&target_path).as_deref() == Some(EXPECTED_VERSION) {
        return Ok(false);
    }

    let host = Platform::host()?;
    let src = bundled_binary(host)?;
    std::fs::copy(&src, &target_path)
        .with_context(|| format!("コピー失敗: {} → {}", src.display(), target_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&target_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&target_path, perms)?;
    }
    Ok(true)
}

/// リモートホストの `uname -sm` を取得して Platform を判定する。
pub fn detect_remote_platform(host_alias: &str) -> Result<Platform> {
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host_alias, "uname -sm"])
        .output()
        .with_context(|| format!("ssh uname 実行失敗 (host={host_alias})"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "リモート '{host_alias}' で uname -sm が失敗: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Platform::from_uname(&raw)
}

/// リモートに `~/.ccc/bin/ccc-claude-code-hook` を配信する。
///
/// 既に存在しバージョンが最新なら何もしない。
pub fn install_remote(host_alias: &str) -> Result<bool> {
    let platform = detect_remote_platform(host_alias)?;

    // 既にインストール済みでバージョンが一致するか確認
    let check = Command::new("ssh")
        .args([
            "-o", "BatchMode=yes", host_alias,
            "test -x ~/.ccc/bin/ccc-claude-code-hook && ~/.ccc/bin/ccc-claude-code-hook --version || true",
        ])
        .output()
        .with_context(|| format!("ssh version check 失敗 (host={host_alias})"))?;
    let remote_version = String::from_utf8_lossy(&check.stdout)
        .split_whitespace()
        .last()
        .map(String::from);
    if remote_version.as_deref() == Some(EXPECTED_VERSION) {
        return Ok(false);
    }

    // ディレクトリを作成
    let mkdir = Command::new("ssh")
        .args([host_alias, "mkdir -p ~/.ccc/bin"])
        .status()
        .with_context(|| format!("ssh mkdir 失敗 (host={host_alias})"))?;
    if !mkdir.success() {
        return Err(anyhow!("リモートに ~/.ccc/bin を作成できませんでした"));
    }

    let src = bundled_binary(platform)?;
    let scp_status = Command::new("scp")
        .args(["-q", "-p"])
        .arg(&src)
        .arg(format!("{host_alias}:.ccc/bin/ccc-claude-code-hook"))
        .status()
        .with_context(|| "scp 実行失敗".to_string())?;
    if !scp_status.success() {
        return Err(anyhow!("scp が失敗: {scp_status}"));
    }

    let chmod = Command::new("ssh")
        .args([host_alias, "chmod 755 ~/.ccc/bin/ccc-claude-code-hook"])
        .status()
        .with_context(|| "ssh chmod 失敗".to_string())?;
    if !chmod.success() {
        return Err(anyhow!("リモート chmod が失敗"));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_from_uname_known() {
        assert_eq!(
            Platform::from_uname("Darwin arm64").unwrap(),
            Platform::DarwinArm64
        );
        assert_eq!(
            Platform::from_uname("Linux x86_64").unwrap(),
            Platform::LinuxAmd64
        );
        assert_eq!(
            Platform::from_uname("Linux aarch64").unwrap(),
            Platform::LinuxArm64
        );
        assert_eq!(
            Platform::from_uname("Linux arm64").unwrap(),
            Platform::LinuxArm64
        );
        assert_eq!(
            Platform::from_uname("Linux amd64").unwrap(),
            Platform::LinuxAmd64
        );
    }

    #[test]
    fn platform_from_uname_with_extra_whitespace() {
        assert_eq!(
            Platform::from_uname("  Darwin   arm64  \n").unwrap(),
            Platform::DarwinArm64
        );
    }

    #[test]
    fn platform_from_uname_unknown() {
        assert!(Platform::from_uname("Windows x86_64").is_err());
        assert!(Platform::from_uname("FreeBSD amd64").is_err());
        assert!(Platform::from_uname("garbage").is_err());
    }

    #[test]
    fn platform_as_str_round_trip() {
        for p in [
            Platform::DarwinArm64,
            Platform::LinuxArm64,
            Platform::LinuxAmd64,
        ] {
            assert!(!p.as_str().is_empty());
        }
    }
}
