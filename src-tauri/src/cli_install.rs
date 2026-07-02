//! 同梱 CLI（`ccc-sessions` / `ccc-ssh`）を `~/.local/bin` にインストールする
//! （VS Code の `code` 方式）。
//!
//! dmg はインストール時スクリプトを持てないため、ccc 本体と同梱したバイナリ
//! （externalBin）への symlink を、アプリから `~/.local/bin` に張る。Homebrew や
//! システムの bin（`/opt/homebrew/bin`・`/usr/local/bin`）は他者の管理領域なので使わず、
//! ユーザー所有の `~/.local/bin` に固定する。PATH に無ければ UI が追加方法を案内する。

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;

/// 同梱バイナリの解決関数
type BinResolver = fn() -> anyhow::Result<PathBuf>;

/// 同梱 CLI 一覧（リンク名, 同梱バイナリの解決関数）。
/// 追加するときは justfile の `prepare-cli` と tauri.conf.json の externalBin も更新する。
const TOOLS: &[(&str, BinResolver)] = &[
    ("ccc-sessions", crate::paths::ccc_sessions_bin),
    ("ccc-ssh", crate::paths::ccc_ssh_bin),
];

/// CLI ツールの導入状況（設定画面の表示用）。複数ツールの集約値を返す。
#[derive(Serialize)]
pub struct CliToolStatus {
    /// 同梱バイナリが全ツール分見つかったか。
    pub bundled_found: bool,
    /// symlink を張る予定/済みのパス（表示用。複数はカンマ区切り）。
    pub link_path: String,
    /// 全ツールの link が存在するか。
    pub installed: bool,
    /// 全ツールの既存 link が現在の同梱バイナリを指しているか（最新か）。
    pub up_to_date: bool,
    /// インストール先（`~/.local/bin`）が PATH に含まれるか（false なら追加が必要）。
    pub in_path: bool,
}

/// インストール結果（`install_cli_tool` の返り）。
#[derive(Serialize)]
pub struct InstallResult {
    pub link_path: String,
    /// インストール先が PATH に含まれるか（false なら PATH 追加を案内する）。
    pub in_path: bool,
}

/// インストール先ディレクトリ `~/.local/bin`。
fn install_dir() -> anyhow::Result<PathBuf> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME 環境変数が未設定です"))?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

/// PATH 環境変数のディレクトリ一覧。
fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

fn is_in_path(dir: &Path) -> bool {
    path_dirs().iter().any(|p| p == dir)
}

/// 1 ツール分の導入状況。
struct ToolStatus {
    bundled: Option<PathBuf>,
    link: Option<PathBuf>,
    installed: bool,
    up_to_date: bool,
}

fn tool_status(name: &str, bundled_bin: fn() -> anyhow::Result<PathBuf>) -> ToolStatus {
    let bundled = bundled_bin().ok().filter(|p| p.exists());
    let link = install_dir().ok().map(|d| d.join(name));
    let installed = link
        .as_ref()
        .map(|l| l.symlink_metadata().is_ok())
        .unwrap_or(false);
    // canonicalize 同士で比較し、symlink の指す実体が同梱バイナリと一致するか確認。
    let up_to_date = match (&bundled, &link) {
        (Some(src), Some(l)) if installed => match (l.canonicalize(), src.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        },
        _ => false,
    };
    ToolStatus {
        bundled,
        link,
        installed,
        up_to_date,
    }
}

/// CLI ツールの現在の導入状況を返す（設定画面の初期表示用）。
#[tauri::command]
pub fn cli_tool_status() -> CliToolStatus {
    let statuses: Vec<ToolStatus> = TOOLS
        .iter()
        .map(|(name, bin)| tool_status(name, *bin))
        .collect();
    let dir = install_dir().ok();
    let in_path = dir.as_ref().map(|d| is_in_path(d)).unwrap_or(false);
    let link_path = statuses
        .iter()
        .filter_map(|s| s.link.as_ref().map(|l| l.display().to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    CliToolStatus {
        bundled_found: statuses.iter().all(|s| s.bundled.is_some()),
        link_path: if link_path.is_empty() {
            "~/.local/bin/ccc-sessions, ccc-ssh".into()
        } else {
            link_path
        },
        installed: statuses.iter().all(|s| s.installed),
        up_to_date: statuses.iter().all(|s| s.up_to_date),
        in_path,
    }
}

/// 同梱 CLI 全ツールへの symlink を `~/.local/bin/` に張る。
/// `~/.local/bin` が無ければ作成し、既存リンクは張り替える。
#[tauri::command]
pub fn install_cli_tool() -> Result<InstallResult, String> {
    install().map_err(|e| e.to_string())
}

fn install() -> anyhow::Result<InstallResult> {
    let dir = install_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("{} を作成できません", dir.display()))?;

    let mut links: Vec<String> = Vec::new();
    for (name, bundled_bin) in TOOLS {
        let src = bundled_bin()?;
        if !src.exists() {
            anyhow::bail!(
                "同梱 {name} バイナリが見つかりません: {}（dev では just prepare-cli を実行）",
                src.display()
            );
        }
        let link = dir.join(name);
        // 既存（symlink でも実ファイルでも）を除去してから張り直す。
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link)
                .with_context(|| format!("既存の {} を削除できません", link.display()))?;
        }
        symlink_file(&src, &link)
            .with_context(|| format!("symlink を作成できません: {}", link.display()))?;
        links.push(link.display().to_string());
    }
    Ok(InstallResult {
        link_path: links.join(", "),
        in_path: is_in_path(&dir),
    })
}

#[cfg(unix)]
fn symlink_file(src: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, link)
}

#[cfg(not(unix))]
fn symlink_file(_src: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "この機能は Unix のみ対応です",
    ))
}
