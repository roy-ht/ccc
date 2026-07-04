//! ssh master の死活プローブと half-open（半死に）状態からの自動復旧（v0.13）。
//!
//! 網断で master の TCP が half-open になっても `ssh -O check` は**ローカルの
//! mux socket で完結**する（MUX_C_ALIVE_CHECK はネットワークを触らない）ため
//! 成功し続け、gpg 世代ゲート（pid 不変）も誤って健全と判定する。実際に
//! ネットワークが生きているかは「mux 経由でリモート実行が返ってくるか」で
//! しか分からない。
//!
//! 本モジュールは
//! 1. [`probe_master`]: `-O check` + mux 経由 `true` 実行で master を 5 状態に分類
//! 2. [`teardown_master`]: `-O exit`（wedged なら kill フォールバック）で確実に畳む
//! 3. [`reestablish_user_cm_master`]: ユーザー CM 尊重モードで `ssh -N -f` により
//!    master を再確立（ユーザー config の RemoteForward が再要求され gpg forward 復活）
//! 4. [`recover_half_open`]: 上記のオーケストレーション（ロック + クールダウン付き）
//!
//! を提供する。GUI の 60 秒ループと `ccc-ssh` の pre-connect フック/heal が使う。
//!
//! half-open のセッションは OpenSSH に transport 再開機構がない以上復元不能
//! （リモート側の tmux が作業を保持する）ため、畳むことによる損失はない。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::agent_socket::Log;
use crate::exec::run_with_timeout;
use crate::forwards::{
    master_pid_detailed, mux_base_args, sanitize_alias, MasterCheck, MUX_OP_TIMEOUT,
};
use crate::ssh_config;

/// mux 経由 `true` 実行によるプローブの上限（GUI ループ用の既定値）。
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// master の死活分類。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MasterLiveness {
    /// master 不在（正常な不在。自動接続はしない）
    NoMaster,
    /// mux 経由のリモート実行が返ってきた
    Alive { pid: Option<u32> },
    /// `-O check` は応答するがリモート実行がタイムアウト（= TCP が half-open）
    HalfOpen { pid: Option<u32> },
    /// ssh が即時に非ゼロ終了（MaxSessions 超過等 = サーバは応答している）。
    /// 生きたセッションを巻き込まないため畳まない
    SessionRefused { pid: Option<u32> },
    /// `-O check` 自体が無応答（master プロセスが固まっている）
    Wedged,
}

/// master を死活プローブする。master 不在時はリモート接続を一切発生させない
/// （60 秒ループから呼んでも意図しない自動接続をしないための重要な性質）。
pub fn probe_master(host_alias: &str, probe_timeout: Duration, log: Log) -> MasterLiveness {
    let pid = match master_pid_detailed(host_alias) {
        None | Some(MasterCheck::NotRunning) => return MasterLiveness::NoMaster,
        Some(MasterCheck::Wedged) => {
            log(&format!(
                "[liveness] {host_alias}: -O check が無応答（master が固まっている）"
            ));
            return MasterLiveness::Wedged;
        }
        Some(MasterCheck::Running { pid }) => pid,
    };

    // ControlMaster=no: 既存 socket があればそれを使い、無ければ master を
    // 新設**しない**（プローブが新規接続や master 生成を誘発しないため）
    let base = mux_base_args(host_alias).unwrap_or_default();
    let outcome = run_with_timeout(
        Command::new("ssh").args(&base).args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ControlMaster=no",
            "-o",
            "ConnectTimeout=10",
            host_alias,
            "true",
        ]),
        probe_timeout,
    );
    match outcome {
        Ok(o) => classify_probe(pid, o.timed_out, o.code),
        // ssh バイナリ起動失敗等。判定不能は安全側（畳まない）
        Err(_) => MasterLiveness::SessionRefused { pid },
    }
}

/// プローブ結果の分類（純粋関数）。畳むトリガーは「タイムアウトのみ」:
/// 即時の非ゼロ終了はサーバかローカル ssh が応答している証拠なので畳まない。
fn classify_probe(pid: Option<u32>, timed_out: bool, code: Option<i32>) -> MasterLiveness {
    if timed_out {
        return MasterLiveness::HalfOpen { pid };
    }
    match code {
        Some(0) => MasterLiveness::Alive { pid },
        Some(255) => MasterLiveness::SessionRefused { pid },
        // リモートまで届いて非ゼロが返った（`true` では起きないはずだが、
        // ネットワークは生きている）→ Alive 扱い
        _ => MasterLiveness::Alive { pid },
    }
}

/// master を確実に畳む。`-O exit` → 無応答なら pid への kill フォールバック。
/// 戻り値は「master が居なくなったことを確認できたか」。
///
/// kill フォールバックは pid 再利用の誤殺を防ぐため、`ps -p <pid> -o command=`
/// の出力が ssh であることを確認してからシグナルを送る。
pub fn teardown_master(host_alias: &str, known_pid: Option<u32>, log: Log) -> bool {
    let base = mux_base_args(host_alias).unwrap_or_default();
    let exit_result = run_with_timeout(
        Command::new("ssh")
            .args(&base)
            .args(["-O", "exit", host_alias]),
        MUX_OP_TIMEOUT,
    );
    match &exit_result {
        Ok(o) if o.success() => {
            log(&format!(
                "[liveness] {host_alias}: -O exit で master を畳んだ"
            ));
        }
        Ok(o) if o.timed_out => {
            log(&format!(
                "[liveness] {host_alias}: -O exit 無応答 → kill フォールバックへ"
            ));
        }
        _ => {
            // socket が既に消えている等。下の確認ループで判定する
        }
    }

    // socket 解放を最大 2 秒待つ
    if wait_master_gone(host_alias, Duration::from_secs(2)) {
        return true;
    }

    // kill フォールバック（wedged master は -O exit を受け付けない）
    let Some(pid) = known_pid else {
        log(&format!(
            "[liveness] {host_alias}: master が残っているが pid 不明のため kill を断念"
        ));
        return false;
    };
    if !pid_is_ssh(pid) {
        log(&format!(
            "[liveness] {host_alias}: pid={pid} は ssh ではない（再利用済み）ため kill を断念"
        ));
        return false;
    }
    log(&format!("[liveness] {host_alias}: kill -TERM {pid}"));
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    if wait_master_gone(host_alias, Duration::from_secs(2)) {
        return true;
    }
    if pid_is_ssh(pid) {
        log(&format!("[liveness] {host_alias}: kill -KILL {pid}"));
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    wait_master_gone(host_alias, Duration::from_secs(2))
}

/// `-O check` が「master 不在」を返すまで待つ（250ms 間隔）。
fn wait_master_gone(host_alias: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match master_pid_detailed(host_alias) {
            None | Some(MasterCheck::NotRunning) => return true,
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// pid のプロセスが ssh かどうか（`ps -p <pid> -o command=`）。
fn pid_is_ssh(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()
        .map(|o| {
            let cmd = String::from_utf8_lossy(&o.stdout);
            let cmd = cmd.trim();
            cmd == "ssh"
                || cmd.starts_with("ssh ")
                || cmd.ends_with("/ssh")
                || cmd.contains("/ssh ")
        })
        .unwrap_or(false)
}

/// ユーザー CM 尊重モードで master を再確立する（`ssh -N -f`）。
///
/// ユーザー config の ControlMaster auto により `-N -f` プロセスが新 master になり、
/// config の RemoteForward（gpg socket）と LocalForward が再要求される。
/// リモート sshd が StreamLocalBindUnlink yes なら残骸 socket も上書きされる。
///
/// - BatchMode: 対話認証が必要な構成では即失敗させる（無人ループからの MFA スパム防止）
/// - ExitOnForwardFailure: forward を張れなければ非ゼロ終了（沈黙故障防止）
/// - config の ServerAliveInterval が 0 なら keepalive を注入し、以後の網断では
///   master が自滅してクリーンな再確立に収束するようにする
pub fn reestablish_user_cm_master(host_alias: &str, log: Log) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "-N".into(),
        "-f".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
    ];
    if ssh_config::server_alive_interval(host_alias).unwrap_or(0) == 0 {
        args.extend([
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
        ]);
    }
    args.push(host_alias.into());

    let outcome = run_with_timeout(Command::new("ssh").args(&args), Duration::from_secs(30))
        .map_err(|e| format!("ssh の起動に失敗: {e}"))?;
    if outcome.success() {
        log(&format!(
            "[liveness] {host_alias}: master を再確立した（ssh -N -f）"
        ));
        Ok(())
    } else if outcome.timed_out {
        Err("ssh -N -f が 30 秒以内に完了しませんでした（ネットワーク未復旧の可能性）".into())
    } else {
        Err(format!(
            "ssh -N -f が失敗しました（対話認証が必要な構成では BatchMode により失敗します）: {}",
            outcome.stderr.trim()
        ))
    }
}

// ─── 多重起動ガード + クールダウン（ファイルベース、GUI/CLI 共有） ──────────

/// 「畳み + 再確立」区間の排他ロック。Drop でロックファイルを削除する。
pub struct RebuildLock {
    path: PathBuf,
}

impl Drop for RebuildLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// ロックの stale 判定（プロセスが死んでロックが残った場合の奪取猶予）。
const LOCK_STALE_SECS: u64 = 120;

/// 再確立失敗後の再試行禁止時間（秒）。`CCC_REBUILD_COOLDOWN` で上書き可能。
const DEFAULT_FAIL_COOLDOWN_SECS: u64 = 600;
/// 再確立成功後の再試行禁止時間（秒）。網フラップ時のリビルドストーム防止。
const SUCCESS_COOLDOWN_SECS: u64 = 120;

fn acquire_rebuild_lock_in(dir: &Path, host_alias: &str) -> Option<RebuildLock> {
    let path = dir.join(format!("{}.rebuild-lock", sanitize_alias(host_alias)));
    let _ = std::fs::create_dir_all(dir);
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Some(RebuildLock { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // stale なら奪取（削除して再試行）
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
                    .is_some_and(|age| age.as_secs() > LOCK_STALE_SECS);
                if !stale {
                    return None;
                }
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => return None,
        }
    }
    None
}

fn acquire_rebuild_lock(host_alias: &str) -> Option<RebuildLock> {
    let dir = crate::paths::forwards_dir().ok()?;
    acquire_rebuild_lock_in(&dir, host_alias)
}

fn fail_cooldown_secs() -> u64 {
    std::env::var("CCC_REBUILD_COOLDOWN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_FAIL_COOLDOWN_SECS)
}

fn cooldown_path_in(dir: &Path, host_alias: &str) -> PathBuf {
    dir.join(format!("{}.rebuild-cooldown", sanitize_alias(host_alias)))
}

/// クールダウン中なら残り秒数を返す。ファイル形式: `<unix秒> ok|fail`
fn cooldown_remaining_in(dir: &Path, host_alias: &str, now_epoch: u64) -> Option<u64> {
    let content = std::fs::read_to_string(cooldown_path_in(dir, host_alias)).ok()?;
    let (ts, result) = content.trim().split_once(' ')?;
    let ts: u64 = ts.parse().ok()?;
    let window = if result == "ok" {
        SUCCESS_COOLDOWN_SECS
    } else {
        fail_cooldown_secs()
    };
    let until = ts.checked_add(window)?;
    (until > now_epoch).then(|| until - now_epoch)
}

fn cooldown_remaining(host_alias: &str) -> Option<u64> {
    let dir = crate::paths::forwards_dir().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    cooldown_remaining_in(&dir, host_alias, now)
}

fn record_rebuild_attempt(host_alias: &str, success: bool) {
    let Ok(dir) = crate::paths::forwards_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(
        cooldown_path_in(&dir, host_alias),
        format!("{now} {}", if success { "ok" } else { "fail" }),
    );
}

// ─── オーケストレーション ────────────────────────────────────────────────────

/// half-open / wedged と判定済みの master を「畳んで（必要なら）再確立」する。
/// 戻り値は「復旧処理を完了できたか」（healthy の意味ではない。gpg forward の
/// 検査・修復は呼び出し側が `ensure_agent_forward` で行う）。
///
/// - GUI/CLI 横断のファイルロックで直列化（取れなければ他プロセスに任せて false）
/// - ロック取得後に再プローブし、他プロセスが復旧済みなら何もしない
/// - `reestablish = true`（ユーザー CM 尊重モード）は `ssh -N -f` で再確立まで行う。
///   ccc 専用 master モードでは false にし、再確立は GUI の `ensure_remote_master`
///   に任せる（hook 用 reverse forward の構成を CLI が知らないため）
pub fn recover_half_open(host_alias: &str, reestablish: bool, log: Log) -> bool {
    let Some(_lock) = acquire_rebuild_lock(host_alias) else {
        log(&format!(
            "[liveness] {host_alias}: 別プロセスが復旧処理中（ロック取得失敗）"
        ));
        return false;
    };

    // ロック待ちの間に他プロセスが復旧させたかもしれないので再測定
    let liveness = probe_master(host_alias, DEFAULT_PROBE_TIMEOUT, log);
    let pid = match &liveness {
        MasterLiveness::Alive { .. } => {
            log(&format!("[liveness] {host_alias}: 再プローブで復旧を確認"));
            return true;
        }
        MasterLiveness::NoMaster => None,
        MasterLiveness::SessionRefused { .. } => {
            log(&format!(
                "[liveness] {host_alias}: サーバ応答あり（session 拒否）のため畳まない"
            ));
            return false;
        }
        MasterLiveness::HalfOpen { pid } => *pid,
        MasterLiveness::Wedged => crate::agent_socket::last_healthy_pid(host_alias),
    };

    if !matches!(liveness, MasterLiveness::NoMaster) && !teardown_master(host_alias, pid, log) {
        log(&format!(
            "[liveness] {host_alias}: master を畳めなかった（復旧中断）"
        ));
        return false;
    }

    if !reestablish {
        return true;
    }

    if let Some(remaining) = cooldown_remaining(host_alias) {
        log(&format!(
            "[liveness] {host_alias}: 再確立クールダウン中（残り {remaining} 秒）"
        ));
        return false;
    }
    match reestablish_user_cm_master(host_alias, log) {
        Ok(()) => {
            record_rebuild_attempt(host_alias, true);
            true
        }
        Err(e) => {
            record_rebuild_attempt(host_alias, false);
            log(&format!("[liveness] {host_alias}: 再確立失敗: {e}"));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_timeout_is_half_open() {
        assert_eq!(
            classify_probe(Some(1), true, None),
            MasterLiveness::HalfOpen { pid: Some(1) }
        );
    }

    #[test]
    fn classify_zero_is_alive() {
        assert_eq!(
            classify_probe(Some(1), false, Some(0)),
            MasterLiveness::Alive { pid: Some(1) }
        );
    }

    #[test]
    fn classify_255_is_session_refused() {
        assert_eq!(
            classify_probe(None, false, Some(255)),
            MasterLiveness::SessionRefused { pid: None }
        );
    }

    #[test]
    fn classify_other_code_is_alive() {
        // リモートまで届いた非ゼロ（ネットワークは生きている）
        assert_eq!(
            classify_probe(Some(2), false, Some(1)),
            MasterLiveness::Alive { pid: Some(2) }
        );
    }

    #[test]
    fn rebuild_lock_is_exclusive_and_released_on_drop() {
        let dir = std::env::temp_dir().join(format!("ccc-liveness-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lock = acquire_rebuild_lock_in(&dir, "host-a").expect("初回取得は成功するはず");
        assert!(
            acquire_rebuild_lock_in(&dir, "host-a").is_none(),
            "ロック中は取得できない"
        );
        assert!(
            acquire_rebuild_lock_in(&dir, "host-b").is_some(),
            "別ホストは独立"
        );
        drop(lock);
        assert!(
            acquire_rebuild_lock_in(&dir, "host-a").is_some(),
            "Drop で解放される"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_lock_is_taken_over() {
        let dir = std::env::temp_dir().join(format!("ccc-liveness-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host-a.rebuild-lock");
        std::fs::write(&path, "").unwrap();
        // mtime を過去に倒す代わりに、判定境界そのものは cooldown 側でテストし、
        // ここでは「新鮮なロックは奪取されない」ことだけ確認する
        assert!(acquire_rebuild_lock_in(&dir, "host-a").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cooldown_windows() {
        let dir = std::env::temp_dir().join(format!("ccc-liveness-cd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = 1_000_000u64;

        std::fs::write(cooldown_path_in(&dir, "h"), format!("{} fail", now - 10)).unwrap();
        assert!(
            cooldown_remaining_in(&dir, "h", now).is_some(),
            "失敗直後は禁止"
        );

        std::fs::write(
            cooldown_path_in(&dir, "h"),
            format!("{} fail", now - DEFAULT_FAIL_COOLDOWN_SECS - 1),
        )
        .unwrap();
        assert!(
            cooldown_remaining_in(&dir, "h", now).is_none(),
            "窓を過ぎたら許可"
        );

        std::fs::write(cooldown_path_in(&dir, "h"), format!("{} ok", now - 10)).unwrap();
        assert!(
            cooldown_remaining_in(&dir, "h", now).is_some(),
            "成功直後も禁止"
        );

        std::fs::write(
            cooldown_path_in(&dir, "h"),
            format!("{} ok", now - SUCCESS_COOLDOWN_SECS - 1),
        )
        .unwrap();
        assert!(
            cooldown_remaining_in(&dir, "h", now).is_none(),
            "成功窓は短い"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
