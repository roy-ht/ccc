use std::io::Write;
use std::path::Path;

/// インスタンスディレクトリの .debug.txt にタイムスタンプ付きで追記する。
/// path が None の場合は何もしない（本番モードでは None を渡す）。
pub(crate) fn append(path: Option<&Path>, msg: &str) {
    let Some(p) = path else { return };
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
    else {
        return;
    };
    let _ = writeln!(f, "{} {msg}", utc_time_str());
}

/// UTC の時刻文字列。ローカル時刻と取り違えないよう `Z` を明示する
/// （chrono 非依存のため固定で UTC。JST との突き合わせは +9h）。
fn utc_time_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = d.as_secs();
    let h = (total_secs % 86400) / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    let ms = d.subsec_millis();
    format!("{h:02}:{m:02}:{s:02}.{ms:03}Z")
}
