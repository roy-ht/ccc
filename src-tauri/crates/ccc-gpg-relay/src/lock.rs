//! advisory `flock` による常駐プロセスの単一化。
//!
//! **`create_new` + mtime による stale 判定（`ccc-sshkit::liveness` の
//! `acquire_rebuild_lock`）は使わない**: プロセスが SIGKILL された場合に
//! ロックファイルが残り、stale と見なされるまで（120 秒）誰も起動できない。
//! `flock` はプロセス消滅時に OS が自動解放するため、stale 判定そのものが不要になる。
//!
//! ロックファイルは**削除しない**。削除すると「別プロセスが同じパスを開いて
//! 別 inode をロックする」レースが生まれ、単一性が壊れる。中身の pid は
//! 診断用で、判定には使わない（判定はあくまで flock が取れるかどうか）。

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// 保持している間だけロックが有効なガード。Drop（= fd の close）で解放される。
#[derive(Debug)]
pub struct ProcessLock {
    _file: File,
    path: PathBuf,
}

impl ProcessLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// ロックの取得を試みる。
///
/// - `Ok(Some(lock))`: 取得できた = **常駐プロセスは居なかった**
/// - `Ok(None)`: 既に他プロセスが保持している = 常駐プロセスが居る
pub fn try_acquire(path: &Path) -> io::Result<Option<ProcessLock>> {
    try_acquire_inner(path, true)
}

/// `record_pid = false` のときは pid を書かない。
///
/// [`is_held`] は「取得できるか」を確かめるためだけに一瞬ロックを取るので、
/// ここで pid を書くと**不在の probe が自分の pid を残してしまい**、
/// 「running: false なのに pid がある」という矛盾した状態表示になる。
fn try_acquire_inner(path: &Path, record_pid: bool) -> io::Result<Option<ProcessLock>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    // SAFETY: 有効な fd に対する flock。LOCK_NB なので待たない。
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Ok(None),
            _ => Err(err),
        };
    }

    // 診断用に pid を残す（判定には使わない）
    if record_pid {
        let _ = file.set_len(0);
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
    }

    Ok(Some(ProcessLock {
        _file: file,
        path: path.to_path_buf(),
    }))
}

/// 常駐プロセスが居るかどうかだけを調べる（取得できたら即解放する）。
///
/// pid の生存確認や mtime より堅牢: プロセスが SIGKILL されても OS が
/// 解放するため、「居ないのに居ると誤判定する」ことがない。
pub fn is_held(path: &Path) -> bool {
    match try_acquire_inner(path, false) {
        Ok(Some(_lock)) => false, // 取れた = 不在（ここで drop して解放）
        Ok(None) => true,
        // 開けない（権限・パス不正）場合は「居ない」と答えて起動側に判断させる
        Err(_) => false,
    }
}

/// ロックファイルに記録された pid（診断表示用）。
pub fn holder_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccc-relay-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_acquire_in_same_process_is_blocked() {
        let dir = temp_dir("dup");
        let path = dir.join("a.lock");
        let first = try_acquire(&path).unwrap();
        assert!(first.is_some(), "初回は取得できる");
        // 同一プロセスでも別 fd なら flock は競合する
        assert!(
            try_acquire(&path).unwrap().is_none(),
            "保持中は取得できない"
        );
        drop(first);
        assert!(
            try_acquire(&path).unwrap().is_some(),
            "Drop で解放されて再取得できる"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_paths_are_independent() {
        let dir = temp_dir("indep");
        let a = try_acquire(&dir.join("a.lock")).unwrap();
        let b = try_acquire(&dir.join("b.lock")).unwrap();
        assert!(a.is_some() && b.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_held_reflects_state() {
        let dir = temp_dir("held");
        let path = dir.join("a.lock");
        assert!(!is_held(&path), "誰も持っていなければ false");
        let lock = try_acquire(&path).unwrap().unwrap();
        assert!(is_held(&path), "保持中は true");
        drop(lock);
        assert!(!is_held(&path), "解放後は false");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_held_does_not_leave_the_lock_taken() {
        let dir = temp_dir("noleak");
        let path = dir.join("a.lock");
        assert!(!is_held(&path));
        // is_held が内部で取ったロックを解放していれば、続けて取得できる
        assert!(try_acquire(&path).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn holder_pid_is_recorded() {
        let dir = temp_dir("pid");
        let path = dir.join("a.lock");
        let _lock = try_acquire(&path).unwrap().unwrap();
        assert_eq!(holder_pid(&path), Some(std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_held_does_not_record_a_pid() {
        let dir = temp_dir("pid-probe");
        let path = dir.join("a.lock");
        // 不在の probe が自分の pid を書き残すと「running: false なのに pid がある」
        // という矛盾した状態表示になる
        assert!(!is_held(&path));
        assert_eq!(holder_pid(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_held_preserves_the_holders_pid() {
        let dir = temp_dir("pid-keep");
        let path = dir.join("a.lock");
        let _lock = try_acquire(&path).unwrap().unwrap();
        assert!(is_held(&path));
        // probe は保持者の pid を消さない
        assert_eq!(holder_pid(&path), Some(std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_parent_directory() {
        let dir = temp_dir("mkdir");
        let path = dir.join("nested/deep/a.lock");
        assert!(try_acquire(&path).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
