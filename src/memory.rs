use std::fs;
use std::path::Path;

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

/// Apply memory budget by pre-warming index pages up to the budget.
///
/// Strategy:
/// - Scan all files in the index directory
/// - Calculate what fraction of the index fits in the budget
/// - For each file, read (warm) that fraction from the start
///   (term dictionaries and posting list heads live at the front)
/// - Use madvise to hint the OS about the rest
///
/// If budget is None (unlimited), warm everything.
pub fn apply_memory_budget(index_path: &Path, budget_str: &str) {
    let index_size = dir_size(index_path);
    let budget = parse_memory_budget(budget_str, index_size);

    match budget {
        None => {
            // Unlimited: warm everything
            println!(
                "  memory: unlimited (warming full index: {})",
                format_bytes(index_size)
            );
            warm_files(index_path, 1.0);
        }
        Some(budget_bytes) => {
            let ratio = if index_size > 0 {
                (budget_bytes as f64 / index_size as f64).min(1.0)
            } else {
                1.0
            };
            println!(
                "  memory: {} budget / {} index ({:.0}% warm)",
                format_bytes(budget_bytes),
                format_bytes(index_size),
                ratio * 100.0
            );
            warm_files(index_path, ratio);

            // Advise OS on cold pages
            #[cfg(unix)]
            advise_cold(index_path, ratio);
        }
    }
}

/// Pre-read the first `ratio` of each file to pull pages into OS cache
fn warm_files(dir: &Path, ratio: f64) {
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
            warm_files(&path, ratio);
            continue;
        }

        if !meta.is_file() {
            continue;
        }

        let file_size = meta.len();
        let warm_bytes = (file_size as f64 * ratio) as usize;

        if warm_bytes == 0 {
            continue;
        }

        // Read the file in chunks to pull pages into cache
        if let Ok(file) = fs::File::open(&path) {
            use std::io::Read;
            let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
            let mut warmed = 0usize;
            let mut buf = [0u8; 256 * 1024];
            while warmed < warm_bytes {
                let to_read = (warm_bytes - warmed).min(buf.len());
                match reader.read(&mut buf[..to_read]) {
                    Ok(0) => break,
                    Ok(n) => warmed += n,
                    Err(_) => break,
                }
            }
        }
    }
}

/// On Unix, use madvise to tell the OS the cold portion is low-priority
#[cfg(unix)]
fn advise_cold(dir: &Path, warm_ratio: f64) {
    use std::os::unix::io::AsRawFd;

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
            advise_cold(&path, warm_ratio);
            continue;
        }

        if !meta.is_file() || meta.len() == 0 {
            continue;
        }

        let file_size = meta.len() as usize;
        let warm_bytes = (file_size as f64 * warm_ratio) as usize;
        let cold_offset = warm_bytes;
        let cold_len = file_size.saturating_sub(cold_offset);

        if cold_len == 0 {
            continue;
        }

        // mmap the file just to call madvise, then unmap
        if let Ok(file) = fs::File::open(&path) {
            let fd = file.as_raw_fd();
            unsafe {
                let ptr = libc::mmap(
                    std::ptr::null_mut(),
                    file_size,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE,
                    fd,
                    0,
                );
                if ptr != libc::MAP_FAILED {
                    // Tell OS: cold pages are low priority
                    libc::madvise(
                        (ptr as *mut u8).add(cold_offset) as *mut libc::c_void,
                        cold_len,
                        libc::MADV_DONTNEED,
                    );
                    libc::munmap(ptr, file_size);
                }
            }
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
}
