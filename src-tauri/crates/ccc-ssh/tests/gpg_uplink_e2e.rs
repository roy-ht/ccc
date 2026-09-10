//! uplink デーモンの統合テスト（実 ssh を使わない）。
//!
//! 本番の経路:
//!
//! ```text
//! gpg → S.gpg-agent → relay daemon → ctl socket → ssh の stdio → uplink → S.gpg-agent.extra
//! ```
//!
//! ここでは `ssh` をモックに差し替え（`CCC_SSH_BIN`）、「最後の引数をローカルで
//! 実行する」だけのスクリプトにする。**ssh は暗号化パイプでしかない**ので、
//! これで uplink デーモン → ブリッジ → relay → チャネル → agent の全経路が
//! そのまま検証できる。
//!
//! `HOME` を一時ディレクトリに差し替えるため、実際の `~/.ccc` は汚さない。
//!
//! **注意**: `ccc-gpg-relay` の *バイナリ* を使うが、`cargo test -p ccc-ssh` は
//! 依存 crate の bin をビルドしない。relay を変更したら
//! `cargo build -p ccc-gpg-relay` を先に走らせること（`cargo test --workspace`
//! なら自動でビルドされる）。

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use ccc_gpg_relay::daemon::{self, DaemonConfig, DaemonHandle};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const HOST: &str = "testhost";

/// 疑似 gpg-agent（接続すると `OK\n`、以後はエコー）。
fn spawn_fake_agent(path: &Path) {
    let listener = UnixListener::bind(path).expect("疑似 agent の bind に失敗");
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { break };
            std::thread::spawn(move || {
                if conn.write_all(b"OK\n").is_err() {
                    return;
                }
                let mut buf = vec![0u8; 8192];
                while let Ok(n) = conn.read(&mut buf) {
                    if n == 0 || conn.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
}

/// ssh のモック。引数の最後（リモートコマンド）をローカルで実行する。
fn write_mock_ssh(path: &Path) {
    let script = r#"#!/bin/sh
# ssh <opts...> <host> <remote command> の最後の引数をローカルで実行する
for a in "$@"; do last="$a"; done
exec sh -c "$last"
"#;
    std::fs::write(path, script).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

struct Env {
    home: PathBuf,
    relay: Option<DaemonHandle>,
    uplink: Option<Child>,
    gpg_socket: PathBuf,
}

impl Env {
    fn setup(tag: &str) -> Env {
        let home =
            std::env::temp_dir().join(format!("ccc-uplink-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".ccc/run")).unwrap();

        // 疑似 gpg-agent（uplink の接続先）
        let agent = home.join("S.local");
        spawn_fake_agent(&agent);

        // gpg.json（forward 定義は ssh config ではなくここに持つ）
        let config = serde_json::json!({
            "schema": 1,
            "hosts": { HOST: { "enabled": true, "local_socket": agent.to_string_lossy() } }
        });
        std::fs::write(
            home.join(".ccc/gpg.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();

        // relay デーモン（本番ではリモート側）。パスは relay の既定と揃える
        let gpg_socket = home.join("S.agent");
        let mut cfg = DaemonConfig::new(
            gpg_socket.clone(),
            home.join(".ccc/run/gpg-relay.sock"),
            home.join(".ccc/run/gpg-relay.lock"),
        );
        cfg.grace = Duration::from_secs(5);
        let relay = daemon::start(cfg, daemon::null_log())
            .expect("relay の起動に失敗")
            .expect("flock を取得できるはず");

        write_mock_ssh(&home.join("mock-ssh"));

        Env {
            home,
            relay: Some(relay),
            uplink: None,
            gpg_socket,
        }
    }

    /// uplink デーモンを子プロセスとして起動する（`ccc-ssh gpg-uplink --daemon`）。
    fn start_uplink(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_ccc-ssh"))
            .args(["gpg-uplink", "--daemon", HOST])
            .env("HOME", &self.home)
            .env("CCC_SSH_BIN", self.home.join("mock-ssh"))
            .env("CCC_GPG_REMOTE_BIN", relay_bin())
            .env_remove("CCC_DEV")
            .env_remove("GNUPGHOME")
            .spawn()
            .expect("uplink デーモンを起動できません");
        self.uplink = Some(child);
    }

    fn wait_uplink_connected(&self) {
        let deadline = Instant::now() + IO_TIMEOUT;
        while Instant::now() < deadline {
            if self.relay.as_ref().unwrap().has_uplink() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "uplink が接続しませんでした（ログ: {}）",
            self.uplink_log().unwrap_or_default()
        );
    }

    fn uplink_log(&self) -> Option<String> {
        std::fs::read_to_string(
            self.home
                .join(".ccc/run")
                .join(format!("uplink-{HOST}.log")),
        )
        .ok()
    }

    fn state_json(&self) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(
            self.home
                .join(".ccc/forwards")
                .join(format!("{HOST}.uplink.json")),
        )
        .ok()?;
        serde_json::from_str(&text).ok()
    }

    fn connect_client(&self) -> UnixStream {
        let stream = UnixStream::connect(&self.gpg_socket).expect("gpg socket への接続に失敗");
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        stream
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(mut child) = self.uplink.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(relay) = self.relay.take() {
            relay.stop();
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// `ccc-gpg-relay` の実行パス（`ccc-ssh` のテストバイナリと同じ target ディレクトリ）。
fn relay_bin() -> PathBuf {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_ccc-ssh"));
    exe.parent().unwrap().join("ccc-gpg-relay")
}

fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + IO_TIMEOUT;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("条件が満たされませんでした: {what}");
}

#[test]
fn uplink_daemon_connects_and_relays_gpg_traffic() {
    if !relay_bin().exists() {
        eprintln!("ccc-gpg-relay が未ビルドのためスキップします");
        return;
    }
    let mut env = Env::setup("basic");
    env.start_uplink();
    env.wait_uplink_connected();

    // gpg クライアント役 → relay → uplink → 疑似 agent の往復
    let mut client = env.connect_client();
    let mut greeting = [0u8; 3];
    client
        .read_exact(&mut greeting)
        .expect("挨拶が届きませんでした");
    assert_eq!(&greeting, b"OK\n");

    client.write_all(b"GETINFO version\n").unwrap();
    let mut echo = vec![0u8; 16];
    client.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"GETINFO version\n");

    // 状態ファイルが connected を示すこと（GUI/CLI はこれを読むだけでよい）
    wait_for(
        || {
            env.state_json()
                .and_then(|v| v["state"].as_str().map(|s| s == "connected"))
                .unwrap_or(false)
        },
        "状態ファイルが connected になる",
    );
    let state = env.state_json().unwrap();
    assert_eq!(state["local_agent"], "ok");
    assert_eq!(state["schema"], 1);
    assert!(state["last_ok_epoch"].as_u64().unwrap() > 0);
}

#[test]
fn second_daemon_defers_to_the_running_one() {
    if !relay_bin().exists() {
        eprintln!("ccc-gpg-relay が未ビルドのためスキップします");
        return;
    }
    let mut env = Env::setup("single");
    env.start_uplink();
    env.wait_uplink_connected();

    // 2 つ目は flock を取れず即座に正常終了する（ensure_uplink の冪等性の根拠）
    let output = Command::new(env!("CARGO_BIN_EXE_ccc-ssh"))
        .args(["gpg-uplink", "--daemon", HOST])
        .env("HOME", &env.home)
        .env("CCC_SSH_BIN", env.home.join("mock-ssh"))
        .env("CCC_GPG_REMOTE_BIN", relay_bin())
        .env_remove("CCC_DEV")
        .output()
        .expect("2 つ目の起動に失敗");
    assert!(output.status.success(), "2 つ目は正常終了する");

    // 1 つ目は生きたまま
    assert!(env.relay.as_ref().unwrap().has_uplink());
}

#[test]
fn uplink_reconnects_after_the_bridge_dies() {
    if !relay_bin().exists() {
        eprintln!("ccc-gpg-relay が未ビルドのためスキップします");
        return;
    }
    let mut env = Env::setup("reconnect");
    env.start_uplink();
    env.wait_uplink_connected();
    let first_generation = env.state_json().unwrap()["generation"].as_u64().unwrap();

    // relay 側から uplink を切る（網断・後勝ちに相当）
    env.relay.as_ref().unwrap().drop_uplink();
    wait_for(
        || !env.relay.as_ref().unwrap().has_uplink(),
        "uplink が切断される",
    );

    // バックオフ後に自力で戻ってくる
    env.wait_uplink_connected();
    wait_for(
        || {
            env.state_json()
                .and_then(|v| v["generation"].as_u64())
                .map(|g| g > first_generation)
                .unwrap_or(false)
        },
        "世代が進んで再接続する",
    );

    // 再接続後も疎通する（socket は relay が保持し続けている）
    let mut client = env.connect_client();
    let mut greeting = [0u8; 3];
    client
        .read_exact(&mut greeting)
        .expect("再接続後に挨拶が届きませんでした");
    assert_eq!(&greeting, b"OK\n");
}

#[test]
fn disabled_host_is_not_started() {
    let env = Env::setup("disabled");
    // enabled を落とす
    std::fs::write(
        env.home.join(".ccc/gpg.json"),
        r#"{"schema":1,"hosts":{"testhost":{"enabled":false}}}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ccc-ssh"))
        .args(["gpg", "up", HOST])
        .env("HOME", &env.home)
        .env_remove("CCC_DEV")
        .output()
        .expect("gpg up の実行に失敗");
    assert!(!output.status.success(), "無効なホストは起動しない");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("無効"), "理由を伝える: {stderr}");
}
