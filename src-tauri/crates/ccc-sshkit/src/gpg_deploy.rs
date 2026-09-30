//! リモートへの `ccc-gpg-relay` 配信（specs/v0.14 §12 フェーズ 3）。
//!
//! 配信元のバイナリは 2 系統から探す（[`staged_binary`]）。`ccc-ssh` は app bundle
//! の中に居るので同梱リソースを自力で引け、**GUI を一度も起動していなくても
//! 配信できる**。GUI 起動時に `~/.ccc/bin/remote/<platform>/` へ展開される
//! コピー（`hook_setup::binary::stage_remote_payload`）も候補に含める。
//!
//! 配信のタイミングは「リモートで relay が起動できなかったとき」だけ。
//! 毎回バージョンを問い合わせると接続のたびに往復が増えるため、
//! 失敗を検知してから配る（`gpg_uplink::ensure_remote_daemon`）。

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::exec::run_with_timeout;
use crate::log::Log;
use crate::upload::upload_file;

/// 配信するバイナリ名。
pub const BIN_NAME: &str = "ccc-gpg-relay";

/// リモート側の配置先（`~` はリモートのシェルが展開する）。
pub const REMOTE_DIR: &str = ".ccc/bin";

const SSH_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// 配信対象のプラットフォーム識別子（本体の `Platform::as_str` と揃える）。
pub fn platform_from_uname(uname_sm: &str) -> Result<&'static str> {
    let parts: Vec<&str> = uname_sm.split_whitespace().collect();
    let (os, arch) = match parts.as_slice() {
        [os, arch, ..] => (*os, *arch),
        _ => return Err(anyhow!("uname -sm の形式が不正: {uname_sm:?}")),
    };
    match (os, arch) {
        ("Darwin", "arm64") => Ok("darwin-arm64"),
        ("Linux", "aarch64") | ("Linux", "arm64") => Ok("linux-arm64"),
        ("Linux", "x86_64") | ("Linux", "amd64") => Ok("linux-amd64"),
        _ => Err(anyhow!(
            "未対応のプラットフォームです: {os} {arch}（gpg relay を配信できません）"
        )),
    }
}

/// 配信用バイナリを探す。先頭から順に試し、最初に見つかった実在パスを返す。
///
/// 1. **app bundle の同梱リソース**: `ccc-ssh` は `ccc.app/Contents/MacOS/` に
///    居るので、`../Resources/binaries/` を自力で引ける。これがあるおかげで
///    **GUI を一度も起動していなくても配信できる**
/// 2. GUI が展開した `~/.ccc/bin/remote/<platform>/`（配布形態が変わっても
///    GUI さえ動いていれば拾える保険）
/// 3. 開発時の `<repo>/src-tauri/binaries/`（`just prepare-remote-bin` の出力）
fn staged_binary_candidates(platform: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let rel = |base: PathBuf| base.join(BIN_NAME).join(platform).join(BIN_NAME);

    for dir in exe_dirs() {
        // 1) macOS app bundle: MacOS/ccc-ssh → ../Resources/binaries/...
        if let Some(contents) = dir.parent() {
            candidates.push(rel(contents.join("Resources").join("binaries")));
        }
        // exe と同階層の binaries/（他 OS 向け配布・手動配置）
        candidates.push(rel(dir.join("binaries")));
        // 3) 開発時: target/<profile>/ccc-ssh → ../../binaries/...
        if let Some(src_tauri) = dir.parent().and_then(|p| p.parent()) {
            candidates.push(rel(src_tauri.join("binaries")));
        }
    }
    // 2) GUI が展開した置き場
    if let Ok(root) = crate::paths::ccc_root() {
        candidates.push(
            root.join("bin")
                .join("remote")
                .join(platform)
                .join(BIN_NAME),
        );
    }
    candidates.dedup();
    candidates
}

/// 探索の起点になる実行ファイルのディレクトリ（**symlink 解決後を優先**）。
///
/// `ccc-ssh` は通常 `~/.local/bin/ccc-ssh` → `ccc.app/Contents/MacOS/ccc-ssh` の
/// symlink 経由で起動される。`current_exe()` はこれを解決しないことがあり
/// （macOS で実測）、その場合 `~/.local/Resources/binaries/...` を探して
/// **app bundle の同梱リソースに到達できない**。`canonicalize` した実体を先に、
/// 元のパスを後に見る（symlink でない環境では同じ 1 件に落ちる）。
fn exe_dirs() -> Vec<PathBuf> {
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    if let Ok(real) = std::fs::canonicalize(&exe) {
        if let Some(dir) = real.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    if let Some(dir) = exe.parent() {
        let dir = dir.to_path_buf();
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// 配信用バイナリのパス（最初に見つかった実在パス）。
pub fn staged_binary(platform: &str) -> Result<PathBuf> {
    let candidates = staged_binary_candidates(platform);
    for cand in &candidates {
        if cand.is_file() {
            return Ok(cand.clone());
        }
    }
    Err(anyhow!(
        "{platform} 用の {BIN_NAME} が見つかりません（探索先: {}）",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// 素の ssh（master に相乗りせず、forward も要求しない）で 1 コマンド実行する。
fn ssh_plain(host_alias: &str, command: &str, timeout: Duration) -> Result<(i32, String, String)> {
    let outcome = run_with_timeout(
        Command::new("ssh").args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            host_alias,
            command,
        ]),
        timeout,
    )?;
    if outcome.timed_out {
        return Err(anyhow!("ssh がタイムアウトしました: {command}"));
    }
    Ok((outcome.code.unwrap_or(-1), outcome.stdout, outcome.stderr))
}

/// リモートの `uname -sm` からプラットフォームを判定する。
pub fn detect_platform(host_alias: &str) -> Result<&'static str> {
    let (code, stdout, stderr) = ssh_plain(host_alias, "uname -sm", SSH_TIMEOUT)?;
    if code != 0 {
        return Err(anyhow!(
            "リモートで uname -sm が失敗しました: {}",
            stderr.trim()
        ));
    }
    platform_from_uname(stdout.trim())
}

/// リモートへ `ccc-gpg-relay` を配信する。
///
/// 実行中のバイナリを直接上書きすると ETXTBSY になり得るため、一時名で置いてから
/// `mv` する（同一ディレクトリ内なので atomic）。動作中の旧世代プロセスは自分の
/// inode を持ったまま生き続け、次の起動から新版になる。
pub fn deliver(host_alias: &str, log: Log) -> Result<()> {
    let platform = detect_platform(host_alias)?;
    let src = staged_binary(platform)?;
    log(&format!(
        "[deploy] {host_alias}: {BIN_NAME} を配信します（{platform}）"
    ));

    let (code, _, stderr) =
        ssh_plain(host_alias, &format!("mkdir -p ~/{REMOTE_DIR}"), SSH_TIMEOUT)?;
    if code != 0 {
        return Err(anyhow!(
            "リモートに ~/{REMOTE_DIR} を作成できません: {}",
            stderr.trim()
        ));
    }

    let tmp = format!("{REMOTE_DIR}/{BIN_NAME}.new");
    let outcome = upload_file(host_alias, &src, &tmp, UPLOAD_TIMEOUT)?;
    if outcome.timed_out {
        return Err(anyhow!("rsync がタイムアウトしました"));
    }
    if !outcome.success() {
        return Err(anyhow!("rsync が失敗しました: {}", outcome.stderr.trim()));
    }

    let (code, _, stderr) = ssh_plain(
        host_alias,
        &format!("chmod 755 ~/{tmp} && mv -f ~/{tmp} ~/{REMOTE_DIR}/{BIN_NAME}"),
        SSH_TIMEOUT,
    )
    .context("配信後の配置に失敗しました")?;
    if code != 0 {
        return Err(anyhow!("リモートでの配置に失敗しました: {}", stderr.trim()));
    }
    log(&format!("[deploy] {host_alias}: {BIN_NAME} を配信しました"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_detection_matches_bundle_layout() {
        assert_eq!(platform_from_uname("Darwin arm64").unwrap(), "darwin-arm64");
        assert_eq!(platform_from_uname("Linux x86_64").unwrap(), "linux-amd64");
        assert_eq!(platform_from_uname("Linux amd64").unwrap(), "linux-amd64");
        assert_eq!(platform_from_uname("Linux aarch64").unwrap(), "linux-arm64");
        assert_eq!(platform_from_uname("Linux arm64").unwrap(), "linux-arm64");
    }

    #[test]
    fn platform_detection_tolerates_whitespace() {
        assert_eq!(
            platform_from_uname("  Darwin   arm64 \n").unwrap(),
            "darwin-arm64"
        );
    }

    #[test]
    fn unsupported_platforms_are_rejected() {
        assert!(platform_from_uname("Windows x86_64").is_err());
        assert!(platform_from_uname("FreeBSD amd64").is_err());
        assert!(platform_from_uname("garbage").is_err());
    }

    #[test]
    fn symlinked_exe_resolves_to_the_real_bundle_dir() {
        // `~/.local/bin/ccc-ssh` → `ccc.app/Contents/MacOS/ccc-ssh` の symlink 経由で
        // 起動された場合でも、実体側のディレクトリが探索起点に入ること。
        // これが無いと app bundle の同梱リソースに到達できない（2026-09-11 実機で発生）
        let dirs = exe_dirs();
        assert!(!dirs.is_empty(), "実行ファイルの位置は必ず取れる");
        let exe = std::env::current_exe().unwrap();
        let real = std::fs::canonicalize(&exe).unwrap();
        assert_eq!(
            dirs[0],
            real.parent().unwrap(),
            "symlink 解決後のディレクトリを最優先で見る"
        );
    }

    #[test]
    fn candidates_have_no_duplicates() {
        let candidates = staged_binary_candidates("linux-amd64");
        let mut sorted = candidates.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), candidates.len(), "重複した探索先を並べない");
    }

    #[test]
    fn bundle_resources_are_searched_before_the_staged_copy() {
        let candidates = staged_binary_candidates("linux-amd64");
        assert!(!candidates.is_empty());
        // 1 番目は自身の実行パス起点（GUI 未起動でも配信できる根拠）
        assert!(candidates[0].ends_with("ccc-gpg-relay/linux-amd64/ccc-gpg-relay"));
        // GUI が展開する置き場も候補に含む
        assert!(candidates
            .iter()
            .any(|p| p.ends_with("bin/remote/linux-amd64/ccc-gpg-relay")));
    }

    #[test]
    fn missing_binary_reports_where_it_looked() {
        // 探索先を列挙したエラーにする（原因の特定を早めるため）
        let err = staged_binary("linux-arm64").unwrap_err().to_string();
        assert!(err.contains("linux-arm64"));
    }
}
