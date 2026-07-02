//! ccc-archive: Claude Code のセッション/メモリをローカル SQLite に集約するコアライブラリ。
//!
//! ccc 本体（Tauri）と `ccc-sessions` CLI の双方が依存する。取り込み・スキーマ・
//! クエリ・FTS を一箇所に集約し、コード重複を避ける。
//!
//! 範囲:
//! - SQLite 接続のオープン（WAL / busy_timeout）／自前 in-process FTS5 lindera トークナイザ登録
//! - スキーマ DDL と `schema_meta` バージョニング
//! - ingest: transcript JSONL の増分取り込み（本体＋subagents、起動時フルスキャン）
//! - memory: メモリ資産（CLAUDE.md / memory / rules）のバージョン付きスナップショット
//!
//! 後続フェーズで sync（リモート pull）を追加する。

mod compress;
mod db;
mod dicts;
mod fts;
mod hooks;
mod ingest;
mod memory;
mod query;
mod schema;
mod strip;
mod sync;

// `open` が返す接続型。下流（CLI）が rusqlite を直接依存せず型注釈できるよう再公開。
pub use rusqlite::Connection;

pub use db::{open, open_in_memory};
pub use dicts::{
    collect_training_samples, invalidate_cache, list as list_dicts, load_decoder, load_encoder,
    retrain_kind, upsert_trained_dict, DictRow, RecompressProgress, RecompressStats, RetrainStats,
    TrainedDict, KIND_PAYLOAD, KIND_RAW,
};
pub use hooks::{record_event, record_session_start, InstanceMeta};
pub use ingest::{ingest_file, ingest_transcript, scan_projects, IngestStats};
pub use memory::{
    line_diff, list_memory, list_memory_for_instance, memory_latest_content, memory_versions,
    snapshot_memory, MemoryEntry, MemoryFilter, MemoryStats, MemoryVersion,
};
pub use query::{
    list_sessions, resolve_session_ids, search, search_session_messages,
    search_sessions_for_location, session_summary, sessions_for_location, show_session, stats,
    ArchiveStats, LabeledCount, ListFilter, MessageRow, SearchHit, SessionHit, SessionRow,
};
pub use schema::SCHEMA_VERSION;
pub use sync::{
    infer_attribution, ingest_pulled, pull_profile, pull_session, staging_profile_dir, PullStats,
};
