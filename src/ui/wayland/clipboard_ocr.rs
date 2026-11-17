use std::io::Cursor;
use std::time::Duration;

use image::ImageDecoder;

use super::actions::run_bounded_with_input;
use super::clipboard::image_dimensions;
use super::clipboard_store::Store;

const MAX_PIXELS: u64 = 40_000_000;
const MAX_OUTPUT: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn start(language: String) {
    std::thread::Builder::new()
        .name("raffi-clipboard-ocr".into())
        .spawn(move || loop {
            let result = Store::open_default().and_then(|mut store| {
                let Some(job) = store.next_ocr_job()? else {
                    return Ok(false);
                };
                let result = recognize(&job.data, &language);
                store.finish_ocr(job.id, &language, result)?;
                Ok(true)
            });
            match result {
                Ok(true) => {}
                Ok(false) => std::thread::sleep(Duration::from_secs(1)),
                Err(error) => {
                    eprintln!("raffi: clipboard OCR: {error}");
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        })
        .ok();
}

fn recognize(data: &[u8], language: &str) -> Result<String, String> {
    let (width, height) = image_dimensions(data).ok_or("unsupported image format")?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err("image is too large for OCR".into());
    }
    let mut decoder = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| "cannot detect image format for OCR")?
        .into_decoder()
        .map_err(|_| "cannot decode image for OCR")?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image =
        image::DynamicImage::from_decoder(decoder).map_err(|_| "cannot decode image for OCR")?;
    image.apply_orientation(orientation);
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|_| "cannot prepare image for OCR")?;
    let output = run_bounded_with_input(
        &["tesseract", "stdin", "stdout", "-l", language],
        &png.into_inner(),
        TIMEOUT,
        MAX_OUTPUT,
    )?;
    let text = String::from_utf8(output).map_err(|_| "Tesseract output is not UTF-8")?;
    Ok(text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_images() {
        assert!(recognize(b"not an image", "eng").is_err());
    }

    #[test]
    #[ignore = "requires Tesseract and its English language data"]
    fn recognizes_screenshot_text() {
        let image = include_bytes!("../../../tests/fixtures/clipboard/ocr.png");
        let text = recognize(image, "eng").unwrap();
        assert!(text.contains("RAFFI OCR SEARCH"), "{text}");
        assert!(recognize(image, "raffi_missing_language").is_err());
    }

    #[test]
    #[ignore = "requires Tesseract and its English language data"]
    fn recognizes_exif_rotated_text() {
        use image::ImageEncoder;

        let image =
            image::load_from_memory(include_bytes!("../../../tests/fixtures/clipboard/ocr.png"))
                .unwrap()
                .rotate270()
                .into_rgb8();
        let mut jpeg = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100);
        encoder
            .set_exif_metadata(
                b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00\x06\x00\x00\x00\x00\x00\x00\x00"
                    .to_vec(),
            )
            .unwrap();
        encoder.encode_image(&image).unwrap();
        let text = recognize(&jpeg, "eng").unwrap();
        assert!(text.contains("RAFFI OCR SEARCH"), "{text}");
    }
}
