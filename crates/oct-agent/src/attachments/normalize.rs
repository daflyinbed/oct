//! Shared ingress normalization for uploads and tool-result images. The
//! canonical derivative is immutable: provider/model changes never resize it.
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits};
use std::io::Cursor;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

pub const NORMALIZER_VERSION: i64 = 2;
pub const MAX_IMAGE_PIXELS: u64 = 40_000_000;
pub const MAX_IMAGE_DIMENSION: u32 = 2_000;
pub const MAX_MODEL_IMAGE_BYTES: usize = 3 * 1024 * 1024;

#[derive(Debug)]
pub struct NormalizedImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
}

pub fn image_mime(mime: &str) -> Option<&'static str> {
    match mime.split(';').next()?.trim().to_ascii_lowercase().as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

pub fn unsupported_mime_reason(mime: &str) -> Option<&'static str> {
    let mime = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mime.starts_with("audio/") || mime.starts_with("video/") {
        Some("暂不支持音视频文件")
    } else if mime == "application/pdf" {
        Some("暂不支持 PDF 文件")
    } else if mime.starts_with("image/") && image_mime(&mime).is_none() {
        Some("不支持的图片类型（支持 PNG/JPEG/WebP/GIF）")
    } else {
        None
    }
}

pub fn validate_text(bytes: &[u8]) -> Result<(), String> {
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err("仅支持图片与文本文件（内容看起来是二进制）".into());
    }
    std::str::from_utf8(bytes).map_err(|_| "仅支持图片与 UTF-8 文本文件".to_string())?;
    Ok(())
}

/// Bound both concurrent decoder memory and work queued on the blocking pool.
/// The permit moves into the task so cancellation cannot release it early.
pub async fn normalize_image(bytes: Vec<u8>, mime: String) -> Result<NormalizedImage, String> {
    static DECODERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permit = DECODERS
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        normalize_image_sync(bytes, &mime)
    })
    .await
    .map_err(|e| format!("图片处理失败: {e}"))?
}

fn normalize_image_sync(bytes: Vec<u8>, mime: &str) -> Result<NormalizedImage, String> {
    let declared = image_mime(mime).ok_or("不支持的图片类型")?;
    let format = image::guess_format(&bytes).map_err(|_| "无法识别图片格式")?;
    let detected = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => return Err("不支持的图片格式".into()),
    };
    if declared != detected {
        return Err("文件内容与声明的图片类型不符".into());
    }
    let (width, height) = ImageReader::with_format(Cursor::new(&bytes), format)
        .into_dimensions()
        .map_err(|e| format!("图片已损坏: {e}"))?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err("图片像素超过上限（40 百万像素）".into());
    }
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    reader.limits(limits.clone());
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("图片已损坏: {e}"))?;
    // Preserve ImageReader::decode's allocation reservation when taking the
    // decoder directly so its orientation can be read before decoding.
    limits
        .reserve(decoder.total_bytes())
        .map_err(|e| format!("图片已损坏: {e}"))?;
    decoder
        .set_limits(limits)
        .map_err(|e| format!("图片已损坏: {e}"))?;
    let orientation = decoder
        .orientation()
        .map_err(|e| format!("图片已损坏: {e}"))?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| format!("图片已损坏: {e}"))?;
    if width <= MAX_IMAGE_DIMENSION
        && height <= MAX_IMAGE_DIMENSION
        && bytes.len() <= MAX_MODEL_IMAGE_BYTES
        && format != ImageFormat::Gif
    {
        return Ok(NormalizedImage {
            bytes,
            mime_type: detected.into(),
            width,
            height,
        });
    }
    // Reencoding drops EXIF metadata. Bake its display orientation into the
    // derivative's pixels; passthrough originals retain their metadata above.
    image.apply_orientation(orientation);
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        image = image.resize(
            MAX_IMAGE_DIMENSION,
            MAX_IMAGE_DIMENSION,
            image::imageops::FilterType::Lanczos3,
        );
    }
    // PNG preserves alpha and small diagrams. JPEG is the bounded fallback
    // for large photographs; only the stable derivative changes encoding.
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    if png.get_ref().len() <= MAX_MODEL_IMAGE_BYTES {
        let (width, height) = image.dimensions();
        return Ok(NormalizedImage {
            bytes: png.into_inner(),
            mime_type: "image/png".into(),
            width,
            height,
        });
    }
    loop {
        let rgb = image.to_rgb8();
        for quality in [85, 70, 50] {
            let mut encoded = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
                .encode_image(&rgb)
                .map_err(|e| e.to_string())?;
            if encoded.len() <= MAX_MODEL_IMAGE_BYTES {
                return Ok(NormalizedImage {
                    bytes: encoded,
                    mime_type: "image/jpeg".into(),
                    width: rgb.width(),
                    height: rgb.height(),
                });
            }
        }
        let (width, height) = image.dimensions();
        if width <= 256 && height <= 256 {
            return Err("图片无法压缩到模型输入上限".into());
        }
        image = image.resize(
            (width * 3 / 4).max(1),
            (height * 3 / 4).max(1),
            image::imageops::FilterType::Lanczos3,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg_with_orientation(width: u32, height: u32, orientation: u8) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, _| {
            if x < width / 2 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            }
        });
        let mut jpeg = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut jpeg, ImageFormat::Jpeg)
            .unwrap();
        let jpeg = jpeg.into_inner();
        // Minimal little-endian EXIF IFD with one SHORT Orientation tag.
        let mut exif =
            b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x01\0\0\0\0\0\0\0".to_vec();
        exif[24] = orientation;
        let mut bytes = jpeg[..2].to_vec();
        bytes.extend_from_slice(&[0xff, 0xe1]);
        bytes.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(&exif);
        bytes.extend_from_slice(&jpeg[2..]);
        bytes
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(width, height)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }
    #[tokio::test]
    async fn validates_the_full_image_and_rejects_mime_mismatches() {
        assert!(
            normalize_image(b"\x89PNG\r\n\x1a\nfake".to_vec(), "image/png".into())
                .await
                .is_err()
        );
        assert!(
            normalize_image(png(1, 1), "image/jpeg".into())
                .await
                .is_err()
        );
        let bytes = png(1, 1);
        let normalized = normalize_image(bytes.clone(), "IMAGE/PNG; ignored=1".into())
            .await
            .unwrap();
        assert_eq!(normalized.bytes, bytes);
        assert_eq!(normalized.mime_type, "image/png");
    }
    #[tokio::test]
    async fn creates_a_stable_bounded_derivative_without_changing_the_original() {
        let bytes = png(2500, 2);
        let normalized = normalize_image(bytes.clone(), "image/png".into())
            .await
            .unwrap();
        assert!(normalized.width <= MAX_IMAGE_DIMENSION);
        assert!(normalized.height <= MAX_IMAGE_DIMENSION);
        assert!(normalized.bytes.len() <= MAX_MODEL_IMAGE_BYTES);
        let again = normalize_image(bytes, "image/png".into()).await.unwrap();
        assert_eq!(normalized.bytes, again.bytes);
    }

    #[tokio::test]
    async fn reencoding_applies_exif_rotation_and_mirroring_to_pixels() {
        for orientation in [6, 2] {
            let normalized = normalize_image(
                jpeg_with_orientation(3000, 1000, orientation),
                "image/jpeg".into(),
            )
            .await
            .unwrap();
            let image = image::load_from_memory(&normalized.bytes)
                .unwrap()
                .to_rgb8();
            assert_eq!((normalized.width, normalized.height), image.dimensions());
            if orientation == 6 {
                assert_eq!(image.dimensions(), (667, 2000));
                assert!(
                    image.get_pixel(100, 100)[0] > 200,
                    "red half rotates to top"
                );
                assert!(
                    image.get_pixel(100, 1900)[2] > 200,
                    "blue half rotates to bottom"
                );
            } else {
                assert_eq!(image.dimensions(), (2000, 667));
                assert!(
                    image.get_pixel(100, 100)[2] > 200,
                    "blue half flips to left"
                );
                assert!(
                    image.get_pixel(1900, 100)[0] > 200,
                    "red half flips to right"
                );
            }
        }
    }

    #[tokio::test]
    async fn passthrough_preserves_original_jpeg_bytes_and_exif_orientation() {
        let original = jpeg_with_orientation(300, 100, 6);
        let normalized = normalize_image(original.clone(), "image/jpeg".into())
            .await
            .unwrap();
        assert_eq!(normalized.bytes, original);
        assert_eq!(normalized.mime_type, "image/jpeg");
    }

    #[tokio::test]
    async fn gif_first_frame_conversion_keeps_small_images_at_their_original_size() {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 1)
            .write_to(&mut bytes, ImageFormat::Gif)
            .unwrap();
        let normalized = normalize_image(bytes.into_inner(), "image/gif".into())
            .await
            .unwrap();
        assert_eq!(normalized.mime_type, "image/png");
        assert_eq!((normalized.width, normalized.height), (2, 1));
        assert_eq!(
            image::load_from_memory(&normalized.bytes)
                .unwrap()
                .dimensions(),
            (2, 1)
        );
    }
}
