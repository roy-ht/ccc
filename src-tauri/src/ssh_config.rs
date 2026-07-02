//! ssh 設定の解決は共有 crate `ccc-sshkit` に移動した（v0.10、ccc-ssh CLI と共用）。
//! 既存の `crate::ssh_config::...` 参照を保つための再エクスポート。

pub use ccc_sshkit::ssh_config::*;
