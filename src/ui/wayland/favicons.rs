//! Site icons for the browser tab list.
//!
//! The extension sends no favicons, so they come from the favicon database
//! of the browser that has the tab open, and failing that from a download
//! service. Each icon is cached once per host.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags};

use crate::browser_host::{self, Browser, Listing};
use crate::FaviconService;

use super::url_match::uri_host;

/// How long a host the service had no icon for is left alone.
const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(super) const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FETCHES: usize = 32;
const FETCH_THREADS: usize = 4;
const MAX_ICON_BYTES: u64 = 512 * 1024;
/// Larger icons are refused: iced decodes them at full size on the UI thread.
const MAX_ICON_SIDE: u32 = 1024;
/// Rows show icons at about this size; smaller ones look blurry.
const WANTED_WIDTH: i64 = 32;
const EXTENSIONS: [&str; 5] = ["png", "jpg", "gif", "ico", "webp"];

/// Hosts the last load found no icon for, waiting for the service.
static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The cache key for the site of `url`: its host, for web pages only.
pub(super) fn host_key(url: &str) -> Option<String> {
    let scheme = url.split_once("://")?.0;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let host = uri_host(url)?.to_ascii_lowercase();
    let valid = !host.is_empty()
        && !host.starts_with('.')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-');
    valid.then_some(host)
}

/// `name` under Raffi's cache directory.
pub(super) fn cache_dir_named(name: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("raffi").join(name))
}

/// Whether `host` is a name rather than an address, so asking a service
/// or the site about it reveals nothing about the local network.
pub(super) fn is_named(host: &str) -> bool {
    host.rsplit('.')
        .next()
        .is_some_and(|label| label.bytes().any(|byte| byte.is_ascii_alphabetic()))
}

fn cache_dir() -> Option<PathBuf> {
    cache_dir_named("favicons")
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Cached {
    Icon(PathBuf),
    /// The service had none recently.
    Miss,
    Unknown,
}

pub(super) fn cached(dir: &Path, host: &str) -> Cached {
    if let Some(path) = EXTENSIONS
        .iter()
        .map(|extension| dir.join(format!("{host}.{extension}")))
        .find(|path| path.is_file())
    {
        return Cached::Icon(path);
    }
    let recent_miss = fs::metadata(dir.join(format!("{host}.none")))
        .and_then(|meta| meta.modified())
        .is_ok_and(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age < MISS_TTL)
        });
    if recent_miss {
        Cached::Miss
    } else {
        Cached::Unknown
    }
}

/// The file extension for image `data`, or None for what rows cannot show.
/// SVG is refused: its renderer reads files the image refers to and decodes
/// embedded images without limits.
pub(super) fn extension(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG") {
        return Some("png");
    }
    if data.starts_with(b"\xff\xd8\xff") {
        return Some("jpg");
    }
    if data.starts_with(b"GIF8") {
        return Some("gif");
    }
    if data.starts_with(&[0, 0, 1, 0]) {
        return Some("ico");
    }
    (data.len() > 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP").then_some("webp")
}

/// Cache `data` as the icon of `host`, through a temporary file so a row
/// never reads half an image. Raster images too large to decode cheaply are
/// refused.
pub(super) fn store(dir: &Path, host: &str, data: &[u8]) -> Option<PathBuf> {
    let extension = extension(data)?;
    if !fits(data, MAX_ICON_SIDE) {
        return None;
    }
    fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("{host}.{extension}"));
    let temp = dir.join(format!(".{host}.{}.tmp", std::process::id()));
    let written = fs::write(&temp, data).and_then(|()| fs::rename(&temp, &path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
        return None;
    }
    let _ = fs::remove_file(dir.join(format!("{host}.none")));
    Some(path)
}

/// Whether the raster image in `data` declares at most `side` pixels each
/// way, read from its header without decoding it.
fn fits(data: &[u8], side: u32) -> bool {
    image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
        .is_some_and(|(width, height)| width <= side && height <= side)
}

pub(super) fn mark_miss(dir: &Path, host: &str) {
    if fs::create_dir_all(dir).is_ok() {
        let _ = fs::write(dir.join(format!("{host}.none")), "");
    }
}

/// The favicon databases `browser` uses: the ones its process has open,
/// else those of its profiles.
pub(super) fn databases(browser: &Browser, home: &Path, config: &Path) -> Vec<(PathBuf, bool)> {
    let mut found: Vec<(PathBuf, bool)> = Vec::new();
    if let Some(Ok(entries)) = browser
        .pid
        .map(|pid| fs::read_dir(format!("/proc/{pid}/fd")))
    {
        for link in entries
            .flatten()
            .filter_map(|fd| fs::read_link(fd.path()).ok())
        {
            let gecko = match link.file_name().and_then(|name| name.to_str()) {
                Some("favicons.sqlite") => true,
                Some("Favicons") => false,
                _ => continue,
            };
            if !found.iter().any(|(path, _)| *path == link) {
                found.push((link, gecko));
            }
        }
    }
    if found.is_empty() {
        found = browser_host::profile_favicon_databases(&browser.id, home, config);
    }
    found
}

/// Open a browser database without waiting on, or taking, its lock: the
/// browsers keep theirs locked while running. Changes still in the
/// write-ahead log are not seen.
pub(super) fn open(path: &Path) -> rusqlite::Result<Connection> {
    let mut uri = String::from("file:");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push_str("?immutable=1");
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

const GECKO_QUERY: &str = "SELECT i.data, i.width FROM moz_pages_w_icons p \
    JOIN moz_icons_to_pages ip ON ip.page_id = p.id \
    JOIN moz_icons i ON i.id = ip.icon_id \
    WHERE (p.page_url LIKE ?1 OR p.page_url LIKE ?2) AND length(i.data) > 0 \
    UNION ALL SELECT data, width FROM moz_icons \
    WHERE root = 1 AND icon_url IN (?3, ?4) AND length(data) > 0 \
    LIMIT 64";
const CHROMIUM_QUERY: &str = "SELECT b.image_data, b.width FROM icon_mapping m \
    JOIN favicon_bitmaps b ON b.icon_id = m.icon_id \
    WHERE (m.page_url LIKE ?1 OR m.page_url LIKE ?2) AND length(b.image_data) > 0 \
    LIMIT 64";

/// The icon the browser stored for pages on `host`: the smallest at least
/// [`WANTED_WIDTH`] wide, else the largest.
fn browser_icon(connection: &Connection, gecko: bool, host: &str) -> Option<Vec<u8>> {
    let pages = (format!("https://{host}/%"), format!("http://{host}/%"));
    let roots = (
        format!("https://{host}/favicon.ico"),
        format!("http://{host}/favicon.ico"),
    );
    let mut icons: Vec<(Vec<u8>, i64)> = if gecko {
        let mut statement = connection.prepare_cached(GECKO_QUERY).ok()?;
        let rows = statement
            .query_map((&pages.0, &pages.1, &roots.0, &roots.1), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .ok()?;
        rows.flatten().collect()
    } else {
        let mut statement = connection.prepare_cached(CHROMIUM_QUERY).ok()?;
        let rows = statement
            .query_map((&pages.0, &pages.1), |row| Ok((row.get(0)?, row.get(1)?)))
            .ok()?;
        rows.flatten().collect()
    };
    icons.retain(|(data, _)| data.len() as u64 <= MAX_ICON_BYTES && extension(data).is_some());
    icons.sort_by_key(|(_, width)| (*width < WANTED_WIDTH, (width - WANTED_WIDTH).abs()));
    icons.into_iter().next().map(|(data, _)| data)
}

/// The cached or browser-stored icon of each site with an open tab, keyed
/// by [`host_key`]. Sites with none are left for [`fetch_pending`].
/// Blocking: run it off the UI thread.
pub(super) fn lookup(browsers: &[(PathBuf, Listing)]) -> HashMap<String, PathBuf> {
    let (Some(dir), Ok((home, config))) = (cache_dir(), browser_host::user_dirs()) else {
        return HashMap::new();
    };
    let (icons, missing) = lookup_in(&dir, &home, &config, browsers);
    if let Ok(mut pending) = PENDING.lock() {
        *pending = missing;
    }
    icons
}

fn lookup_in(
    dir: &Path,
    home: &Path,
    config: &Path,
    browsers: &[(PathBuf, Listing)],
) -> (HashMap<String, PathBuf>, Vec<String>) {
    let mut icons: HashMap<String, PathBuf> = HashMap::new();
    let mut missing: BTreeSet<String> = BTreeSet::new();
    for (_, listing) in browsers {
        let mut unresolved: Vec<String> = Vec::new();
        let hosts: BTreeSet<String> = listing
            .tabs
            .iter()
            .filter_map(|tab| host_key(&tab.url))
            .collect();
        for host in hosts {
            if icons.contains_key(&host) {
                continue;
            }
            match cached(dir, &host) {
                Cached::Icon(path) => {
                    icons.insert(host, path);
                }
                Cached::Miss => {}
                Cached::Unknown => unresolved.push(host),
            }
        }
        if unresolved.is_empty() {
            continue;
        }
        for (database, gecko) in databases(&listing.browser, home, config) {
            let Ok(connection) = open(&database) else {
                continue;
            };
            unresolved.retain(|host| {
                match browser_icon(&connection, gecko, host)
                    .and_then(|data| store(dir, host, &data))
                {
                    Some(path) => {
                        icons.insert(host.clone(), path);
                        false
                    }
                    None => true,
                }
            });
            if unresolved.is_empty() {
                break;
            }
        }
        missing.extend(unresolved);
    }
    missing.retain(|host| !icons.contains_key(host) && is_named(host));
    (icons, missing.into_iter().collect())
}

pub(super) fn service_url(service: FaviconService, host: &str) -> Option<String> {
    match service {
        FaviconService::Twenty => Some(format!("https://twenty-icons.com/{host}/64")),
        FaviconService::Google => Some(format!(
            "https://www.google.com/s2/favicons?domain={host}&sz=64"
        )),
        FaviconService::None => None,
    }
}

/// The body at `url`, or None past `limit` bytes.
pub(super) fn download(agent: &ureq::Agent, url: &str, limit: u64) -> Option<Vec<u8>> {
    let response = agent.get(url).call().ok()?;
    let mut data = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut data)
        .ok()?;
    (data.len() as u64 <= limit).then_some(data)
}

/// Download the icons the last [`lookup`] did not find. Returns whether any
/// arrived, so the list is worth loading again. Blocking.
pub(super) fn fetch_pending(service: FaviconService) -> bool {
    let mut hosts = PENDING
        .lock()
        .map(|mut pending| std::mem::take(&mut *pending))
        .unwrap_or_default();
    let Some(dir) = cache_dir() else {
        return false;
    };
    if service == FaviconService::None || hosts.is_empty() {
        return false;
    }
    hosts.truncate(MAX_FETCHES);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .into();
    let next = AtomicUsize::new(0);
    let fetched = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..FETCH_THREADS.min(hosts.len()) {
            scope.spawn(|| {
                while let Some(host) = hosts.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let icon = service_url(service, host)
                        .and_then(|url| download(&agent, &url, MAX_ICON_BYTES))
                        .and_then(|data| store(&dir, host, &data));
                    match icon {
                        Some(_) => fetched.store(true, Ordering::Relaxed),
                        None => mark_miss(&dir, host),
                    }
                }
            });
        }
    });
    fetched.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_host::Tab;

    /// A transparent 1x1 PNG.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15\xc4\x89\x00\x00\x00\x0dIDAT\x78\x9c\x63\xf8\xcf\xc0\xf0\x1f\x00\x05\x00\x01\xff\x89\x99\x3d\x1d\x00\x00\x00\x00IEND\xae\x42\x60\x82";

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("raffi-favicons-{name}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn png(width: u8) -> Vec<u8> {
        let mut data = PNG.to_vec();
        data.push(width);
        data
    }

    fn listing(id: &str, urls: &[&str]) -> Listing {
        Listing {
            browser: Browser {
                id: id.to_string(),
                name: id.to_string(),
                pid: None,
            },
            tabs: urls
                .iter()
                .map(|url| Tab {
                    url: url.to_string(),
                    ..Tab::default()
                })
                .collect(),
        }
    }

    #[test]
    fn test_host_key_keeps_web_pages_only() {
        assert_eq!(
            host_key("https://Mail.Google.com:443/mail/u/0/#inbox").as_deref(),
            Some("mail.google.com")
        );
        assert_eq!(host_key("http://localhost:8080/"), None);
        assert_eq!(host_key("about:blank"), None);
        assert_eq!(host_key("file:///home/me/a.html"), None);
        assert_eq!(host_key("chrome://settings"), None);
        assert_eq!(host_key("moz-extension://abc/page.html"), None);
    }

    #[test]
    fn test_extension_sniffs_supported_images() {
        assert_eq!(extension(PNG), Some("png"));
        assert_eq!(extension(b"\xff\xd8\xff\xe0"), Some("jpg"));
        assert_eq!(extension(&[0, 0, 1, 0, 1]), Some("ico"));
        assert_eq!(extension(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(
            extension(b"<?xml version=\"1.0\"?>\n<svg xmlns=\"\"/>"),
            None
        );
        assert_eq!(extension(b"<!doctype html><html>"), None);
    }

    #[test]
    fn test_cache_hits_misses_and_expired_misses() {
        let dir = temp_dir("cache");
        assert_eq!(cached(&dir, "example.com"), Cached::Unknown);
        mark_miss(&dir, "example.com");
        assert_eq!(cached(&dir, "example.com"), Cached::Miss);
        let old = SystemTime::now() - MISS_TTL - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(dir.join("example.com.none"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(cached(&dir, "example.com"), Cached::Unknown);

        let path = store(&dir, "example.com", PNG).unwrap();
        assert_eq!(path, dir.join("example.com.png"));
        assert_eq!(cached(&dir, "example.com"), Cached::Icon(path));
        assert!(!dir.join("example.com.none").exists());
        assert_eq!(store(&dir, "bad.com", b"<html>"), None);
        let huge = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x10\x00\x00\x00\x10\x00\x08\x06\x00\x00\x00\xf2\xa3\x24\x17";
        assert_eq!(store(&dir, "huge.com", huge), None);
        let _ = fs::remove_dir_all(dir);
    }

    fn gecko_db(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE moz_icons (id INTEGER PRIMARY KEY, icon_url TEXT, width INTEGER, root INTEGER, data BLOB);
                 CREATE TABLE moz_pages_w_icons (id INTEGER PRIMARY KEY, page_url TEXT);
                 CREATE TABLE moz_icons_to_pages (page_id INTEGER, icon_id INTEGER);
                 INSERT INTO moz_pages_w_icons VALUES (1, 'https://mail.google.com/mail/u/0/');",
            )
            .unwrap();
        for (id, width) in [(1, 16u8), (2, 32), (3, 192)] {
            connection
                .execute(
                    "INSERT INTO moz_icons VALUES (?1, 'https://mail.google.com/icon.png', ?2, 0, ?3)",
                    (id, width, png(width)),
                )
                .unwrap();
            connection
                .execute("INSERT INTO moz_icons_to_pages VALUES (1, ?1)", [id])
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO moz_icons VALUES (9, 'https://github.com/favicon.ico', 16, 1, ?1)",
                [png(16)],
            )
            .unwrap();
    }

    fn chromium_db(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE icon_mapping (id INTEGER PRIMARY KEY, page_url TEXT, icon_id INTEGER);
                 CREATE TABLE favicon_bitmaps (id INTEGER PRIMARY KEY, icon_id INTEGER, image_data BLOB, width INTEGER);
                 INSERT INTO icon_mapping VALUES (1, 'http://news.example.org/today', 7);",
            )
            .unwrap();
        for width in [16u8, 64] {
            connection
                .execute(
                    "INSERT INTO favicon_bitmaps (icon_id, image_data, width) VALUES (7, ?1, ?2)",
                    (png(width), width),
                )
                .unwrap();
        }
    }

    #[test]
    fn test_browser_icon_picks_the_closest_size_at_least_wanted() {
        let dir = temp_dir("db");
        let gecko = dir.join("favicons sqlite");
        gecko_db(&gecko);
        let connection = open(&gecko).unwrap();
        assert_eq!(
            browser_icon(&connection, true, "mail.google.com"),
            Some(png(32))
        );
        // Root icons count for hosts with no page mapping.
        assert_eq!(browser_icon(&connection, true, "github.com"), Some(png(16)));
        assert_eq!(browser_icon(&connection, true, "google.com"), None);

        let chromium = dir.join("Favicons");
        chromium_db(&chromium);
        let connection = open(&chromium).unwrap();
        assert_eq!(
            browser_icon(&connection, false, "news.example.org"),
            Some(png(64))
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_lookup_uses_cache_then_profile_databases_and_reports_missing() {
        let home = temp_dir("lookup");
        let config = home.join(".config");
        let cache = home.join("cache");
        let profile = home.join(".mozilla/firefox/abc.default");
        fs::create_dir_all(&profile).unwrap();
        gecko_db(&profile.join("favicons.sqlite"));
        store(&cache, "cached.example", PNG).unwrap();
        mark_miss(&cache, "gone.example");

        let browsers = vec![(
            PathBuf::from("1.sock"),
            listing(
                "firefox",
                &[
                    "https://mail.google.com/mail/u/0/#inbox",
                    "https://cached.example/",
                    "https://gone.example/",
                    "https://unknown.example/",
                    "http://192.168.1.1/admin",
                    "about:blank",
                ],
            ),
        )];
        let (icons, missing) = lookup_in(&cache, &home, &config, &browsers);
        let mut hosts: Vec<&str> = icons.keys().map(String::as_str).collect();
        hosts.sort_unstable();
        assert_eq!(hosts, vec!["cached.example", "mail.google.com"]);
        assert_eq!(icons["mail.google.com"], cache.join("mail.google.com.png"));
        assert_eq!(
            fs::read(cache.join("mail.google.com.png")).unwrap(),
            png(32)
        );
        assert_eq!(missing, vec!["unknown.example"]);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn test_service_urls() {
        assert_eq!(
            service_url(FaviconService::Twenty, "github.com").as_deref(),
            Some("https://twenty-icons.com/github.com/64")
        );
        assert_eq!(
            service_url(FaviconService::Google, "github.com").as_deref(),
            Some("https://www.google.com/s2/favicons?domain=github.com&sz=64")
        );
        assert_eq!(service_url(FaviconService::None, "github.com"), None);
    }
}
