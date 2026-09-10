//! `ccc-gpg-relay`: gpg agent forward の relay（リモート常駐バイナリ）。
//!
//! | フラグ | 動作 |
//! |---|---|
//! | `--daemon` | 前景で常駐する。既に別のデーモンが居れば何もせず正常終了 |
//! | `--ensure-daemon` | 冪等起動。居なければ自分を切り離して起動する（uplink が接続のたびに打つ） |
//! | `--uplink` | stdio ↔ 制御 socket のブリッジ（`ssh <host> ccc-gpg-relay --uplink`） |
//! | `--status` | 状態を JSON で出力 |
//! | `--stop` | 常駐デーモンを停止する |
//! | `--platform` | 配信されたバイナリのプラットフォーム識別子 |

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;

use ccc_gpg_relay::daemon::{self, DaemonConfig};
use ccc_gpg_relay::logging::make_log;
use ccc_gpg_relay::{bridge, lock, paths, signal, VERSION};

#[derive(Parser, Debug)]
#[command(
    name = "ccc-gpg-relay",
    version,
    about = "gpg agent forward の relay デーモン（ccc v0.14）"
)]
struct Cli {
    /// 前景で常駐する
    #[arg(long, group = "mode")]
    daemon: bool,

    /// 常駐していなければ切り離して起動する（冪等）
    #[arg(long, group = "mode")]
    ensure_daemon: bool,

    /// stdio と制御 socket を繋ぐ
    #[arg(long, group = "mode")]
    uplink: bool,

    /// 状態を JSON で出力する
    #[arg(long, group = "mode")]
    status: bool,

    /// 常駐デーモンを停止する
    #[arg(long, group = "mode")]
    stop: bool,

    /// プラットフォーム識別子を出力する（配信バイナリの突き合わせ用）
    #[arg(long, group = "mode")]
    platform: bool,

    /// gpg クライアントが繋ぐ socket（既定: $GNUPGHOME/S.gpg-agent）
    #[arg(long)]
    gpg_socket: Option<PathBuf>,

    /// uplink が繋ぐ制御 socket（既定: ~/.ccc/run/gpg-relay.sock）
    #[arg(long)]
    ctl_socket: Option<PathBuf>,

    /// flock のパス（既定: ~/.ccc/run/gpg-relay.lock）
    #[arg(long)]
    lock_path: Option<PathBuf>,

    /// uplink 未接続時に接続を待たせる秒数
    #[arg(long, default_value_t = 5)]
    grace: u64,

    /// 待たせられる接続数の上限
    #[arg(long, default_value_t = 16)]
    pending_limit: usize,

    /// ログの出力先（既定: ~/.ccc/run/gpg-relay.log）
    #[arg(long)]
    log_file: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.platform {
        println!("{}", platform_id());
        return Ok(());
    }
    if cli.uplink {
        let ctl = resolve_ctl(&cli)?;
        return bridge::run(&ctl).context("uplink ブリッジが異常終了しました");
    }
    if cli.status {
        return print_status(&cli);
    }
    if cli.stop {
        return stop_daemon(&cli);
    }
    if cli.ensure_daemon {
        return ensure_daemon(&cli);
    }
    if cli.daemon {
        return run_daemon(&cli);
    }

    Err(anyhow!(
        "動作モードを指定してください（--daemon / --ensure-daemon / --uplink / --status / --stop）"
    ))
}

// ─── 各モード ────────────────────────────────────────────────────────────────

fn run_daemon(cli: &Cli) -> Result<()> {
    let cfg = build_config(cli)?;
    let log = make_log(resolve_log_path(cli)?, true);

    // シグナルハンドラは socket を bind する前に設置する
    // （起動直後に SIGTERM が来ても残骸を残さない）
    let mut shutdown = signal::install().context("シグナルハンドラを設置できません")?;

    let Some(handle) = daemon::start(cfg, Arc::clone(&log))? else {
        log("[relay] 既に別のデーモンが動作しているため終了します");
        return Ok(());
    };

    shutdown.wait().context("停止要求の待受に失敗しました")?;
    let aborted = handle.aborted();
    if aborted {
        // socket を奪われた場合（§11.2 の層 3）。次の uplink 接続時に
        // `--ensure-daemon` が新しいデーモンを起こし、所有権を取り戻す
        log("[relay] socket の所有権を失ったため終了します");
    } else {
        log("[relay] 停止要求を受け取りました");
    }
    let (accepted, dropped) = handle.stats();
    handle.stop();
    log(&format!(
        "[relay] 停止しました（受理 {accepted} 件 / 破棄 {dropped} 件）"
    ));
    Ok(())
}

/// 冪等起動。既に居れば何もしない。
///
/// 起動する場合は自分自身を `--daemon` で spawn し、`setsid` で親から切り離す。
/// **stdio を `/dev/null` に落とすのは必須**: 継承したままだと
/// `ssh <host> ccc-gpg-relay --ensure-daemon` が fd の閉鎖を待って返らなくなる。
fn ensure_daemon(cli: &Cli) -> Result<()> {
    let lock_path = resolve_lock(cli)?;
    if lock::is_held(&lock_path) {
        println!("already running (pid {})", pid_label(&lock_path));
        return Ok(());
    }

    let exe = std::env::current_exe().context("自身の実行パスを取得できません")?;
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon");
    if let Some(p) = &cli.gpg_socket {
        cmd.arg("--gpg-socket").arg(p);
    }
    if let Some(p) = &cli.ctl_socket {
        cmd.arg("--ctl-socket").arg(p);
    }
    if let Some(p) = &cli.lock_path {
        cmd.arg("--lock-path").arg(p);
    }
    if let Some(p) = &cli.log_file {
        cmd.arg("--log-file").arg(p);
    }
    cmd.arg("--grace").arg(cli.grace.to_string());
    cmd.arg("--pending-limit")
        .arg(cli.pending_limit.to_string());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: fork と exec の間で呼ぶのは setsid のみ（async-signal-safe）
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            // 既にセッションリーダーなら EPERM。切り離せていれば十分なので無視する
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("デーモンを起動できません")?;

    // 子がロックを取るまで待つ（取れなければ起動失敗）
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if lock::is_held(&lock_path) {
            println!("started (pid {})", pid_label(&lock_path));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(anyhow!(
        "デーモンが 2 秒以内に起動しませんでした（ログを確認してください: {}）",
        resolve_log_path(cli)?.display()
    ))
}

fn print_status(cli: &Cli) -> Result<()> {
    let lock_path = resolve_lock(cli)?;
    let gpg_socket = resolve_gpg(cli)?;
    let ctl_socket = resolve_ctl(cli)?;
    let running = lock::is_held(&lock_path);
    let status = serde_json::json!({
        "version": VERSION,
        "platform": platform_id(),
        "running": running,
        // 停止済みなら pid は意味を持たない（前世代の残骸を見せない）
        "pid": running.then(|| lock::holder_pid(&lock_path)).flatten(),
        "gpg_socket": gpg_socket.to_string_lossy(),
        "gpg_socket_exists": gpg_socket.exists(),
        "ctl_socket": ctl_socket.to_string_lossy(),
        "ctl_socket_exists": ctl_socket.exists(),
        // relay が socket を常時保持するため autostart は不要（むしろ有害）。
        // ccc は書き換えず、検出して警告するに留める
        "no_autostart": paths::has_no_autostart(),
        "log_file": resolve_log_path(cli)?.to_string_lossy(),
    });
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

fn stop_daemon(cli: &Cli) -> Result<()> {
    let lock_path = resolve_lock(cli)?;
    if !lock::is_held(&lock_path) {
        println!("not running");
        return Ok(());
    }
    let Some(pid) = lock::holder_pid(&lock_path) else {
        return Err(anyhow!(
            "デーモンは動作していますが pid を特定できません（{}）",
            lock_path.display()
        ));
    };
    if !pid_is_relay(pid) {
        return Err(anyhow!(
            "pid {pid} は ccc-gpg-relay ではありません（pid が再利用されています）"
        ));
    }
    // SAFETY: 自分たちが記録した pid への TERM
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    // ロックが空く = プロセスが消えた
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !lock::is_held(&lock_path) {
            println!("stopped (pid {pid})");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(anyhow!("pid {pid} が 5 秒以内に終了しませんでした"))
}

/// ロックファイルに記録された pid の表示用ラベル。
fn pid_label(lock_path: &std::path::Path) -> String {
    match lock::holder_pid(lock_path) {
        Some(pid) => pid.to_string(),
        None => "unknown".into(),
    }
}

/// pid が本当に relay かを確認する（pid 再利用による誤射を防ぐ）。
/// macOS の `ps -o comm=` はフルパスを返すため `ends_with` で見る。
fn pid_is_relay(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .ends_with("ccc-gpg-relay")
        })
        .unwrap_or(false)
}

// ─── 設定の解決 ──────────────────────────────────────────────────────────────

fn build_config(cli: &Cli) -> Result<DaemonConfig> {
    let mut cfg = DaemonConfig::new(resolve_gpg(cli)?, resolve_ctl(cli)?, resolve_lock(cli)?);
    cfg.grace = Duration::from_secs(cli.grace);
    cfg.pending_limit = cli.pending_limit;
    cfg.mux.grace_secs = cli.grace;
    Ok(cfg)
}

fn resolve_gpg(cli: &Cli) -> Result<PathBuf> {
    match &cli.gpg_socket {
        Some(p) => Ok(p.clone()),
        None => paths::default_gpg_socket(),
    }
}

fn resolve_ctl(cli: &Cli) -> Result<PathBuf> {
    match &cli.ctl_socket {
        Some(p) => Ok(p.clone()),
        None => paths::default_ctl_socket(),
    }
}

fn resolve_lock(cli: &Cli) -> Result<PathBuf> {
    match &cli.lock_path {
        Some(p) => Ok(p.clone()),
        None => paths::default_lock(),
    }
}

fn resolve_log_path(cli: &Cli) -> Result<PathBuf> {
    match &cli.log_file {
        Some(p) => Ok(p.clone()),
        None => paths::default_log(),
    }
}

/// 配信バイナリのプラットフォーム識別子（ccc 本体の `Platform::as_str` と揃える）。
fn platform_id() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-arm64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-amd64"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_id_is_known_on_supported_hosts() {
        assert_ne!(platform_id(), "unknown");
    }
}
