use std::fs;
use std::path::Path;

use super::state::FileBrowserEntry;

/// Read a directory and return entries, with dirs sorted first then files, alphabetically.
pub(super) fn read_directory(path: &str, show_hidden: bool) -> Vec<FileBrowserEntry> {
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
        let full_path = entry.path().to_string_lossy().to_string();
        let file_browser_entry = FileBrowserEntry {
            name,
            full_path,
            is_dir,
        };

        if is_dir {
            dirs.push(file_browser_entry);
        } else {
            files.push(file_browser_entry);
        }
    }

    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    dirs.append(&mut files);
    crate::debug_log!(
        "browser: read_directory path={path:?} show_hidden={show_hidden} entries={}",
        dirs.len()
    );
    dirs
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

        let entries = read_directory(dir.to_str().expect("invalid temp dir"), true);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();

        assert_eq!(names, vec!["Alpha", "zeta", "beta.txt", "gamma.txt"]);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_read_directory_hides_dotfiles_by_default() {
        let dir = temp_dir("browser-hidden");
        fs::write(dir.join(".secret"), "").expect("failed to create hidden file");
        fs::write(dir.join("visible.txt"), "").expect("failed to create visible file");

        let hidden = read_directory(dir.to_str().expect("invalid temp dir"), false);
        let shown = read_directory(dir.to_str().expect("invalid temp dir"), true);

        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].name, "visible.txt");
        assert_eq!(shown.len(), 2);

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
