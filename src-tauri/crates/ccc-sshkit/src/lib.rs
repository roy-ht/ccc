//! ssh ControlMaster まわりの運用ロジック共有 crate。
//!
//! - `ssh_config`: `~/.ssh/config` / `ssh -G` のパース
//! - `forwards`: port forward 台帳・mux 操作・世代検知リプレイ
//! - `agent_socket`: gpg agent forward の健全性チェックと修復
//! - `liveness`: master の死活プローブと half-open 復旧（v0.13）
//! - `exec`: タイムアウト付き子プロセス実行
//! - `paths`: `~/.ccc[/dev]` の解決（CCC_DEV 環境変数で切替）
//!
//! ccc 本体（tauri アプリ）と `ccc-ssh` CLI の両方から使う。
//! ログ出力先が呼び出し元で異なるため、`agent_socket` はログをコールバックで
//! 受け取る。GUI/CLI 間で共有すべき状態（台帳・gpg 世代ゲート）はすべて
//! ファイルベース（`~/.ccc[/dev]/forwards/`）に置く。

pub mod agent_socket;
pub mod exec;
pub mod forwards;
pub mod liveness;
pub mod paths;
pub mod ssh_config;
