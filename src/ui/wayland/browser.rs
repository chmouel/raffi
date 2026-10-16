use std::cmp::Ordering;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use super::state::FileBrowserEntry;
use crate::FileBrowserSort;

/// Read a directory and return entries, directories first then files, each
/// group ordered by `sort`. Date sorts put the newest first; entries without
/// the timestamp go last, and ties fall back to the name. `reverse` flips the
/// order inside each group, still keeping undated entries last.
pub(super) fn read_directory(
    path: &str,
    show_hidden: bool,
    sort: FileBrowserSort,
    reverse: bool,
) -> Vec<FileBrowserEntry> {
    let dir_path = Path::new(path);
    let entries = match fs::read_dir(dir_path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        let timestamp = match sort {
            FileBrowserSort::Name => None,
            // Follow symlinks so a link sorts by its target's dates.
            FileBrowserSort::Modified => fs::metadata(entry.path())
                .or_else(|_| entry.metadata())
                .and_then(|meta| meta.modified())
                .ok(),
            FileBrowserSort::Created => fs::metadata(entry.path())
                .or_else(|_| entry.metadata())
                .and_then(|meta| meta.created())
                .ok(),
        };
        let full_path = entry.path().to_string_lossy().to_string();
        let file_browser_entry = FileBrowserEntry {
            name,
            full_path,
            is_dir,
        };

        let key = file_browser_entry.name.to_lowercase();
        if is_dir {
            dirs.push((timestamp, key, file_browser_entry));
        } else {
            files.push((timestamp, key, file_browser_entry));
        }
    }

    let order = |group: &mut Vec<(Option<SystemTime>, String, FileBrowserEntry)>| {
        group.sort_by(|(a_time, a_name, _), (b_time, b_name, _)| {
            match (a_time, b_time) {
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                _ => {}
            }
            let ordering = b_time.cmp(a_time).then_with(|| a_name.cmp(b_name));
            if reverse {
                ordering.reverse()
            } else {
                ordering
            }
        });
    };
    order(&mut dirs);
    order(&mut files);
    let entries: Vec<FileBrowserEntry> = dirs
        .into_iter()
        .chain(files)
        .map(|(_, _, entry)| entry)
        .collect();
    crate::debug_log!(
        "browser: read_directory path={path:?} show_hidden={show_hidden} sort={sort:?} reverse={reverse} entries={}",
        entries.len()
    );
    entries
}

/// Guess a mimetype icon name from a file extension.
pub(super) fn mimetype_icon_name(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "txt" | "md" | "log" | "cfg" | "conf" | "ini" | "toml" | "yaml" | "yml" | "json"
        | "xml" | "csv" | "rst" | "tex" => "text-x-generic",
        "rs" | "py" | "js" | "ts" | "go" | "c" | "cpp" | "h" | "java" | "rb" | "sh" | "bash"
        | "zsh" | "fish" | "pl" | "lua" | "hs" | "ml" | "ex" | "exs" | "clj" | "scala" | "kt"
        | "swift" | "r" | "sql" | "html" | "css" | "scss" | "less" | "jsx" | "tsx" | "vue"
        | "svelte" => "text-x-script",
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "svg" | "webp" | "ico" | "tiff" | "tif" => {
            "image-x-generic"
        }
        "mp3" | "wav" | "flac" | "ogg" | "aac" | "wma" | "m4a" | "opus" => "audio-x-generic",
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" => "video-x-generic",
        "pdf" => "application-pdf",
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" => "package-x-generic",
        "deb" | "rpm" => "package-x-generic",
        "iso" | "img" => "media-optical",
        "doc" | "docx" | "odt" | "rtf" => "x-office-document",
        "xls" | "xlsx" | "ods" => "x-office-spreadsheet",
        "ppt" | "pptx" | "odp" => "x-office-presentation",
        _ => "text-x-generic",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{mimetype_icon_name, read_directory};
    use crate::FileBrowserSort;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("raffi-{name}-{unique}"));
        fs::create_dir_all(&path).expect("failed to create temp dir");
        path
    }

    #[test]
    fn test_read_directory_sorts_directories_before_files() {
        let dir = temp_dir("browser-sort");
        fs::create_dir(dir.join("zeta")).expect("failed to create directory");
        fs::create_dir(dir.join("Alpha")).expect("failed to create directory");
        fs::write(dir.join("beta.txt"), "").expect("failed to create file");
        fs::write(dir.join("gamma.txt"), "").expect("failed to create file");

        let entries = read_directory(
            dir.to_str().expect("invalid temp dir"),
            true,
            FileBrowserSort::Name,
            false,
        );
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();

        assert_eq!(names, vec!["Alpha", "zeta", "beta.txt", "gamma.txt"]);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_read_directory_hides_dotfiles_by_default() {
        let dir = temp_dir("browser-hidden");
        fs::write(dir.join(".secret"), "").expect("failed to create hidden file");
        fs::write(dir.join("visible.txt"), "").expect("failed to create visible file");

        let hidden = read_directory(
            dir.to_str().expect("invalid temp dir"),
            false,
            FileBrowserSort::Name,
            false,
        );
        let shown = read_directory(
            dir.to_str().expect("invalid temp dir"),
            true,
            FileBrowserSort::Name,
            false,
        );

        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].name, "visible.txt");
        assert_eq!(shown.len(), 2);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_read_directory_sorts_by_modified_newest_first() {
        let dir = temp_dir("browser-mtime");
        let base = SystemTime::now();
        for (name, age) in [("old.txt", 300), ("new.txt", 10), ("mid.txt", 100)] {
            let file = fs::File::create(dir.join(name)).expect("failed to create file");
            file.set_modified(base - std::time::Duration::from_secs(age))
                .expect("failed to set mtime");
        }
        fs::create_dir(dir.join("sub")).expect("failed to create directory");

        let entries = read_directory(
            dir.to_str().expect("invalid temp dir"),
            true,
            FileBrowserSort::Modified,
            false,
        );
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();

        assert_eq!(names, vec!["sub", "new.txt", "mid.txt", "old.txt"]);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_read_directory_reverse_keeps_directories_first() {
        let dir = temp_dir("browser-reverse");
        let base = SystemTime::now();
        for (name, age) in [("old.txt", 300), ("new.txt", 10)] {
            let file = fs::File::create(dir.join(name)).expect("failed to create file");
            file.set_modified(base - std::time::Duration::from_secs(age))
                .expect("failed to set mtime");
        }
        fs::create_dir(dir.join("a-dir")).expect("failed to create directory");
        fs::create_dir(dir.join("z-dir")).expect("failed to create directory");
        let path = dir.to_str().expect("invalid temp dir");
        let names = |sort| -> Vec<String> {
            read_directory(path, true, sort, true)
                .into_iter()
                .map(|entry| entry.name)
                .collect()
        };

        assert_eq!(
            names(FileBrowserSort::Name),
            vec!["z-dir", "a-dir", "old.txt", "new.txt"]
        );
        assert_eq!(
            names(FileBrowserSort::Modified)[2..],
            ["old.txt", "new.txt"]
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_mimetype_icon_name_maps_common_extensions() {
        assert_eq!(mimetype_icon_name("/tmp/file.rs"), "text-x-script");
        assert_eq!(mimetype_icon_name("/tmp/file.pdf"), "application-pdf");
        assert_eq!(mimetype_icon_name("/tmp/file.png"), "image-x-generic");
        assert_eq!(mimetype_icon_name("/tmp/file.unknown"), "text-x-generic");
    }
}
