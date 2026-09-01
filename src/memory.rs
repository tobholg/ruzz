use std::fs;
use std::path::{Path, PathBuf};

/// Parse a memory budget string like "512MB", "2GB", "100%", "unlimited"
/// Returns the budget in bytes, or None for unlimited/100%
pub fn parse_memory_budget(budget: &str, index_size: u64) -> Option<u64> {
    let s = budget.trim().to_lowercase();

    if s == "unlimited" || s == "100%" {
        return None; // No limit
    }

    // Percentage of index size
    if s.ends_with('%') {
        let pct: f64 = s.trim_end_matches('%').parse().unwrap_or(100.0);
        if pct >= 100.0 {
            return None;
        }
        return Some((index_size as f64 * pct / 100.0) as u64);
    }

    // Absolute size
    let (num_str, multiplier) = if s.ends_with("gb") {
        (s.trim_end_matches("gb"), 1024u64 * 1024 * 1024)
    } else if s.ends_with("mb") {
        (s.trim_end_matches("mb"), 1024u64 * 1024)
    } else if s.ends_with("kb") {
        (s.trim_end_matches("kb"), 1024u64)
    } else {
        // Assume bytes
        (s.as_str(), 1u64)
    };

    let num: f64 = num_str.trim().parse().unwrap_or(0.0);
    let bytes = (num * multiplier as f64) as u64;

    if bytes == 0 {
        return None; // Invalid = unlimited
    }

    Some(bytes)
}

/// Get total size of all files in a directory
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                total += dir_size(&entry.path());
            }
        }
    }
    total
}

/// Which file to warm first when the budget cannot hold everything. Term
/// dictionaries and fast fields are touched by every query; posting lists
/// next; stored documents only when a hit is rendered.
fn warm_priority(path: &Path) -> u8 {
    match path.extension().and_then(|e| e.to_str()) {
        Some("term") => 0,
        Some("fast") => 1,
        Some("fieldnorm") => 2,
        Some("idx") => 3,
        Some("store") => 5,
        _ => 4,
    }
}

/// Pre-read index files into the OS page cache, hottest structures first,
/// until the budget is spent.
///
/// This is a warm-up, not a limit: residency is the operating system's call,
/// and nothing here can cap the process. To actually bound memory, use the
/// platform's mechanism (cgroups, a VM size). What the budget buys is a
/// cold-start without page-fault latency on the structures every query
/// touches, in a controlled amount of I/O.
pub fn apply_memory_budget(index_path: &Path, budget_str: &str) {
    let index_size = dir_size(index_path);
    let budget = parse_memory_budget(budget_str, index_size);

    let mut files: Vec<(u8, u64, PathBuf)> = Vec::new();
    collect_files(index_path, &mut files);
    files.sort_by_key(|(priority, size, _)| (*priority, *size));

    let mut remaining = budget.unwrap_or(u64::MAX);
    let mut warmed_bytes = 0u64;
    for (_, size, path) in &files {
        if remaining == 0 {
            break;
        }
        let take = (*size).min(remaining);
        warm_file(path, take);
        warmed_bytes += take;
        remaining = remaining.saturating_sub(take);
    }

    match budget {
        None => println!(
            "  memory: unlimited (warmed full index: {})",
            format_bytes(index_size)
        ),
        Some(budget_bytes) => println!(
            "  memory: warmed {} of {} index (budget {}); this is a warm-up, \
             not a cap — cap the process with cgroups or similar",
            format_bytes(warmed_bytes),
            format_bytes(index_size),
            format_bytes(budget_bytes),
        ),
    }
}

fn collect_files(dir: &Path, out: &mut Vec<(u8, u64, PathBuf)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            collect_files(&path, out);
        } else if meta.is_file() && meta.len() > 0 {
            out.push((warm_priority(&path), meta.len(), path));
        }
    }
}

/// Read the first `bytes` of a file to pull its pages into the OS cache.
fn warm_file(path: &Path, bytes: u64) {
    use std::io::Read;
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut warmed = 0u64;
    let mut buf = [0u8; 256 * 1024];
    while warmed < bytes {
        let to_read = ((bytes - warmed) as usize).min(buf.len());
        match reader.read(&mut buf[..to_read]) {
            Ok(0) => break,
            Ok(n) => warmed += n as u64,
            Err(_) => break,
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2}GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0}KB", bytes as f64 / KB as f64)
    } else {
        format!("{}B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_budget() {
        assert_eq!(parse_memory_budget("100%", 1000), None);
        assert_eq!(parse_memory_budget("unlimited", 1000), None);
        assert_eq!(parse_memory_budget("50%", 1000), Some(500));
        assert_eq!(parse_memory_budget("512MB", 0), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory_budget("2GB", 0), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(
            parse_memory_budget("10%", 545 * 1024 * 1024),
            Some(54 * 1024 * 1024 + 524288)
        );
    }

    #[test]
    fn hot_structures_warm_first() {
        assert!(warm_priority(Path::new("x.term")) < warm_priority(Path::new("x.idx")));
        assert!(warm_priority(Path::new("x.fast")) < warm_priority(Path::new("x.idx")));
        assert!(warm_priority(Path::new("x.idx")) < warm_priority(Path::new("x.store")));
        assert!(warm_priority(Path::new("meta.json")) < warm_priority(Path::new("x.store")));
    }
}
