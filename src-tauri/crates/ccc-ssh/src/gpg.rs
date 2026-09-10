//! `ccc-ssh gpg ...` サブコマンドと uplink デーモンの起動口（specs/v0.14）。
//!
//! forward の定義は `~/.ssh/config` ではなく `~/.ccc[/dev]/gpg.json` に持つ。
//! config に定義が無ければ素の `ssh` はそもそも forward を要求できないため、
//! relay の socket が横取りされる余地が構造的に消える。

use std::time::{SystemTime, UNIX_EPOCH};

use ccc_sshkit::gpg_config::{self, GpgConfig};
use ccc_sshkit::gpg_uplink::{self, EnsureOutcome, UplinkPhase};

pub fn print_help() {
    eprintln!(
        "ccc-ssh gpg: gpg agent forward（relay 方式）

  ccc-ssh gpg enable <host>    このホストで relay を有効にする
  ccc-ssh gpg disable <host>   無効にする（uplink が動いていれば停止する）
  ccc-ssh gpg up <host>        uplink を起動する（既に居れば何もしない）
  ccc-ssh gpg down <host>      uplink を停止する
  ccc-ssh gpg status [<host>]  状態を表示する（省略時は有効な全ホスト）
  ccc-ssh gpg list             設定済みホストの一覧
  ccc-ssh gpg doctor <host>    移行漏れ・前提条件を診断する

設定は ~/.ccc/gpg.json（CCC_DEV=1 なら ~/.ccc/dev/gpg.json）。
接続方法（HostName / User / IdentityFile / ProxyJump 等）は ~/.ssh/config を
そのまま使います。uplink は ControlMaster に相乗りせず、config の
RemoteForward もこの接続では要求しません。"
    );
}

pub fn dispatch(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("enable") => with_host(args, "enable", |host| set_enabled(host, true)),
        Some("disable") => with_host(args, "disable", |host| set_enabled(host, false)),
        Some("up") => with_host(args, "up", cmd_up),
        Some("down") => with_host(args, "down", cmd_down),
        Some("status") => cmd_status(args.get(1).map(String::as_str)),
        Some("list") => cmd_list(),
        Some("doctor") => with_host(args, "doctor", cmd_doctor),
        _ => {
            print_help();
            2
        }
    }
}

/// uplink デーモン本体（`ccc-ssh gpg-uplink --daemon <host>`）。
///
/// `ensure_uplink` が `setsid` + stdio を `/dev/null` にして spawn するため、
/// ログはファイルだけが手がかりになる。
pub fn run_uplink_daemon(args: &[String]) -> i32 {
    let (Some(flag), Some(host)) = (args.first(), args.get(1)) else {
        eprintln!("使い方: ccc-ssh gpg-uplink --daemon <host>");
        return 2;
    };
    if flag != "--daemon" {
        eprintln!("使い方: ccc-ssh gpg-uplink --daemon <host>");
        return 2;
    }

    let log_path = match ccc_gpg_relay::paths::run_dir() {
        Ok(dir) => dir.join(format!("uplink-{}.log", sanitize(host))),
        Err(e) => {
            eprintln!("ccc-ssh: ログの出力先を決められません: {e}");
            return 1;
        }
    };
    let log = ccc_gpg_relay::logging::make_log(log_path, false);
    let log_ref: &dyn Fn(&str) = &*log;

    match gpg_uplink::run_daemon(host, &log_ref) {
        Ok(()) => 0,
        Err(e) => {
            log(&format!("[uplink] {host}: 異常終了しました: {e}"));
            eprintln!("ccc-ssh: {e}");
            1
        }
    }
}

// ─── 各サブコマンド ──────────────────────────────────────────────────────────

fn with_host(args: &[String], name: &str, f: impl Fn(&str) -> i32) -> i32 {
    let Some(host) = args.get(1) else {
        eprintln!("使い方: ccc-ssh gpg {name} <host>");
        return 2;
    };
    f(host)
}

fn set_enabled(host: &str, enabled: bool) -> i32 {
    let mut config = match gpg_config::load() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    config.set_enabled(host, enabled);
    if let Err(e) = gpg_config::save(&config) {
        return fail(e);
    }
    if enabled {
        println!("{host}: relay を有効にしました（ccc-ssh gpg up {host} で起動）");
        println!();
        println!("移行時の注意: ~/.ssh/config に gpg 用の RemoteForward が残っていると、");
        println!("素の ssh で接続した瞬間にリモートの socket が上書きされます。削除してください。");
    } else {
        println!("{host}: relay を無効にしました");
        // 動いている uplink は止める（設定と実態を揃える）
        if gpg_uplink::is_running(host) {
            match gpg_uplink::stop_uplink(host, &stderr_log) {
                Ok(true) => println!("{host}: uplink を停止しました"),
                Ok(false) => {}
                Err(e) => eprintln!("ccc-ssh: uplink の停止に失敗しました: {e}"),
            }
        }
    }
    0
}

fn cmd_up(host: &str) -> i32 {
    let launcher = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ccc-ssh: 自身の実行パスを取得できません: {e}");
            return 1;
        }
    };
    match gpg_uplink::ensure_uplink(host, &launcher, &stderr_log) {
        Ok(EnsureOutcome::Disabled) => {
            eprintln!(
                "ccc-ssh: {host} は無効です（ccc-ssh gpg enable {host} で有効にしてください）"
            );
            1
        }
        Ok(EnsureOutcome::AlreadyRunning) => {
            println!("{host}: 既に動作しています");
            print_one_status(host);
            0
        }
        Ok(EnsureOutcome::Started) => {
            println!("{host}: uplink を起動しました");
            print_one_status(host);
            0
        }
        Ok(EnsureOutcome::StartFailed) => {
            eprintln!("ccc-ssh: {host}: uplink の起動を確認できませんでした");
            1
        }
        Err(e) => fail(e),
    }
}

fn cmd_down(host: &str) -> i32 {
    match gpg_uplink::stop_uplink(host, &stderr_log) {
        Ok(true) => 0,
        Ok(false) => {
            println!("{host}: 動作していません");
            0
        }
        Err(e) => fail(e),
    }
}

fn cmd_status(host: Option<&str>) -> i32 {
    if let Some(host) = host {
        print_one_status(host);
        return 0;
    }
    let config = match gpg_config::load() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let hosts = config.enabled_hosts();
    if hosts.is_empty() {
        println!("(有効なホストがありません)");
        return 0;
    }
    for host in hosts {
        print_one_status(&host);
    }
    0
}

fn cmd_list() -> i32 {
    let config: GpgConfig = match gpg_config::load() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    if config.hosts.is_empty() {
        println!("(設定なし)");
        return 0;
    }
    for (alias, _) in config.hosts.iter() {
        let mark = if config.is_enabled(alias) {
            "有効"
        } else {
            "無効"
        };
        let running = if gpg_uplink::is_running(alias) {
            "、uplink 動作中"
        } else {
            ""
        };
        println!("{alias:<24} [{mark}{running}]");
    }
    0
}

fn print_one_status(host: &str) {
    let running = gpg_uplink::is_running(host);
    let Some(state) = gpg_uplink::read_state(host) else {
        let hint = if running {
            "起動直後（状態ファイル未作成）"
        } else {
            "停止中"
        };
        println!("{host}: {hint}");
        return;
    };

    // ロックを持つプロセスが居ないなら、状態ファイルは前回の残骸
    let phase = if running {
        phase_label(state.state)
    } else {
        "停止中（下は前回の記録）"
    };
    println!("{host}: {phase}");
    println!(
        "  世代 {} / 再接続 {} 回",
        state.generation, state.reconnects
    );
    if state.last_ok_epoch > 0 {
        println!("  最終疎通: {}", ago(state.last_ok_epoch));
    }
    println!(
        "  リモート socket: {}",
        state.remote_socket.as_deref().unwrap_or("(relay の既定)")
    );
    println!(
        "  ローカル socket: {} [{}]",
        state.local_socket, state.local_agent
    );
    if let Some(err) = &state.last_error {
        println!("  直近のエラー: {err}");
    }
    if running && !state.is_healthy() {
        println!("  ヒント: ccc-ssh gpg down {host} && ccc-ssh gpg up {host} で張り直せます");
    }
}

// ─── doctor ──────────────────────────────────────────────────────────────────

/// 移行漏れと前提条件を診断する（specs/v0.14 §11.3）。
///
/// リモート側の項目は 1 回の ssh でまとめて取る（往復を増やさない）。
fn cmd_doctor(host: &str) -> i32 {
    println!("{host} の診断:");
    let mut problems = 0;

    // 1. ssh config に古い RemoteForward が残っていないか（**最重要**）
    match gpg_uplink::stale_config_forwards(host) {
        Ok(found) if found.is_empty() => {
            ok("ssh config に unix socket の RemoteForward はありません");
        }
        Ok(found) => {
            problems += 1;
            ng(&format!(
                "ssh config に RemoteForward が残っています（{} 件）",
                found.len()
            ));
            for line in &found {
                println!("         {line}");
            }
            println!("         → 素の ssh で接続した瞬間にリモートの socket が上書きされます。");
            println!("           ~/.ssh/config から削除してください（定義は ~/.ccc/gpg.json が持ちます）。");
        }
        Err(e) => {
            problems += 1;
            ng(&format!("ssh -G を実行できません: {e}"));
        }
    }

    // 2. 設定
    match gpg_config::load() {
        Ok(config) if config.is_enabled(host) => ok("gpg.json で有効になっています"),
        Ok(_) => {
            problems += 1;
            ng(&format!(
                "gpg.json で有効になっていません（ccc-ssh gpg enable {host}）"
            ));
        }
        Err(e) => {
            problems += 1;
            ng(&format!("gpg.json を読めません: {e}"));
        }
    }

    // 3. ローカル gpg-agent
    match gpg_config::default_local_socket() {
        Ok(path) => match std::os::unix::net::UnixStream::connect(&path) {
            Ok(_) => ok(&format!(
                "ローカル gpg-agent に接続できます（{}）",
                path.display()
            )),
            Err(e) => {
                problems += 1;
                ng(&format!(
                    "ローカル gpg-agent に接続できません（{}）: {e}",
                    path.display()
                ));
                println!("         → gpgconf --launch gpg-agent を試してください");
            }
        },
        Err(e) => {
            problems += 1;
            ng(&format!("ローカル socket のパスを解決できません: {e}"));
        }
    }

    // 4. 配信元バイナリ（リモートに relay が無いときに配る元）
    match ccc_sshkit::gpg_deploy::detect_platform(host) {
        Ok(platform) => match ccc_sshkit::gpg_deploy::staged_binary(platform) {
            Ok(path) => ok(&format!(
                "配信用 relay があります（{platform}: {}）",
                path.display()
            )),
            Err(e) => {
                problems += 1;
                ng(&format!("配信用 relay が見つかりません: {e}"));
                println!(
                    "         → ccc.app を再インストールするか、ccc 本体を一度起動してください"
                );
            }
        },
        Err(e) => {
            // リモートに繋がらない場合はここでは判定しない（次項で報告される）
            let _ = e;
        }
    }

    // 5-7. リモート側（1 回の ssh でまとめて取得）
    match probe_remote(host) {
        Ok(remote) => report_remote(&remote),
        Err(e) => {
            problems += 1;
            ng(&format!("リモートを調べられません: {e}"));
        }
    }

    // 7. uplink の状態
    if gpg_uplink::is_running(host) {
        match gpg_uplink::read_state(host) {
            Some(state) if state.is_healthy() => ok("uplink: 疎通しています"),
            Some(state) => {
                problems += 1;
                ng(&format!("uplink: {}", phase_label(state.state)));
                if let Some(err) = state.last_error {
                    println!("         {err}");
                }
            }
            None => warn("uplink: 起動直後（状態ファイル未作成）"),
        }
    } else {
        warn(&format!("uplink: 停止中（ccc-ssh gpg up {host} で起動）"));
    }

    println!();
    if problems == 0 {
        println!("問題は見つかりませんでした。");
        0
    } else {
        println!("{problems} 件の問題が見つかりました。");
        1
    }
}

struct RemoteProbe {
    relay_status: Option<serde_json::Value>,
    sshd_streamlocal: String,
}

/// リモート側の情報を 1 回の ssh でまとめて取る。
fn probe_remote(host: &str) -> anyhow::Result<RemoteProbe> {
    // relay の --status は JSON（version / running / no_autostart を含む）。
    // sshd_config は読めないことがあるので、その場合は unknown を返させる
    const CMD: &str = r#"~/.ccc/bin/ccc-gpg-relay --status 2>/dev/null || echo '{}'
echo '---CCC-SEP---'
grep -iE '^[[:space:]]*AllowStreamLocalForwarding' /etc/ssh/sshd_config 2>/dev/null | tail -1 || true"#;

    let out = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ClearAllForwardings=yes",
            host,
            CMD,
        ])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (status_part, sshd_part) = text
        .split_once("---CCC-SEP---")
        .unwrap_or((text.as_ref(), ""));
    Ok(RemoteProbe {
        relay_status: serde_json::from_str(status_part.trim()).ok(),
        sshd_streamlocal: sshd_part.trim().to_string(),
    })
}

/// リモート側の診断結果を表示する。
///
/// ここで出るのはすべて「注意」（動作は妨げない）なので、問題としては数えない。
/// relay 未配信は次の接続で自動的に解消し、`no_autostart` と
/// `AllowStreamLocalForwarding` は推奨設定の案内にすぎない。
fn report_remote(remote: &RemoteProbe) {
    match &remote.relay_status {
        Some(status) if status.get("version").is_some() => {
            let version = status["version"].as_str().unwrap_or("?");
            let running = status["running"].as_bool().unwrap_or(false);
            if version == ccc_gpg_relay::VERSION {
                ok(&format!(
                    "リモート relay: {version}（{}）",
                    if running { "動作中" } else { "停止中" }
                ));
            } else {
                warn(&format!(
                    "リモート relay のバージョンが異なります（リモート {version} / ローカル {}）",
                    ccc_gpg_relay::VERSION
                ));
                println!("         → 次の接続時に自動で配信されます");
            }

            if status["no_autostart"].as_bool().unwrap_or(false) {
                ok("リモート gpg.conf に no-autostart があります");
            } else {
                warn("リモート gpg.conf に no-autostart がありません");
                println!("         → relay が socket を常時保持するため autostart は不要です。");
                println!("           入れておくと、鍵を持たない gpg-agent が socket を奪う事故を防げます。");
            }
        }
        _ => {
            warn("リモートに relay が未配信です（次の接続時に自動で配信されます）");
        }
    }

    // AllowStreamLocalForwarding（§11.2 の層 2）
    let value = remote
        .sshd_streamlocal
        .split_whitespace()
        .last()
        .unwrap_or("")
        .to_lowercase();
    match value.as_str() {
        "no" => ok("リモート sshd で streamlocal forward が禁止されています（推奨設定）"),
        "" => warn("リモート sshd の AllowStreamLocalForwarding を確認できませんでした"),
        other => {
            warn(&format!(
                "リモート sshd の AllowStreamLocalForwarding が {other} です"
            ));
            println!(
                "         → relay 方式では streamlocal forward を使いません。no にしておくと、"
            );
            println!(
                "           素の ssh の RemoteForward が socket を奪う事故を根本的に防げます。"
            );
        }
    }
}

fn ok(msg: &str) {
    println!("  [OK]   {msg}");
}

fn warn(msg: &str) {
    println!("  [注意] {msg}");
}

fn ng(msg: &str) {
    println!("  [問題] {msg}");
}

fn phase_label(phase: UplinkPhase) -> &'static str {
    match phase {
        UplinkPhase::Connecting => "接続中",
        UplinkPhase::Connected => "疎通しています",
        UplinkPhase::Retrying => "再接続待ち",
        UplinkPhase::AuthFailed => "認証に失敗（BatchMode では復旧しません）",
        UplinkPhase::Stopped => "停止",
    }
}

/// `UplinkState` は表示にしか使わないので、素朴な相対時刻で十分。
fn ago(epoch: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(epoch);
    match diff {
        0..=59 => format!("{diff} 秒前"),
        60..=3599 => format!("{} 分前", diff / 60),
        3600..=86_399 => format!("{} 時間前", diff / 3600),
        _ => format!("{} 日前", diff / 86_400),
    }
}

fn sanitize(alias: &str) -> String {
    alias
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn stderr_log(msg: &str) {
    eprintln!("ccc-ssh: {msg}");
}

fn fail(e: anyhow::Error) -> i32 {
    eprintln!("ccc-ssh: {e}");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_path_separators() {
        assert_eq!(sanitize("host/with\\sep"), "host_with_sep");
        assert_eq!(sanitize("dev-host.example"), "dev-host.example");
    }

    #[test]
    fn ago_switches_units() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(ago(now).ends_with("秒前"));
        assert!(ago(now - 120).ends_with("分前"));
        assert!(ago(now - 7200).ends_with("時間前"));
        assert!(ago(now - 200_000).ends_with("日前"));
    }

    #[test]
    fn future_timestamps_do_not_underflow() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(ago(now + 100), "0 秒前");
    }
}
