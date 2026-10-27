//! Small raster copies of icons. iced decodes an image in full to lay
//! it out and scales it down on every draw, so a 1024px icon shown at 40px
//! costs milliseconds on each launch, and its SVG renderer scans every system
//! font before drawing the first SVG. Thumbnails are written in the background
//! and used from the next launch on.

use std::cell::{OnceCell, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use resvg::usvg::fontdb;

/// Largest side kept: enough for the 40px rows on a 3x display.
const SIZE: u32 = 128;

/// Bump when thumbnail output changes so older files are not reused.
const VERSION: u32 = 3;

pub(crate) const DIR_NAME: &str = "icon-thumbnails";

/// Point SVG and oversized raster icons at their thumbnails and create the
/// missing ones on a background thread.
pub(super) fn substitute(paths: &mut [Option<String>]) {
    let Some(dir) = thumbnail_dir() else {
        return;
    };
    for (source, thumbnail) in substitute_in(&dir, paths) {
        queue(source, thumbnail);
    }
}

thread_local! {
    static SVG_THUMBNAILS: RefCell<HashMap<PathBuf, Option<PathBuf>>> =
        RefCell::new(HashMap::new());
}

/// The thumbnail to draw instead of an SVG icon resolved by the view, or
/// `None` until one is written, in which case it is queued. Looked up once
/// per icon and process, since the view runs on every update.
pub(super) fn for_svg(source: &Path) -> Option<PathBuf> {
    SVG_THUMBNAILS.with_borrow_mut(|thumbnails| {
        if let Some(thumbnail) = thumbnails.get(source) {
            return thumbnail.clone();
        }
        let thumbnail = thumbnail_dir()
            .and_then(|dir| thumbnail_path(&dir, source))
            .and_then(|thumbnail| {
                if thumbnail.exists() {
                    return Some(thumbnail);
                }
                queue(source.to_path_buf(), thumbnail);
                None
            });
        thumbnails.insert(source.to_path_buf(), thumbnail.clone());
        thumbnail
    })
}

/// Write `thumbnail` on the one background thread that writes them all, so
/// system fonts are loaded at most once per process.
fn queue(source: PathBuf, thumbnail: PathBuf) {
    static WORKER: OnceLock<Sender<(PathBuf, PathBuf)>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<(PathBuf, PathBuf)>();
        std::thread::spawn(move || {
            if let Some(dir) = thumbnail_dir() {
                remove_stale_partials(&dir);
            }
            let fonts = OnceCell::new();
            for (source, thumbnail) in receiver {
                if thumbnail.exists() {
                    continue;
                }
                if let Err(error) = write_thumbnail(&source, &thumbnail, &fonts) {
                    crate::debug_log!("icons: no thumbnail for {}: {error}", source.display());
                }
            }
        });
        sender
    });
    let _ = worker.send((source, thumbnail));
}

fn thumbnail_dir() -> Option<PathBuf> {
    let dir = crate::ui::get_mru_cache_path()
        .ok()?
        .with_file_name(DIR_NAME);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Drop partial files left behind when a previous Raffi exited mid-write.
fn remove_stale_partials(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = SystemTime::now() - Duration::from_secs(60);
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified < cutoff);
        if stale && path.extension().is_some_and(|ext| ext == "tmp") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Swap in existing thumbnails and return the `(source, thumbnail)` pairs
/// still to write.
fn substitute_in(dir: &Path, paths: &mut [Option<String>]) -> Vec<(PathBuf, PathBuf)> {
    let mut missing = Vec::new();
    for path in paths.iter_mut().flatten() {
        let source = PathBuf::from(path.as_str());
        let Some(thumbnail) = thumbnail_path(dir, &source) else {
            continue;
        };
        if thumbnail.exists() {
            *path = thumbnail.to_string_lossy().into_owned();
        } else if is_svg(&source) || oversized(&source) {
            missing.push((source, thumbnail));
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

fn is_svg(source: &Path) -> bool {
    source
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
}

/// The thumbnail location for an icon, keyed on its path, size and
/// modification time so a replaced icon gets a fresh thumbnail.
fn thumbnail_path(dir: &Path, source: &Path) -> Option<PathBuf> {
    if !is_svg(source) && image::ImageFormat::from_path(source).is_err() {
        return None;
    }
    let metadata = std::fs::metadata(source).ok()?;
    let mut hasher = DefaultHasher::new();
    VERSION.hash(&mut hasher);
    source.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    metadata.modified().ok()?.hash(&mut hasher);
    Some(dir.join(format!("{:016x}.png", hasher.finish())))
}

fn oversized(source: &Path) -> bool {
    image::image_dimensions(source).is_ok_and(|(width, height)| width.max(height) > SIZE)
}

fn write_thumbnail(
    source: &Path,
    thumbnail: &Path,
    fonts: &OnceCell<Arc<fontdb::Database>>,
) -> Result<(), String> {
    let image = if is_svg(source) {
        rasterize_svg(source, fonts)?
    } else {
        image::open(source)
            .map_err(|error| error.to_string())?
            .thumbnail(SIZE, SIZE)
    };
    let partial = thumbnail.with_extension(format!("{}.tmp", std::process::id()));
    image
        .save_with_format(&partial, image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    std::fs::rename(&partial, thumbnail).map_err(|error| error.to_string())
}

/// Point generic families such as `sans-serif` at installed fonts. fontdb
/// takes the first font fontconfig prefers for them even when it is not
/// installed, and usvg then drops the text.
fn use_installed_generic_families(database: &mut fontdb::Database) {
    const SANS: &[&str] = &["DejaVu Sans", "Noto Sans", "Liberation Sans", "Cantarell"];
    const SERIF: &[&str] = &["DejaVu Serif", "Noto Serif", "Liberation Serif"];
    const MONO: &[&str] = &["DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono"];
    let installed = |database: &fontdb::Database, family: fontdb::Family| {
        database
            .query(&fontdb::Query {
                families: &[family],
                ..Default::default()
            })
            .is_some()
    };
    let pick = |database: &fontdb::Database, names: &[&str]| -> Option<String> {
        names
            .iter()
            .find(|name| installed(database, fontdb::Family::Name(name)))
            .map(|name| name.to_string())
            .or_else(|| {
                database
                    .faces()
                    .find_map(|face| face.families.first().map(|(name, _)| name.clone()))
            })
    };
    type Setter = fn(&mut fontdb::Database, String);
    let setters: [(fontdb::Family, &[&str], Setter); 5] = [
        (fontdb::Family::SansSerif, SANS, |db, name| {
            db.set_sans_serif_family(name)
        }),
        (fontdb::Family::Serif, SERIF, |db, name| {
            db.set_serif_family(name)
        }),
        (fontdb::Family::Monospace, MONO, |db, name| {
            db.set_monospace_family(name)
        }),
        (fontdb::Family::Cursive, SANS, |db, name| {
            db.set_cursive_family(name)
        }),
        (fontdb::Family::Fantasy, SANS, |db, name| {
            db.set_fantasy_family(name)
        }),
    ];
    for (family, names, set) in setters {
        if !installed(database, family) {
            if let Some(name) = pick(database, names) {
                set(database, name);
            }
        }
    }
}

/// Render an SVG so its longest side is [`SIZE`]. System fonts are loaded
/// once per batch so SVG text renders as it would in iced.
fn rasterize_svg(
    source: &Path,
    fonts: &OnceCell<Arc<fontdb::Database>>,
) -> Result<image::DynamicImage, String> {
    use resvg::{tiny_skia, usvg};

    let data = std::fs::read(source).map_err(|error| error.to_string())?;
    let fontdb = fonts
        .get_or_init(|| {
            let mut database = fontdb::Database::new();
            database.load_system_fonts();
            use_installed_generic_families(&mut database);
            Arc::new(database)
        })
        .clone();
    let options = usvg::Options {
        resources_dir: source.parent().map(Path::to_path_buf),
        fontdb,
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(&data, &options).map_err(|error| error.to_string())?;
    let size = tree.size();
    let scale = SIZE as f32 / size.width().max(size.height());
    let width = (size.width() * scale).round().max(1.0) as u32;
    let height = (size.height() * scale).round().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("empty SVG")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let pixels = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect();
    image::RgbaImage::from_raw(width, height, pixels)
        .map(image::DynamicImage::ImageRgba8)
        .ok_or_else(|| "invalid SVG raster".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_and_oversized_raster_icons_use_a_thumbnail_once_written() {
        let dir = std::env::temp_dir().join(format!("raffi-thumbs-{}", std::process::id()));
        let thumbs = dir.join("thumbs");
        std::fs::create_dir_all(&thumbs).unwrap();
        let large = dir.join("large.png");
        let small = dir.join("small.png");
        let vector = dir.join("icon.svg");
        image::RgbaImage::new(600, 300).save(&large).unwrap();
        image::RgbaImage::new(64, 64).save(&small).unwrap();
        std::fs::write(
            &vector,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="32"><rect width="16" height="32" fill="red"/></svg>"#,
        )
        .unwrap();
        let originals: Vec<Option<String>> = [&large, &small, &vector]
            .iter()
            .map(|path| Some(path.to_string_lossy().into_owned()))
            .chain([None])
            .collect();

        let mut paths = originals.clone();
        let mut missing = substitute_in(&thumbs, &mut paths);
        assert_eq!(paths, originals);
        missing.sort_by_key(|(source, _)| source != &large);
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0].0, large);
        assert_eq!(missing[1].0, vector);

        for (source, thumbnail) in &missing {
            write_thumbnail(source, thumbnail, &OnceCell::new()).unwrap();
        }
        assert_eq!(image::image_dimensions(&missing[0].1).unwrap(), (128, 64));
        let svg = image::open(&missing[1].1).unwrap().to_rgba8();
        assert_eq!(svg.dimensions(), (64, 128));
        assert_eq!(svg.get_pixel(32, 64).0, [255, 0, 0, 255]);

        assert!(substitute_in(&thumbs, &mut paths).is_empty());
        assert_eq!(paths[0].as_deref(), missing[0].1.to_str());
        assert_eq!(paths[1], originals[1]);
        assert_eq!(paths[2].as_deref(), missing[1].1.to_str());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn svg_text_survives_thumbnailing() {
        let dir = std::env::temp_dir().join(format!("raffi-thumbs-text-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let vector = dir.join("text.svg");
        std::fs::write(
            &vector,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><text x="0" y="56" font-size="64" font-family="sans-serif">W</text></svg>"#,
        )
        .unwrap();
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        if database.is_empty() {
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        // As when fontconfig prefers fonts that are not installed.
        database.set_sans_serif_family("Raffi Missing Sans");
        database.set_serif_family("Raffi Missing Serif");
        use_installed_generic_families(&mut database);
        let fonts = OnceCell::from(Arc::new(database));
        let image = rasterize_svg(&vector, &fonts).unwrap().to_rgba8();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(image.pixels().any(|pixel| pixel.0[3] > 0));
    }
}
