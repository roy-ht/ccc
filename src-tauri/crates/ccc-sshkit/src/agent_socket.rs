//! gpg agent forward（RemoteForward の unix ソケット転送）の健全性チェックと自動修復。
//!
//! container-host 系ホストは `RemoteForward /home/.../S.gpg-agent /Users/.../S.gpg-agent.extra`
//! でローカルの gpg-agent をリモートへ転送している。master が異常終了すると
//! リモート側にソケットファイルの残骸が残り、次の master の bind が黙って失敗
//! して `gpg: decryption failed: No secret key` → エージェントコマンド即死、
//! という障害になる（2026-06-10 実例）。
//!
//! さらに、bind 失敗後にリモート側で gpg-agent が誤起動してソケットを所有すると
//! 「agent は応答するが鍵を持っていない」状態になり、単純な疎通確認では
//! すり抜ける（2026-06-10 リリース版で実例: 疎通 OK 判定の 1 秒後に
//! `No secret key`）。
//!
//! 対策: 接続前に
//! 1. `gpg-connect-agent --no-autostart 'getinfo socket_name' /bye` をリモートで
//!    実行し、応答を分類して疎通＋「応答しているのが本物の forward か」を確認
//!    （restricted ソケットへの forward なら `Forbidden`、誤起動 agent なら
//!    リモート側パスを名乗る `D <path>` が返る — 詳細は [`classify_agent_reply`]）
//! 2. 不調なら、リモートの残骸ソケット削除（+ 誤起動した remote gpg-agent
//!    の kill）→ mux コマンド `-O forward -R <socket spec>` で forward だけ再要求
//!
//! master 自体は再起動しない（ターミナルと共有しているため切断できない）。
//!
//! 壊れた状態が生まれるのは master 世代交代の瞬間に限られるため、「最後に疎通
//! OK を確認した master pid」を**ファイル**（GUI と CLI で共有）にキャッシュし、
//! pid 不変ならリモート実行ゼロでスキップする（世代ゲート）。
//!
//! さらに、v0.10.2 で「新 master 起動の**前**に ccc 側から明示的に残骸 socket を
//! unlink する」層を追加した（[`cleanup_stale_remote_sockets`]）。理由: sshd の
//! `StreamLocalBindUnlink yes` に頼っていると、旧世代 sshd の forward 子プロセスが
//! 直近の TCP 断を検知しきれずに socket を握ったまま残っているケースで新 master の
//! bind が沈黙失敗（あるいは `ExitOnForwardFailure=yes` で `ssh -N -f` ごと死ぬ）
//! → 「接続は張れているが gpg forward が切れる」障害が再発する。sshd 実装差にも
//! 依存するため、ccc から明示的に消す方が確実。

use std::process::Command;
use std::time::Duration;

use crate::exec::run_with_timeout;
use crate::forwards::{master_pid, mux_base_args, sanitize_alias, MUX_OP_TIMEOUT};
use crate::ssh_config;

/// mux 経由リモート実行の上限。健全なら 1 秒未満で返るが、gpg-connect-agent の
/// 初回応答に余裕を持たせる。half-open master ではここで必ず打ち切られる。
/// `CCC_SSH_EXEC_TIMEOUT`（秒）で上書き可能。
const DEFAULT_REMOTE_EXEC_TIMEOUT_SECS: u64 = 15;

fn remote_exec_timeout() -> Duration {
    let secs = std::env::var("CCC_SSH_EXEC_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_REMOTE_EXEC_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// ログ出力コールバック。GUI はインスタンスの .debug.txt、CLI は stderr へ流す。
pub type Log<'a> = &'a dyn Fn(&str);

/// unix ソケットの RemoteForward 1 本分（`ssh -G` の `remoteforward` 行由来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketForward {
    /// リモート側 listen ソケットパス
    pub remote_path: String,
    /// ローカル側接続先ソケットパス
    pub local_path: String,
}

/// `rm -f` に渡す 1 引数として安全な形に single-quote する。
/// 内部の `'` は `'\''` に分割して閉じ・エスケープ・再開。
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// forward 群の remote_path をシェル用に quote して連結する。
fn quoted_remote_paths(forwards: &[SocketForward]) -> String {
    forwards
        .iter()
        .map(|f| shell_single_quote(&f.remote_path))
        .collect::<Vec<_>>()
        .join(" ")
}

impl SocketForward {
    /// `-R` に渡す `remote:local` 形式
    fn to_r_arg(&self) -> String {
        format!("{}:{}", self.remote_path, self.local_path)
    }
}

/// `ssh -G` の出力から unix ソケットの RemoteForward を抽出する。
/// ポート転送の RemoteForward（数字や host:port 形式）は対象外。
fn parse_socket_remote_forwards(ssh_g_output: &str) -> Vec<SocketForward> {
    ssh_g_output
        .lines()
        .filter_map(|line| line.strip_prefix("remoteforward "))
        .filter_map(|rest| {
            let (remote, local) = rest.trim().split_once(' ')?;
            (remote.starts_with('/') && local.starts_with('/')).then(|| SocketForward {
                remote_path: remote.to_string(),
                local_path: local.to_string(),
            })
        })
        .collect()
}

/// リモートでコマンドを実行し exit code を返す（master 経由なので接続コストは小さい）。
/// タイムアウト（half-open master でのハング）は None = 実行不能扱い。
fn remote_exec(host_alias: &str, mux_base: &[String], cmd: &str) -> Option<i32> {
    remote_exec_output(host_alias, mux_base, cmd).map(|(code, _)| code)
}

/// リモートでコマンドを実行し (exit code, stdout) を返す。
/// タイムアウトは None = 実行不能扱い（half-open の判定自体は liveness 層の責務）。
fn remote_exec_output(host_alias: &str, mux_base: &[String], cmd: &str) -> Option<(i32, String)> {
    let out = run_with_timeout(
        Command::new("ssh")
            .args(mux_base)
            .args(["-o", "BatchMode=yes", host_alias, cmd]),
        remote_exec_timeout(),
    )
    .ok()?;
    if out.timed_out {
        return None;
    }
    Some((out.code?, out.stdout))
}

/// gpg agent forward の疎通確認。
/// `--no-autostart` が重要: これが無いと gpg-connect-agent がリモート側で
/// gpg-agent を自動起動してソケットを奪い、状況をさらに悪化させる。
fn check(host_alias: &str, mux_base: &[String], forwards: &[SocketForward]) -> CheckResult {
    const CMD: &str = "command -v gpg-connect-agent >/dev/null 2>&1 || exit 9; \
                       gpg-connect-agent --no-autostart 'getinfo socket_name' /bye 2>&1";
    match remote_exec_output(host_alias, mux_base, CMD) {
        Some((0, out)) => classify_agent_reply(&out, forwards),
        Some((9, _)) => CheckResult::NoGpg,
        Some(_) => CheckResult::Broken,
        None => CheckResult::Unreachable,
    }
}

/// 「ソケットの先に agent がいる」(rc=0) ときの `getinfo socket_name` 応答から、
/// 本物の forward かリモートで誤起動した agent（鍵を持たない）かを見分ける。
///
/// - `ERR ... Forbidden`: restricted ソケット（`.extra`）への forward に届いて
///   いる証拠（restricted 接続では getinfo が禁止される）→ 健全
/// - `D <path>` がリモート側 listen パスと一致: リモートの gpg-agent が
///   ソケットを所有している（forward の bind 失敗後に誤起動した個体）→ 不調。
///   forward 経由ならローカル側のソケットパスを名乗るため一致しない
/// - それ以外（ローカル側パス・解析不能）: 健全扱い。誤って不調と判定すると
///   修復（kill + ソケット削除）が正常な agent を巻き込むため、保守的に倒す
fn classify_agent_reply(reply: &str, forwards: &[SocketForward]) -> CheckResult {
    if reply.contains("Forbidden") {
        return CheckResult::Healthy;
    }
    let reported = reply
        .lines()
        .find_map(|line| line.strip_prefix("D "))
        .map(str::trim);
    match reported {
        Some(path) if forwards.iter().any(|f| f.remote_path == path) => CheckResult::Broken,
        _ => CheckResult::Healthy,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CheckResult {
    Healthy,
    /// リモートに gpg-connect-agent が無い（チェック・修復とも不可能なのでスキップ）
    NoGpg,
    Broken,
    Unreachable,
}

/// GUI 監視ループから参照する forward 疎通状態。`CheckResult` のバリアントに
/// 「そもそも forward 設定が無い」（=判定対象外）を加えた公開型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardHealth {
    /// unix socket の RemoteForward が config に無い（監視対象外）
    NoForward,
    /// リモートに `gpg-connect-agent` が無く判定不能
    NoGpg,
    /// 疎通 OK（実 forward or restricted forward の Forbidden 応答が返る）
    Healthy,
    /// 不調（listener 死・ソケット残骸・rogue agent 誤起動 の何れか）
    Broken,
    /// mux 経由リモート実行に失敗（master が居ない/半死に。判定不能）
    Unreachable,
}

impl ForwardHealth {
    /// UI と CLI で共通のスラッグ表現（Tauri event 用）。
    pub fn as_slug(self) -> &'static str {
        match self {
            ForwardHealth::NoForward => "no_forward",
            ForwardHealth::NoGpg => "no_gpg",
            ForwardHealth::Healthy => "healthy",
            ForwardHealth::Broken => "broken",
            ForwardHealth::Unreachable => "unreachable",
        }
    }
}

/// **状態確認のみ**を行う軽量プローブ（`ensure_agent_forward` と違って修復しない）。
///
/// 世代ゲート・TTL は無視。60 秒監視ループから毎サイクル呼ばれる想定。
/// コスト: `ssh -G` + mux 経由 1 コマンド実行 = 概ね 100–200ms（既存 `check` と同じ）。
///
/// `test -S`（socket file 存在確認のみ）でさらに軽量化する案もあるが、rogue agent
/// による誤起動（socket file はあるが鍵無し agent が listen）を見逃すため採用しない。
/// 実装は既存 `check` を再利用する（v0.9 で確定した `classify_agent_reply` の
/// 分類ロジックが必要）。
pub fn probe_agent_forward(host_alias: &str, log: Log) -> ForwardHealth {
    let forwards = match ssh_config::run_ssh_g(host_alias) {
        Ok(out) => parse_socket_remote_forwards(&out),
        Err(e) => {
            log(&format!(
                "[agent-socket] {host_alias}: probe ssh -G 失敗（unreachable 扱い）: {e}"
            ));
            return ForwardHealth::Unreachable;
        }
    };
    if forwards.is_empty() {
        return ForwardHealth::NoForward;
    }
    let mux_base = match mux_base_args(host_alias) {
        Ok(v) => v,
        Err(e) => {
            log(&format!(
                "[agent-socket] {host_alias}: probe mux_base_args 失敗: {e}"
            ));
            return ForwardHealth::Unreachable;
        }
    };
    match check(host_alias, &mux_base, &forwards) {
        CheckResult::Healthy => ForwardHealth::Healthy,
        CheckResult::NoGpg => ForwardHealth::NoGpg,
        CheckResult::Broken => ForwardHealth::Broken,
        CheckResult::Unreachable => ForwardHealth::Unreachable,
    }
}

// ─── 世代ゲート（ファイルベース、GUI/CLI 共有） ──────────────────────────────

fn gate_path(host_alias: &str) -> Option<std::path::PathBuf> {
    crate::paths::forwards_dir()
        .ok()
        .map(|d| d.join(format!("{}.gpg-ok-pid", sanitize_alias(host_alias))))
}

/// gate ファイルの寿命（秒）。CCC_GPG_GATE_TTL で上書き可能。
/// 0 を指定すると常に再チェック。デフォルト 300 秒。
const DEFAULT_GATE_TTL_SECS: u64 = 300;

fn gate_ttl_secs() -> u64 {
    std::env::var("CCC_GPG_GATE_TTL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_GATE_TTL_SECS)
}

fn read_gate(host_alias: &str) -> Option<u32> {
    let path = gate_path(host_alias)?;
    // mtime ベース TTL: 古すぎる gate は無視して再チェックさせる。
    // master pid 不変でも forward 単独が壊れるケース（誤起動 gpg-agent 等）を
    // 一定時間で必ず検知できるようにするため。
    let meta = std::fs::metadata(&path).ok()?;
    let mtime = meta.modified().ok()?;
    let ttl = gate_ttl_secs();
    if ttl > 0 {
        let age = std::time::SystemTime::now().duration_since(mtime).ok()?;
        if age.as_secs() > ttl {
            return None;
        }
    }
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// 最後に疎通 OK を確認した master の pid（TTL 無視で読む）。
/// liveness 層が wedged master（-O check 無応答 = pid 取得不能）を kill する際の
/// 対象特定に使う。
pub fn last_healthy_pid(host_alias: &str) -> Option<u32> {
    let path = gate_path(host_alias)?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn write_gate(host_alias: &str, pid: u32) {
    let Some(path) = gate_path(host_alias) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, pid.to_string());
}

// ─── 新 master 起動前の残骸 socket 掃除 ───────────────────────────────────────

/// 掃除 ssh の上限。素の TCP 接続 → 1 コマンド実行 → 切断で健全なら数百 ms〜数秒。
const CLEANUP_SSH_TIMEOUT: Duration = Duration::from_secs(30);

/// リモート側 unix socket forward の残骸を素の ssh で `rm -f` する。
///
/// **新 master を bind する直前**（`ssh -N -f` / `ssh -M -N -f` の前）に呼ぶ想定。
/// リモート sshd の `StreamLocalBindUnlink yes` に頼らず ccc 側から確実に unlink する。
///
/// mux は使わない（master が居ない・畳んだ直後の使用を想定）。以下の `-o` で
/// 「新 master を張らない」「config の RemoteForward を要求しない」ことを保証する:
/// - `ControlMaster=no` + `ControlPath=none`: 相乗り/新設どちらも抑止
/// - `ClearAllForwardings=yes`: `-L`/`-R`/`-D` の要求を出さない
/// - `ExitOnForwardFailure=no`: 掃除 ssh が forward で死なないよう明示的に off
///
/// **`gpg-agent` の kill はしない**（`repair` と異なる）。リモート人間ユーザーや
/// 進行中スクリプトの gpg 使用を巻き込まないため。誤起動 rogue agent が listener を
/// 握ったままでも、新 socket が path に bind されれば以降の connect() は新 socket に
/// 向く（旧 listener は orphan になるだけ）。socket が既に存在しなければ `rm -f` は
/// no-op なので毎回呼んでも安全。
///
/// 失敗しても panic しない — ログに残して呼び出し側（新 master 起動）を続行する。
/// 掃除に失敗しても、その後の `ensure_agent_forward` が世代交代検知 → check → repair
/// で最終的に救う設計。
pub fn cleanup_stale_remote_sockets(host_alias: &str, log: Log) {
    let forwards = match ssh_config::run_ssh_g(host_alias) {
        Ok(out) => parse_socket_remote_forwards(&out),
        Err(e) => {
            log(&format!(
                "[agent-socket] {host_alias}: 残骸掃除の前段 ssh -G 失敗（続行）: {e}"
            ));
            return;
        }
    };
    if forwards.is_empty() {
        return;
    }

    let cmd = format!("rm -f {}", quoted_remote_paths(&forwards));
    let outcome = run_with_timeout(
        Command::new("ssh").args([
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
            host_alias,
            &cmd,
        ]),
        CLEANUP_SSH_TIMEOUT,
    );
    match outcome {
        Ok(o) if o.success() => log(&format!(
            "[agent-socket] {host_alias}: リモート残骸 socket 掃除 OK ({cmd})"
        )),
        Ok(o) if o.timed_out => log(&format!(
            "[agent-socket] {host_alias}: リモート残骸 socket 掃除タイムアウト（続行）"
        )),
        Ok(o) => log(&format!(
            "[agent-socket] {host_alias}: リモート残骸 socket 掃除失敗（続行、code={:?}）: {}",
            o.code,
            o.stderr.trim()
        )),
        Err(e) => log(&format!(
            "[agent-socket] {host_alias}: 掃除 ssh の起動に失敗（続行）: {e}"
        )),
    }
}

// ─── 修復 ────────────────────────────────────────────────────────────────────

/// 残骸掃除 + forward 再要求。master は再起動しない。
fn repair(host_alias: &str, mux_base: &[String], forwards: &[SocketForward], log: Log) {
    // 1. 誤起動した remote gpg-agent を止め、残骸ソケットを削除する
    let paths = quoted_remote_paths(forwards);
    let cleanup = format!("gpgconf --kill gpg-agent 2>/dev/null; rm -f {paths}");
    let rc = remote_exec(host_alias, mux_base, &cleanup);
    log(&format!(
        "[agent-socket] {host_alias}: リモート残骸掃除 rc={rc:?} ({cleanup})"
    ));

    // 2. mux 経由で forward だけ再要求（cancel は失敗してよい: 元々張れていない場合がある）。
    //    -O forward -R はサーバ応答を待つため、half-open では MUX_OP_TIMEOUT で打ち切る。
    for fwd in forwards {
        let spec = fwd.to_r_arg();
        let _ = run_with_timeout(
            Command::new("ssh")
                .args(mux_base)
                .args(["-O", "cancel", "-R", &spec, host_alias]),
            MUX_OP_TIMEOUT,
        );
        let out = run_with_timeout(
            Command::new("ssh")
                .args(mux_base)
                .args(["-O", "forward", "-R", &spec, host_alias]),
            MUX_OP_TIMEOUT,
        );
        match out {
            Ok(o) if o.success() => {
                log(&format!("[agent-socket] {host_alias}: 再要求 OK: {spec}"));
            }
            Ok(o) if o.timed_out => {
                log(&format!(
                    "[agent-socket] {host_alias}: 再要求タイムアウト（master が half-open の可能性）: {spec}"
                ));
            }
            Ok(o) => {
                log(&format!(
                    "[agent-socket] {host_alias}: 再要求失敗: {spec}: {}",
                    o.stderr.trim()
                ));
            }
            Err(e) => {
                log(&format!("[agent-socket] {host_alias}: ssh 起動失敗: {e}"));
            }
        }
    }
}

/// チェック＋修復のエントリポイント。戻り値は「最終的に健全か」
/// （NoGpg / Unreachable / forward 設定なし は「判定不能 = true 扱い」で返す。
/// 呼び出し側が起動をブロックしない設計のため）。
///
/// `force = true` を指定すると世代ゲートをバイパスして必ずリモートチェックを行う。
/// `ccc-ssh heal` のような明示的な再診断要求はこちらを使う。
///
/// config に unix ソケットの RemoteForward が無いホストでは何もしない。
pub fn ensure_agent_forward(host_alias: &str, force: bool, log: Log) -> bool {
    let forwards = match ssh_config::run_ssh_g(host_alias) {
        Ok(out) => parse_socket_remote_forwards(&out),
        Err(e) => {
            log(&format!("[agent-socket] {host_alias}: ssh -G 失敗: {e}"));
            return true;
        }
    };
    if forwards.is_empty() {
        return true;
    }

    let mux_base = match mux_base_args(host_alias) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("[agent-socket] {host_alias}: {e}"));
            return true;
        }
    };

    // 世代ゲート: 現 master の pid が「最後に疎通 OK を確認した pid」と同じなら
    // 何もしない（リモート実行ゼロ）。pid が取れない（master 不在）場合はチェックに
    // 進む — チェック用 ssh が ControlMaster auto で新 master を確立し、その世代の
    // bind 失敗をその場で検知・修復できるため。
    // force=true（heal 等）の場合は明示的バイパス。
    if !force {
        if let Some(pid) = master_pid(host_alias) {
            if read_gate(host_alias) == Some(pid) {
                log(&format!(
                    "[agent-socket] {host_alias}: master 世代不変 (pid={pid})、チェック省略"
                ));
                return true;
            }
        }
    } else {
        log(&format!(
            "[agent-socket] {host_alias}: force=true、世代ゲートをバイパス"
        ));
    }

    let mark_healthy = |log: Log| {
        if let Some(pid) = master_pid(host_alias) {
            write_gate(host_alias, pid);
            log(&format!(
                "[agent-socket] {host_alias}: 疎通 OK を記録 (pid={pid})"
            ));
        }
    };

    match check(host_alias, &mux_base, &forwards) {
        CheckResult::Healthy => {
            log(&format!("[agent-socket] {host_alias}: gpg agent 疎通 OK"));
            mark_healthy(log);
            true
        }
        CheckResult::NoGpg => {
            log(&format!(
                "[agent-socket] {host_alias}: gpg-connect-agent 無し（チェックをスキップ）"
            ));
            true
        }
        CheckResult::Unreachable => {
            log(&format!(
                "[agent-socket] {host_alias}: リモート実行不可（チェックをスキップ）"
            ));
            true
        }
        CheckResult::Broken => {
            log(&format!(
                "[agent-socket] {host_alias}: gpg agent forward 不調 → 自動修復を試行"
            ));
            repair(host_alias, &mux_base, &forwards, log);
            let healthy = check(host_alias, &mux_base, &forwards) == CheckResult::Healthy;
            if healthy {
                mark_healthy(log);
                log(&format!("[agent-socket] {host_alias}: 修復成功"));
            } else {
                log(&format!(
                    "[agent-socket] {host_alias}: 修復失敗（コマンドが gpg を要求する場合は失敗します）"
                ));
            }
            healthy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_only_unix_socket_forwards() {
        let out = "\
hostname 127.0.0.1
remoteforward /home/vscode/.gnupg/S.gpg-agent /Users/me/.gnupg/S.gpg-agent.extra
remoteforward /home/vscode/.gnupg/S.gpg-agent.extra /Users/me/.gnupg/S.gpg-agent.extra
remoteforward 127.0.0.1:8080 [localhost]:8080
streamlocalbindunlink yes
";
        let fwds = parse_socket_remote_forwards(out);
        assert_eq!(fwds.len(), 2);
        assert_eq!(fwds[0].remote_path, "/home/vscode/.gnupg/S.gpg-agent");
        assert_eq!(fwds[0].local_path, "/Users/me/.gnupg/S.gpg-agent.extra");
        assert_eq!(
            fwds[0].to_r_arg(),
            "/home/vscode/.gnupg/S.gpg-agent:/Users/me/.gnupg/S.gpg-agent.extra"
        );
    }

    #[test]
    fn parse_ignores_port_forwards_and_other_lines() {
        let out = "remoteforward 51389 [127.0.0.1]:51389\nlocalforward 3000 [localhost]:3000\n";
        assert!(parse_socket_remote_forwards(out).is_empty());
    }

    fn fwds() -> Vec<SocketForward> {
        vec![
            SocketForward {
                remote_path: "/home/vscode/.gnupg/S.gpg-agent".into(),
                local_path: "/Users/me/.gnupg/S.gpg-agent.extra".into(),
            },
            SocketForward {
                remote_path: "/home/vscode/.gnupg/S.gpg-agent.extra".into(),
                local_path: "/Users/me/.gnupg/S.gpg-agent.extra".into(),
            },
        ]
    }

    #[test]
    fn classify_forbidden_means_restricted_forward_is_healthy() {
        let reply = "ERR 67109115 Forbidden <GPG Agent>\n";
        assert_eq!(classify_agent_reply(reply, &fwds()), CheckResult::Healthy);
    }

    #[test]
    fn classify_remote_owned_socket_is_broken() {
        // リモートで誤起動した gpg-agent は自分の（= リモート側の）パスを名乗る
        let reply = "D /home/vscode/.gnupg/S.gpg-agent\nOK\n";
        assert_eq!(classify_agent_reply(reply, &fwds()), CheckResult::Broken);
    }

    #[test]
    fn classify_local_path_means_plain_forward_is_healthy() {
        // 非 restricted ソケットの forward だとローカル側パスが返る
        let reply = "D /Users/me/.gnupg/S.gpg-agent\nOK\n";
        assert_eq!(classify_agent_reply(reply, &fwds()), CheckResult::Healthy);
    }

    #[test]
    fn classify_unparseable_reply_defaults_to_healthy() {
        assert_eq!(classify_agent_reply("", &fwds()), CheckResult::Healthy);
        assert_eq!(classify_agent_reply("OK\n", &fwds()), CheckResult::Healthy);
    }

    #[test]
    fn shell_single_quote_wraps_plain_string() {
        assert_eq!(shell_single_quote("/tmp/S.gpg-agent"), "'/tmp/S.gpg-agent'");
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quote() {
        // it's → 'it'\''s' （閉じ・エスケープ・再開）
        assert_eq!(shell_single_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn quoted_remote_paths_joins_all() {
        let forwards = vec![
            SocketForward {
                remote_path: "/a".into(),
                local_path: "/x".into(),
            },
            SocketForward {
                remote_path: "/b b".into(),
                local_path: "/y".into(),
            },
        ];
        assert_eq!(quoted_remote_paths(&forwards), "'/a' '/b b'");
    }
}
