//! dev/release ビルドの同時起動防止（単一インスタンスロック）。
//!
//! dev 版（`CCC_DEV` 設定時）と配布版はデータディレクトリこそ分かれている
//! （`~/.ccc/dev/` / `~/.ccc/`）が、Claude Code から呼ばれる hook ラッパー
//! `~/.ccc/bin/ccc-hook.sh` と `agent_settings/` は共有している。両者が同時に
//! 起動すると、後に起動した側がラッパーを自分の endpoint/token で上書きし、
//! 先に起動していた側へ hook が届かなくなる（SessionStart 喪失 → セッション
//! 履歴・live 表示の欠落）。これを根本防止するため、共有ルート
//! `~/.ccc/ccc.lock` の排他 flock をプロセス生存中保持し、取れなければ
//! 起動を拒否する。
//!
//! flock はプロセス終了（クラッシュ含む）で OS が自動解放するため、stale lock
//! の後始末は不要。ロックファイル本文は診断用の保持者情報（pid / モード）で、
//! 排他制御そのものには使わない。

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

/// 取得済みロックのガード。プロセス生存中 drop しないこと
/// （drop すると fd が閉じ、flock が解放される）。
pub struct InstanceLock {
    _file: std::fs::File,
}

/// ロック取得の結果。
pub enum LockOutcome {
    /// 取得成功。ガードを保持し続けること。
    Acquired(InstanceLock),
    /// 別の ccc プロセスが保持中。文字列は診断用の保持者情報（ロックファイル本文）。
    Held(String),
}

/// `~/.ccc/ccc.lock` の排他 flock を試みる（非ブロッキング）。
pub fn try_acquire() -> anyhow::Result<LockOutcome> {
    let root = ccc_sshkit::paths::ccc_root()?;
    std::fs::create_dir_all(&root)?;
    let path = root.join("ccc.lock");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let mut holder = String::new();
        let _ = file.read_to_string(&mut holder);
        return Ok(LockOutcome::Held(holder.trim().to_string()));
    }

    // 保持者情報を書き込む（診断用）。書き込み失敗してもロック自体は有効。
    let mode = if ccc_sshkit::paths::is_dev_mode() {
        "dev"
    } else {
        "release"
    };
    let _ = file.set_len(0);
    let _ = write!(file, "pid={} mode={mode}", std::process::id());
    let _ = file.flush();
    Ok(LockOutcome::Acquired(InstanceLock { _file: file }))
}

/// 二重起動をユーザーに通知して終了する（macOS はネイティブダイアログ併用）。
pub fn abort_duplicate(holder: &str) -> ! {
    let holder_desc = if holder.is_empty() {
        "別の ccc".to_string()
    } else {
        format!("別の ccc（{holder}）")
    };
    let msg = format!(
        "{holder_desc} が起動中のため終了します。\n\
         dev 版と配布版は hook 連携（~/.ccc/bin/ccc-hook.sh）を共有しており、\n\
         同時起動するとセッション集約が壊れるため二重起動を禁止しています。"
    );
    eprintln!("[ccc] {}", msg.replace('\n', " "));
    #[cfg(target_os = "macos")]
    {
        // AppleScript 文字列向けに \ " 改行をエスケープして埋め込む。
        let as_msg = msg
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        let script = format!(
            "display dialog \"{as_msg}\" with title \"ccc\" buttons {{\"OK\"}} \
             default button 1 with icon stop"
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .status();
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_flock_on_same_path_fails() {
        // try_acquire は HOME 固定パスを使うため、ここでは flock の挙動だけを
        // 一時ファイルで検証する（実行中の ccc と干渉しない）。
        let dir = std::env::temp_dir().join(format!("ccc-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ccc.lock");

        let f1 = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(f1.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "1 本目は取得できる"
        );

        let f2 = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        assert_ne!(
            unsafe { libc::flock(f2.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "2 本目は弾かれる"
        );

        // f1 を閉じると解放され、再取得できる。
        drop(f1);
        assert_eq!(
            unsafe { libc::flock(f2.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "解放後は取得できる"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
