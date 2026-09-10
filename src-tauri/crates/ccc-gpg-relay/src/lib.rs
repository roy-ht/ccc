//! gpg agent forward の relay（v0.14）。
//!
//! ssh の `RemoteForward` と ControlMaster への相乗りをやめ、**socket を
//! ccc の常駐プロセスが所有する**方式へ移行するための中核ライブラリ。
//! 設計の全体像は `specs/v0.14-gpg-relay.md`。
//!
//! ```text
//! ローカル                              リモート
//!   uplink ──ssh の stdio 1 本──▶ ccc-gpg-relay --uplink
//!     │  (mux::run_connector)              │ (bridge)
//!     ▼                                    ▼
//!   S.gpg-agent.extra              ctl socket ─▶ daemon (mux::run_acceptor)
//!   （ローカル gpg-agent）                        └─ S.gpg-agent を所有
//! ```
//!
//! バイナリ (`ccc-gpg-relay`) はリモートへ配信され、デーモンとブリッジを担う。
//! ライブラリ部分はローカルの uplink（`ccc-ssh`）からも使い、両端で
//! 同一の多重化実装を共有する。

pub mod bridge;
pub mod daemon;
pub mod lock;
pub mod logging;
pub mod mux;
pub mod paths;
pub mod protocol;
pub mod signal;
pub mod socket;

/// バイナリのバージョン。ccc 本体の配信ロジックが `--version` と突き合わせる。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
