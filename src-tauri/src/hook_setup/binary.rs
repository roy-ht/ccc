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

/// 配信する 1 バイナリの仕様。
///
/// v0.14 で gpg relay が加わり、配信対象が 2 つになった。名前と期待バージョン
/// だけが違い、探索・配信の手順は完全に共通なので spec で切り替える。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinarySpec {
    /// バイナリ名（同梱ディレクトリ名・配信先ファイル名を兼ねる）
    pub name: &'static str,
    /// 期待するバージョン（`--version` 出力の最後のトークンと突き合わせる）
    pub version: &'static str,
}

/// Claude Code の hook ブリッジ。`ccc-claude-code-hook` crate の version と同期。
pub const HOOK: BinarySpec = BinarySpec {
    name: "ccc-claude-code-hook",
    version: "0.3.0",
};

/// gpg agent forward の relay（specs/v0.14）。
pub const GPG_RELAY: BinarySpec = BinarySpec {
    name: "ccc-gpg-relay",
    version: ccc_gpg_relay::VERSION,
};

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
fn bundled_binary_search_paths(spec: BinarySpec, platform: Platform) -> Result<Vec<PathBuf>> {
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
                .join(spec.name)
                .join(platform.as_str())
                .join(spec.name),
        );
    }

    // 2) exe_dir 直下の binaries/ (Linux/Windows 配布や、手動配置の応急対応用)
    candidates.push(
        exe_dir
            .join("binaries")
            .join(spec.name)
            .join(platform.as_str())
            .join(spec.name),
    );

    // 3) 開発時の prepare-hook 配置先 (`<repo>/src-tauri/binaries/ccc-claude-code-hook/<platform>/`)
    //    exe_dir = `<repo>/src-tauri/target/<profile>` → 親の親 = `<repo>/src-tauri/`
    if let Some(src_tauri_dir) = exe_dir.parent().and_then(|p| p.parent()) {
        candidates.push(
            src_tauri_dir
                .join("binaries")
                .join(spec.name)
                .join(platform.as_str())
                .join(spec.name),
        );
    }

    // 4) ホスト用は cargo build の出力場所もフォールバックとして見る
    if platform == Platform::host()? {
        candidates.push(exe_dir.join(spec.name));
        if let Some(target_dir) = exe_dir.parent() {
            candidates.push(target_dir.join("debug").join(spec.name));
            candidates.push(target_dir.join("release").join(spec.name));
        }
    }

    Ok(candidates)
}

/// 同梱バイナリのうち、最初に見つかった実在パスを返す。
pub fn bundled_binary(spec: BinarySpec, platform: Platform) -> Result<PathBuf> {
    for cand in bundled_binary_search_paths(spec, platform)? {
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(anyhow!(
        "{} 用の同梱バイナリが見つかりません（src-tauri/binaries/{}/{}/ に配置してください）",
        platform.as_str(),
        spec.name,
        platform.as_str()
    ))
}

/// 既存の `~/.ccc/bin/<name>` の `--version` 出力を取得。
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
    // `clap` のデフォルト形式は "<name> X.Y.Z"
    s.split_whitespace().last().map(String::from)
}

/// ローカル `~/.ccc/bin/<name>` を最新バイナリで上書きする。
/// バージョンが既に最新なら何もせず Ok(false) を返す。
pub fn install_local(spec: BinarySpec) -> Result<bool> {
    let target_dir = hook_bin_dir()?;
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("ディレクトリ作成失敗: {}", target_dir.display()))?;
    let target_path = target_dir.join(spec.name);

    if local_installed_version(&target_path).as_deref() == Some(spec.version) {
        return Ok(false);
    }

    let host = Platform::host()?;
    let src = bundled_binary(spec, host)?;
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

/// リモートへ配る用に、**全プラットフォーム分**を
/// `~/.ccc/bin/remote/<platform>/<name>` へ展開する。戻り値は配置できた数。
///
/// 同梱リソース（`.app/Contents/Resources/binaries/...`）を持っているのは GUI 本体
/// だけなので、GUI 起動時にここへ置いておく。こうすると **`ccc-ssh` 単独でも**
/// リモートへ配信でき、GUI の起動有無に依存しなくなる（specs/v0.14 §4）。
///
/// 見つからないプラットフォームは黙って飛ばす（開発時は host 用しか無いことが多い）。
pub fn stage_remote_payload(spec: BinarySpec) -> Result<usize> {
    let base = hook_bin_dir()?.join("remote");
    let mut staged = 0usize;
    for platform in [
        Platform::DarwinArm64,
        Platform::LinuxArm64,
        Platform::LinuxAmd64,
    ] {
        let Ok(src) = bundled_binary(spec, platform) else {
            continue;
        };
        let dir = base.join(platform.as_str());
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("ディレクトリ作成失敗: {}", dir.display()))?;
        let dest = dir.join(spec.name);
        // 同一内容なら触らない（毎起動のコピーを避ける）
        if same_file_contents(&src, &dest) {
            staged += 1;
            continue;
        }
        std::fs::copy(&src, &dest)
            .with_context(|| format!("コピー失敗: {} → {}", src.display(), dest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dest, perms)?;
        }
        staged += 1;
    }
    Ok(staged)
}

/// サイズと更新時刻で同一とみなす（内容比較まではしない）。
fn same_file_contents(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    ma.len() == mb.len()
        && match (ma.modified(), mb.modified()) {
            (Ok(ta), Ok(tb)) => ta == tb,
            _ => false,
        }
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
pub fn install_remote(spec: BinarySpec, host_alias: &str) -> Result<bool> {
    let platform = detect_remote_platform(host_alias)?;
    let remote_path = format!("~/.ccc/bin/{}", spec.name);

    // 既にインストール済みでバージョンが一致するか確認
    let check = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            host_alias,
            &format!("test -x {remote_path} && {remote_path} --version || true"),
        ])
        .output()
        .with_context(|| format!("ssh version check 失敗 (host={host_alias})"))?;
    let remote_version = String::from_utf8_lossy(&check.stdout)
        .split_whitespace()
        .last()
        .map(String::from);
    if remote_version.as_deref() == Some(spec.version) {
        return Ok(false);
    }

    // ディレクトリを作成
    let mkdir = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host_alias, "mkdir -p ~/.ccc/bin"])
        .status()
        .with_context(|| format!("ssh mkdir 失敗 (host={host_alias})"))?;
    if !mkdir.success() {
        return Err(anyhow!("リモートに ~/.ccc/bin を作成できませんでした"));
    }

    // 実行中のバイナリを上書きすると ETXTBSY になり得るため、一時名で置いてから
    // rename する（rename は同一ディレクトリ内なので atomic）。既に動いている
    // 旧世代プロセスは自分の inode を持ったまま動き続け、次の起動から新版になる
    let src = bundled_binary(spec, platform)?;
    let tmp_remote = format!(".ccc/bin/{}.new", spec.name);
    let scp_status = Command::new("scp")
        .args(["-q", "-p"])
        .arg(&src)
        .arg(format!("{host_alias}:{tmp_remote}"))
        .status()
        .with_context(|| "scp 実行失敗".to_string())?;
    if !scp_status.success() {
        return Err(anyhow!("scp が失敗: {scp_status}"));
    }

    let finalize = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            host_alias,
            &format!("chmod 755 ~/{tmp_remote} && mv -f ~/{tmp_remote} {remote_path}"),
        ])
        .status()
        .with_context(|| "ssh chmod/mv 失敗".to_string())?;
    if !finalize.success() {
        return Err(anyhow!("リモートでの配置に失敗しました"));
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
