//! relay デーモン本体。
//!
//! 2 つの socket を持つ:
//!
//! - **gpg socket**（`~/.gnupg/S.gpg-agent`）: gpg クライアントが繋いでくる先。
//!   このファイルを所有するのが relay である、というのが v0.14 の中心的な不変条件
//! - **ctl socket**（`~/.ccc/run/gpg-relay.sock`）: uplink が繋いでくる先。
//!   `ccc-gpg-relay --uplink` が stdio との橋渡しをする
//!
//! uplink が繋がっていない間に来た接続は最大 `grace` 秒だけ待たせる
//! （specs/v0.14 §8）。一時的な網断や uplink の再接続を gpg 側から見えなくするため。
//! 待たせる数には上限があり、溢れた分は即座に閉じる。

use std::collections::VecDeque;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::lock::{self, ProcessLock};
use crate::mux::{run_acceptor, Interrupt, MuxConfig, MuxHooks, Role};
use crate::socket::{bind_owned, OwnedSocket};

/// ログ出力（stderr / ファイル）。スレッドをまたぐため Send + Sync。
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// 何も出力しないログ（テスト用）。
pub fn null_log() -> Log {
    Arc::new(|_: &str| {})
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// gpg クライアントが繋ぐ socket（relay が所有する）
    pub gpg_socket: PathBuf,
    /// uplink が繋ぐ制御 socket
    pub ctl_socket: PathBuf,
    /// 単一性を保証する flock のパス
    pub lock_path: PathBuf,
    /// uplink 未接続時に接続を待たせる上限
    pub grace: Duration,
    /// 待たせられる接続数の上限（溢れたら即閉じる）
    pub pending_limit: usize,
    /// gpg socket の所有権を確認する間隔（§11.2 の層 3）
    pub socket_watch_interval: Duration,
    pub mux: MuxConfig,
}

impl DaemonConfig {
    /// 既定値（grace 5 秒 / 待機上限 16。specs/v0.14 §8）。
    pub fn new(gpg_socket: PathBuf, ctl_socket: PathBuf, lock_path: PathBuf) -> Self {
        let mux = MuxConfig {
            role: Role::Acceptor,
            socket_path: gpg_socket.to_string_lossy().into_owned(),
            grace_secs: 5,
            ..MuxConfig::default()
        };
        DaemonConfig {
            gpg_socket,
            ctl_socket,
            lock_path,
            grace: Duration::from_secs(5),
            pending_limit: 16,
            socket_watch_interval: Duration::from_secs(5),
            mux,
        }
    }
}

/// grace 待ちの接続。
struct Pending {
    stream: UnixStream,
    since: Instant,
}

/// 現在の uplink セッション。
struct Current {
    generation: u64,
    /// チャネル化する接続の受け渡し口
    tx: Sender<UnixStream>,
    /// 後勝ち調停で旧セッションの reader を解除するための複製
    ctl: UnixStream,
}

struct Shared {
    stop: AtomicBool,
    /// socket の所有権を奪われて自ら停止した
    aborted: AtomicBool,
    current: Mutex<Option<Current>>,
    pending: Mutex<VecDeque<Pending>>,
    generation: AtomicU64,
    /// 診断用カウンタ
    accepted: AtomicU64,
    dropped: AtomicU64,
    cfg: DaemonConfig,
    log: Log,
}

impl Shared {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// gpg クライアントの接続をチャネル化する。uplink が居なければ待たせる。
    fn dispatch(&self, stream: UnixStream) {
        // セッションが畳まれた直後は send が失敗する。その場合は接続を
        // 取り戻して（`SendError` が中身を返す）下の待機キューに回す
        let stream = {
            let current = self.current.lock().unwrap();
            match current.as_ref() {
                Some(cur) => match cur.tx.send(stream) {
                    Ok(()) => {
                        self.accepted.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    Err(returned) => returned.0,
                },
                None => stream,
            }
        };
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= self.cfg.pending_limit {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            (self.log)(&format!(
                "[relay] 待機キューが上限（{}）に達したため接続を閉じました",
                self.cfg.pending_limit
            ));
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        (self.log)(&format!(
            "[relay] uplink 未接続のため接続を待機させます（{} 件目、最大 {} 秒）",
            pending.len() + 1,
            self.cfg.grace.as_secs()
        ));
        pending.push_back(Pending {
            stream,
            since: Instant::now(),
        });
    }

    /// 待機中の接続を新しいセッションへ流し込む（grace 超過分は捨てる）。
    fn flush_pending(&self, tx: &Sender<UnixStream>) {
        let mut pending = self.pending.lock().unwrap();
        let mut flushed = 0usize;
        while let Some(item) = pending.pop_front() {
            if item.since.elapsed() > self.cfg.grace {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                let _ = item.stream.shutdown(Shutdown::Both);
                continue;
            }
            if tx.send(item.stream).is_err() {
                break;
            }
            flushed += 1;
        }
        if flushed > 0 {
            self.accepted.fetch_add(flushed as u64, Ordering::Relaxed);
            (self.log)(&format!(
                "[relay] 待機していた {flushed} 件の接続を新しい uplink に引き渡しました"
            ));
        }
    }

    /// grace を過ぎた待機接続を閉じる。
    fn expire_pending(&self) {
        let mut pending = self.pending.lock().unwrap();
        let mut expired = 0usize;
        pending.retain(|item| {
            if item.since.elapsed() > self.cfg.grace {
                // カウンタは shutdown より先に確定させる。逆順だと、切断に気づいた
                // 観測者が統計を読んだときにまだ加算されていないことがある
                self.dropped.fetch_add(1, Ordering::Relaxed);
                let _ = item.stream.shutdown(Shutdown::Both);
                expired += 1;
                false
            } else {
                true
            }
        });
        if expired > 0 {
            (self.log)(&format!(
                "[relay] {expired} 件の接続が grace（{} 秒）を過ぎたため閉じました",
                self.cfg.grace.as_secs()
            ));
        }
    }

    /// 新しい uplink を採用する。**後勝ち**（specs/v0.14 §5）。
    ///
    /// 古い uplink は half-open かもしれず、生死を判別できない。新しい接続が
    /// 到来したこと自体が「ローカル側が切れていると判断した」証拠なので、
    /// 迷わず旧セッションを畳む。
    fn adopt_uplink(self: &Arc<Self>, ctl: UnixStream) -> io::Result<()> {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let reader = ctl.try_clone()?;
        let writer = ctl.try_clone()?;
        let shutdown_handle = ctl.try_clone()?;
        let (tx, rx) = mpsc::channel::<UnixStream>();

        // mux が停止を決めたとき（ping タイムアウト等）に reader を叩き起こす。
        // これが無いと沈黙した uplink を検知してもセッションが畳まれない
        let interrupt_stream = ctl.try_clone()?;
        let interrupt: Interrupt = Arc::new(move || {
            let _ = interrupt_stream.shutdown(Shutdown::Both);
        });

        {
            let mut current = self.current.lock().unwrap();
            if let Some(old) = current.take() {
                (self.log)(&format!(
                    "[relay] 新しい uplink を採用し、世代 {} を切断します（後勝ち）",
                    old.generation
                ));
                let _ = old.ctl.shutdown(Shutdown::Both);
            }
            *current = Some(Current {
                generation,
                tx: tx.clone(),
                ctl: shutdown_handle,
            });
        }
        (self.log)(&format!(
            "[relay] uplink を採用しました（世代 {generation}）"
        ));

        self.flush_pending(&tx);

        let shared = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("ccc-gpg-session-{generation}"))
            .spawn(move || {
                let result = run_acceptor(
                    reader,
                    writer,
                    rx,
                    shared.cfg.mux.clone(),
                    MuxHooks::new(Arc::clone(&interrupt)),
                );
                match result {
                    Ok(end) => (shared.log)(&format!(
                        "[relay] 世代 {generation} のセッションが終了しました: {end:?}"
                    )),
                    Err(e) => (shared.log)(&format!(
                        "[relay] 世代 {generation} のセッションが異常終了しました: {e}"
                    )),
                }
                // HELLO 段階での失敗も含め、終了時は必ず ctl socket を閉じて
                // uplink 側に切断を伝える
                interrupt();
                // 自分がまだ current なら外す（後勝ちで既に置換済みなら触らない）
                let mut current = shared.current.lock().unwrap();
                if current.as_ref().is_some_and(|c| c.generation == generation) {
                    *current = None;
                }
            })?;
        Ok(())
    }
}

/// 起動中のデーモン。[`DaemonHandle::stop`] で停止する。
pub struct DaemonHandle {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    /// Drop で socket ファイルを unlink する（監視スレッドと共有するため Arc）
    _gpg: Arc<OwnedSocket>,
    _ctl: OwnedSocket,
    /// 保持している間だけ単一性が保証される
    _lock: ProcessLock,
}

impl DaemonHandle {
    /// 停止して全スレッドを畳む。socket ファイルは Drop で unlink される。
    pub fn stop(self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // accept ループは非ブロッキングでポーリングしているので、フラグを立てるだけで
        // 畳める。「自分の socket へ self-connect して accept を起こす」方式は、
        // socket を横取りされた状態（§11.2）では接続先が他人になり永久に起きない
        if let Some(cur) = self.shared.current.lock().unwrap().take() {
            let _ = cur.ctl.shutdown(Shutdown::Both);
        }
        for handle in self.threads {
            let _ = handle.join();
        }
    }

    /// 診断用の統計（受理数, 破棄数）。
    pub fn stats(&self) -> (u64, u64) {
        (
            self.shared.accepted.load(Ordering::Relaxed),
            self.shared.dropped.load(Ordering::Relaxed),
        )
    }

    /// uplink が接続中か。
    pub fn has_uplink(&self) -> bool {
        self.shared.current.lock().unwrap().is_some()
    }

    /// 現在の uplink セッションの世代（居なければ 0）。
    /// 後勝ちの置き換わりは `has_uplink` では観測できないためこちらを見る。
    pub fn uplink_generation(&self) -> u64 {
        self.shared
            .current
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.generation)
            .unwrap_or(0)
    }

    /// gpg socket の所有権を奪われて自ら停止したか（§11.2 の層 3）。
    pub fn aborted(&self) -> bool {
        self.shared.aborted.load(Ordering::Relaxed)
    }

    /// 現在の uplink セッションを切る。戻り値は「切る相手が居たか」。
    ///
    /// gpg socket は保持したままなので、uplink 側が再接続すれば復旧する。
    pub fn drop_uplink(&self) -> bool {
        match self.shared.current.lock().unwrap().take() {
            Some(cur) => {
                let _ = cur.ctl.shutdown(Shutdown::Both);
                true
            }
            None => false,
        }
    }
}

/// デーモンを起動する（非ブロッキング）。
///
/// flock を取得できなければ `Ok(None)` を返す = 既に別のデーモンが動いている。
/// 呼び出し側はそのまま正常終了してよい（`--ensure-daemon` の冪等性はこれで担保する）。
pub fn start(cfg: DaemonConfig, log: Log) -> io::Result<Option<DaemonHandle>> {
    let Some(process_lock) = lock::try_acquire(&cfg.lock_path)? else {
        return Ok(None);
    };

    // ロックを取得した後にのみ bind する（unlink の前提を満たすため）
    let gpg = bind_owned(&cfg.gpg_socket)?;
    let ctl = bind_owned(&cfg.ctl_socket)?;
    log(&format!(
        "[relay] 起動しました（gpg socket: {}, ctl socket: {}）",
        cfg.gpg_socket.display(),
        cfg.ctl_socket.display()
    ));

    let gpg = Arc::new(gpg);
    let gpg_listener = gpg.try_clone_listener()?;
    let ctl_listener = ctl.try_clone_listener()?;
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        aborted: AtomicBool::new(false),
        current: Mutex::new(None),
        pending: Mutex::new(VecDeque::new()),
        generation: AtomicU64::new(0),
        accepted: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        cfg,
        log,
    });

    let mut threads = Vec::new();
    threads.push(spawn_accept_loop(
        "ccc-gpg-accept-client",
        gpg_listener,
        Arc::clone(&shared),
        |shared, stream| shared.dispatch(stream),
    )?);
    threads.push(spawn_accept_loop(
        "ccc-gpg-accept-uplink",
        ctl_listener,
        Arc::clone(&shared),
        |shared, stream| {
            if let Err(e) = shared.adopt_uplink(stream) {
                (shared.log)(&format!("[relay] uplink の採用に失敗しました: {e}"));
            }
        },
    )?);
    threads.push({
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ccc-gpg-expire".into())
            .spawn(move || {
                while !shared.stopping() {
                    std::thread::sleep(Duration::from_millis(200));
                    shared.expire_pending();
                }
            })?
    });
    threads.push(spawn_socket_watch(Arc::clone(&shared), Arc::clone(&gpg))?);

    Ok(Some(DaemonHandle {
        shared,
        threads,
        _gpg: gpg,
        _ctl: ctl,
        _lock: process_lock,
    }))
}

/// 停止要求を確認する刻み（監視間隔はこの刻みで数える）。
const WATCH_TICK: Duration = Duration::from_millis(200);

/// gpg socket の所有権を見張る（specs/v0.14 §5, §11.2 の層 3）。
///
/// 奪われていたら **listener を差し替えるのではなくデーモン自身を終了する**。
/// 差し替えでは accept でブロック中のスレッドを綺麗に畳めず、複雑さに見合わない。
/// 終了すれば uplink が切断を検知して再接続し、`--ensure-daemon` が新しい
/// デーモンを起こして `bind_owned` が所有権を取り戻す（数秒で復旧する）。
fn spawn_socket_watch(shared: Arc<Shared>, gpg: Arc<OwnedSocket>) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("ccc-gpg-watch-socket".into())
        .spawn(move || {
            let interval = shared.cfg.socket_watch_interval;
            let mut waited = Duration::ZERO;
            while !shared.stopping() {
                std::thread::sleep(WATCH_TICK);
                waited += WATCH_TICK;
                if waited < interval {
                    continue;
                }
                waited = Duration::ZERO;
                if shared.stopping() || gpg.is_still_mine() {
                    continue;
                }
                (shared.log)(&format!(
                    "[relay] gpg socket の所有権を失いました（{}）。\
                     素の ssh の RemoteForward が発動した可能性があります。\
                     デーモンを終了し、次の uplink 接続で作り直します",
                    gpg.path().display()
                ));
                shared.aborted.store(true, Ordering::Relaxed);
                shared.stop.store(true, Ordering::Relaxed);
                // 前景で待っている main を起こす（シグナルと同じ経路）
                crate::signal::trigger();
                return;
            }
        })
}

/// accept のポーリング間隔。接続レイテンシに直接乗るので短くする
/// （syscall 1 回 / 20ms / socket なのでコストは無視できる）。
const ACCEPT_TICK: Duration = Duration::from_millis(20);

/// accept ループ。
///
/// **非ブロッキング + ポーリング**にしてある。ブロッキング accept だと停止時に
/// スレッドを畳めず、「自分の socket へ self-connect して起こす」回避策は
/// socket を横取りされた状態（§11.2）で破綻する（接続先が他人になる）。
fn spawn_accept_loop(
    name: &str,
    listener: UnixListener,
    shared: Arc<Shared>,
    handle: fn(&Arc<Shared>, UnixStream),
) -> io::Result<JoinHandle<()>> {
    listener.set_nonblocking(true)?;
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || loop {
            if shared.stopping() {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    // **macOS/BSD では accept した socket が O_NONBLOCK を継承する**
                    // （Linux は継承しない）。以降は素直にブロックさせたいので明示的に戻す
                    if let Err(e) = stream.set_nonblocking(false) {
                        (shared.log)(&format!("[relay] 接続を blocking に戻せませんでした: {e}"));
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    handle(&shared, stream);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_TICK);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    if !shared.stopping() {
                        (shared.log)(&format!("[relay] accept に失敗しました: {e}"));
                    }
                    return;
                }
            }
        })
}
