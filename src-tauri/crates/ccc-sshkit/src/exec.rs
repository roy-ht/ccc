//! タイムアウト付きの子プロセス実行。
//!
//! half-open（半死に）の ssh master 経由でリモート実行すると、応答が永遠に
//! 来ないため `Command::output()` が無期限ブロックする（v0.13 の障害モデル）。
//! この crate の ssh 子プロセス起動はすべて [`run_with_timeout`] を経由し、
//! 上限時間で kill して制御を返す。
//!
//! wait-timeout crate を使わない理由: Unix 実装がグローバル SIGCHLD ハンドラを
//! 差し替えるため、tokio（features=full、子プロセス回収に SIGCHLD を使用）と
//! 同居する GUI プロセスで回収が壊れるリスクがある。`try_wait()`（WNOHANG 相当）
//! の 25ms ポーリングで代替する。用途は毎分数回・上限 5〜15 秒なのでコストは
//! 無視できる。

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// タイムアウト付き実行の結果。
#[derive(Debug)]
pub struct ExecOutcome {
    /// 終了コード（シグナル死は None）
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// true = 上限超過で kill した（code はその際の wait 結果）
    pub timed_out: bool,
}

impl ExecOutcome {
    /// 「タイムアウトせず exit code 0 で終了」の短縮形
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

/// 子プロセスを spawn し、`timeout` 以内の終了を待つ。超過したら kill する。
///
/// stdout/stderr は piped にして reader スレッドで読み切る（子が pipe バッファを
/// 埋めて書き込み待ちになるデッドロックの回避）。stdin は null。
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<ExecOutcome> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout_reader = spawn_pipe_reader(child.stdout.take());
    let stderr_reader = spawn_pipe_reader(child.stderr.take());

    let timed_out = !wait_with_deadline(&mut child, timeout);
    if timed_out {
        let _ = child.kill();
    }
    // kill 後（または正常終了後）の回収。kill 直後でも wait は即座に返る。
    let status = child.wait()?;

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    Ok(ExecOutcome {
        code: status.code(),
        stdout,
        stderr,
        timed_out,
    })
}

/// `timeout` 以内に子が終了したら true。ポーリング間隔 25ms。
fn wait_with_deadline(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            // try_wait 自体のエラーは「待てない」なので kill 側に倒す
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// pipe を EOF まで読み切るスレッドを立てる。ハンドルが None なら空文字を返す。
fn spawn_pipe_reader<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_within_timeout() {
        let out = run_with_timeout(
            Command::new("sh").args(["-c", "echo hello; echo err >&2"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout.trim(), "hello");
        assert_eq!(out.stderr.trim(), "err");
    }

    #[test]
    fn kills_on_timeout() {
        let t = Instant::now();
        let out = run_with_timeout(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(out.timed_out);
        assert!(!out.success());
        assert!(t.elapsed() < Duration::from_secs(5), "kill が効いていない");
    }

    #[test]
    fn large_output_does_not_deadlock() {
        // pipe バッファ（64KB 前後）を大きく超える出力でも読み切れること
        let out = run_with_timeout(
            Command::new("sh").args(["-c", "head -c 1048576 /dev/zero | tr '\\0' 'x'"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout.len(), 1048576);
    }

    #[test]
    fn nonzero_exit_is_not_success() {
        let out = run_with_timeout(
            Command::new("sh").args(["-c", "exit 9"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!out.timed_out);
        assert_eq!(out.code, Some(9));
        assert!(!out.success());
    }
}
