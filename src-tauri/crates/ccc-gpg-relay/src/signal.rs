//! シグナルによる停止要求を self-pipe で通常のコードパスへ橋渡しする。
//!
//! シグナルハンドラ内でできることは async-signal-safe な操作に限られるため、
//! ハンドラは pipe に 1 バイト書くだけにして、実際の後始末（socket の unlink =
//! [`crate::socket::OwnedSocket`] の Drop）はメインスレッドで行う。
//!
//! これにより SIGTERM / SIGINT / SIGHUP では socket 残骸を残さずに終了できる。
//! SIGKILL だけは捕捉できないが、次回起動時の `bind_owned`（unlink してから bind）
//! が残骸を回収する。

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::io::FromRawFd;
use std::sync::atomic::{AtomicI32, Ordering};

/// ハンドラから書き込む pipe の write 端。未設定は -1。
static SHUTDOWN_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// pipe に 1 バイト書いて待受を起こす。async-signal-safe な操作しか行わない。
fn notify() {
    let fd = SHUTDOWN_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [b'x'];
        // SAFETY: write は async-signal-safe。失敗（EPIPE 等）は無視してよい
        unsafe {
            libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
}

extern "C" fn on_signal(_sig: libc::c_int) {
    notify();
}

/// シグナル以外の理由で停止要求を出す（socket の所有権を奪われた場合など）。
///
/// [`install`] がまだ呼ばれていなければ何もしない（テストからの利用を想定）。
pub fn trigger() {
    notify();
}

/// 停止要求の受け口。[`ShutdownSignal::wait`] でブロックする。
#[derive(Debug)]
pub struct ShutdownSignal {
    read: File,
}

impl ShutdownSignal {
    /// 停止要求が届くまでブロックする。
    ///
    /// 戻ったら呼び出し側は後始末をして終了する。EINTR は読み直す。
    pub fn wait(&mut self) -> io::Result<()> {
        let mut buf = [0u8; 1];
        loop {
            match self.read.read(&mut buf) {
                Ok(0) => return Ok(()), // write 端が閉じた = 実質の停止要求
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// SIGTERM / SIGINT / SIGHUP を捕捉する。
pub fn install() -> io::Result<ShutdownSignal> {
    install_for(&[libc::SIGTERM, libc::SIGINT, libc::SIGHUP])
}

/// 捕捉するシグナルを指定して設置する（テスト用に分けてある）。
pub fn install_for(signals: &[libc::c_int]) -> io::Result<ShutdownSignal> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: 長さ 2 の配列を渡す規約通りの呼び出し
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // macOS に pipe2 が無いため、生成後に CLOEXEC を立てる
    for fd in fds {
        // SAFETY: 直前に生成した有効な fd
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    SHUTDOWN_WRITE_FD.store(write_fd, Ordering::Relaxed);

    for &sig in signals {
        // SAFETY: sigaction を規約通りに初期化して設置する
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }

    // SAFETY: pipe が返した read 端の所有権を File に移す
    Ok(ShutdownSignal {
        read: unsafe { File::from_raw_fd(read_fd) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テストランナーが使わないシグナルで検証する（SIGTERM を差し替えない）。
    #[test]
    fn signal_wakes_the_waiter() {
        let mut shutdown = install_for(&[libc::SIGUSR2]).unwrap();
        let handle = std::thread::spawn(move || {
            shutdown.wait().unwrap();
            "woke"
        });
        // ハンドラ設置と wait 開始を待ってから送る
        std::thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: 自プロセスへのシグナル送出
        unsafe {
            libc::raise(libc::SIGUSR2);
        }
        assert_eq!(handle.join().unwrap(), "woke");
    }
}
