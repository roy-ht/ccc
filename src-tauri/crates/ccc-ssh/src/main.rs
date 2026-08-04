//! ccc-ssh: ssh ラッパー CLI。
//!
//! - サブコマンド（`fwd` / `down` / `heal`）以外はすべて素の ssh へ透過する。
//!   透過前に pre-connect フック（gpg agent forward の世代ゲート付きチェック＋
//!   port forward 台帳のリプレイ）を実行する
//! - 素の ssh と同じ config・ControlMaster・ソケットに相乗りするだけなので、
//!   素の ssh と併用しても安全（台帳に載せた forward の削除だけは ccc 側で行う）

use std::io::{IsTerminal, Write};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use ccc_sshkit::liveness::{self, MasterLiveness};
use ccc_sshkit::{agent_socket, forwards, ssh_config};

/// 親（ccc-ssh 自身）が受けたシグナルを子（ssh）に転送するために共有する PID。
/// シグナルハンドラから async-signal-safe に読み書きしたいので AtomicI32。
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let exit_code = match args.first().map(String::as_str) {
        Some("fwd") => cmd_fwd(&args[1..]),
        Some("down") => cmd_down(&args[1..]),
        Some("heal") => cmd_heal(&args[1..]),
        Some("--ccc-help") | None => {
            print_help();
            0
        }
        _ => passthrough(&args),
    };
    std::process::exit(exit_code);
}

fn print_help() {
    eprintln!(
        "ccc-ssh: port forward 台帳と gpg agent forward 修復を仕込んだ ssh ラッパー

使い方:
  ccc-ssh <ssh引数...>                  pre-connect フック後に ssh へ透過
  ccc-ssh fwd list <host>               forward 一覧（台帳 + ssh config）
  ccc-ssh fwd add <host> <L:H:P|port>   forward 追加（例: 8080:localhost:80、port のみなら同番転送）
  ccc-ssh fwd rm <host> <listen_port>   ccc 台帳の forward を削除
  ccc-ssh down <host>                   master を安全に終了（-O exit。無応答なら kill まで行う）
  ccc-ssh heal <host>                   強制的にリモート forward socket を張り直す（gpg forward が
                                        「check は通るのに実際は切れている」ケース対応。健全な forward
                                        も一瞬切断されて即再バインドされます）。mux で直らない場合
                                        （master 起動時に bind 失敗した forward）は master を世代交代
                                        （-O stop → 再確立。既存セッションは維持）して張り直します

pre-connect フックは網断で half-open になった master も検知し、自動で畳んで
再確立します（ユーザー ControlMaster 設定時は ssh -N -f で復旧）。

環境変数:
  CCC_SSH_VERBOSE=1                     pre-connect フックのログをすべて表示
  CCC_DEV=1                             台帳を ~/.ccc/dev/forwards/ に切替（ccc dev 版と揃える）"
    );
}

// ─── pre-connect フック付き透過実行 ──────────────────────────────────────────

fn passthrough(args: &[String]) -> i32 {
    // 接続を伴わない呼び出し（-G/-O/-Q/-V）はフック不要。そのまま透過する。
    let no_hook = args
        .iter()
        .any(|a| matches!(a.as_str(), "-G" | "-O" | "-Q" | "-V"));

    if !no_hook {
        if let Some(host) = guess_destination(args) {
            pre_connect_hook(&host);
        }
    }

    // 旧実装は exec で ssh に置き換えていたが、強制切断で DECSET の途中（マウス
    // トラッキング 1000/1002/1003/1006, bracketed paste 2004, FocusIn/Out 1004,
    // カーソル非表示 25 など）で session が切れると、ローカル TTY がその状態のまま
    // 残って `\x1b[<35;23;55M` のようなマウスレポートが画面に垂れ流される。
    // ssh を子プロセスとして待ち、終了後に冪等な無効化シーケンスを書き出す。
    let mut child = match Command::new("ssh").args(args).spawn() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("ccc-ssh: ssh の起動に失敗しました: {err}");
            return 127;
        }
    };

    // SIGTERM/SIGHUP/SIGINT/SIGQUIT を ssh へ転送する。これで `kill <ccc-ssh-pid>`
    // や端末クローズ時にも ssh が先に畳まれて wait() が戻り、TTY 復旧シーケンスが
    // 走る。SIGKILL だけはどうしようもない（カーネル仕様）。
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    install_signal_forwarders();

    let exit_code = match child.wait() {
        Ok(status) => status.code().unwrap_or_else(|| {
            // シグナルで終了した場合は 128 + signum 慣例で返す（情報がなければ 130）。
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    return 128 + sig;
                }
            }
            130
        }),
        Err(err) => {
            eprintln!("ccc-ssh: ssh の終了待ちに失敗しました: {err}");
            1
        }
    };

    restore_terminal_modes();
    exit_code
}

/// 親が受けた終端系シグナルを ssh に転送するハンドラ。
/// async-signal-safe な操作のみで構成する必要がある（kill(2) は OK）。
extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill(2) is async-signal-safe per POSIX.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

fn install_signal_forwarders() {
    let handler = forward_signal as *const () as libc::sighandler_t;
    // SAFETY: signal(2) を単純なリレー用途で呼ぶだけ。ハンドラは async-signal-safe。
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGQUIT, handler);
    }
}

/// ssh 終了時にローカル TTY のモードを安全側へ戻す。
///
/// リモートが既に rmcup 済みなら冪等な no-op になる。stdout が TTY でない（パイプ／
/// リダイレクト）ときは何もしない（出力にゴミを混ぜないため）。
fn restore_terminal_modes() {
    let mut out = std::io::stdout();
    if !out.is_terminal() {
        return;
    }
    // DECRST 群: X10 (9), X11 mouse (1000), highlight (1001), button-event (1002),
    // any-event (1003), FocusIn/Out (1004), UTF-8 mouse (1005), SGR mouse (1006),
    // urxvt mouse (1015), SGR-Pixels mouse (1016), bracketed paste (2004) を off に、
    // カーソル (25) を表示に戻す。
    const CLEANUP: &[u8] = b"\
        \x1b[?9l\
        \x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\
        \x1b[?1004l\
        \x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\
        \x1b[?2004l\
        \x1b[?25h";
    let _ = out.write_all(CLEANUP);
    let _ = out.flush();
}

fn pre_connect_hook(host: &str) {
    let verbose = matches!(std::env::var("CCC_SSH_VERBOSE").as_deref(), Ok(v) if !v.is_empty());
    let log = move |msg: &str| {
        // 普段は修復・復旧関連のみ表示し、ノイズを抑える
        if verbose
            || msg.contains("不調")
            || msg.contains("修復")
            || msg.contains("再要求")
            || msg.contains("[liveness]")
        {
            eprintln!("ccc-ssh: {msg}");
        }
    };
    preflight_master(host, &log);
    // gpg forward の check → 修復。mux で直らない場合（master 起動時に bind 失敗した
    // config RemoteForward）は、ユーザー CM モードなら master 世代交代（-O stop →
    // ssh -N -f。既存セッションは維持）まで自動で行う。世代ゲート + 再確立
    // クールダウン付きなので、接続のたびに重い処理が連発することはない。
    agent_socket::heal_agent_forward(host, false, true, &log);
    forwards::sync_ledger(host);
}

/// half-open / wedged master の検知と復旧（v0.13）。
///
/// GUI の 60 秒ループは偽陽性対策で 2 サイクル連続の不調を待つが、CLI は
/// 「ユーザーがまさに接続しようとしている」文脈なので単発判定で即復旧する
/// （代替は half-open master への mux で ssh ごと固まること）。全段タイムアウト
/// 付きなので、最悪でもプローブ 15 秒 + 復旧数十秒で必ず ssh 起動に到達する。
fn preflight_master(host: &str, log: &dyn Fn(&str)) {
    let pid = match liveness::probe_master(host, Duration::from_secs(15), log) {
        MasterLiveness::HalfOpen { pid } => {
            log(&format!(
                "[liveness] {host}: master が half-open（網断の残骸）→ 畳んで復旧します"
            ));
            pid
        }
        MasterLiveness::Wedged => agent_socket::last_healthy_pid(host),
        MasterLiveness::NoMaster => {
            // 前世代の master が居た痕跡（last_healthy_pid）があるなら、次に走る
            // 透過 ssh がユーザー config の ControlMaster=auto で新 master を立てる。
            // その bind の前にリモート残骸 socket を ccc から明示的に unlink する
            // （sshd の StreamLocalBindUnlink 頼みの沈黙失敗を回避）。
            // last_healthy_pid が None（初回接続）は掃除不要なのでスキップ。
            if agent_socket::last_healthy_pid(host).is_some() {
                agent_socket::cleanup_stale_remote_sockets(host, log);
            }
            return;
        }
        // Alive / SessionRefused は手を出さない
        _ => return,
    };

    if ssh_config::user_has_control_master(host).unwrap_or(false) {
        // ユーザー CM 尊重モード: 畳み + `ssh -N -f` 再確立まで行う
        // （config の RemoteForward = gpg forward が新 master で復活する）
        liveness::recover_half_open(host, true, log);
    } else {
        // ccc 専用 master（GUI 所有）: hook 逆転送の構成は GUI しか知らないため
        // 畳むだけ。この後の透過 ssh は素の接続として張られ、GUI 側は次の
        // 60 秒サイクルで master を再確立する
        liveness::teardown_master(host, pid, log);
    }
}

/// ssh の引数列から接続先（host alias）を推定する。
/// 確信が持てない場合は None（フックをスキップして透過性を優先する）。
fn guess_destination(args: &[String]) -> Option<String> {
    // 値を取るオプション（ssh(1) の一覧に基づく）
    const VALUE_OPTS: &[char] = &[
        'b', 'B', 'c', 'D', 'E', 'e', 'F', 'I', 'i', 'J', 'L', 'l', 'm', 'O', 'o', 'p', 'Q', 'R',
        'S', 'W', 'w',
    ];
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return iter.next().map(|d| strip_user(d));
        }
        if let Some(rest) = arg.strip_prefix('-') {
            let mut chars = rest.chars();
            let Some(first) = chars.next() else {
                return None; // "-" 単体は不明として諦める
            };
            if VALUE_OPTS.contains(&first) && chars.next().is_none() {
                // "-p 2222" 形式: 次の引数が値
                iter.next();
            }
            // "-p2222" 形式や値なしフラグはそのまま読み飛ばす
            continue;
        }
        if arg.starts_with("ssh://") {
            return None; // URI 形式は対象外（フックなしで透過）
        }
        return Some(strip_user(arg));
    }
    None
}

fn strip_user(dest: &str) -> String {
    dest.rsplit_once('@')
        .map(|(_, host)| host.to_string())
        .unwrap_or_else(|| dest.to_string())
}

// ─── サブコマンド ────────────────────────────────────────────────────────────

fn stderr_log(msg: &str) {
    eprintln!("ccc-ssh: {msg}");
}

fn cmd_fwd(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => {
            let Some(host) = args.get(1) else {
                eprintln!("使い方: ccc-ssh fwd list <host>");
                return 2;
            };
            forwards::sync_ledger(host);
            match forwards::list(host, None) {
                Ok(rows) if rows.is_empty() => {
                    println!("(forward なし)");
                    0
                }
                Ok(rows) => {
                    for r in rows {
                        let listen = format!(
                            "{}:{}",
                            r.spec.listen_host.as_deref().unwrap_or("localhost"),
                            r.spec.listen_port
                        );
                        let arrow = if r.reverse { "<-" } else { "->" };
                        let dest = format!("{}:{}", r.spec.dest_host, r.spec.dest_port);
                        let mut tags = vec![r.origin.clone()];
                        if r.stale {
                            tags.push("失効".into());
                        }
                        println!("{listen:>22} {arrow} {dest:<22} [{}]", tags.join(", "));
                        if let Some(err) = r.error {
                            println!("{:>25}! {err}", "");
                        }
                    }
                    0
                }
                Err(e) => {
                    eprintln!("ccc-ssh: {e}");
                    1
                }
            }
        }
        Some("add") => {
            let (Some(host), Some(spec_str)) = (args.get(1), args.get(2)) else {
                eprintln!("使い方: ccc-ssh fwd add <host> <listen:dest_host:dest_port | port>");
                return 2;
            };
            let Some(spec) = parse_l_spec(spec_str) else {
                eprintln!("ccc-ssh: spec の形式が不正です: {spec_str}（例: 8080:localhost:80）");
                return 2;
            };
            match forwards::add(host, spec) {
                Ok(()) => {
                    println!("追加しました");
                    0
                }
                Err(e) => {
                    eprintln!("ccc-ssh: {e}");
                    1
                }
            }
        }
        Some("rm") => {
            let (Some(host), Some(port_str)) = (args.get(1), args.get(2)) else {
                eprintln!("使い方: ccc-ssh fwd rm <host> <listen_port>");
                return 2;
            };
            let Ok(port) = port_str.parse::<u16>() else {
                eprintln!("ccc-ssh: ポート番号が不正です: {port_str}");
                return 2;
            };
            // remove は listen キーで照合するため dest はダミーでよい
            let spec = forwards::ForwardSpec {
                listen_host: None,
                listen_port: port,
                dest_host: String::new(),
                dest_port: 0,
            };
            match forwards::remove(host, spec) {
                Ok(()) => {
                    println!("削除しました");
                    0
                }
                Err(e) => {
                    eprintln!("ccc-ssh: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("使い方: ccc-ssh fwd <list|add|rm> ...");
            2
        }
    }
}

fn cmd_down(args: &[String]) -> i32 {
    let Some(host) = args.first() else {
        eprintln!("使い方: ccc-ssh down <host>");
        return 2;
    };
    if matches!(
        forwards::master_pid_detailed(host),
        None | Some(forwards::MasterCheck::NotRunning)
    ) {
        eprintln!("ccc-ssh: master が見つかりません");
        return 1;
    }
    // -O exit: master と全接続を畳んでポートも解放する。
    // -O stop はソケットだけ消して master とポートが残る（ゾンビ化）ので提供しない。
    // teardown_master は -O exit 無応答（wedged）時に kill フォールバックまで行う。
    if liveness::teardown_master(host, agent_socket::last_healthy_pid(host), &stderr_log) {
        println!("master を終了しました（次の接続で forward は自動再適用されます）");
        0
    } else {
        eprintln!("ccc-ssh: master が見つからないか、終了に失敗しました");
        1
    }
}

fn cmd_heal(args: &[String]) -> i32 {
    let Some(host) = args.first() else {
        eprintln!("使い方: ccc-ssh heal <host>");
        return 2;
    };
    // heal はユーザーが明示的に「疑わしいから今すぐ張り直せ」と要求する経路。
    //
    // 1. preflight_master: half-open/wedged なら畳んで復旧。NoMaster+痕跡ありなら
    //    後段の透過 ssh 用に残骸 socket を掃除
    // 2. cleanup_stale_remote_sockets: **master が Alive でも**リモート socket を
    //    明示的に unlink する。これにより、check が「Healthy」と誤判定するケース
    //    （例: getinfo がローカル側パスを返し保守的に Healthy 扱いになる）でも、
    //    直後の check が Broken を返し repair の -O cancel/-O forward -R が走って
    //    forward が確実に張り直される。健全な forward も一瞬切断されるが、直後の
    //    repair で数百 ms〜数秒で復元する（gpg 操作中は再試行を求む）
    // 3. heal_agent_forward(force=true, cooldown 無視): check → mux repair →
    //    それでも Broken なら master 世代交代（-O stop → ssh -N -f）→ 再 check。
    //    mux repair は「master 起動時に bind 失敗した config RemoteForward」を
    //    復旧できない（mux の重複検出で再要求が no-op になる）ため、世代交代が
    //    唯一の修復手段になるケースがある
    // 4. sync_ledger: 台帳の port forward をリプレイ
    preflight_master(host, &stderr_log);
    agent_socket::cleanup_stale_remote_sockets(host, &stderr_log);
    let outcome = agent_socket::heal_agent_forward(host, true, false, &stderr_log);
    forwards::sync_ledger(host);
    match outcome {
        agent_socket::HealOutcome::Healthy | agent_socket::HealOutcome::Indeterminate => {
            println!("OK");
            0
        }
        agent_socket::HealOutcome::NeedsMasterRebuild => {
            eprintln!(
                "ccc-ssh: mux では修復できず、ccc 専用 master のため世代交代は GUI が行います。\n\
                 ccc GUI が起動中なら 60 秒以内の監視サイクルで自動復旧します。\n\
                 今すぐ直したい場合は GUI でインスタンスを再接続してください。"
            );
            1
        }
        agent_socket::HealOutcome::Failed => 1,
    }
}

/// `listen[:dest_host]:dest_port` または `port`（同番 localhost 転送）をパースする。
fn parse_l_spec(s: &str) -> Option<forwards::ForwardSpec> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [port] => {
            let p = port.parse().ok()?;
            Some(forwards::ForwardSpec {
                listen_host: None,
                listen_port: p,
                dest_host: "localhost".into(),
                dest_port: p,
            })
        }
        [listen, dest_host, dest_port] => Some(forwards::ForwardSpec {
            listen_host: None,
            listen_port: listen.parse().ok()?,
            dest_host: (*dest_host).to_string(),
            dest_port: dest_port.parse().ok()?,
        }),
        [bind, listen, dest_host, dest_port] => Some(forwards::ForwardSpec {
            listen_host: Some((*bind).to_string()),
            listen_port: listen.parse().ok()?,
            dest_host: (*dest_host).to_string(),
            dest_port: dest_port.parse().ok()?,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn guess_plain_host() {
        assert_eq!(
            guess_destination(&v(&["dev-host"])).as_deref(),
            Some("dev-host")
        );
        assert_eq!(
            guess_destination(&v(&["user@dev-host", "ls"])).as_deref(),
            Some("dev-host")
        );
    }

    #[test]
    fn guess_skips_value_options() {
        assert_eq!(
            guess_destination(&v(&["-p", "2222", "-i", "~/.ssh/key", "dev-host"])).as_deref(),
            Some("dev-host")
        );
        // 結合形式 "-p2222" は値を消費しない
        assert_eq!(
            guess_destination(&v(&["-p2222", "dev-host"])).as_deref(),
            Some("dev-host")
        );
    }

    #[test]
    fn guess_skips_flags_and_handles_dashdash() {
        assert_eq!(
            guess_destination(&v(&["-A", "-t", "--", "dev-host"])).as_deref(),
            Some("dev-host")
        );
        assert_eq!(guess_destination(&v(&["-L", "8080:l:80"])), None);
        assert_eq!(guess_destination(&v(&["ssh://x/"])), None);
    }

    #[test]
    fn parse_l_spec_variants() {
        let s = parse_l_spec("8080").unwrap();
        assert_eq!(
            (s.listen_port, s.dest_host.as_str(), s.dest_port),
            (8080, "localhost", 8080)
        );
        let s = parse_l_spec("8080:db:5432").unwrap();
        assert_eq!(
            (s.listen_port, s.dest_host.as_str(), s.dest_port),
            (8080, "db", 5432)
        );
        let s = parse_l_spec("0.0.0.0:8080:db:5432").unwrap();
        assert_eq!(s.listen_host.as_deref(), Some("0.0.0.0"));
        assert!(parse_l_spec("not:a:valid:spec:x").is_none());
    }
}
