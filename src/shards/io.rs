//! Filesystem helpers for the shard cache.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Shard count. Fixed so bucket assignment stays stable across processes.
pub(super) const BUCKET_COUNT: usize = 16;

pub(super) fn mtime_seconds(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

pub(super) fn bucket_for(hash_hex: &str) -> String {
    let first = hash_hex
        .get(..2)
        .and_then(|prefix| u8::from_str_radix(prefix, 16).ok())
        .unwrap_or(0);
    bucket_name(first % BUCKET_COUNT as u8)
}

pub(super) fn bucket_name(index: u8) -> String {
    format!("shards/{:03}.jsonl", index % BUCKET_COUNT as u8)
}

/// Lowercase hex without a dependency on an encoding crate.
pub(super) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Write through a temp file with fsync, then rename into place.
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_dir(path);
    Ok(())
}

/// Best-effort directory sync after a rename.
fn sync_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

pub(super) fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = (seconds / 86_400) as i64;
    let clock = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        clock / 3600,
        (clock % 3600) / 60,
        clock % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_bucket_names_stay_under_shards() {
        for byte in 0..=255u8 {
            let hash = format!("{byte:02x}").repeat(32);
            let name = bucket_for(&hash);
            let path = Path::new(&name);
            assert_eq!(path.parent(), Some(Path::new("shards")));
            assert!(path.file_name().is_some());
            assert!(!name.contains(".."));
        }
    }

    #[test]
    fn bucket_assignment_is_stable_and_in_range() {
        let hex = "00ff8811".repeat(8);
        assert_eq!(bucket_for(&hex), bucket_name(0));
        let hex_ff = "ff".to_string() + &"a1".repeat(31);
        assert_eq!(bucket_for(&hex_ff), bucket_name(15));
    }

    #[test]
    fn hex_matches_known_vectors() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
    }
}
