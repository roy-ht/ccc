//! デーモンのログ出力。
//!
//! relay も uplink も stdio を `/dev/null` に落として常駐するため、ログファイルが
//! 唯一の手がかりになる。追記しっぱなしにしないよう、起動時にサイズを見て切り詰める。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::daemon::Log;

/// ログが肥大しないよう、起動時にこのサイズを超えていたら truncate する。
pub const LOG_MAX_BYTES: u64 = 1 << 20;

/// ファイル（+ 任意で stderr）へ書くログを作る。
pub fn make_log(path: PathBuf, also_stderr: bool) -> Log {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 追記しっぱなしにしない（リモートのディスクを圧迫させない）
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > LOG_MAX_BYTES {
        let _ = std::fs::write(&path, b"");
    }
    Arc::new(move |msg: &str| {
        let line = format!("{} {msg}\n", timestamp());
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = file.write_all(line.as_bytes());
        }
        if also_stderr {
            eprint!("{line}");
        }
    })
}

/// `YYYY-MM-DDThh:mm:ssZ`（UTC）。依存を増やさないための最小実装。
pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant の civil_from_days（1970-01-01 からの日数 → 年月日）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        // 2000-03-01（うるう年の境界を跨ぐ）
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // 2026-01-01
        assert_eq!(civil_from_days(20_454), (2026, 1, 1));
    }

    #[test]
    fn timestamp_has_expected_shape() {
        let ts = timestamp();
        assert_eq!(ts.len(), 20, "YYYY-MM-DDThh:mm:ssZ の 20 文字");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn log_writes_to_file_and_truncates_when_large() {
        let dir = std::env::temp_dir().join(format!("ccc-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("test.log");

        let log = make_log(path.clone(), false);
        log("hello");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("hello"));
        assert!(content.ends_with('\n'));

        // 上限超のファイルは次の起動で切り詰められる
        std::fs::write(&path, vec![b'x'; (LOG_MAX_BYTES + 1) as usize]).unwrap();
        let log = make_log(path.clone(), false);
        log("after truncate");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.len() < 200, "truncate されている");
        assert!(content.contains("after truncate"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
