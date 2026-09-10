//! 既定パスの解決。
//!
//! `ccc-sshkit::paths` と同じ流儀（`~/.ccc` + `CCC_DEV` で `dev` サブディレクトリ）
//! を採るが、**依存はしない**。このバイナリはリモートへ配信するため、ssh 前提の
//! ロジックを持ち込まず単体で完結させる。
//!
//! socket パスは `sun_path` の長さ制限（macOS 104 / Linux 108 バイト）を受ける。
//! `~/.ccc/run/gpg-relay.sock` は十分短いが、`--ctl-socket` で長いパスを渡すと
//! bind が失敗する点に注意。

use std::path::PathBuf;

use anyhow::{anyhow, Result};

fn home() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME が設定されていません"))?;
    Ok(PathBuf::from(home))
}

pub fn is_dev_mode() -> bool {
    matches!(std::env::var("CCC_DEV").as_deref(), Ok(v) if !v.is_empty() && v != "0")
}

/// 可変データの保存先（`~/.ccc/` または dev 時 `~/.ccc/dev/`）。
pub fn data_root() -> Result<PathBuf> {
    let root = home()?.join(".ccc");
    Ok(if is_dev_mode() {
        root.join("dev")
    } else {
        root
    })
}

/// 常駐プロセスの実行時ファイル置き場（`~/.ccc[/dev]/run/`）。
pub fn run_dir() -> Result<PathBuf> {
    Ok(data_root()?.join("run"))
}

/// uplink が繋ぐ制御 socket。
pub fn default_ctl_socket() -> Result<PathBuf> {
    Ok(run_dir()?.join("gpg-relay.sock"))
}

/// デーモンの単一性を保証する flock のパス。
pub fn default_lock() -> Result<PathBuf> {
    Ok(run_dir()?.join("gpg-relay.lock"))
}

/// デーモンのログ（stdio は `/dev/null` に落とすため、ここだけが手がかりになる）。
pub fn default_log() -> Result<PathBuf> {
    Ok(run_dir()?.join("gpg-relay.log"))
}

/// gpg クライアントが繋ぐ socket。`GNUPGHOME` を尊重する。
pub fn default_gpg_socket() -> Result<PathBuf> {
    Ok(gnupg_home()?.join("S.gpg-agent"))
}

pub fn gnupg_home() -> Result<PathBuf> {
    match std::env::var("GNUPGHOME") {
        Ok(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => Ok(home()?.join(".gnupg")),
    }
}

/// リモートの `gpg.conf` に `no-autostart` が入っているか。
///
/// relay が socket を常時保持するため autostart は不要で、むしろ有害
/// （relay の socket を奪う個体が湧く）。**ccc は書き換えない** —
/// ユーザーの gpg 設定を勝手に触らず、検出して警告するに留める。
pub fn has_no_autostart() -> bool {
    let Ok(path) = gnupg_home().map(|d| d.join("gpg.conf")) else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|line| {
        let line = line.trim();
        line == "no-autostart" || line.starts_with("no-autostart ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_paths_stay_within_sun_path_limit() {
        // 実行環境の HOME 次第だが、既定パスが極端に長くないことを確認する
        // （sun_path は macOS で 104 バイト）
        if let Ok(path) = default_ctl_socket() {
            assert!(
                path.as_os_str().len() < 104,
                "ctl socket のパスが長すぎます: {}",
                path.display()
            );
        }
    }

    #[test]
    fn run_dir_is_under_data_root() {
        if let (Ok(run), Ok(root)) = (run_dir(), data_root()) {
            assert!(run.starts_with(&root));
        }
    }
}
