//! relay デーモン ↔ uplink の E2E テスト（リモートを使わずローカルで完結）。
//!
//! 本番の構成:
//!
//! ```text
//! gpg → S.gpg-agent → relay daemon → ctl socket → ssh の stdio → uplink → S.gpg-agent.extra
//! ```
//!
//! テストでは ssh を挟まず、uplink 役が ctl socket へ直接繋いで
//! [`mux::run_connector`] を回す（ssh は暗号化パイプでしかないため、
//! 多重化の正しさはこの構成で完全に検証できる）。
//!
//! フェーズ 1 の目的は「リモートを巻き込む前にプロトコルの正しさを確定させる」こと。

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ccc_gpg_relay::daemon::{self, DaemonConfig, DaemonHandle};
use ccc_gpg_relay::mux::{self, MuxConfig, MuxHooks};
use ccc_gpg_relay::protocol::{write_frame, Hello, PROTOCOL_VERSION};

/// テスト内の待ち時間の上限（デッドロックしたテストを固まらせない）。
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// 疑似 gpg-agent。接続すると `OK\n` を返し、以後は受け取ったバイト列をそのまま返す。
struct FakeAgent {
    path: PathBuf,
    connections: Arc<AtomicUsize>,
    _thread: JoinHandle<()>,
}

impl FakeAgent {
    fn start(path: PathBuf) -> FakeAgent {
        let listener = UnixListener::bind(&path).expect("疑似 agent の bind に失敗");
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        let thread = std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { break };
                counter.fetch_add(1, Ordering::Relaxed);
                std::thread::spawn(move || {
                    // 本物の gpg-agent と同じく接続直後に挨拶を返す
                    if conn.write_all(b"OK\n").is_err() {
                        return;
                    }
                    let mut buf = vec![0u8; 8192];
                    loop {
                        match conn.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                if conn.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        });
        FakeAgent {
            path,
            connections,
            _thread: thread,
        }
    }

    fn connection_count(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }
}

/// テスト用の一式（一時ディレクトリ + デーモン）。
struct Harness {
    dir: PathBuf,
    daemon: Option<DaemonHandle>,
    agent: FakeAgent,
    gpg_socket: PathBuf,
    ctl_socket: PathBuf,
}

impl Harness {
    fn start(tag: &str, grace: Duration) -> Harness {
        Harness::start_with(tag, grace, 16)
    }

    fn start_with(tag: &str, grace: Duration, pending_limit: usize) -> Harness {
        // テストが ping・socket 監視で切れないよう、既定では十分長い間隔にする
        Harness::start_full(
            tag,
            grace,
            pending_limit,
            Duration::from_secs(60),
            2,
            Duration::from_secs(60),
        )
    }

    /// socket 所有の監視を短い間隔で回す（横取り検知のテスト用）。
    fn start_watching(tag: &str, watch_interval: Duration) -> Harness {
        Harness::start_full(
            tag,
            Duration::from_secs(5),
            16,
            Duration::from_secs(60),
            2,
            watch_interval,
        )
    }

    fn start_full(
        tag: &str,
        grace: Duration,
        pending_limit: usize,
        ping_interval: Duration,
        ping_miss_limit: u32,
        socket_watch_interval: Duration,
    ) -> Harness {
        // socket パスは sun_path の制限（macOS 104 バイト）を受けるため短く保つ
        let dir = std::env::temp_dir().join(format!("ccc-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let gpg_socket = dir.join("S.agent");
        let ctl_socket = dir.join("ctl");
        let agent = FakeAgent::start(dir.join("S.local"));

        let mut cfg = DaemonConfig::new(gpg_socket.clone(), ctl_socket.clone(), dir.join("lock"));
        cfg.grace = grace;
        cfg.pending_limit = pending_limit;
        cfg.mux.ping_interval = ping_interval;
        cfg.mux.ping_miss_limit = ping_miss_limit;
        cfg.socket_watch_interval = socket_watch_interval;
        let daemon = daemon::start(cfg, daemon::null_log())
            .expect("デーモンの起動に失敗")
            .expect("flock を取得できるはず");

        Harness {
            dir,
            daemon: Some(daemon),
            agent,
            gpg_socket,
            ctl_socket,
        }
    }

    /// uplink 役を起動する（ctl socket に繋いで connector を回す）。
    fn connect_uplink(&self) -> UplinkHandle {
        let stream = UnixStream::connect(&self.ctl_socket).expect("ctl socket への接続に失敗");
        let reader = stream.try_clone().unwrap();
        let writer = stream.try_clone().unwrap();
        let target = self.agent.path.clone();
        let cfg = MuxConfig {
            // テストが ping で切れないよう十分長くする
            ping_interval: Duration::from_secs(60),
            ..MuxConfig::default()
        };
        // uplink 役の割り込みは ctl socket の shutdown（本番では ssh の kill）
        let interrupt_stream = stream.try_clone().unwrap();
        let interrupt: mux::Interrupt = Arc::new(move || {
            let _ = interrupt_stream.shutdown(Shutdown::Both);
        });
        let thread = std::thread::spawn(move || {
            let _ = mux::run_connector(reader, writer, &target, cfg, MuxHooks::new(interrupt));
        });
        UplinkHandle {
            stream,
            thread: Some(thread),
        }
    }

    /// uplink が採用されるまで待つ（採用は非同期に行われるため）。
    fn wait_uplink(&self, expected: bool) {
        let deadline = Instant::now() + IO_TIMEOUT;
        while Instant::now() < deadline {
            if self.daemon.as_ref().unwrap().has_uplink() == expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("uplink の接続状態が {expected} になりませんでした");
    }

    /// gpg クライアント役として接続する。
    fn connect_client(&self) -> UnixStream {
        let stream = UnixStream::connect(&self.gpg_socket).expect("gpg socket への接続に失敗");
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        stream
    }

    fn stats(&self) -> (u64, u64) {
        self.daemon.as_ref().unwrap().stats()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.stop();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct UplinkHandle {
    stream: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl UplinkHandle {
    /// uplink を切断する（ssh が落ちた状況を模す）。
    fn disconnect(mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 条件が満たされるまで待つ（並行に進む状態変化を観測するため）。
fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + IO_TIMEOUT;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("条件が満たされませんでした: {what}");
}

/// 挨拶（`OK\n`）を読み飛ばす。
fn read_greeting(stream: &mut UnixStream) {
    let mut buf = [0u8; 3];
    stream
        .read_exact(&mut buf)
        .expect("挨拶が届きませんでした（チャネルが確立していない）");
    assert_eq!(&buf, b"OK\n");
}

fn roundtrip(stream: &mut UnixStream, payload: &[u8]) -> Vec<u8> {
    stream.write_all(payload).expect("送信に失敗");
    let mut out = vec![0u8; payload.len()];
    stream.read_exact(&mut out).expect("応答の受信に失敗");
    out
}

// ─── テスト ──────────────────────────────────────────────────────────────────

#[test]
fn client_reaches_agent_through_relay_and_uplink() {
    let h = Harness::start("basic", Duration::from_secs(5));
    let uplink = h.connect_uplink();
    h.wait_uplink(true);

    let mut client = h.connect_client();
    read_greeting(&mut client);
    assert_eq!(
        roundtrip(&mut client, b"GETINFO version\n"),
        b"GETINFO version\n"
    );

    assert_eq!(h.agent.connection_count(), 1, "agent 側にも 1 本繋がる");
    assert_eq!(h.stats().0, 1, "受理は 1 件");
    uplink.disconnect();
}

#[test]
fn multiple_channels_do_not_cross_talk() {
    let h = Harness::start("multi", Duration::from_secs(5));
    let uplink = h.connect_uplink();
    h.wait_uplink(true);

    // gpg / gpgsm / 並列 git のように同時接続する
    let mut clients: Vec<UnixStream> = (0..3).map(|_| h.connect_client()).collect();
    for client in clients.iter_mut() {
        read_greeting(client);
    }
    // 各チャネルに異なるデータを流し、混線しないことを確認する
    for (i, client) in clients.iter_mut().enumerate() {
        let payload = format!("channel-{i}-payload\n");
        assert_eq!(
            roundtrip(client, payload.as_bytes()),
            payload.as_bytes(),
            "チャネル {i} の応答が混線している"
        );
    }
    assert_eq!(h.agent.connection_count(), 3);
    uplink.disconnect();
}

#[test]
fn large_payload_is_reassembled_across_frames() {
    let h = Harness::start("large", Duration::from_secs(5));
    let uplink = h.connect_uplink();
    h.wait_uplink(true);

    let mut client = h.connect_client();
    read_greeting(&mut client);
    // DATA_CHUNK（32 KiB）を跨いで分割・再構成されること
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let writer_payload = payload.clone();
    let mut write_half = client.try_clone().unwrap();
    // 相手が echo し返すため、書き切る前に読まないと双方が詰まる
    let writer = std::thread::spawn(move || {
        write_half.write_all(&writer_payload).unwrap();
    });
    let mut out = vec![0u8; payload.len()];
    client.read_exact(&mut out).expect("大きな応答の受信に失敗");
    writer.join().unwrap();
    assert_eq!(out, payload);
    uplink.disconnect();
}

#[test]
fn connection_waits_for_uplink_within_grace() {
    let h = Harness::start("grace-ok", Duration::from_secs(5));
    // uplink がまだ居ない状態でクライアントが来る（網断中の gpg 操作）
    let mut client = h.connect_client();

    // grace の内に uplink が復帰すれば、待たされていた接続がそのまま繋がる
    std::thread::sleep(Duration::from_millis(300));
    let uplink = h.connect_uplink();
    h.wait_uplink(true);

    read_greeting(&mut client);
    assert_eq!(roundtrip(&mut client, b"late\n"), b"late\n");
    assert_eq!(h.stats(), (1, 0), "受理 1 件・破棄 0 件");
    uplink.disconnect();
}

#[test]
fn connection_is_closed_after_grace_expires() {
    let h = Harness::start("grace-expire", Duration::from_millis(300));
    let mut client = h.connect_client();

    // uplink が復帰しないまま grace を過ぎると閉じられる（固まらせずに失敗させる）
    let mut buf = [0u8; 1];
    let result = client.read(&mut buf);
    match result {
        Ok(0) => {}
        Err(e) => panic!("EOF ではなくエラーが返りました: {e}"),
        Ok(n) => panic!("閉じられるはずが {n} バイト読めました"),
    }
    wait_for(|| h.stats() == (0, 1), "受理 0 件・破棄 1 件");
}

#[test]
fn pending_queue_has_an_upper_bound() {
    let h = Harness::start_with("pending-limit", Duration::from_secs(5), 2);
    // 上限 2 に対して 4 本繋ぐ。溢れた分は即座に閉じられる
    let mut clients: Vec<UnixStream> = (0..4).map(|_| h.connect_client()).collect();
    wait_for(|| h.stats().1 == 2, "上限を超えた 2 本が破棄される");

    // 溢れた側（accept は接続順なので後の 2 本）は EOF になる。
    // 閉じられた socket への setsockopt は EINVAL になり得るため、
    // タイムアウトは connect_client で設定済みのものをそのまま使う
    let mut buf = [0u8; 1];
    let last = clients.last_mut().unwrap();
    assert!(matches!(last.read(&mut buf), Ok(0)));
}

#[test]
fn newer_uplink_wins_and_old_one_is_dropped() {
    let h = Harness::start("last-wins", Duration::from_secs(5));
    let first = h.connect_uplink();
    h.wait_uplink(true);

    // 別マシンの ccc から繋がれた、あるいはローカルが切断と判断して張り直した状況。
    // 古い方は half-open かもしれず判別できないため、迷わず後勝ちにする
    let second = h.connect_uplink();
    // 置き換わりは has_uplink（常に true のまま）では観測できないので世代で待つ。
    // ここを待たずに旧 uplink を切ると、まだ current が旧世代のうちに
    // クライアント接続が死んだセッションへ振られる
    wait_for(
        || h.daemon.as_ref().unwrap().uplink_generation() == 2,
        "2 つ目の uplink が採用される",
    );

    // 旧セッションは畳まれている（run_connector が戻る）
    first.disconnect();

    // 新しい uplink で通信が成立する
    let mut client = h.connect_client();
    read_greeting(&mut client);
    assert_eq!(roundtrip(&mut client, b"after-swap\n"), b"after-swap\n");
    second.disconnect();
}

#[test]
fn socket_file_survives_uplink_disconnect() {
    // v0.14 の中心的な不変条件:
    // socket ファイルの寿命は ssh 接続の生死から独立している
    let h = Harness::start("survive", Duration::from_millis(300));
    let uplink = h.connect_uplink();
    h.wait_uplink(true);
    assert!(h.gpg_socket.exists());

    uplink.disconnect();
    h.wait_uplink(false);

    assert!(
        h.gpg_socket.exists(),
        "uplink が切れても socket ファイルは relay が保持し続ける"
    );

    // 再接続すれば何事もなく通る（gpg 側から見れば一時的な失敗だけで済む）
    let uplink = h.connect_uplink();
    h.wait_uplink(true);
    let mut client = h.connect_client();
    read_greeting(&mut client);
    assert_eq!(roundtrip(&mut client, b"reconnected\n"), b"reconnected\n");
    uplink.disconnect();
}

#[test]
fn client_half_close_still_delivers_pending_response() {
    let h = Harness::start("half-close", Duration::from_secs(5));
    let uplink = h.connect_uplink();
    h.wait_uplink(true);

    let mut client = h.connect_client();
    read_greeting(&mut client);
    client.write_all(b"BYE\n").unwrap();
    // 「もう送らない」だけを伝える。応答はまだ読む
    client.shutdown(Shutdown::Write).unwrap();

    let mut out = [0u8; 4];
    client
        .read_exact(&mut out)
        .expect("half close 後も応答が届くこと");
    assert_eq!(&out, b"BYE\n");
    uplink.disconnect();
}

#[test]
fn daemon_stop_unlinks_sockets() {
    let dir = std::env::temp_dir().join(format!("ccc-e2e-unlink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let gpg_socket = dir.join("S.agent");
    let ctl_socket = dir.join("ctl");
    let cfg = DaemonConfig::new(gpg_socket.clone(), ctl_socket.clone(), dir.join("lock"));
    let handle = daemon::start(cfg, daemon::null_log()).unwrap().unwrap();
    assert!(gpg_socket.exists() && ctl_socket.exists());

    handle.stop();
    assert!(!gpg_socket.exists(), "停止時に gpg socket を unlink する");
    assert!(!ctl_socket.exists(), "停止時に ctl socket を unlink する");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn second_daemon_defers_to_the_running_one() {
    let dir = std::env::temp_dir().join(format!("ccc-e2e-single-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let lock_path = dir.join("lock");
    let cfg = DaemonConfig::new(dir.join("S.agent"), dir.join("ctl"), lock_path.clone());

    let first = daemon::start(cfg.clone(), daemon::null_log()).unwrap();
    assert!(first.is_some(), "1 つ目は起動する");

    // 2 つ目は flock を取れず None（`--ensure-daemon` の冪等性の根拠）
    let second = daemon::start(cfg.clone(), daemon::null_log()).unwrap();
    assert!(second.is_none(), "2 つ目は起動を譲る");

    first.unwrap().stop();
    // 解放されたので改めて起動できる
    let third = daemon::start(cfg, daemon::null_log()).unwrap();
    assert!(third.is_some(), "解放後は起動できる");
    third.unwrap().stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// uplink 側の `S.gpg-agent.extra` が居ない場合、そのチャネルだけが失敗する。
#[test]
fn missing_local_agent_fails_only_that_channel() {
    let h = Harness::start("no-agent", Duration::from_secs(5));
    // 疑似 agent の socket を消して「ローカル gpg-agent が死んでいる」状況を作る
    let _ = std::fs::remove_file(&h.agent.path);
    let uplink = h.connect_uplink();
    h.wait_uplink(true);

    let mut client = h.connect_client();
    let mut buf = [0u8; 1];
    // connect に失敗したチャネルは閉じられる（セッションは維持される）
    assert!(matches!(client.read(&mut buf), Ok(0)));
    assert!(
        h.daemon.as_ref().unwrap().has_uplink(),
        "1 チャネルの失敗でセッションは落ちない"
    );
    uplink.disconnect();
}

/// 実バイナリの `--uplink` ブリッジを経路に含めた検証。
///
/// 本番との違いは ssh を挟むかどうかだけ（ssh は暗号化パイプでしかない）。
/// 子プロセスの stdio 越しに多重化が成立することを確認する。
#[test]
fn bridge_binary_relays_stdio_to_ctl_socket() {
    use std::process::{Command, Stdio};

    let h = Harness::start("bridge-bin", Duration::from_secs(5));
    let mut child = Command::new(env!("CARGO_BIN_EXE_ccc-gpg-relay"))
        .arg("--uplink")
        .arg("--ctl-socket")
        .arg(&h.ctl_socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("ccc-gpg-relay --uplink を起動できません");

    let reader = child.stdout.take().unwrap();
    let writer = child.stdin.take().unwrap();
    let target = h.agent.path.clone();
    let cfg = MuxConfig {
        ping_interval: Duration::from_secs(60),
        ..MuxConfig::default()
    };
    let connector = std::thread::spawn(move || {
        let _ = mux::run_connector(
            reader,
            writer,
            &target,
            cfg,
            MuxHooks::new(mux::no_interrupt()),
        );
    });
    h.wait_uplink(true);

    let mut client = h.connect_client();
    read_greeting(&mut client);
    assert_eq!(
        roundtrip(&mut client, b"through-the-bridge\n"),
        b"through-the-bridge\n"
    );

    // ssh が落ちた状況に相当（uplink 側は子プロセスの kill で解除する）
    let _ = child.kill();
    let _ = child.wait();
    let _ = connector.join();
    h.wait_uplink(false);
    assert!(
        h.gpg_socket.exists(),
        "ブリッジが落ちても socket は relay が保持し続ける"
    );
}

/// gpg socket を横取りされたら relay は自ら終了する（specs/v0.14 §11.2 の層 3）。
///
/// 素の `ssh` の `RemoteForward` が誤って発動した場合を模す。クライアント側・
/// サーバ側どちらの設定にも依存しない最後の砦。
#[test]
fn stolen_socket_aborts_the_daemon() {
    let h = Harness::start_watching("stolen", Duration::from_millis(200));
    let uplink = h.connect_uplink();
    h.wait_uplink(true);

    // 別プロセス（sshd 等）が同じパスに自分の socket を bind した状況
    let _ = std::fs::remove_file(&h.gpg_socket);
    let thief = UnixListener::bind(&h.gpg_socket).expect("横取り側の bind に失敗");

    wait_for(
        || h.daemon.as_ref().unwrap().aborted(),
        "所有権の喪失を検知して自ら停止する",
    );

    // 横取りした socket は生きたまま（relay が他人の socket を消していない）
    drop(thief);
    uplink.disconnect();
}

/// 所有権を失った後の Drop は他人の socket を消さない。
///
/// 無条件に unlink すると、specs/v0.14 §1 欠陥 3 で sshd がやっているのと
/// 同じ「他人の socket をパスで消す」事故を自分で起こす。
#[test]
fn drop_does_not_unlink_a_stolen_socket() {
    let dir = std::env::temp_dir().join(format!("ccc-e2e-nosteal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let gpg_socket = dir.join("S.agent");
    let cfg = DaemonConfig::new(gpg_socket.clone(), dir.join("ctl"), dir.join("lock"));
    let handle = daemon::start(cfg, daemon::null_log()).unwrap().unwrap();

    // 横取り
    let _ = std::fs::remove_file(&gpg_socket);
    let _thief = UnixListener::bind(&gpg_socket).unwrap();
    let stolen_inode = std::fs::metadata(&gpg_socket).unwrap().ino();

    handle.stop();

    assert!(
        gpg_socket.exists(),
        "奪われた socket を Drop で消してはいけない"
    );
    assert_eq!(
        std::fs::metadata(&gpg_socket).unwrap().ino(),
        stolen_inode,
        "横取りした側の socket がそのまま残る"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// HELLO だけ返して以後沈黙する uplink を作る（half-open の TCP を模す）。
fn connect_silent_uplink(ctl_socket: &Path, role: &str, version: u32) -> UnixStream {
    let mut stream = UnixStream::connect(ctl_socket).expect("ctl socket への接続に失敗");
    let hello = Hello {
        version,
        role: role.to_string(),
        socket_path: String::new(),
        grace_secs: 0,
    };
    write_frame(&mut stream, &hello.to_frame()).expect("HELLO の送信に失敗");
    stream
}

/// ping に応答しない uplink はセッションごと畳まれる。
///
/// **これが v0.14 で「壊れたことに気付けない」を解決する仕組み**（specs/v0.14 §6）。
/// ssh の `ServerAliveInterval` に頼らず、アプリ層で half-open を検知する。
#[test]
fn silent_uplink_is_dropped_by_ping_timeout() {
    let h = Harness::start_full(
        "ping-timeout",
        Duration::from_secs(5),
        16,
        Duration::from_millis(200),
        1,
        Duration::from_secs(60),
    );
    let _silent = connect_silent_uplink(&h.ctl_socket, "uplink", PROTOCOL_VERSION);
    h.wait_uplink(true);

    // interval * (miss_limit + 1) = 400ms を過ぎても PONG が返らないので切断される
    wait_for(
        || !h.daemon.as_ref().unwrap().has_uplink(),
        "ping 無応答の uplink が切断される",
    );

    // socket ファイルは残っており、繋ぎ直せば復旧する
    assert!(h.gpg_socket.exists());
    let uplink = h.connect_uplink();
    h.wait_uplink(true);
    let mut client = h.connect_client();
    read_greeting(&mut client);
    uplink.disconnect();
}

/// プロトコル版が違う uplink は採用されない（両側の ccc 更新を促す）。
#[test]
fn protocol_version_mismatch_is_rejected() {
    let h = Harness::start("version", Duration::from_secs(5));
    let _bad = connect_silent_uplink(&h.ctl_socket, "uplink", PROTOCOL_VERSION + 1);
    wait_for(
        || !h.daemon.as_ref().unwrap().has_uplink(),
        "版が合わない uplink は切断される",
    );
}

/// 両側が同じ役割を名乗ったら（設定ミス）セッションを張らない。
#[test]
fn role_collision_is_rejected() {
    let h = Harness::start("role", Duration::from_secs(5));
    let _bad = connect_silent_uplink(&h.ctl_socket, "relay", PROTOCOL_VERSION);
    wait_for(
        || !h.daemon.as_ref().unwrap().has_uplink(),
        "役割が衝突する接続は切断される",
    );
}
