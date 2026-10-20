//! Page previews for the selected browser tab: the description and image
//! Firefox keeps in its history, else those the page itself declares.

use std::collections::HashMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Once};
use std::time::{Duration, SystemTime};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::browser_host::{self, Browser};
use crate::FaviconService;

use super::favicons::{self, Cached};
use super::preview::PreviewBody;

/// Selection must rest this long before a tab is looked up, so scrolling
/// through the list does not fetch every page on the way.
const SETTLE: Duration = Duration::from_millis(200);
const METADATA_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Cached previews unused for this long are removed, once per run.
const PRUNE_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_PAGE_BYTES: u64 = 512 * 1024;
const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024;
/// Decoding limits for a downloaded image, which may declare any size.
const MAX_IMAGE_SIDE: u32 = 8192;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;
/// Images are stored scaled down to fit this box, a little over what the
/// preview pane shows on a scaled display.
const THUMBNAIL_SIZE: (u32, u32) = (960, 720);
const MAX_DESCRIPTION_CHARS: usize = 600;
const USER_AGENT: &str = "Mozilla/5.0 (compatible; raffi)";

static META_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<meta\b[^>]*>").unwrap());
static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)([a-zA-Z_:-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))"#).unwrap()
});

/// What a page says about itself, image as a URL.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Metadata {
    description: Option<String>,
    image: Option<String>,
}

/// The page the selected row shows, shared with the lookups in flight so
/// those for rows already left stop before their next request.
#[derive(Debug, Clone, Default)]
pub(super) struct Wanted(Arc<Mutex<String>>);

impl Wanted {
    pub(super) fn set(&self, url: &str) {
        if let Ok(mut wanted) = self.0.lock() {
            if *wanted != url {
                *wanted = url.to_string();
            }
        }
    }

    /// Drop pending lookups: no tab preview is on screen.
    pub(super) fn clear(&self) {
        self.set("");
    }

    pub(super) fn is(&self, url: &str) -> bool {
        self.0.lock().is_ok_and(|wanted| *wanted == url)
    }
}

/// The preview bodies for `url`, or None when another row was selected
/// meanwhile. With `service` set to none nothing leaves the machine.
/// Blocking: run it off the UI thread.
pub(super) fn load(
    url: &str,
    browser: &Browser,
    service: FaviconService,
    wanted: &Wanted,
) -> Option<Vec<PreviewBody>> {
    std::thread::sleep(SETTLE);
    if !wanted.is(url) {
        return None;
    }
    let dir = favicons::cache_dir_named("previews");
    if let Some(dir) = &dir {
        static PRUNED: Once = Once::new();
        PRUNED.call_once(|| prune(dir));
    }
    let online = service != FaviconService::None;
    let Some(host) = favicons::host_key(url) else {
        return Some(Vec::new());
    };
    let agent = agent(true);
    let mut metadata = history(url, &host, browser).unwrap_or_default();
    if metadata.description.is_none()
        && metadata.image.is_none()
        && online
        && favicons::is_named(&host)
    {
        if let Some(dir) = &dir {
            metadata = page_metadata(dir, url, &agent, || wanted.is(url))?;
        }
    }
    let image = match metadata.image.as_deref().zip(dir.as_deref()) {
        Some((image, dir)) => image_file(dir, image, online.then_some(&agent), || wanted.is(url))?,
        None => None,
    };
    Some(bodies(metadata.description, image))
}

/// The agent for page and image requests. With `public_only` it connects
/// to public addresses only, redirects included, so a page cannot point
/// Raffi at a service on the local network or this machine; proxies are
/// not used then, since they would resolve names themselves.
fn agent(public_only: bool) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(favicons::FETCH_TIMEOUT))
        .user_agent(USER_AGENT)
        .proxy(None)
        .build();
    if public_only {
        ureq::Agent::with_parts(
            config,
            ureq::unversioned::transport::DefaultConnector::new(),
            PublicResolver::default(),
        )
    } else {
        config.into()
    }
}

/// The system resolver, keeping public addresses only.
#[derive(Debug, Default)]
struct PublicResolver(ureq::unversioned::resolver::DefaultResolver);

impl ureq::unversioned::resolver::Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let mut public = self.empty();
        for &address in &self.0.resolve(uri, config, timeout)? {
            if is_public(address.ip()) {
                public.push(address);
            }
        }
        if public.is_empty() {
            Err(ureq::Error::HostNotFound)
        } else {
            Ok(public)
        }
    }
}

/// Whether `ip` is a public internet address, not one of this machine,
/// the local network, or a reserved range.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_public_v4(mapped);
            }
            let segments = ip.segments();
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [.., high, low] = segments;
                return is_public_v4(Ipv4Addr::from((u32::from(high) << 16) | u32::from(low)));
            }
            let [first, second, ..] = segments;
            // Global unicast only, without the ranges that are reserved or
            // tunnel to IPv4 addresses (Teredo, 6to4).
            first & 0xe000 == 0x2000
                && !(first == 0x2001 && (second < 0x0200 || second == 0x0db8))
                && first != 0x2002
                && !(first == 0x3fff && second < 0x1000)
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    !(first == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || first >= 240
        || (first == 100 && second & 0xc0 == 64)
        || (first == 198 && second & 0xfe == 18)
        || (first == 192 && second == 0 && ip.octets()[2] == 0))
}

fn bodies(description: Option<String>, image: Option<PathBuf>) -> Vec<PreviewBody> {
    let mut bodies = Vec::new();
    if let Some(description) = description {
        bodies.push(PreviewBody::Prose {
            heading: None,
            content: description,
            truncated: false,
        });
    }
    if let Some(path) = image {
        bodies.push(PreviewBody::Thumbnail {
            path: path.to_string_lossy().into_owned(),
        });
    }
    bodies
}

/// The description and image Firefox stored when the page was visited.
/// Chromium browsers keep neither.
fn history(url: &str, host: &str, browser: &Browser) -> Option<Metadata> {
    let (home, config) = browser_host::user_dirs().ok()?;
    let rev_host: String = host.chars().rev().chain(['.']).collect();
    favicons::databases(browser, &home, &config)
        .into_iter()
        .filter(|(_, gecko)| *gecko)
        .find_map(|(favicons, _)| {
            let connection = favicons::open(&favicons.with_file_name("places.sqlite")).ok()?;
            history_in(&connection, url, &rev_host)
        })
}

fn history_in(connection: &rusqlite::Connection, url: &str, rev_host: &str) -> Option<Metadata> {
    let (description, image): (Option<String>, Option<String>) = connection
        .query_row(
            "SELECT description, preview_image_url FROM moz_places \
             WHERE rev_host = ?1 AND url = ?2 LIMIT 1",
            (rev_host, url),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    let metadata = Metadata {
        description: description.and_then(|text| clean(&text)),
        image: image.and_then(|image| resolve(url, &image)),
    };
    (metadata != Metadata::default()).then_some(metadata)
}

/// The metadata of the page at `url`, from the cache when recent. The page
/// is fetched without the browser's cookies, so pages behind a login give
/// what they show logged out.
fn page_metadata(
    dir: &Path,
    url: &str,
    agent: &ureq::Agent,
    wanted: impl Fn() -> bool,
) -> Option<Metadata> {
    let path = dir.join(format!("{}.json", cache_key(url)));
    let recent = fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|modified| age(modified) < METADATA_TTL);
    if recent {
        if let Some(metadata) = fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
        {
            return Some(metadata);
        }
    }
    if !wanted() {
        return None;
    }
    let metadata = fetch_head(agent, url)
        .map(|(base, head)| parse_metadata(&head, &base))
        .unwrap_or_default();
    if fs::create_dir_all(dir).is_ok() {
        if let Ok(data) = serde_json::to_vec(&metadata) {
            let _ = fs::write(&path, data);
        }
    }
    Some(metadata)
}

/// The address the page at `url` ended up at after redirects, which its
/// relative links are relative to, and the start of its HTML.
fn fetch_head(agent: &ureq::Agent, url: &str) -> Option<(String, String)> {
    let response = agent
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml")
        .call()
        .ok()?;
    let html = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("html"));
    if !html {
        return None;
    }
    let base = ureq::ResponseExt::get_uri(&response).to_string();
    let data = read_head(response.into_body().into_reader()).ok()?;
    Some((base, String::from_utf8_lossy(&data).into_owned()))
}

/// The start of a page up to its `</head>` (or `MAX_PAGE_BYTES`), so a page
/// that streams its body slowly does not run into the timeout.
fn read_head(reader: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    const END: &[u8] = b"</head>";
    let mut reader = std::io::Read::take(reader, MAX_PAGE_BYTES);
    let mut data = Vec::new();
    let mut chunk = [0; 16 * 1024];
    loop {
        let read = std::io::Read::read(&mut reader, &mut chunk)?;
        if read == 0 {
            return Ok(data);
        }
        let from = data.len().saturating_sub(END.len() - 1);
        data.extend_from_slice(&chunk[..read]);
        if data[from..]
            .windows(END.len())
            .any(|window| window.eq_ignore_ascii_case(END))
        {
            return Ok(data);
        }
    }
}

/// The description and image the `<meta>` tags of `html` declare, image
/// resolved against `url`.
fn parse_metadata(html: &str, url: &str) -> Metadata {
    let head = html
        .to_ascii_lowercase()
        .find("</head>")
        .map_or(html, |end| &html[..end]);
    let mut values: HashMap<String, String> = HashMap::new();
    for tag in META_TAG.find_iter(head) {
        let mut attributes: HashMap<String, String> = HashMap::new();
        for capture in ATTRIBUTE.captures_iter(tag.as_str()) {
            let value = capture
                .get(2)
                .or_else(|| capture.get(3))
                .or_else(|| capture.get(4))
                .map_or("", |value| value.as_str());
            attributes.insert(capture[1].to_ascii_lowercase(), decode_entities(value));
        }
        let key = attributes
            .get("property")
            .or_else(|| attributes.get("name"))
            .map(|key| key.to_ascii_lowercase());
        if let (Some(key), Some(content)) = (key, attributes.remove("content")) {
            values.entry(key).or_insert(content);
        }
    }
    let first = |keys: &[&str]| keys.iter().find_map(|key| values.get(*key).cloned());
    Metadata {
        description: first(&["og:description", "description", "twitter:description"])
            .and_then(|text| clean(&text)),
        image: first(&[
            "og:image",
            "og:image:url",
            "twitter:image",
            "twitter:image:src",
        ])
        .and_then(|image| resolve(url, &image)),
    }
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let decoded = rest[1..]
            .find(';')
            .filter(|end| *end <= 10)
            .and_then(|end| {
                let entity = &rest[1..=end];
                let character = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                        .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                        .and_then(char::from_u32),
                }?;
                Some((character, end + 2))
            });
        match decoded {
            Some((character, length)) => {
                out.push(character);
                rest = &rest[length..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `text` on one line and shortened, or None when blank.
fn clean(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= MAX_DESCRIPTION_CHARS {
        return Some(text);
    }
    let mut short: String = text.chars().take(MAX_DESCRIPTION_CHARS).collect();
    short.push('…');
    Some(short)
}

/// `href` as an absolute web address on a named host, relative to `base`.
fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    let (scheme, rest) = base.split_once("://")?;
    let path_start = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let origin = &base[..scheme.len() + 3 + path_start];
    let absolute = if href.contains("://") {
        href.to_string()
    } else if let Some(network) = href.strip_prefix("//") {
        format!("{scheme}://{network}")
    } else if href.starts_with('/') {
        format!("{origin}{href}")
    } else if href.is_empty() || href.contains(':') {
        return None;
    } else {
        let path = rest[path_start..].split(['?', '#']).next().unwrap_or("");
        let directory = path.rfind('/').map_or("/", |end| &path[..=end]);
        format!("{origin}{directory}{href}")
    };
    favicons::host_key(&absolute)
        .filter(|host| favicons::is_named(host))
        .map(|_| absolute)
}

/// The cached thumbnail of the image at `url`, downloaded first when an
/// agent is given. None when `wanted` turned false before the download.
fn image_file(
    dir: &Path,
    url: &str,
    agent: Option<&ureq::Agent>,
    wanted: impl Fn() -> bool,
) -> Option<Option<PathBuf>> {
    let key = cache_key(url);
    Some(match favicons::cached(dir, &key) {
        Cached::Icon(path) => {
            // Its age is the time since it was last shown, for prune.
            let _ = fs::File::options()
                .append(true)
                .open(&path)
                .and_then(|file| file.set_modified(SystemTime::now()));
            Some(path)
        }
        Cached::Miss => None,
        Cached::Unknown => {
            let Some(agent) = agent else {
                return Some(None);
            };
            if !wanted() {
                return None;
            }
            let stored = favicons::download(agent, url, MAX_IMAGE_BYTES)
                .and_then(|data| thumbnail(&data))
                .and_then(|data| favicons::store(dir, &key, &data));
            if stored.is_none() {
                favicons::mark_miss(dir, &key);
            }
            stored
        }
    })
}

/// `data` decoded within fixed limits and scaled down to the thumbnail
/// size, as PNG. The preview pane then never decodes a large image, and
/// formats it cannot show, such as SVG, are refused.
fn thumbnail(data: &[u8]) -> Option<Vec<u8>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let mut image = reader.decode().ok()?;
    let (width, height) = THUMBNAIL_SIZE;
    if image.width() > width || image.height() > height {
        image = image.thumbnail(width, height);
    }
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image.into_rgba8())
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// A file name for `text` that stays the same across runs (FNV-1a).
fn cache_key(text: &str) -> String {
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

fn age(time: SystemTime) -> Duration {
    SystemTime::now().duration_since(time).unwrap_or_default()
}

fn prune(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| age(modified) > PRUNE_AGE);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_head_stops_at_the_end_of_head() {
        struct Stalled;
        impl std::io::Read for Stalled {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::TimedOut.into())
            }
        }
        let page = std::io::Read::chain(
            std::io::Read::chain(&b"<html><head><meta name=x></HE"[..], &b"AD><body>"[..]),
            Stalled,
        );
        let data = read_head(page).unwrap();
        assert!(data.starts_with(b"<html><head><meta name=x></HEAD>"));
    }

    #[test]
    fn test_parse_metadata_prefers_open_graph_and_resolves_the_image() {
        let html = r#"<html><head>
            <meta name="description" content="Plain description">
            <meta property='og:description' content="Rust &amp; GUIs,
               made   simple &#x27;today&#39;">
            <meta content="/assets/card.png" property="og:image" />
            </head><body><meta property="og:image" content="https://late.example.com/x.png">"#;
        assert_eq!(
            parse_metadata(html, "https://docs.example.com/guide/intro?x=1"),
            Metadata {
                description: Some("Rust & GUIs, made simple 'today'".to_string()),
                image: Some("https://docs.example.com/assets/card.png".to_string()),
            }
        );
        assert_eq!(
            parse_metadata(
                "<meta name=description content=Short>",
                "https://a.example.com/"
            ),
            Metadata {
                description: Some("Short".to_string()),
                image: None,
            }
        );
    }

    #[test]
    fn test_resolve_keeps_web_addresses_on_named_hosts() {
        let base = "https://example.com/docs/page.html#top";
        assert_eq!(
            resolve(base, "img/a.png").as_deref(),
            Some("https://example.com/docs/img/a.png")
        );
        assert_eq!(
            resolve(base, "//cdn.example.org/a.png").as_deref(),
            Some("https://cdn.example.org/a.png")
        );
        assert_eq!(
            resolve("https://example.com", "a.png").as_deref(),
            Some("https://example.com/a.png")
        );
        assert_eq!(resolve(base, "data:image/png;base64,AAAA"), None);
        assert_eq!(resolve(base, "http://192.168.1.1/a.png"), None);
        assert_eq!(resolve(base, "http://localhost/a.png"), None);
    }

    #[test]
    fn test_clean_flattens_and_shortens() {
        assert_eq!(clean("  \n "), None);
        assert_eq!(clean(" a \n b ").as_deref(), Some("a b"));
        let long = clean(&"x".repeat(MAX_DESCRIPTION_CHARS + 5)).unwrap();
        assert_eq!(long.chars().count(), MAX_DESCRIPTION_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn test_history_reads_firefox_places() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT, rev_host TEXT, \
                 description TEXT, preview_image_url TEXT);
                 INSERT INTO moz_places (url, rev_host, description, preview_image_url) VALUES
                 ('https://github.com/', 'moc.buhtig.', ' Where   code lives ', '/og.png'),
                 ('https://github.com/empty', 'moc.buhtig.', NULL, NULL);",
            )
            .unwrap();
        assert_eq!(
            history_in(&connection, "https://github.com/", "moc.buhtig."),
            Some(Metadata {
                description: Some("Where code lives".to_string()),
                image: Some("https://github.com/og.png".to_string()),
            })
        );
        assert_eq!(
            history_in(&connection, "https://github.com/empty", "moc.buhtig."),
            None
        );
        assert_eq!(
            history_in(&connection, "https://github.com/other", "moc.buhtig."),
            None
        );
    }

    #[test]
    fn test_load_skips_rows_no_longer_selected() {
        let browser = Browser {
            id: "firefox".to_string(),
            name: "Firefox".to_string(),
            pid: None,
        };
        let wanted = Wanted::default();
        wanted.set("https://example.com/b");
        assert_eq!(
            load(
                "https://example.com/a",
                &browser,
                FaviconService::None,
                &wanted
            ),
            None
        );
        wanted.set("about:blank");
        assert_eq!(
            load("about:blank", &browser, FaviconService::None, &wanted),
            Some(Vec::new())
        );
        let cleared = wanted.clone();
        let clear = std::thread::spawn(move || {
            std::thread::sleep(SETTLE / 4);
            cleared.clear();
        });
        assert_eq!(
            load("about:blank", &browser, FaviconService::None, &wanted),
            None
        );
        clear.join().unwrap();
    }

    #[test]
    fn test_prune_keeps_previews_shown_this_week() {
        let dir = std::env::temp_dir().join(format!("raffi-previews-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let old = SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
        let shown = "https://example.com/shown.png";
        for name in [
            format!("{}.png", cache_key(shown)),
            "unused.png".to_string(),
        ] {
            let file = dir.join(name);
            fs::write(&file, tiny_png()).unwrap();
            fs::File::options()
                .append(true)
                .open(&file)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        assert!(image_file(&dir, shown, None, || true).unwrap().is_some());
        prune(&dir);
        assert!(dir.join(format!("{}.png", cache_key(shown))).exists());
        assert!(!dir.join("unused.png").exists());
        let _ = fs::remove_dir_all(dir);
    }

    fn tiny_png() -> Vec<u8> {
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    #[test]
    fn test_cache_key_is_stable() {
        assert_eq!(cache_key(""), "cbf29ce484222325");
        assert_eq!(cache_key("a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn test_only_public_addresses_are_reached() {
        for address in [
            "93.184.215.14",
            "2606:2800:21f:cb07:6820:80da:af6b:8b2c",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(is_public(address.parse().unwrap()), "{address}");
        }
        for address in [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
            "64:ff9b::a00:1",
            "64:ff9b:1::a00:1",
            "2001:db8::1",
            "2001::1",
            "2002:a00:1::1",
            "3fff::1",
            "100::1",
            "::a00:1",
        ] {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
    }

    /// A server answering each connection with the next of `responses`,
    /// and the number of connections it took.
    fn serve(responses: impl FnOnce(u16) -> Vec<String>) -> (u16, std::thread::JoinHandle<usize>) {
        let listener = std::net::TcpListener::bind("localhost:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let responses = responses(port);
        let server = std::thread::spawn(move || {
            let mut served = 0;
            let until = std::time::Instant::now() + Duration::from_secs(3);
            while served < responses.len() && std::time::Instant::now() < until {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut request = [0; 4096];
                let _ = std::io::Read::read(&mut stream, &mut request);
                let _ = std::io::Write::write_all(&mut stream, responses[served].as_bytes());
                served += 1;
            }
            served
        });
        (port, server)
    }

    fn html_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn test_pages_on_this_machine_are_not_fetched() {
        let (port, server) = serve(|_| {
            vec![html_response(
                r#"<head><meta name="description" content="private"></head>"#,
            )]
        });
        for host in ["127.0.0.1", "localhost"] {
            assert_eq!(
                fetch_head(&agent(true), &format!("http://{host}:{port}/")),
                None
            );
        }
        assert_eq!(server.join().unwrap(), 0);
    }

    #[test]
    fn test_pages_are_read_from_where_redirects_end() {
        let (port, server) = serve(|port| {
            vec![
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/new/path/page\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                html_response(r#"<head><meta property="og:image" content="card.png"></head>"#),
            ]
        });
        let (base, head) =
            fetch_head(&agent(false), &format!("http://localhost:{port}/old")).unwrap();
        assert_eq!(server.join().unwrap(), 2);
        assert_eq!(base, format!("http://localhost:{port}/new/path/page"));
        assert!(head.contains("card.png"));
    }

    #[test]
    fn test_thumbnails_are_bounded_and_svg_is_refused() {
        let mut large = Vec::new();
        image::DynamicImage::new_rgb8(2000, 1000)
            .write_to(
                &mut std::io::Cursor::new(&mut large),
                image::ImageFormat::Png,
            )
            .unwrap();
        let small = image::load_from_memory(&thumbnail(&large).unwrap()).unwrap();
        assert_eq!((small.width(), small.height()), (960, 480));

        // A header declaring 20000x20000 pixels, refused before decoding.
        let huge = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x4e\x20\x00\x00\x4e\x20\x08\x06\x00\x00\x00\xe3\x70\x46\x39";
        assert_eq!(thumbnail(huge), None);
        assert_eq!(
            thumbnail(br#"<svg xmlns="http://www.w3.org/2000/svg"/>"#),
            None
        );
    }
}
