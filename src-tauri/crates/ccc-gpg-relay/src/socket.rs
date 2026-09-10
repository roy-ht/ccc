//! UNIX socket の所有（bind とライフサイクル）。
//!
//! v0.14 の中心となる不変条件:
//! **socket ファイルを所有するのは sshd ではなく relay デーモンである。**
//! ssh 接続の生死と socket ファイルの寿命を完全に分離することで、
//! 「接続は健全なのに socket が消えている/古い」（specs/v0.14 §1 欠陥 3）が
//! 構造的に起きなくなる。

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

/// bind した socket。Drop でパスを unlink する。
#[derive(Debug)]
pub struct OwnedSocket {
    listener: UnixListener,
    path: PathBuf,
    /// bind 直後の inode。所有権が奪われていないかの判定に使う
    inode: u64,
}

impl OwnedSocket {
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// bind したときの inode。
    pub fn inode(&self) -> u64 {
        self.inode
    }

    /// accept ループを回す側が read タイムアウト等を設定できるよう複製を渡す。
    pub fn try_clone_listener(&self) -> io::Result<UnixListener> {
        self.listener.try_clone()
    }

    /// このパスに居るのが今も自分の socket か。
    ///
    /// `false` になるのは 2 通り: パスから消された、あるいは別の誰か
    /// （素の `ssh` の `RemoteForward` 等）に置き換えられた。どちらも
    /// gpg クライアントから見れば「relay に届かない」状態を意味する。
    pub fn is_still_mine(&self) -> bool {
        std::fs::metadata(&self.path)
            .map(|m| m.ino() == self.inode)
            .unwrap_or(false)
    }
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        // **奪われた後は消さない**。無条件に unlink すると、specs/v0.14 §1 欠陥 3 で
        // sshd がやっているのと同じ「他人の socket をパスで消す」事故を自分で起こす
        if self.is_still_mine() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `path` を **unlink してから** bind する。
///
/// **必ず `lock::try_acquire` でロックを取得した後に呼ぶこと。** unlink は
/// 「他に所有者が居ない」ことを前提とした操作で、ロックがその前提を保証する。
/// SIGKILL で死んだ前世代の残骸はここで回収される。
///
/// パーミッションは 0600（gpg-agent の socket と同じ）に設定する。
pub fn bind_owned(path: &Path) -> io::Result<OwnedSocket> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 前世代の残骸。存在しなければ no-op
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let listener = UnixListener::bind(path)?;
    let meta = std::fs::metadata(path)?;
    let mut perms = meta.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;

    Ok(OwnedSocket {
        listener,
        path: path.to_path_buf(),
        inode: meta.ino(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccc-relay-sock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bind_creates_socket_with_owner_only_permissions() {
        let dir = temp_dir("perm");
        let path = dir.join("S.test");
        let sock = bind_owned(&path).unwrap();
        assert!(path.exists());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_unlinks_the_socket() {
        let dir = temp_dir("drop");
        let path = dir.join("S.test");
        {
            let _sock = bind_owned(&path).unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "Drop で unlink される");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_replaces_stale_socket_file() {
        let dir = temp_dir("stale");
        let path = dir.join("S.test");
        // 前世代が SIGKILL された状況を模す（socket ファイルだけが残る）
        let stale = UnixListener::bind(&path).unwrap();
        std::mem::forget(stale); // Drop で消させない
        assert!(path.exists());

        let sock = bind_owned(&path).unwrap();
        // 新しい listener で accept できること = 残骸ではなく自分が所有している
        let client_path = path.clone();
        let handle = std::thread::spawn(move || {
            let mut c = UnixStream::connect(&client_path).unwrap();
            c.write_all(b"ping").unwrap();
        });
        let (mut conn, _) = sock.listener().accept().unwrap();
        let mut buf = [0u8; 4];
        conn.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_creates_parent_directory() {
        let dir = temp_dir("mkdir");
        let path = dir.join("nested/run/S.test");
        let _sock = bind_owned(&path).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
