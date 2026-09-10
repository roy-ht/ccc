//! ローカル uplink デーモン（specs/v0.14 §4, §6）。
//!
//! ホスト単位に 1 個の常駐プロセスを置き、**GUI も `ccc-ssh` も対等なクライアント**
//! として扱う。所有権は先に flock を取った方が持ち、それ以外は状態ファイルを
//! 読むだけになる。uplink は GUI の子プロセスではないので、GUI を終了しても
//! 生き残り、`ccc-ssh` 単独運用でも同じものが共有される。
//!
//! ```text
//! ssh -T -o ControlMaster=no -o ControlPath=none -o ClearAllForwardings=yes
//!     <alias> ~/.ccc/bin/ccc-gpg-relay --uplink
//! ```
//!
//! 接続方法（HostName / User / Port / IdentityFile / ProxyJump …）は
//! **ユーザーの `~/.ssh/config` をそのまま使う**。打ち消すのは「接続の共有」
//! （ControlMaster/ControlPath）と「forward の要求」（ClearAllForwardings）だけ。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use ccc_gpg_relay::mux::{self, MuxConfig, MuxHooks};
use ccc_gpg_relay::{lock, signal};
use serde::{Deserialize, Serialize};

use crate::forwards::sanitize_alias;
use crate::gpg_config::{self, ResolvedHost};
use crate::log::Log;

/// リモートに配信された relay バイナリのパス（hook バイナリと同じ流儀）。
const REMOTE_BIN: &str = "~/.ccc/bin/ccc-gpg-relay";

/// 起動する ssh。`CCC_SSH_BIN` で差し替えられる（テストと、ssh のパスを
/// 固定したい環境向け）。
fn ssh_bin() -> String {
    std::env::var("CCC_SSH_BIN").unwrap_or_else(|_| "ssh".to_string())
}

/// リモートで実行する relay バイナリ。`CCC_GPG_REMOTE_BIN` で差し替えられる。
fn remote_bin() -> String {
    std::env::var("CCC_GPG_REMOTE_BIN").unwrap_or_else(|_| REMOTE_BIN.to_string())
}

/// 再接続バックオフの下限・上限。
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// 認証に失敗した場合のバックオフ。無人ループからの認証連打を避ける
/// （MFA スパム・fail2ban 防止。v0.13 の方針を継承）。
const AUTH_FAIL_BACKOFF: Duration = Duration::from_secs(600);

/// リモート relay の冪等起動にかける上限。
const ENSURE_REMOTE_TIMEOUT: Duration = Duration::from_secs(30);

/// ローカル gpg-agent の生死を確認する間隔。
const LOCAL_AGENT_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// 停止要求を確認する刻み。
const TICK: Duration = Duration::from_millis(200);

// ─── 状態ファイル ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UplinkPhase {
    /// ssh を起動して HELLO 交換を待っている
    Connecting,
    /// 疎通している
    Connected,
    /// 切断され、バックオフ待ち
    Retrying,
    /// BatchMode で認証できない（長いバックオフに入る）
    AuthFailed,
    /// 正常停止
    Stopped,
}

/// GUI・CLI が読む uplink の状態（specs/v0.14 §9）。
///
/// **これを読むだけで健全性が分かる**ので、v0.13 までの `probe_agent_forward`
/// （mux 経由リモート実行 100–200ms/回）が不要になる。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UplinkState {
    pub schema: u32,
    pub pid: u32,
    pub state: UplinkPhase,
    pub generation: u64,
    pub since_epoch: u64,
    pub last_ok_epoch: u64,
    pub remote_socket: Option<String>,
    pub local_socket: String,
    /// ローカル gpg-agent へ繋げるか（"ok" / "down" / "unknown"）
    pub local_agent: String,
    pub reconnects: u64,
    pub last_error: Option<String>,
}

impl UplinkState {
    /// gpg が使える見込みがあるか（UI のバッジ判定用）。
    pub fn is_healthy(&self) -> bool {
        self.state == UplinkPhase::Connected && self.local_agent != "down"
    }
}

/// UI が読む疎通状態。
///
/// v0.13 の `agent_socket::ForwardHealth` のスラッグを踏襲しているため、
/// フロントエンド（`useGpgForwardStatus`）の型は変わらない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardHealth {
    /// このホストでは relay を使わない（監視対象外・バッジ非表示）
    NoForward,
    /// 疎通していて、ローカル gpg-agent も生きている
    Healthy,
    /// 繋がってはいるが gpg は使えない（ローカル agent 死亡・認証失敗）
    Broken,
    /// まだ繋がっていない（接続中・再接続待ち・停止中）
    Unreachable,
}

impl ForwardHealth {
    pub fn as_slug(self) -> &'static str {
        match self {
            ForwardHealth::NoForward => "no_forward",
            ForwardHealth::Healthy => "healthy",
            ForwardHealth::Broken => "broken",
            ForwardHealth::Unreachable => "unreachable",
        }
    }
}

/// 設定と状態ファイルだけから健全性を判定する（**リモート実行ゼロ**）。
///
/// v0.13 までは mux 経由で `gpg-connect-agent` を叩いていた（100–200ms/回）。
/// v0.14 では uplink が自分の状態を書き出しているので、ファイルを読むだけで済む。
pub fn health(alias: &str) -> ForwardHealth {
    let enabled = gpg_config::load()
        .map(|c| c.is_enabled(alias))
        .unwrap_or(false);
    if !enabled {
        return ForwardHealth::NoForward;
    }
    if !is_running(alias) {
        return ForwardHealth::Unreachable;
    }
    match read_state(alias) {
        Some(state) if state.is_healthy() => ForwardHealth::Healthy,
        // 繋がっているのに不健全 = ローカル agent 側の問題
        Some(state) if state.state == UplinkPhase::Connected => ForwardHealth::Broken,
        Some(state) if state.state == UplinkPhase::AuthFailed => ForwardHealth::Broken,
        // 接続中・再接続待ち・停止直後
        _ => ForwardHealth::Unreachable,
    }
}

pub fn state_path(alias: &str) -> Result<PathBuf> {
    Ok(crate::paths::forwards_dir()?.join(format!("{}.uplink.json", sanitize_alias(alias))))
}

pub fn lock_path(alias: &str) -> Result<PathBuf> {
    Ok(crate::paths::forwards_dir()?.join(format!("{}.uplink.lock", sanitize_alias(alias))))
}

/// 状態ファイルを読む（無ければ `None`）。
pub fn read_state(alias: &str) -> Option<UplinkState> {
    let path = state_path(alias).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// uplink デーモンが動いているか。
///
/// pid の生存確認や mtime ではなく **flock が取れるかどうか**で判定する。
/// プロセスが SIGKILL されても OS がロックを解放するため誤検知しない。
pub fn is_running(alias: &str) -> bool {
    match lock_path(alias) {
        Ok(path) => lock::is_held(&path),
        Err(_) => false,
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// atomic write（tmp + rename）。読み手が中途半端な JSON を見ないようにする。
fn write_state(alias: &str, state: &UplinkState) {
    let Ok(path) = state_path(alias) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(text) = serde_json::to_string_pretty(state) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

// ─── ensure（GUI / CLI 共通の入口） ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// 設定で無効（このホストは relay を使わない）
    Disabled,
    /// 既に常駐していた
    AlreadyRunning,
    /// 起動した
    Started,
    /// 起動を試みたが確認できなかった
    StartFailed,
}

/// uplink が居ることを保証する（冪等・数 ms）。
///
/// 1. flock を **non-blocking で試す**
/// 2. 失敗 = 既に常駐している → 何もしない
/// 3. 成功 = 不在 → 解放して `launcher gpg-uplink --daemon <alias>` を spawn
///
/// 手順 3 の隙間で二重 spawn が起こり得るが、子側が起動直後に flock を取れなければ
/// 即 exit するので 1 つに収束する。
///
/// `launcher` は `ccc-ssh` の実行パス。CLI からは `current_exe`、GUI からは
/// 同梱 sidecar のパスを渡す（**GUI と CLI が同じコードパスを通る**）。
pub fn ensure_uplink(alias: &str, launcher: &Path, log: Log) -> Result<EnsureOutcome> {
    if !gpg_config::load()?.is_enabled(alias) {
        return Ok(EnsureOutcome::Disabled);
    }
    let lock = lock_path(alias)?;
    if lock::is_held(&lock) {
        return Ok(EnsureOutcome::AlreadyRunning);
    }

    let mut cmd = Command::new(launcher);
    cmd.args(["gpg-uplink", "--daemon", alias])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: fork と exec の間で呼ぶのは setsid のみ（async-signal-safe）。
    // 親（GUI や短命な CLI）が終了しても uplink を道連れにしないために必要
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .with_context(|| format!("uplink デーモンを起動できません: {}", launcher.display()))?;

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if lock::is_held(&lock) {
            log(&format!("[uplink] {alias}: デーモンを起動しました"));
            return Ok(EnsureOutcome::Started);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    log(&format!(
        "[uplink] {alias}: デーモンの起動を確認できませんでした"
    ));
    Ok(EnsureOutcome::StartFailed)
}

/// uplink デーモンを停止する。戻り値は「停止させたか」。
pub fn stop_uplink(alias: &str, log: Log) -> Result<bool> {
    let lock = lock_path(alias)?;
    if !lock::is_held(&lock) {
        return Ok(false);
    }
    let Some(pid) = lock::holder_pid(&lock) else {
        return Err(anyhow!("uplink は動作していますが pid を特定できません"));
    };
    // SAFETY: 自分たちが記録した pid への TERM
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !lock::is_held(&lock) {
            log(&format!("[uplink] {alias}: 停止しました（pid {pid}）"));
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(anyhow!("pid {pid} が 5 秒以内に終了しませんでした"))
}

/// `ssh -G` に残っている unix socket の `RemoteForward` を列挙する（診断用）。
///
/// **移行時の最大のリスク**（specs/v0.14 §11.2 の層 1）: ここに残っていると、
/// 素の `ssh` で接続した瞬間に sshd が relay の socket を上書きしてしまう。
pub fn stale_config_forwards(alias: &str) -> Result<Vec<String>> {
    let out = crate::ssh_config::run_ssh_g(alias)?;
    Ok(parse_socket_remote_forwards(&out))
}

/// `ssh -G` 出力から unix socket 転送の `remoteforward` 行を抜き出す。
/// ポート転送（数字や host:port 形式）は対象外。
fn parse_socket_remote_forwards(ssh_g_output: &str) -> Vec<String> {
    ssh_g_output
        .lines()
        .filter_map(|line| line.strip_prefix("remoteforward "))
        .filter_map(|rest| {
            let (remote, local) = rest.trim().split_once(' ')?;
            (remote.starts_with('/') && local.starts_with('/')).then(|| format!("{remote} {local}"))
        })
        .collect()
}

// ─── デーモン本体 ────────────────────────────────────────────────────────────

/// 常駐ループ。flock を取れなければ即座に正常終了する（二重起動の収束点）。
pub fn run_daemon(alias: &str, log: Log) -> Result<()> {
    let lock_file = lock_path(alias)?;
    let Some(_lock) = lock::try_acquire(&lock_file)? else {
        log(&format!(
            "[uplink] {alias}: 既に別のデーモンが動作しているため終了します"
        ));
        return Ok(());
    };

    let host = gpg_config::load()?
        .resolve(alias)?
        .ok_or_else(|| anyhow!("{alias} は gpg.json で有効になっていません"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

    // SIGTERM / SIGINT で停止する
    let mut shutdown = signal::install().context("シグナルハンドラを設置できません")?;
    {
        let stop = Arc::clone(&stop);
        let child_slot = Arc::clone(&child_slot);
        std::thread::Builder::new()
            .name("ccc-uplink-signal".into())
            .spawn(move || {
                let _ = shutdown.wait();
                stop.store(true, Ordering::Relaxed);
                kill_child(&child_slot);
            })?;
    }

    let mut tracker = StateTracker::new(alias, &host, &stop);
    tracker.publish(UplinkPhase::Connecting, None);

    let mut backoff = INITIAL_BACKOFF;
    while !stop.load(Ordering::Relaxed) {
        tracker.begin_generation();
        match session_once(&host, &child_slot, &stop, &mut tracker, log) {
            Ok(()) => {
                // 相手が正常に切断した（relay の停止・後勝ちなど）。すぐ張り直す
                backoff = INITIAL_BACKOFF;
                tracker.publish(UplinkPhase::Retrying, None);
            }
            Err(SessionError::Auth(msg)) => {
                log(&format!("[uplink] {alias}: 認証に失敗しました: {msg}"));
                tracker.publish(UplinkPhase::AuthFailed, Some(msg));
                backoff = AUTH_FAIL_BACKOFF;
            }
            Err(SessionError::MissingBinary) => {
                // ensure_remote_daemon が配信して再試行しても解決しなかった場合のみ届く
                tracker.publish(UplinkPhase::Retrying, Some("relay の配信に失敗".into()));
                backoff = next_backoff(backoff);
            }
            Err(SessionError::Other(msg)) => {
                log(&format!("[uplink] {alias}: セッションが切れました: {msg}"));
                tracker.publish(UplinkPhase::Retrying, Some(msg));
                backoff = next_backoff(backoff);
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tracker.count_reconnect();
        sleep_until_stop(backoff, &stop);
    }

    kill_child(&child_slot);
    tracker.publish(UplinkPhase::Stopped, None);
    log(&format!("[uplink] {alias}: 停止しました"));
    Ok(())
}

enum SessionError {
    /// BatchMode で認証できない（長いバックオフへ）
    Auth(String),
    /// リモートに relay バイナリが無い（配信して再試行する）
    MissingBinary,
    Other(String),
}

/// 1 回分の接続。相手が切るか停止要求が来るまでブロックする。
fn session_once(
    host: &ResolvedHost,
    child_slot: &Arc<Mutex<Option<Child>>>,
    stop: &Arc<AtomicBool>,
    tracker: &mut StateTracker,
    log: Log,
) -> Result<(), SessionError> {
    let alias = &host.alias;

    // 1. リモート relay を冪等起動する（毎回打つ。リモート再起動からも自動復帰する）
    ensure_remote_daemon(host, log)?;

    // 2. stdio ブリッジを起動する
    let mut child = Command::new(ssh_bin())
        .args(uplink_ssh_args(host))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SessionError::Other(format!("ssh を起動できません: {e}")))?;

    let stdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let stderr_tail = spawn_stderr_reader(stderr);
    *child_slot.lock().unwrap() = Some(child);

    // 3. 多重化セッション。停止時は **ssh 子プロセスの kill** で reader を解除する
    //    （パイプは shutdown できない）
    let interrupt = {
        let child_slot = Arc::clone(child_slot);
        Arc::new(move || kill_child(&child_slot)) as mux::Interrupt
    };
    let on_ready = tracker.ready_callback();
    let cfg = MuxConfig {
        socket_path: host.local_socket.to_string_lossy().into_owned(),
        grace_secs: host.grace_secs,
        ..MuxConfig::default()
    };
    let outcome = mux::run_connector(
        stdout,
        stdin,
        &host.local_socket,
        cfg,
        MuxHooks::new(interrupt).with_ready(on_ready),
    );

    // 4. 後始末
    let status = reap_child(child_slot);
    let stderr_text = stderr_tail.join().unwrap_or_default();

    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    match outcome {
        Ok(end) => {
            log(&format!("[uplink] {alias}: セッション終了: {end:?}"));
            Ok(())
        }
        Err(e) => {
            // 接続すらできなかった場合は ssh の stderr の方が情報量が多い
            let detail = if stderr_text.trim().is_empty() {
                e.to_string()
            } else {
                stderr_text.trim().to_string()
            };
            if is_auth_failure(&status, &stderr_text) {
                Err(SessionError::Auth(detail))
            } else {
                Err(SessionError::Other(detail))
            }
        }
    }
}

/// リモートの relay デーモンを冪等に起動する。
///
/// バイナリが未配信なら 1 度だけ配信してから再試行する。毎回バージョンを
/// 問い合わせると接続のたびに往復が増えるため、**失敗を検知してから配る**。
fn ensure_remote_daemon(host: &ResolvedHost, log: Log) -> Result<(), SessionError> {
    match try_ensure_remote_daemon(host, log) {
        Err(SessionError::MissingBinary) => {
            log(&format!(
                "[uplink] {}: リモートに relay がないため配信します",
                host.alias
            ));
            crate::gpg_deploy::deliver(&host.alias, log)
                .map_err(|e| SessionError::Other(format!("relay の配信に失敗しました: {e}")))?;
            match try_ensure_remote_daemon(host, log) {
                Err(SessionError::MissingBinary) => Err(SessionError::Other(
                    "配信後もリモートで relay を起動できませんでした".into(),
                )),
                other => other,
            }
        }
        other => other,
    }
}

/// リモートに relay バイナリが無いことを示す応答か。
///
/// POSIX シェルは「コマンドが見つからない」を 127 で返す。メッセージはシェルと
/// ロケールで揺れるため、コードを主・文言を従に見る。
fn is_missing_binary(code: &Option<i32>, stderr: &str) -> bool {
    if *code == Some(127) {
        return true;
    }
    let lower = stderr.to_lowercase();
    lower.contains("no such file or directory") || lower.contains("not found")
}

fn try_ensure_remote_daemon(host: &ResolvedHost, log: Log) -> Result<(), SessionError> {
    let mut args = base_ssh_args(host);
    args.push(host.alias.clone());
    args.push(remote_command(host, "--ensure-daemon"));

    let outcome =
        crate::exec::run_with_timeout(Command::new(ssh_bin()).args(&args), ENSURE_REMOTE_TIMEOUT)
            .map_err(|e| SessionError::Other(format!("ssh を起動できません: {e}")))?;

    if outcome.timed_out {
        return Err(SessionError::Other(
            "リモート relay の起動確認がタイムアウトしました".into(),
        ));
    }
    if !outcome.success() {
        let stderr = outcome.stderr.trim().to_string();
        if is_auth_failure(&outcome.code, &stderr) {
            return Err(SessionError::Auth(stderr));
        }
        if is_missing_binary(&outcome.code, &stderr) {
            return Err(SessionError::MissingBinary);
        }
        return Err(SessionError::Other(format!(
            "リモート relay を起動できません（code={:?}）: {stderr}",
            outcome.code
        )));
    }
    log(&format!(
        "[uplink] {}: リモート relay を確認しました（{}）",
        host.alias,
        outcome.stdout.trim()
    ));
    Ok(())
}

/// uplink 接続に共通の ssh オプション。
///
/// **接続方法（HostName/User/Port/IdentityFile/ProxyJump…）はユーザーの
/// `~/.ssh/config` をそのまま使う。** ここで打ち消すのは 2 つだけ:
///
/// - `ControlMaster=no` + `ControlPath=none`: master に相乗りも新設もしない。
///   ユーザーのターミナルや tmux セッションと完全に無関係な接続にする
/// - `ClearAllForwardings=yes`: config の `-L`/`-R`/`-D` を要求しない。
///   古い gpg 用 `RemoteForward` が残っていても、この接続では発動させない
fn base_ssh_args(host: &ResolvedHost) -> Vec<String> {
    let mut args: Vec<String> = [
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ExitOnForwardFailure=no",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    // keepalive が無い構成では、網断時に ssh 自身が死なず uplink の再接続も
    // 遅れる。アプリ層 ping があるので必須ではないが、二重に効かせる
    if crate::ssh_config::server_alive_interval(&host.alias).unwrap_or(0) == 0 {
        args.extend(
            [
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    }
    args
}

fn uplink_ssh_args(host: &ResolvedHost) -> Vec<String> {
    let mut args = base_ssh_args(host);
    args.push(host.alias.clone());
    args.push(remote_command(host, "--uplink"));
    args
}

/// リモートで実行するコマンド行。`~` はリモートのシェルが展開する。
fn remote_command(host: &ResolvedHost, mode: &str) -> String {
    let mut cmd = format!("{} {mode}", remote_bin());
    if let Some(socket) = &host.remote_socket {
        cmd.push_str(&format!(" --gpg-socket {}", shell_quote(socket)));
    }
    if mode == "--ensure-daemon" {
        cmd.push_str(&format!(" --grace {}", host.grace_secs));
    }
    cmd
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// ssh の終了コードと stderr から「BatchMode で認証できない」を見分ける。
fn is_auth_failure(code: &Option<i32>, stderr: &str) -> bool {
    if *code != Some(255) {
        return false;
    }
    let lower = stderr.to_lowercase();
    lower.contains("permission denied")
        || lower.contains("too many authentication failures")
        || lower.contains("no supported authentication methods")
        || lower.contains("host key verification failed")
}

fn next_backoff(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > MAX_BACKOFF {
        MAX_BACKOFF
    } else {
        doubled
    }
}

/// 停止要求を見ながら待つ（長いバックオフ中でも即座に止まれるように）。
fn sleep_until_stop(total: Duration, stop: &Arc<AtomicBool>) {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(TICK);
    }
}

fn kill_child(slot: &Arc<Mutex<Option<Child>>>) {
    if let Ok(mut guard) = slot.lock() {
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
        }
    }
}

fn reap_child(slot: &Arc<Mutex<Option<Child>>>) -> Option<i32> {
    let mut guard = slot.lock().ok()?;
    let mut child = guard.take()?;
    let _ = child.kill();
    child.wait().ok().and_then(|s| s.code())
}

/// ssh の stderr を読み続け、末尾数行を返す（診断用）。
///
/// ログ関数はスレッドをまたげないため、行はスレッド内で溜めて呼び出し側に返す。
/// 読み続けないと ssh のパイプが詰まる点にも注意（捨てるだけではいけない）。
fn spawn_stderr_reader(stderr: std::process::ChildStderr) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut tail: Vec<String> = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            tail.push(line);
            if tail.len() > 10 {
                tail.remove(0);
            }
        }
        tail.join("\n")
    })
}

// ─── 状態の発行 ──────────────────────────────────────────────────────────────

/// 状態ファイルの発行元。
///
/// `on_ready` コールバックは mux のスレッドから呼ばれるため、状態は
/// `Arc<Mutex<_>>` で共有する（スナップショットを配ると、コールバックが書いた
/// `connected` と本体が書く次の状態が食い違う）。
struct StateTracker {
    alias: String,
    state: Arc<Mutex<UplinkState>>,
    stop: Arc<AtomicBool>,
    local_agent_checked: Instant,
}

impl StateTracker {
    fn new(alias: &str, host: &ResolvedHost, stop: &Arc<AtomicBool>) -> Self {
        StateTracker {
            alias: alias.to_string(),
            state: Arc::new(Mutex::new(UplinkState {
                schema: 1,
                pid: std::process::id(),
                state: UplinkPhase::Connecting,
                generation: 0,
                since_epoch: now_epoch(),
                last_ok_epoch: 0,
                remote_socket: host.remote_socket.clone(),
                local_socket: host.local_socket.to_string_lossy().into_owned(),
                local_agent: check_local_agent(&host.local_socket),
                reconnects: 0,
                last_error: None,
            })),
            stop: Arc::clone(stop),
            local_agent_checked: Instant::now(),
        }
    }

    fn begin_generation(&mut self) {
        self.state.lock().unwrap().generation += 1;
    }

    fn count_reconnect(&mut self) {
        self.state.lock().unwrap().reconnects += 1;
    }

    fn publish(&mut self, phase: UplinkPhase, error: Option<String>) {
        // ローカル gpg-agent の生死も定期的に反映する（ローカル完結なので安価）
        let recheck = self.local_agent_checked.elapsed() > LOCAL_AGENT_CHECK_INTERVAL;
        let mut state = self.state.lock().unwrap();
        if recheck {
            state.local_agent = check_local_agent(Path::new(&state.local_socket));
            self.local_agent_checked = Instant::now();
        }
        state.state = phase;
        state.last_error = error;
        write_state(&self.alias, &state);
    }

    /// HELLO 交換が済んだ時点で `connected` に遷移させるコールバック。
    fn ready_callback(&self) -> mux::Interrupt {
        let alias = self.alias.clone();
        let shared = Arc::clone(&self.state);
        let stop = Arc::clone(&self.stop);
        Arc::new(move || {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let mut state = shared.lock().unwrap();
            state.local_agent = check_local_agent(Path::new(&state.local_socket));
            state.state = UplinkPhase::Connected;
            state.last_ok_epoch = now_epoch();
            state.last_error = None;
            write_state(&alias, &state);
        })
    }
}

/// ローカル gpg-agent へ繋げるか（connect するだけで即閉じる）。
fn check_local_agent(path: &Path) -> String {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => "ok".into(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "down".into(),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => "down".into(),
        Err(_) => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> ResolvedHost {
        ResolvedHost {
            alias: "mybox".into(),
            remote_socket: None,
            local_socket: PathBuf::from("/Users/user/.gnupg/S.gpg-agent.extra"),
            grace_secs: 5,
        }
    }

    #[test]
    fn ssh_args_neutralize_sharing_and_forwarding() {
        let args = base_ssh_args(&host()).join(" ");
        // 接続の共有をしない
        assert!(args.contains("ControlMaster=no"));
        assert!(args.contains("ControlPath=none"));
        // config に残った RemoteForward をこの接続では発動させない
        assert!(args.contains("ClearAllForwardings=yes"));
        // 無人ループなので対話認証はしない
        assert!(args.contains("BatchMode=yes"));
        // HostName/User/IdentityFile 等は打ち消さない（config をそのまま使う）
        assert!(!args.contains("-o HostName"));
        assert!(!args.contains("IdentityFile"));
    }

    #[test]
    fn uplink_args_end_with_alias_and_remote_command() {
        let args = uplink_ssh_args(&host());
        assert_eq!(args[args.len() - 2], "mybox");
        assert_eq!(args[args.len() - 1], "~/.ccc/bin/ccc-gpg-relay --uplink");
    }

    #[test]
    fn remote_command_passes_custom_socket() {
        let mut h = host();
        h.remote_socket = Some("/run/user/1000/gnupg/S.gpg-agent".into());
        assert_eq!(
            remote_command(&h, "--uplink"),
            "~/.ccc/bin/ccc-gpg-relay --uplink --gpg-socket '/run/user/1000/gnupg/S.gpg-agent'"
        );
    }

    #[test]
    fn ensure_daemon_command_carries_grace() {
        let h = host();
        assert_eq!(
            remote_command(&h, "--ensure-daemon"),
            "~/.ccc/bin/ccc-gpg-relay --ensure-daemon --grace 5"
        );
    }

    #[test]
    fn shell_quote_escapes_embedded_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn auth_failure_needs_both_code_and_message() {
        assert!(is_auth_failure(
            &Some(255),
            "user@host: Permission denied (publickey)."
        ));
        assert!(is_auth_failure(&Some(255), "Host key verification failed."));
        // 255 でも認証以外の理由なら通常のバックオフに載せる
        assert!(!is_auth_failure(&Some(255), "connection timed out"));
        // 認証文言があってもコードが違えば認証失敗ではない
        assert!(!is_auth_failure(&Some(1), "permission denied"));
        assert!(!is_auth_failure(&None, "permission denied"));
    }

    #[test]
    fn parse_extracts_only_unix_socket_forwards() {
        let out = "\
hostname 127.0.0.1
remoteforward /home/user/.gnupg/S.gpg-agent /Users/user/.gnupg/S.gpg-agent.extra
remoteforward 127.0.0.1:8080 [localhost]:8080
localforward 3000 [localhost]:3000
";
        let found = parse_socket_remote_forwards(out);
        assert_eq!(found.len(), 1, "unix socket 転送だけを拾う");
        assert!(found[0].starts_with("/home/user/.gnupg/S.gpg-agent"));
    }

    #[test]
    fn parse_returns_empty_for_clean_config() {
        assert!(parse_socket_remote_forwards("hostname example\nport 22\n").is_empty());
    }

    #[test]
    fn missing_binary_is_detected_by_exit_code() {
        // POSIX シェルの「コマンドが見つからない」
        assert!(is_missing_binary(&Some(127), ""));
        // 文言だけでも拾う（シェルによって code が異なる場合の保険）
        assert!(is_missing_binary(
            &Some(1),
            "bash: ~/.ccc/bin/ccc-gpg-relay: No such file or directory"
        ));
        // 起動はできたが別の理由で失敗したケースは配信しない
        assert!(!is_missing_binary(&Some(1), "permission denied"));
        assert!(!is_missing_binary(&Some(255), "connection closed"));
    }

    #[test]
    fn backoff_doubles_and_saturates() {
        assert_eq!(next_backoff(Duration::from_secs(1)), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(32)), MAX_BACKOFF);
        assert_eq!(next_backoff(MAX_BACKOFF), MAX_BACKOFF);
    }

    #[test]
    fn local_agent_is_down_when_socket_is_missing() {
        assert_eq!(
            check_local_agent(Path::new("/nonexistent/ccc/S.gpg-agent")),
            "down"
        );
    }

    #[test]
    fn state_paths_are_per_alias_and_sanitized() {
        // ファイル名にパス区切りを持ち込まない
        let path = state_path("host/with/sep").unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(name, "host_with_sep.uplink.json");
    }

    #[test]
    fn health_slugs_match_the_frontend_contract() {
        // useGpgForwardStatus.ts の GpgForwardHealth と対応する
        assert_eq!(ForwardHealth::NoForward.as_slug(), "no_forward");
        assert_eq!(ForwardHealth::Healthy.as_slug(), "healthy");
        assert_eq!(ForwardHealth::Broken.as_slug(), "broken");
        assert_eq!(ForwardHealth::Unreachable.as_slug(), "unreachable");
    }

    #[test]
    fn healthy_requires_connected_and_live_agent() {
        let mut state = UplinkState {
            schema: 1,
            pid: 1,
            state: UplinkPhase::Connected,
            generation: 1,
            since_epoch: 0,
            last_ok_epoch: 0,
            remote_socket: None,
            local_socket: "/tmp/x".into(),
            local_agent: "ok".into(),
            reconnects: 0,
            last_error: None,
        };
        assert!(state.is_healthy());
        state.local_agent = "down".into();
        assert!(!state.is_healthy(), "ローカル agent が死んでいれば不健全");
        state.local_agent = "ok".into();
        state.state = UplinkPhase::Retrying;
        assert!(!state.is_healthy());
    }
}
