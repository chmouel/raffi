//! Keeps the files Raffi reads at startup in the page cache.
//!
//! A cold launch is slow because of disk reads, not work: the binary, the
//! Vulkan and Mesa libraries, and the headers of every system font, which
//! iced scans before it lays out any text. Once the window is up, the UI
//! records the pages it touched in its mapped files and the small files it
//! read. The cache service re-reads them and replays the font scan on a
//! timer, so the kernel keeps them cached between launches.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
const FILE_NAME: &str = "hot-files.json";
pub(super) const INTERVAL: Duration = Duration::from_secs(180);
/// Upper bound on one pass, so a stale or odd list cannot read gigabytes.
const MAX_BYTES: u64 = 512 * 1024 * 1024;
/// Small files next to the config and in the cache directory are read whole.
const MAX_SMALL_FILE: u64 = 4 * 1024 * 1024;
const CHUNK: usize = 256 * 1024;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct HotFiles {
    version: u32,
    files: Vec<HotFile>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct HotFile {
    path: PathBuf,
    /// `(offset, length)` byte ranges; empty means the whole file.
    #[serde(default)]
    ranges: Vec<(u64, u64)>,
}

#[derive(Debug, PartialEq)]
struct Mapping {
    start: u64,
    end: u64,
    offset: u64,
    path: PathBuf,
}

fn list_path() -> anyhow::Result<PathBuf> {
    Ok(super::cache::cache_root()?.join(FILE_NAME))
}

pub(super) fn clear() {
    if let Ok(path) = list_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Parse one `/proc/self/maps` line into a file-backed mapping.
fn parse_maps_line(line: &str) -> Option<Mapping> {
    let mut rest = line;
    let mut fields = [""; 5];
    for field in &mut fields {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        *field = &rest[..end];
        rest = &rest[end..];
    }
    let [range, _perms, offset, _dev, inode] = fields;
    let path = rest.trim();
    if inode == "0" || !path.starts_with('/') || path.ends_with(" (deleted)") {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    Some(Mapping {
        start: u64::from_str_radix(start, 16).ok()?,
        end: u64::from_str_radix(end, 16).ok()?,
        offset: u64::from_str_radix(offset, 16).ok()?,
        path: PathBuf::from(path),
    })
}

/// File byte ranges for the pages of a mapping marked in `touched`.
fn ranges_from_pages(touched: &[bool], offset: u64, page: u64) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for (index, _) in touched.iter().enumerate().filter(|(_, touched)| **touched) {
        let start = offset + index as u64 * page;
        match ranges.last_mut() {
            Some((from, len)) if *from + *len == start => *len += page,
            _ => ranges.push((start, page)),
        }
    }
    ranges
}

/// Sort and merge overlapping or touching ranges.
fn merge_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, len) in ranges {
        match merged.last_mut() {
            Some((from, current)) if start <= *from + *current => {
                *current = (*current).max(start + len - *from);
            }
            _ => merged.push((start, len)),
        }
    }
    merged
}

/// Pages of `mapping` present in this process's page tables, read from
/// `/proc/self/pagemap` (bit 63 of each entry). Unlike page cache residency,
/// this leaves out pages of shared libraries that only other programs use.
fn touched_ranges(pagemap: &File, mapping: &Mapping, page: u64) -> Vec<(u64, u64)> {
    let pages = mapping.end.saturating_sub(mapping.start) / page;
    let Ok(len) = usize::try_from(pages * 8) else {
        return Vec::new();
    };
    let mut entries = vec![0u8; len];
    if pagemap
        .read_exact_at(&mut entries, mapping.start / page * 8)
        .is_err()
    {
        return Vec::new();
    }
    let touched: Vec<bool> = entries
        .as_chunks::<8>()
        .0
        .iter()
        .map(|entry| u64::from_le_bytes(*entry) >> 63 == 1)
        .collect();
    ranges_from_pages(&touched, mapping.offset, page)
}

fn small_files_in(dir: &Path, filter: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| filter(path))
        .filter(|path| {
            std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_SMALL_FILE)
        })
        .collect()
}

/// Record what this process needed to start: the touched pages of its
/// mapped files, then whole small files: `icons`, the YAML files next to
/// `config_path`, the cache directory and the desktop entries.
pub(super) fn record(config_path: Option<PathBuf>, icons: Vec<PathBuf>) {
    let started = Instant::now();
    let Ok(target) = list_path() else {
        return;
    };
    // SAFETY: sysconf only reads a system constant.
    let page = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        size if size > 0 => size as u64,
        _ => 4096,
    };
    let mut mapped: BTreeMap<PathBuf, Vec<(u64, u64)>> = BTreeMap::new();
    if let (Ok(maps), Ok(pagemap)) = (
        std::fs::read_to_string("/proc/self/maps"),
        File::open("/proc/self/pagemap"),
    ) {
        for mapping in maps.lines().filter_map(parse_maps_line) {
            if !mapping.path.is_file() {
                continue;
            }
            let ranges = touched_ranges(&pagemap, &mapping, page);
            if !ranges.is_empty() {
                mapped.entry(mapping.path).or_default().extend(ranges);
            }
        }
    }
    let mut files: Vec<HotFile> = mapped
        .into_iter()
        .map(|(path, ranges)| HotFile {
            path,
            ranges: merge_ranges(ranges),
        })
        .collect();

    let mut whole: Vec<PathBuf> = icons;
    if let Some(dir) = config_path.as_deref().and_then(Path::parent) {
        whole.extend(small_files_in(dir, |path| {
            path.extension().is_some_and(|ext| ext == "yaml")
        }));
    }
    if let Ok(root) = super::cache::cache_root() {
        whole.extend(small_files_in(&root, |path| path != target));
    }
    for dir in crate::desktop_entry_dirs() {
        whole.extend(small_files_in(&dir, |path| {
            path.extension().is_some_and(|ext| ext == "desktop")
        }));
    }
    let mut seen: HashSet<PathBuf> = files.iter().map(|file| file.path.clone()).collect();
    whole.retain(|path| seen.insert(path.clone()));
    files.extend(whole.into_iter().map(|path| HotFile {
        path,
        ranges: Vec::new(),
    }));

    let list = HotFiles {
        version: VERSION,
        files,
    };
    if let Err(error) = super::cache::write_private_json(&target, &list) {
        crate::debug_log!("hot files: cannot write {}: {error:#}", target.display());
        return;
    }
    crate::debug_log!(
        "hot files: recorded {} files in {}ms",
        list.files.len(),
        started.elapsed().as_millis()
    );
}

/// Read `file`'s ranges into `buffer`, spending at most `budget` bytes.
fn read_file(file: &HotFile, buffer: &mut [u8], budget: &mut u64) -> u64 {
    let Ok(handle) = File::open(&file.path) else {
        return 0;
    };
    let whole;
    let ranges = if file.ranges.is_empty() {
        whole = [(0, handle.metadata().map_or(0, |meta| meta.len()))];
        &whole[..]
    } else {
        &file.ranges[..]
    };
    let mut total = 0;
    for &(offset, len) in ranges {
        let end = offset.saturating_add(len);
        let mut position = offset;
        while position < end && *budget > 0 {
            let want = (end - position).min(*budget).min(buffer.len() as u64) as usize;
            match handle.read_at(&mut buffer[..want], position) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    position += read as u64;
                    *budget -= read as u64;
                    total += read as u64;
                }
            }
        }
    }
    total
}

/// Re-read the recorded files and replay the system font scan.
pub(super) fn warm() {
    let started = Instant::now();
    let mut total = 0;
    let list = list_path()
        .ok()
        .and_then(|path| super::cache::read_private_json::<HotFiles>(&path))
        .filter(|list| list.version == VERSION);
    if let Some(list) = &list {
        let mut budget = MAX_BYTES;
        let mut buffer = vec![0; CHUNK];
        for file in &list.files {
            total += read_file(file, &mut buffer, &mut budget);
        }
    }
    // The same scan iced runs on startup: it touches the header pages of
    // every installed font, which is most of a cold launch.
    let mut fonts = resvg::usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    crate::debug_log!(
        "hot files: read {} files ({} KiB) and {} fonts in {}ms",
        list.as_ref().map_or(0, |list| list.files.len()),
        total / 1024,
        fonts.len(),
        started.elapsed().as_millis()
    );
}

/// Warm the startup files now and then every `INTERVAL`.
pub(super) fn keep_warm() {
    std::thread::spawn(|| loop {
        warm();
        std::thread::sleep(INTERVAL);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_lines_keep_only_named_files() {
        assert_eq!(
            parse_maps_line(
                "7f00a000-7f00c000 r--p 00002000 103:02 1234   /usr/share/fonts/My Font.ttf"
            ),
            Some(Mapping {
                start: 0x7f00a000,
                end: 0x7f00c000,
                offset: 0x2000,
                path: PathBuf::from("/usr/share/fonts/My Font.ttf"),
            })
        );
        assert_eq!(
            parse_maps_line("7f00a000-7f00c000 rw-p 00000000 00:00 0   [heap]"),
            None
        );
        assert_eq!(
            parse_maps_line("7f00a000-7f00c000 rw-s 00000000 00:01 77   /memfd:pool (deleted)"),
            None
        );
        assert_eq!(
            parse_maps_line("7f00a000-7f00c000 rw-p 00000000 00:00 0"),
            None
        );
    }

    #[test]
    fn touched_pages_become_file_ranges() {
        assert_eq!(
            ranges_from_pages(
                &[true, true, false, true, false, false, true],
                0x1000,
                0x1000
            ),
            vec![(0x1000, 0x2000), (0x4000, 0x1000), (0x7000, 0x1000)]
        );
        assert_eq!(
            merge_ranges(vec![(8, 4), (0, 4), (4, 2), (20, 1), (9, 1)]),
            vec![(0, 6), (8, 4), (20, 1)]
        );
    }

    #[test]
    fn reading_stops_at_the_budget_and_end_of_file() {
        let dir = std::env::temp_dir().join(format!("raffi-hot-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data");
        std::fs::write(&path, vec![7u8; 10_000]).unwrap();
        let mut buffer = vec![0; 1024];

        let mut budget = 2_500;
        let ranged = HotFile {
            path: path.clone(),
            ranges: vec![(0, 2_000), (8_000, 4_096)],
        };
        assert_eq!(read_file(&ranged, &mut buffer, &mut budget), 2_500);
        assert_eq!(budget, 0);

        let mut budget = u64::MAX;
        assert_eq!(read_file(&ranged, &mut buffer, &mut budget), 4_000);

        let mut budget = u64::MAX;
        let whole = HotFile {
            path,
            ranges: Vec::new(),
        };
        assert_eq!(read_file(&whole, &mut buffer, &mut budget), 10_000);
        let _ = std::fs::remove_dir_all(dir);
    }
}
