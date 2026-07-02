use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

use super::consts::{
    find_clear_sequence, find_marker, tmux_death_watch_cmd, tmux_new_shell_group_cmd,
    tmux_new_shell_window_cmd, DEFAULT_PTY_COLS, DEFAULT_PTY_ROWS, TMUX, TMUX_SET_OPTS_SUFFIX,
};

/// hook 通信用の環境変数セット。tmux `new-session -e` で claude code に注入される。
///
/// `CCC_HOOK_ENDPOINT` / `CCC_SESSION_TOKEN` は ccc 再起動で値が変わるため、
/// tmux 焼き込みではなく `~/.ccc/bin/ccc-hook.sh` のラッパースクリプト経由で
/// 注入する。ここで焼き込むのは「instance ごとに固定」される `CCC_INSTANCE_ID`
/// のみで、ccc 再起動を跨いでも変わらず安全に再利用できる。
#[derive(Debug, Clone, Default)]
pub struct HookEnv<'a> {
    pub instance_id: Option<&'a str>,
}

/// PTY 上でプロセスを起動し、tmux 経由で I/O を行う共通インスタンス。
/// ローカル ($SHELL -l) もリモート (ssh -t alias) も同じ構造。
pub struct PtyInstance {
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
}

/// `start_pty` の設定一式。
struct PtyStartConfig<'a> {
    program: &'a str,
    args: &'a [String],
    setup_cmds: Vec<(String, String)>,
    attach_cmd: String,
    agent_cmd: Option<String>,
    output_tx: mpsc::Sender<Vec<u8>>,
    ready_tx: Option<oneshot::Sender<Result<(), String>>>,
    log_path: Option<std::path::PathBuf>,
}

impl PtyInstance {
    // ─── ローカル ─────────────────────────────────────────────────────────────

    /// 新しいローカルインスタンスを起動する。
    /// `claude_config_dir` は tmux の `-e CLAUDE_CONFIG_DIR=...` で session の env に注入される。
    /// `hook_env` の3変数は ccc-claude-code-hook が ccc 受信サーバへ通信するために必要。
    pub fn spawn_local(
        command: &str,
        directory: &str,
        tmux_session: &str,
        output_tx: mpsc::Sender<Vec<u8>>,
        claude_config_dir: Option<&str>,
        hook_env: &HookEnv<'_>,
    ) -> anyhow::Result<Self> {
        let escaped_dir = directory.replace('\'', "'\\''");
        let env_flag = build_tmux_env_flags(&[
            ("CLAUDE_CONFIG_DIR", claude_config_dir),
            ("CCC_INSTANCE_ID", hook_env.instance_id),
        ]);

        let mut setup_cmds: Vec<(String, String)> = vec![
            ("new-session".into(), format!(
                " {TMUX} new-session -d -s '{tmux_session}'{env_flag} -c '{escaped_dir}' -x {DEFAULT_PTY_COLS} -y {DEFAULT_PTY_ROWS} -- \"$SHELL\" -l; echo __CCC__$?\n",
            )),
            ("set-options".into(), format!(
                " {TMUX} {TMUX_SET_OPTS_SUFFIX}; echo __CCC__$?\n",
            )),
        ];
        if let Some(iid) = hook_env.instance_id {
            setup_cmds.push((
                "death-watch".into(),
                tmux_death_watch_cmd(tmux_session, iid),
            ));
        }

        let attach_cmd = format!(" exec {TMUX} attach-session -t '{tmux_session}'\n");
        let agent_cmd = Some(format!("exec {command}\r"));

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Self::start_pty(PtyStartConfig {
            program: &shell,
            args: &["-l".into()],
            setup_cmds,
            attach_cmd,
            agent_cmd,
            output_tx,
            ready_tx: None,
            log_path: None,
        })
    }

    /// 既存のローカル tmux セッションに再アタッチする（復元時に使用）。
    pub fn reattach_local(
        tmux_session: &str,
        output_tx: mpsc::Sender<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let attach_cmd = format!(" exec {TMUX} attach-session -t '{tmux_session}'\n");

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Self::start_pty(PtyStartConfig {
            program: &shell,
            args: &["-l".into()],
            setup_cmds: Vec::new(),
            attach_cmd,
            agent_cmd: None,
            output_tx,
            ready_tx: None,
            log_path: None,
        })
    }

    // ─── リモート ─────────────────────────────────────────────────────────────

    /// 新しいリモートインスタンスを起動する。
    /// ready_rx で Phase 1 完了（= SSH接続成功）を通知する。
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_remote(
        ssh_args: &[String],
        command: &str,
        directory: Option<&str>,
        tmux_session: &str,
        output_tx: mpsc::Sender<Vec<u8>>,
        claude_config_dir: Option<&str>,
        hook_env: &HookEnv<'_>,
        log_path: Option<std::path::PathBuf>,
    ) -> anyhow::Result<(Self, oneshot::Receiver<Result<(), String>>)> {
        let dir_flag = match directory {
            Some(dir) if !dir.is_empty() => {
                let escaped_dir = dir.replace('\'', "'\\''");
                format!(" -c '{escaped_dir}'")
            }
            _ => String::new(),
        };

        let env_flag = build_tmux_env_flags(&[
            ("CLAUDE_CONFIG_DIR", claude_config_dir),
            ("CCC_INSTANCE_ID", hook_env.instance_id),
        ]);

        let mut setup_cmds: Vec<(String, String)> = vec![
            ("new-session".into(), format!(
                " {TMUX} new-session -d -s '{tmux_session}'{env_flag}{dir_flag} -x {DEFAULT_PTY_COLS} -y {DEFAULT_PTY_ROWS} -- \"$SHELL\" -l; echo __CCC__$?\n",
            )),
            ("set-options".into(), format!(
                " {TMUX} {TMUX_SET_OPTS_SUFFIX}; echo __CCC__$?\n",
            )),
        ];
        if let Some(iid) = hook_env.instance_id {
            setup_cmds.push((
                "death-watch".into(),
                tmux_death_watch_cmd(tmux_session, iid),
            ));
        }

        // `exec` で tmux に置き換えることで、tmux が失敗終了した場合に
        // ログインシェルも消えて ssh PTY が EOF を返し、ready_rx が Err で
        // 解決する。`exec` 無しだと tmux 失敗時にシェルが生き残り PTY が
        // 開いたままになって ready_rx が永遠に未解決になる。
        let attach_cmd = format!(" exec {TMUX} attach-session -t '{tmux_session}'\n");
        let agent_cmd = Some(format!("exec {command}\r"));

        let (ready_tx, ready_rx) = oneshot::channel();
        let instance = Self::start_pty(PtyStartConfig {
            program: "ssh",
            args: ssh_args,
            setup_cmds,
            attach_cmd,
            agent_cmd,
            output_tx,
            ready_tx: Some(ready_tx),
            log_path,
        })?;
        Ok((instance, ready_rx))
    }

    /// 既存のリモート tmux セッションに再アタッチする。
    /// ready_rx で Phase 2 完了（= SSH接続 + tmux attach 成功）を通知する。
    pub fn reattach_remote(
        ssh_args: &[String],
        tmux_session: &str,
        output_tx: mpsc::Sender<Vec<u8>>,
        log_path: Option<std::path::PathBuf>,
    ) -> anyhow::Result<(Self, oneshot::Receiver<Result<(), String>>)> {
        // `exec` 付き: リモートの tmux 側で対象セッションが消えている場合
        // (devcontainer 再起動など) でも、tmux がエラー終了 → ログインシェル
        // 消滅 → ssh PTY EOF → ready_rx に Err が届き、Disconnected として
        // 一覧表示に出る。`exec` 無しだとシェルが残って ready_rx が解決せず
        // フロントが Connecting のまま固まる。
        let attach_cmd = format!(" exec {TMUX} attach-session -t '{tmux_session}'\n");

        let (ready_tx, ready_rx) = oneshot::channel();
        let instance = Self::start_pty(PtyStartConfig {
            program: "ssh",
            args: ssh_args,
            setup_cmds: Vec::new(),
            attach_cmd,
            agent_cmd: None,
            output_tx,
            ready_tx: Some(ready_tx),
            log_path,
        })?;
        Ok((instance, ready_rx))
    }

    // ─── Shell タブ（Terminal タブ）用 ────────────────────────────────────────
    //
    // Agent 用 tmux session (`ccc-XXXX`) と同じ window 集合を共有する
    // session-group メンバー (`ccc-XXXX-shell`) を作り、そこに attach する。
    // current window だけ各 client で独立できるため、Agent タブの window 0 と
    // Terminal タブの shell window を同時に独立表示できる。

    /// ローカルの Terminal タブ用 PTY を起動する。
    pub fn spawn_local_shell(
        agent_session: &str,
        group_session: &str,
        directory: &str,
        output_tx: mpsc::Sender<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let setup_cmds: Vec<(String, String)> = vec![
            (
                "shell-new-window".into(),
                tmux_new_shell_window_cmd(agent_session, directory),
            ),
            (
                "shell-new-group".into(),
                tmux_new_shell_group_cmd(agent_session, group_session),
            ),
        ];
        let attach_cmd = format!(" exec {TMUX} attach-session -t '{group_session}'\n");

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Self::start_pty(PtyStartConfig {
            program: &shell,
            args: &["-l".into()],
            setup_cmds,
            attach_cmd,
            agent_cmd: None,
            output_tx,
            ready_tx: None,
            log_path: None,
        })
    }

    /// リモートの Terminal タブ用 PTY を起動する。
    pub fn spawn_remote_shell(
        ssh_args: &[String],
        agent_session: &str,
        group_session: &str,
        directory: Option<&str>,
        output_tx: mpsc::Sender<Vec<u8>>,
        log_path: Option<std::path::PathBuf>,
    ) -> anyhow::Result<(Self, oneshot::Receiver<Result<(), String>>)> {
        // リモートの $HOME 配下を初期 cwd にするため、directory 未指定時は
        // `~` を渡さず空文字列のままにしておき、tmux 側で `new-window -c` の
        // 値だけ調整する。
        let dir = directory.unwrap_or("");
        let setup_cmds: Vec<(String, String)> = vec![
            (
                "shell-new-window".into(),
                tmux_new_shell_window_cmd(agent_session, dir),
            ),
            (
                "shell-new-group".into(),
                tmux_new_shell_group_cmd(agent_session, group_session),
            ),
        ];
        let attach_cmd = format!(" exec {TMUX} attach-session -t '{group_session}'\n");

        let (ready_tx, ready_rx) = oneshot::channel();
        let instance = Self::start_pty(PtyStartConfig {
            program: "ssh",
            args: ssh_args,
            setup_cmds,
            attach_cmd,
            agent_cmd: None,
            output_tx,
            ready_tx: Some(ready_tx),
            log_path,
        })?;
        Ok((instance, ready_rx))
    }

    // ─── 共通 PTY 起動 ───────────────────────────────────────────────────────

    /// PTY を作成し、コマンドをシーケンシャルに実行する。
    ///
    /// 1. setup_cmds を順次送信、マーカー "__CCC__0" で成功確認
    /// 2. attach_cmd を送信、\x1b[?1049h で検知
    /// 3. 500ms 待機後に agent_cmd を注入（フロント表示開始）
    ///
    /// ready_tx が Some の場合（リモート）:
    /// - setup_cmds あり: Phase 1 の最初のマーカー成功で Ok を送信
    /// - setup_cmds なし (reattach): Phase 2 の clear_sequence 検知で Ok を送信
    /// - EOF 検出時: 蓄積出力を Err で送信
    fn start_pty(cfg: PtyStartConfig<'_>) -> anyhow::Result<Self> {
        let PtyStartConfig {
            program,
            args,
            setup_cmds,
            attach_cmd,
            agent_cmd,
            output_tx,
            ready_tx,
            log_path,
        } = cfg;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: DEFAULT_PTY_ROWS,
            cols: DEFAULT_PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.env("TERM", "xterm-256color");
        let _child = pair.slave.spawn_command(cmd)?;

        let writer_raw = pair.master.take_writer()?;
        let reader = pair.master.try_clone_reader()?;
        let master = pair.master;

        let writer = Arc::new(Mutex::new(writer_raw));
        let writer_for_thread = writer.clone();
        let ssh_info = format!("{program} {}", args.join(" "));

        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut raw_buf = [0u8; 4096];
            let mut reader = reader;
            let mut acc = Vec::<u8>::new();
            let mut ready_tx = ready_tx;

            let dlog = |msg: &str| {
                super::debug_log::append(
                    log_path.as_deref(),
                    &format!("[pty +{}ms] {msg}", start.elapsed().as_millis()),
                );
            };

            dlog(&format!("PTY起動: {ssh_info}"));

            /// writer にデータを送信するヘルパー
            fn send(writer: &Arc<Mutex<Box<dyn std::io::Write + Send>>>, data: &[u8]) {
                let mut w = writer.blocking_lock();
                let _ = std::io::Write::write_all(&mut **w, data);
                let _ = std::io::Write::flush(&mut **w);
            }

            /// マーカーが来るまで読み続ける。見つかったら終了コードを返す。
            fn wait_marker(
                reader: &mut dyn std::io::Read,
                raw_buf: &mut [u8],
                acc: &mut Vec<u8>,
            ) -> Option<char> {
                loop {
                    match std::io::Read::read(reader, raw_buf) {
                        Ok(0) | Err(_) => return None,
                        Ok(n) => {
                            acc.extend_from_slice(&raw_buf[..n]);
                            if let Some(rc) = find_marker(acc) {
                                acc.clear();
                                return Some(rc);
                            }
                        }
                    }
                }
            }

            // Phase 1: セットアップコマンドを順次実行
            for (i, (label, cmd)) in setup_cmds.iter().enumerate() {
                dlog(&format!("Phase 1 ({label}): コマンド送信"));
                send(&writer_for_thread, cmd.as_bytes());
                let phase_start = std::time::Instant::now();
                match wait_marker(&mut *reader, &mut raw_buf, &mut acc) {
                    Some(rc) => {
                        dlog(&format!(
                            "Phase 1 ({label}): マーカー受信 (rc={rc}, +{}ms)",
                            phase_start.elapsed().as_millis()
                        ));
                        // 最初のマーカー成功 = 接続確立
                        if i == 0 {
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(Ok(()));
                            }
                        }
                        if rc != '0' {
                            eprintln!("[ccc] tmux {label} failed (rc={rc}): {}", cmd.trim());
                        }
                    }
                    None => {
                        // PTY closed (ローカルは PTY 異常、リモートは SSH 接続失敗)
                        dlog(&format!(
                            "Phase 1 ({label}): EOF受信 → SSH接続失敗 (+{}ms)",
                            phase_start.elapsed().as_millis()
                        ));
                        if let Some(tx) = ready_tx.take() {
                            let msg = String::from_utf8_lossy(&acc).to_string();
                            let _ = tx.send(Err(msg));
                        }
                        return;
                    }
                }
            }

            // Phase 2: tmux attach → \x1b[?1049h を待つ
            dlog("Phase 2 (attach): コマンド送信");
            send(&writer_for_thread, attach_cmd.as_bytes());
            let phase2_start = std::time::Instant::now();
            loop {
                match std::io::Read::read(&mut *reader, &mut raw_buf) {
                    Ok(0) | Err(_) => {
                        // reattach で EOF → SSH 接続失敗
                        dlog(&format!(
                            "Phase 2 (attach): EOF受信 → SSH接続失敗 (+{}ms)",
                            phase2_start.elapsed().as_millis()
                        ));
                        if let Some(tx) = ready_tx.take() {
                            let msg = String::from_utf8_lossy(&acc).to_string();
                            let _ = tx.send(Err(msg));
                        }
                        return;
                    }
                    Ok(n) => {
                        acc.extend_from_slice(&raw_buf[..n]);
                        if find_clear_sequence(&acc).is_some() {
                            dlog(&format!(
                                "Phase 2 (attach): clear_sequence 受信 (+{}ms)",
                                phase2_start.elapsed().as_millis()
                            ));
                            acc.clear();
                            // reattach 時（setup_cmds が空）: ここで接続成功を通知
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(Ok(()));
                            }
                            break;
                        }
                    }
                }
            }

            // Phase 3: プロンプト初期化を待ってからエージェントコマンドを送信
            dlog("Phase 3: 500ms sleep 開始");
            std::thread::sleep(std::time::Duration::from_millis(500));
            dlog("Phase 3: 500ms sleep 完了");
            if let Some(ref cmd) = agent_cmd {
                send(&writer_for_thread, cmd.as_bytes());
            }

            // Phase 4: 通常の I/O 転送
            dlog("Phase 4: I/O転送開始");
            // エージェントコマンドがある場合、そのエコー行が終わるまで出力を破棄する。
            // シェルは送信した "exec {cmd}\r" を PTY にエコーするので、
            // その文字列 + 改行を見つけた次の位置から転送を開始する。
            if let Some(ref cmd) = agent_cmd {
                let echo_needle: Vec<u8> = cmd.bytes().take_while(|&b| b != b'\r').collect();
                let mut drain_buf = Vec::<u8>::new();
                'skip: loop {
                    match std::io::Read::read(&mut *reader, &mut raw_buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            drain_buf.extend_from_slice(&raw_buf[..n]);
                            if let Some(pos) = drain_buf
                                .windows(echo_needle.len())
                                .position(|w| w == echo_needle.as_slice())
                            {
                                let after = pos + echo_needle.len();
                                if let Some(nl) =
                                    drain_buf[after..].iter().position(|&b| b == b'\n')
                                {
                                    let start = after + nl + 1;
                                    if start < drain_buf.len() {
                                        let _ =
                                            output_tx.blocking_send(drain_buf[start..].to_vec());
                                    }
                                    break 'skip;
                                }
                            }
                        }
                    }
                }
            }

            loop {
                match std::io::Read::read(&mut *reader, &mut raw_buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if output_tx.blocking_send(raw_buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            writer,
            master: Arc::new(Mutex::new(master)),
        })
    }

    pub async fn write(&self, data: &[u8]) -> anyhow::Result<()> {
        let mut w = self.writer.lock().await;
        std::io::Write::write_all(&mut *w, data)?;
        Ok(())
    }

    pub async fn resize(&self, rows: u16, cols: u16) -> anyhow::Result<()> {
        let master = self.master.lock().await;
        master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }
}

/// `tmux new-session` の `-e KEY=VALUE` フラグを組み立てる。
///
/// 各エントリは `(key, Some(value))` を渡す。`value` が None または空文字列のものは
/// 出力に含めない（hook 用 env を未設定時にスキップするため）。
fn build_tmux_env_flags(entries: &[(&str, Option<&str>)]) -> String {
    let mut out = String::new();
    for (key, value) in entries {
        let Some(v) = value else { continue };
        if v.is_empty() {
            continue;
        }
        let escaped = v.replace('\'', "'\\''");
        out.push_str(&format!(" -e '{key}={escaped}'"));
    }
    out
}

/// ローカル ccc 専用 tmux ソケットで指定セッションが生きているかを確認する。
/// `tmux -f /dev/null -L ccc has-session -t <name>` が 0 を返せば true。
pub fn local_tmux_session_exists(tmux_session: &str) -> bool {
    std::process::Command::new("tmux")
        .args([
            "-f",
            "/dev/null",
            "-L",
            "ccc",
            "has-session",
            "-t",
            tmux_session,
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ─── スクロールバックキャプチャ ───────────────────────────────────────────────

/// スクロールバック取得の対象。
pub enum ScrollbackTarget<'a> {
    Local {
        tmux_session: &'a str,
    },
    Remote {
        host_alias: &'a str,
        tmux_session: &'a str,
    },
}

/// tmux セッションのスクロールバックを取得する。
/// `-E -1` で現在の画面（tmux がアタッチ後に送信する）との重複を避ける。
pub fn capture_tmux_scrollback(target: ScrollbackTarget<'_>, lines: u32) -> Option<Vec<u8>> {
    if lines == 0 {
        return None;
    }

    let stdout = match target {
        ScrollbackTarget::Local { tmux_session } => {
            std::process::Command::new("tmux")
                .args([
                    "-f",
                    "/dev/null",
                    "-L",
                    "ccc",
                    "capture-pane",
                    "-p", // stdout に出力
                    "-J", // 折り返し行を結合
                    "-S",
                    &format!("-{lines}"), // N 行前から
                    "-E",
                    "-1", // 現在の画面の1行前まで（現在画面は除外）
                    "-t",
                    tmux_session,
                ])
                .output()
                .ok()?
                .stdout
        }
        ScrollbackTarget::Remote {
            host_alias,
            tmux_session,
        } => {
            let cmd = format!(
                "tmux -f /dev/null -L ccc capture-pane -p -J -S -{lines} -E -1 -t '{tmux_session}'"
            );
            std::process::Command::new("ssh")
                .args([host_alias, &cmd])
                .output()
                .ok()?
                .stdout
        }
    };

    if stdout.is_empty() {
        return None;
    }

    // 各行の末尾スペースを除去し \r\n に変換する。
    // capture-pane はパネル幅分のスペースでパディングするため、
    // そのまま送ると xterm.js 上でレイアウトが崩れる。
    // また \n のみだとカーソルが桁 0 に戻らないため \r\n が必要。
    let text = String::from_utf8_lossy(&stdout);
    let mut processed: Vec<u8> = text
        .lines()
        .flat_map(|line| {
            let mut bytes = line.trim_end().as_bytes().to_vec();
            bytes.extend_from_slice(b"\r\n");
            bytes
        })
        .collect();

    if processed.is_empty() {
        return None;
    }

    // 末尾に \x1b[H\x1b[2J を追加して viewport を scrollback に push する。
    // xterm.js の `scrollOnEraseInDisplay: true` 設定により、`\x1b[2J` 受信時に
    // viewport の内容が scrollback に移動する。これを行わないと
    // pending_scrollback の末尾 (viewport 行数分) は scrollback に積まれず、
    // 直後の tmux 描画で上書きされて消えてしまう。
    processed.extend_from_slice(b"\x1b[H\x1b[2J");

    Some(processed)
}
