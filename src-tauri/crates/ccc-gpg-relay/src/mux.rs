//! 1 本のバイトストリーム上で複数の assuan セッションを多重化するエンジン。
//!
//! 両端で同じコードを使い、役割だけが異なる:
//!
//! | 役割 | 実体 | チャネルの起点 |
//! |---|---|---|
//! | [`Role::Acceptor`] | リモートの relay デーモン | 自分が accept した接続を OPEN で通知する |
//! | [`Role::Connector`] | ローカルの uplink | OPEN を受けてローカル agent へ connect する |
//!
//! gpg クライアントはリモートにしか居ないため、OPEN は acceptor → connector の
//! 一方向にしか流れない。
//!
//! スレッド構成（セッションごと）:
//!
//! - reader: [`run_acceptor`] / [`run_connector`] を呼んだスレッドがそのまま担う
//! - writer: 送信キューを drain する 1 本
//! - ping: 死活監視 1 本。**ssh の `ServerAliveInterval` に頼らずアプリ層で
//!   half-open を検知する**（specs/v0.14 §6）
//! - pump: チャネルごとに 1 本（ローカル socket → 相手）
//!
//! 送信バッファはチャネル単位とセッション単位の両方に上限を持つ。前者を超えた
//! ときはそのチャネルだけを閉じ、後者を超えたときはセッションを落として
//! 再接続に倒す（詰まったまま無限に溜め込まない）。

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::protocol::{
    read_frame, write_frame, Frame, FrameType, Hello, DATA_CHUNK, PROTOCOL_VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// socket を listen し、accept した接続をチャネルにする側（relay デーモン）
    Acceptor,
    /// OPEN を受けてローカルの agent socket へ繋ぐ側（uplink）
    Connector,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Acceptor => "relay",
            Role::Connector => "uplink",
        }
    }
}

/// セッションの終了理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxEnd {
    /// 相手が正常に切断した
    PeerClosed,
    /// ping 応答が途絶えた（half-open の検知）
    PingTimeout,
    /// 外部から停止要求を受けた
    Stopped,
    /// 送信キューがセッション上限を超えた
    SendOverflow,
}

#[derive(Debug, Clone)]
pub struct MuxConfig {
    pub role: Role,
    /// ping 送出間隔
    pub ping_interval: Duration,
    /// 連続で応答を落とせるとみなす回数（これを超えたら切断）
    pub ping_miss_limit: u32,
    /// チャネル 1 本あたりの送信キュー上限
    pub per_channel_buffer_limit: usize,
    /// セッション全体の送信キュー上限
    pub total_buffer_limit: usize,
    /// HELLO で相手に申告する socket パス（診断用）
    pub socket_path: String,
    /// HELLO で相手に申告する grace 秒（診断用）
    pub grace_secs: u64,
}

/// ping スレッドが停止要求を確認する間隔。`ping_interval` はこの刻みで数える。
const PING_TICK: Duration = Duration::from_millis(100);

/// reader を叩き起こすための割り込み。
///
/// **これが無いと half-open を検知してもセッションが畳まれない。** reader は
/// `read_frame` でブロックしており、相手が沈黙している限り自力では戻れないため、
/// 停止時には下位のストリームを外部から壊す必要がある:
///
/// - relay 側: ctl socket を `shutdown(Both)` する
/// - uplink 側: ssh 子プロセスを kill する（パイプは shutdown できない）
pub type Interrupt = Arc<dyn Fn() + Send + Sync>;

/// 何もしない割り込み（相手が必ず EOF を返す前提のテスト用）。
pub fn no_interrupt() -> Interrupt {
    Arc::new(|| {})
}

/// セッションに渡すコールバック群。
#[derive(Clone)]
pub struct MuxHooks {
    /// 停止時に reader を叩き起こす（[`Interrupt`] 参照）
    pub interrupt: Interrupt,
    /// HELLO 交換が成立した直後に 1 回だけ呼ぶ。uplink はこれを契機に
    /// 状態ファイルを `connected` にする（チャネルが開くのを待たない）
    pub on_ready: Option<Interrupt>,
}

impl MuxHooks {
    pub fn new(interrupt: Interrupt) -> Self {
        MuxHooks {
            interrupt,
            on_ready: None,
        }
    }

    pub fn with_ready(mut self, on_ready: Interrupt) -> Self {
        self.on_ready = Some(on_ready);
        self
    }
}

impl std::fmt::Debug for MuxHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MuxHooks")
            .field("on_ready", &self.on_ready.is_some())
            .finish()
    }
}

impl Default for MuxConfig {
    fn default() -> Self {
        MuxConfig {
            role: Role::Connector,
            ping_interval: Duration::from_secs(15),
            ping_miss_limit: 2,
            per_channel_buffer_limit: 1 << 20,
            total_buffer_limit: 8 << 20,
            socket_path: String::new(),
            grace_secs: 0,
        }
    }
}

// ─── 送信キュー ──────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum PushError {
    /// このチャネルの滞留が上限を超えた（当該チャネルのみ閉じる）
    ChannelOverflow,
    /// セッション全体の滞留が上限を超えた（セッションを落とす）
    SessionOverflow,
    /// キューは閉じている
    Closed,
}

#[derive(Debug)]
struct QueueInner {
    q: VecDeque<Frame>,
    total: usize,
    per_channel: HashMap<u32, usize>,
    closed: bool,
}

#[derive(Debug)]
struct SendQueue {
    inner: Mutex<QueueInner>,
    cv: Condvar,
    per_channel_limit: usize,
    total_limit: usize,
}

impl SendQueue {
    fn new(per_channel_limit: usize, total_limit: usize) -> Self {
        SendQueue {
            inner: Mutex::new(QueueInner {
                q: VecDeque::new(),
                total: 0,
                per_channel: HashMap::new(),
                closed: false,
            }),
            cv: Condvar::new(),
            per_channel_limit,
            total_limit,
        }
    }

    fn push(&self, frame: Frame) -> Result<(), PushError> {
        let len = frame.wire_len();
        let channel = frame.channel;
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(PushError::Closed);
        }
        if inner.total + len > self.total_limit {
            return Err(PushError::SessionOverflow);
        }
        // 制御フレーム（channel 0）は上限の対象外。詰まりの原因にならず、
        // むしろ詰まりを解消する PONG などを落とさないため
        if channel != 0 {
            let used = inner.per_channel.entry(channel).or_insert(0);
            if *used + len > self.per_channel_limit {
                return Err(PushError::ChannelOverflow);
            }
            *used += len;
        }
        inner.total += len;
        inner.q.push_back(frame);
        self.cv.notify_one();
        Ok(())
    }

    /// キューが閉じられ、かつ空になったら `None`。
    fn pop(&self) -> Option<Frame> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(frame) = inner.q.pop_front() {
                let len = frame.wire_len();
                inner.total = inner.total.saturating_sub(len);
                if frame.channel != 0 {
                    if let Some(used) = inner.per_channel.get_mut(&frame.channel) {
                        *used = used.saturating_sub(len);
                    }
                }
                return Some(frame);
            }
            if inner.closed {
                return None;
            }
            inner = self.cv.wait(inner).unwrap();
        }
    }

    /// 当該チャネルの滞留フレームを捨てる（チャネルを閉じるときに呼ぶ）。
    fn drop_channel(&self, channel: u32) {
        let mut inner = self.inner.lock().unwrap();
        let mut kept = VecDeque::with_capacity(inner.q.len());
        let mut freed = 0usize;
        while let Some(frame) = inner.q.pop_front() {
            if frame.channel == channel && frame.ty == FrameType::Data {
                freed += frame.wire_len();
            } else {
                kept.push_back(frame);
            }
        }
        inner.q = kept;
        inner.total = inner.total.saturating_sub(freed);
        inner.per_channel.remove(&channel);
    }

    fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        self.cv.notify_all();
    }
}

// ─── チャネル表 ──────────────────────────────────────────────────────────────

#[derive(Debug)]
struct ChannelState {
    /// 相手 → ローカル へ書く側
    write_half: UnixStream,
    /// 相手から CLOSE を受け取った
    peer_closed: bool,
    /// ローカル側の EOF を検出して CLOSE を送った
    local_closed: bool,
}

/// セッション共有状態。
struct Session {
    tx: SendQueue,
    channels: Mutex<HashMap<u32, ChannelState>>,
    stop: AtomicBool,
    /// 最後に PONG を受けた時刻（`Instant` を保持できないので起点からの ms）
    last_pong_ms: AtomicU64,
    started: Instant,
    next_channel: AtomicU32,
    pumps: Mutex<Vec<JoinHandle<()>>>,
    interrupt: Interrupt,
}

impl Session {
    fn new(cfg: &MuxConfig, interrupt: Interrupt) -> Self {
        Session {
            tx: SendQueue::new(cfg.per_channel_buffer_limit, cfg.total_buffer_limit),
            channels: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            last_pong_ms: AtomicU64::new(0),
            started: Instant::now(),
            next_channel: AtomicU32::new(1),
            pumps: Mutex::new(Vec::new()),
            interrupt,
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn request_stop(&self) {
        // 複数の経路（ping タイムアウト・reader 終了・送信 overflow）から
        // 呼ばれるが、割り込みは 1 回だけでよい
        if self.stop.swap(true, Ordering::Relaxed) {
            self.tx.close();
            return;
        }
        self.tx.close();
        // pump スレッドを解除するため、全チャネルの socket を落とす
        {
            let channels = self.channels.lock().unwrap();
            for state in channels.values() {
                let _ = state.write_half.shutdown(Shutdown::Both);
            }
        }
        // reader は相手が沈黙していると自力で戻れないので外から叩き起こす
        (self.interrupt)();
    }

    /// チャネルを登録し、ローカル socket → 相手 の pump を起動する。
    fn register_channel(self: &Arc<Self>, id: u32, stream: UnixStream) -> io::Result<()> {
        let read_half = stream.try_clone()?;
        {
            let mut channels = self.channels.lock().unwrap();
            channels.insert(
                id,
                ChannelState {
                    write_half: stream,
                    peer_closed: false,
                    local_closed: false,
                },
            );
        }
        let session = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name(format!("ccc-gpg-pump-{id}"))
            .spawn(move || session.pump_channel(id, read_half))?;
        self.pumps.lock().unwrap().push(handle);
        Ok(())
    }

    /// ローカル socket から読んで DATA として送り出す。
    fn pump_channel(self: Arc<Self>, id: u32, mut read_half: UnixStream) {
        let mut buf = vec![0u8; DATA_CHUNK];
        loop {
            if self.stopping() {
                return;
            }
            let n = match read_half.read(&mut buf) {
                Ok(0) => break, // ローカル側 EOF
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            match self.tx.push(Frame::data(id, buf[..n].to_vec())) {
                Ok(()) => {}
                Err(PushError::ChannelOverflow) => {
                    // このチャネルだけを切る。セッションは維持する
                    self.tx.drop_channel(id);
                    let _ = self.tx.push(Frame::close(id, "channel buffer overflow"));
                    self.close_local_side(id);
                    return;
                }
                Err(PushError::SessionOverflow) => {
                    self.request_stop();
                    return;
                }
                Err(PushError::Closed) => return,
            }
        }
        // ローカル EOF: half close を通知する
        let _ = self.tx.push(Frame::close(id, "eof"));
        self.mark_local_closed(id);
    }

    /// ローカル側の EOF を記録し、両方向が閉じていればチャネルを破棄する。
    fn mark_local_closed(&self, id: u32) {
        let mut channels = self.channels.lock().unwrap();
        let done = match channels.get_mut(&id) {
            Some(state) => {
                state.local_closed = true;
                state.peer_closed
            }
            None => false,
        };
        if done {
            channels.remove(&id);
        }
    }

    /// 相手からの CLOSE を記録し、ローカル socket に EOF を伝える。
    fn mark_peer_closed(&self, id: u32) {
        let mut channels = self.channels.lock().unwrap();
        let done = match channels.get_mut(&id) {
            Some(state) => {
                // half close: これ以上は書かないことをローカル agent に伝える。
                // ローカルからの応答はまだ読み続ける
                let _ = state.write_half.shutdown(Shutdown::Write);
                state.peer_closed = true;
                state.local_closed
            }
            None => false,
        };
        if done {
            channels.remove(&id);
        }
    }

    /// チャネルを強制的に閉じる（overflow / 書き込み失敗時）。
    fn close_local_side(&self, id: u32) {
        let mut channels = self.channels.lock().unwrap();
        if let Some(state) = channels.remove(&id) {
            let _ = state.write_half.shutdown(Shutdown::Both);
        }
    }

    /// 相手から届いた DATA をローカル socket へ書く。
    fn deliver(&self, id: u32, payload: &[u8]) {
        let mut channels = self.channels.lock().unwrap();
        let failed = match channels.get_mut(&id) {
            Some(state) => state.write_half.write_all(payload).is_err(),
            // 未知のチャネル = 既に閉じた後に届いた DATA。捨ててよい
            None => false,
        };
        if failed {
            if let Some(state) = channels.remove(&id) {
                let _ = state.write_half.shutdown(Shutdown::Both);
            }
            drop(channels);
            let _ = self.tx.push(Frame::close(id, "local write failed"));
        }
    }

    fn alloc_channel_id(&self) -> u32 {
        self.next_channel.fetch_add(1, Ordering::Relaxed)
    }
}

// ─── エントリポイント ────────────────────────────────────────────────────────

/// connector 側（uplink）のセッションを回す。
///
/// OPEN を受けるたびに `target`（ローカルの `S.gpg-agent.extra` 等）へ connect する。
/// 呼び出したスレッドが reader を担い、セッション終了までブロックする。
pub fn run_connector<R, W>(
    reader: R,
    writer: W,
    target: &Path,
    cfg: MuxConfig,
    hooks: MuxHooks,
) -> io::Result<MuxEnd>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut cfg = cfg;
    cfg.role = Role::Connector;
    run_session(reader, writer, cfg, Some(target.to_path_buf()), None, hooks)
}

/// acceptor 側（relay デーモン）のセッションを回す。
///
/// `incoming` から受け取った接続をチャネルとして OPEN する。accept と grace 待機は
/// 呼び出し側（[`crate::daemon`]）の責務で、ここには確立済みの接続だけが届く。
pub fn run_acceptor<R, W>(
    reader: R,
    writer: W,
    incoming: Receiver<UnixStream>,
    cfg: MuxConfig,
    hooks: MuxHooks,
) -> io::Result<MuxEnd>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut cfg = cfg;
    cfg.role = Role::Acceptor;
    run_session(reader, writer, cfg, None, Some(incoming), hooks)
}

fn run_session<R, W>(
    mut reader: R,
    mut writer: W,
    cfg: MuxConfig,
    connect_target: Option<PathBuf>,
    incoming: Option<Receiver<UnixStream>>,
    hooks: MuxHooks,
) -> io::Result<MuxEnd>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    // HELLO 交換（最初のフレームは必ず HELLO）
    let hello = Hello {
        version: PROTOCOL_VERSION,
        role: cfg.role.as_str().to_string(),
        socket_path: cfg.socket_path.clone(),
        grace_secs: cfg.grace_secs,
    };
    write_frame(&mut writer, &hello.to_frame())?;
    let peer = Hello::from_frame(&read_frame(&mut reader)?)?;
    if peer.version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "プロトコル版が一致しません（自分 {} / 相手 {}）。両側の ccc を更新してください",
                PROTOCOL_VERSION, peer.version
            ),
        ));
    }
    if peer.role == cfg.role.as_str() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("役割が衝突しています（両側とも {}）", peer.role),
        ));
    }

    // ここまで来れば相手と役割・版が噛み合っている。uplink はこの時点で
    // 「接続済み」とみなす（最初のチャネルが開くのを待たない）
    if let Some(ready) = &hooks.on_ready {
        ready();
    }

    let session = Arc::new(Session::new(&cfg, hooks.interrupt));
    session
        .last_pong_ms
        .store(session.now_ms(), Ordering::Relaxed);

    // writer スレッド
    let writer_thread = {
        let session = Arc::clone(&session);
        std::thread::Builder::new()
            .name("ccc-gpg-writer".into())
            .spawn(move || {
                while let Some(frame) = session.tx.pop() {
                    if write_frame(&mut writer, &frame).is_err() {
                        session.stop.store(true, Ordering::Relaxed);
                        session.tx.close();
                        break;
                    }
                }
            })?
    };

    // ping スレッド
    let ping_thread = {
        let session = Arc::clone(&session);
        let interval = cfg.ping_interval;
        let miss_limit = cfg.ping_miss_limit;
        std::thread::Builder::new()
            .name("ccc-gpg-ping".into())
            .spawn(move || {
                let deadline_ms = interval.as_millis() as u64 * (miss_limit as u64 + 1);
                let mut waited = Duration::ZERO;
                while !session.stopping() {
                    // ping 間隔をそのまま sleep すると停止要求への反応が最大
                    // ping_interval 秒遅れる（セッション終了時の join が固まる）。
                    // 細かく刻んで停止を確認する
                    std::thread::sleep(PING_TICK);
                    waited += PING_TICK;
                    if waited < interval {
                        continue;
                    }
                    waited = Duration::ZERO;
                    if session.stopping() {
                        return;
                    }
                    let last = session.last_pong_ms.load(Ordering::Relaxed);
                    if session.now_ms().saturating_sub(last) > deadline_ms {
                        // half-open の検知。セッションを畳んで再接続に倒す
                        session.request_stop();
                        return;
                    }
                    if session.tx.push(Frame::control(FrameType::Ping)).is_err() {
                        return;
                    }
                }
            })?
    };

    // acceptor 側: 新規接続をチャネル化するスレッド
    let accept_thread = incoming.map(|rx| {
        let session = Arc::clone(&session);
        std::thread::Builder::new()
            .name("ccc-gpg-accept".into())
            .spawn(move || {
                while !session.stopping() {
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(stream) => {
                            let id = session.alloc_channel_id();
                            if session.tx.push(Frame::open(id)).is_err() {
                                return;
                            }
                            if session.register_channel(id, stream).is_err() {
                                let _ = session.tx.push(Frame::close(id, "register failed"));
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => continue,
                        // 送り手が畳まれた = daemon 側の停止
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
    });
    let accept_thread = match accept_thread {
        Some(result) => Some(result?),
        None => None,
    };

    // reader ループ（このスレッド）
    let end = reader_loop(&mut reader, &session, connect_target.as_deref());

    session.request_stop();
    let _ = writer_thread.join();
    let _ = ping_thread.join();
    if let Some(handle) = accept_thread {
        let _ = handle.join();
    }
    // pump は socket の shutdown で解除される
    let pumps: Vec<_> = std::mem::take(&mut *session.pumps.lock().unwrap());
    for handle in pumps {
        let _ = handle.join();
    }

    end
}

fn reader_loop<R: Read>(
    reader: &mut R,
    session: &Arc<Session>,
    connect_target: Option<&Path>,
) -> io::Result<MuxEnd> {
    loop {
        if session.stopping() {
            // ping タイムアウトと送信 overflow はどちらも stop 経由で来る。
            // 呼び出し側は「切れたから再接続する」以上の区別を必要としない
            return Ok(MuxEnd::Stopped);
        }
        let frame = match read_frame(reader) {
            Ok(frame) => frame,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(MuxEnd::PeerClosed);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if session.stopping() {
                    return Ok(MuxEnd::Stopped);
                }
                return Err(e);
            }
        };
        match frame.ty {
            FrameType::Hello => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HELLO がセッション途中で再送されました",
                ));
            }
            FrameType::Ping => {
                let _ = session.tx.push(Frame::control(FrameType::Pong));
            }
            FrameType::Pong => {
                session
                    .last_pong_ms
                    .store(session.now_ms(), Ordering::Relaxed);
            }
            FrameType::Open => {
                let Some(target) = connect_target else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "acceptor 側が OPEN を受け取りました（OPEN は一方向です）",
                    ));
                };
                match UnixStream::connect(target) {
                    Ok(stream) => {
                        if session.register_channel(frame.channel, stream).is_err() {
                            let _ = session
                                .tx
                                .push(Frame::close(frame.channel, "register failed"));
                        }
                    }
                    Err(e) => {
                        // ローカル gpg-agent が居ない/死んでいる。このチャネルだけ失敗させる
                        let _ = session
                            .tx
                            .push(Frame::close(frame.channel, &format!("connect failed: {e}")));
                    }
                }
            }
            FrameType::Data => session.deliver(frame.channel, &frame.payload),
            FrameType::Close => session.mark_peer_closed(frame.channel),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_pops_in_fifo_order() {
        let q = SendQueue::new(1 << 20, 8 << 20);
        q.push(Frame::data(1, b"a".to_vec())).unwrap();
        q.push(Frame::data(2, b"b".to_vec())).unwrap();
        assert_eq!(q.pop().unwrap().payload, b"a");
        assert_eq!(q.pop().unwrap().payload, b"b");
    }

    #[test]
    fn queue_close_unblocks_pop() {
        let q = Arc::new(SendQueue::new(1 << 20, 8 << 20));
        let q2 = Arc::clone(&q);
        let handle = std::thread::spawn(move || q2.pop());
        std::thread::sleep(Duration::from_millis(30));
        q.close();
        assert!(handle.join().unwrap().is_none(), "閉じたら None で抜ける");
    }

    #[test]
    fn per_channel_limit_rejects_only_that_channel() {
        let q = SendQueue::new(200, 8 << 20);
        assert_eq!(
            q.push(Frame::data(1, vec![0u8; 300])),
            Err(PushError::ChannelOverflow)
        );
        // 別チャネルは影響を受けない
        assert!(q.push(Frame::data(2, vec![0u8; 100])).is_ok());
    }

    #[test]
    fn session_limit_takes_precedence() {
        let q = SendQueue::new(1 << 20, 200);
        assert_eq!(
            q.push(Frame::data(1, vec![0u8; 300])),
            Err(PushError::SessionOverflow)
        );
    }

    #[test]
    fn control_frames_bypass_per_channel_limit() {
        let q = SendQueue::new(1, 8 << 20);
        // channel 0 の制御フレームは上限に関わらず通る（PONG を落とさない）
        assert!(q.push(Frame::control(FrameType::Pong)).is_ok());
        assert!(q.push(Frame::control(FrameType::Ping)).is_ok());
    }

    #[test]
    fn pop_releases_accounted_bytes() {
        let q = SendQueue::new(200, 8 << 20);
        q.push(Frame::data(1, vec![0u8; 150])).unwrap();
        // 滞留したままでは 2 通目が入らない
        assert_eq!(
            q.push(Frame::data(1, vec![0u8; 150])),
            Err(PushError::ChannelOverflow)
        );
        q.pop();
        // drain されれば再び受け付ける
        assert!(q.push(Frame::data(1, vec![0u8; 150])).is_ok());
    }

    #[test]
    fn drop_channel_frees_pending_data() {
        let q = SendQueue::new(500, 8 << 20);
        q.push(Frame::data(1, vec![0u8; 100])).unwrap();
        q.push(Frame::data(2, vec![0u8; 100])).unwrap();
        q.drop_channel(1);
        // チャネル 2 のフレームだけが残る
        let frame = q.pop().unwrap();
        assert_eq!(frame.channel, 2);
        assert!(q.inner.lock().unwrap().q.is_empty());
    }

    #[test]
    fn drop_channel_keeps_close_frame() {
        let q = SendQueue::new(500, 8 << 20);
        q.push(Frame::data(1, vec![0u8; 100])).unwrap();
        q.push(Frame::close(1, "bye")).unwrap();
        q.drop_channel(1);
        // CLOSE は相手に伝える必要があるので捨てない
        let frame = q.pop().unwrap();
        assert_eq!(frame.ty, FrameType::Close);
    }

    #[test]
    fn push_after_close_fails() {
        let q = SendQueue::new(1 << 20, 8 << 20);
        q.close();
        assert_eq!(q.push(Frame::open(1)), Err(PushError::Closed));
    }
}
