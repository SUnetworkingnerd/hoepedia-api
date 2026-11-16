//! File uploads: versioned blobs stored in SQLite, content-sniffed (the client's Content-Type is
//! never trusted), with image dimensions parsed from the headers. SVGs are stored only in
//! sanitised form (`svg.rs`). Raster images can be scaled down on request; thumbnails are
//! made lazily, bounded in size and concurrency, and kept in a memory cache.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::{
    error::{ApiError, Res},
    title::{Title, NS_FILE},
    users::Actor,
    util::{iso, now, sha256_hex, truncate_chars},
    wiki::{find_page, log_event, EditReq, Wiki},
};

/// (mime, allowed extensions)
type Kind = (&'static str, &'static [&'static str]);

pub fn sniff(d: &[u8]) -> Option<Kind> {
    let starts = |p: &[u8]| d.len() >= p.len() && &d[..p.len()] == p;
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", &["png"]))
    } else if starts(&[0xFF, 0xD8, 0xFF]) {
        Some(("image/jpeg", &["jpg", "jpeg"]))
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some(("image/gif", &["gif"]))
    } else if d.len() >= 12 && &d[..4] == b"RIFF" && &d[8..12] == b"WEBP" {
        Some(("image/webp", &["webp"]))
    } else if starts(b"%PDF-") {
        Some(("application/pdf", &["pdf"]))
    } else if starts(b"OggS") {
        Some(("audio/ogg", &["ogg", "oga", "opus"]))
    } else if starts(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some(("video/webm", &["webm"]))
    } else if d.len() >= 12 && &d[4..8] == b"ftyp" {
        Some(("video/mp4", &["mp4", "m4a"]))
    } else if starts(b"ID3") || (d.len() > 2 && d[0] == 0xFF && (d[1] & 0xE0) == 0xE0) {
        Some(("audio/mpeg", &["mp3"]))
    } else {
        None
    }
}

fn be16(d: &[u8], i: usize) -> i64 {
    d.get(i..i + 2).map_or(0, |b| ((b[0] as i64) << 8) | b[1] as i64)
}
fn le16(d: &[u8], i: usize) -> i64 {
    d.get(i..i + 2).map_or(0, |b| ((b[1] as i64) << 8) | b[0] as i64)
}
fn le24(d: &[u8], i: usize) -> i64 {
    d.get(i..i + 3).map_or(0, |b| (b[0] as i64) | ((b[1] as i64) << 8) | ((b[2] as i64) << 16))
}

pub fn dimensions(mime: &str, d: &[u8]) -> (i64, i64) {
    match mime {
        "image/png" if d.len() >= 24 => {
            let g = |i: usize| ((d[i] as i64) << 24) | ((d[i + 1] as i64) << 16) | ((d[i + 2] as i64) << 8) | d[i + 3] as i64;
            (g(16), g(20))
        }
        "image/gif" => (le16(d, 6), le16(d, 8)),
        "image/jpeg" => {
            let mut i = 2;
            while i + 9 < d.len() {
                if d[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let m = d[i + 1];
                if m == 0xFF {
                    i += 1;
                    continue;
                }
                if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                    return (be16(d, i + 7), be16(d, i + 5));
                }
                if m == 0xD8 || (0xD0..=0xD7).contains(&m) || m == 0x01 {
                    i += 2;
                } else {
                    i += 2 + be16(d, i + 2) as usize;
                }
            }
            (0, 0)
        }
        "image/webp" if d.len() >= 30 => match &d[12..16] {
            b"VP8X" => (1 + le24(d, 24), 1 + le24(d, 27)),
            b"VP8L" => {
                let (b0, b1, b2, b3) = (d[21] as i64, d[22] as i64, d[23] as i64, d[24] as i64);
                (1 + (b0 | ((b1 & 0x3F) << 8)), 1 + ((b1 >> 6) | (b2 << 2) | ((b3 & 0xF) << 10)))
            }
            b"VP8 " => (le16(d, 26) & 0x3FFF, le16(d, 28) & 0x3FFF),
            _ => (0, 0),
        },
        _ => (0, 0),
    }
}

#[derive(Clone, Debug)]
pub struct Version {
    pub id: i64,
    pub ts: i64,
    pub user: String,
    pub comment: String,
    pub mime: String,
    pub size: i64,
    pub width: i64,
    pub height: i64,
    pub sha256: String,
    /// Seconds (audio, video); 0 = unknown.
    pub duration: f64,
    /// PDF page count; 0 = unknown.
    pub pages: i64,
}

#[derive(Clone, Debug)]
pub struct FileInfo {
    pub id: i64,
    pub name: String,
    pub latest: Version,
}

const VCOLS: &str = "v.id,v.ts,v.user_name,v.comment,v.mime,v.size,v.width,v.height,v.sha256,v.duration,v.pages";

fn version(r: &rusqlite::Row, o: usize) -> rusqlite::Result<Version> {
    Ok(Version {
        id: r.get(o)?,
        ts: r.get(o + 1)?,
        user: r.get(o + 2)?,
        comment: r.get(o + 3)?,
        mime: r.get(o + 4)?,
        size: r.get(o + 5)?,
        width: r.get(o + 6)?,
        height: r.get(o + 7)?,
        sha256: r.get(o + 8)?,
        duration: r.get(o + 9)?,
        pages: r.get(o + 10)?,
    })
}

pub fn file_title(raw: &str) -> Res<Title> {
    let t = Title::parse_in(raw, NS_FILE).map_err(ApiError::bad_title)?.title;
    if t.ns != NS_FILE {
        return Err(ApiError::bad("badfilename", "file names must be in the File: namespace"));
    }
    Ok(t)
}

pub fn find_file(c: &Connection, name: &str) -> Res<Option<FileInfo>> {
    Ok(c.prepare_cached(&format!(
        "SELECT f.id,f.name,{VCOLS} FROM file f JOIN file_version v ON v.id=f.latest WHERE f.name=?1 AND f.deleted=0"
    ))?
    .query_row(params![name], |r| Ok(FileInfo { id: r.get(0)?, name: r.get(1)?, latest: version(r, 2)? }))
    .optional()?)
}

pub fn file_history(c: &Connection, file_id: i64, limit: i64) -> Res<Vec<Version>> {
    let mut st = c.prepare(&format!("SELECT {VCOLS} FROM file_version v WHERE v.file=?1 ORDER BY v.id DESC LIMIT ?2"))?;
    let rows = st.query_map(params![file_id, limit], |r| version(r, 0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub struct FileData {
    pub version: i64,
    pub mime: String,
    pub sha256: String,
    pub width: i64,
    pub height: i64,
    pub duration: f64,
    pub data: Vec<u8>,
}

/// The latest or a specific version of a live file.
pub fn file_data(c: &Connection, name: &str, ver: Option<i64>) -> Res<FileData> {
    let f = find_file(c, name)?.ok_or_else(|| ApiError::not_found("missingfile", format!("file '{name}' does not exist")))?;
    let id = ver.unwrap_or(f.latest.id);
    c.prepare_cached("SELECT mime,sha256,width,height,data,duration FROM file_version WHERE id=?1 AND file=?2")?
        .query_row(params![id, f.id], |r| Ok(FileData { version: id, mime: r.get(0)?, sha256: r.get(1)?, width: r.get(2)?, height: r.get(3)?, data: r.get(4)?, duration: r.get(5)? }))
        .optional()?
        .ok_or_else(|| ApiError::not_found("missingversion", "no such file version"))
}

// ---------------------------------------------------------------------------------------
// thumbnails
// ---------------------------------------------------------------------------------------

/// Thumbnails are only made at these widths (a request is rounded up to the next one), so
/// the number of distinct renditions per file is small no matter what clients ask for.
pub const THUMB_STEPS: &[u32] =
    &[20, 40, 60, 80, 100, 120, 160, 200, 220, 250, 300, 320, 400, 480, 500, 640, 800, 960, 1024, 1280, 1600, 1920, 2560, 3840];

/// Size of the rendition that answers a request for at most `want_w` x `want_h`.
/// `None` = the original is already small enough (images are never scaled up).
pub fn thumb_size(ow: i64, oh: i64, want_w: Option<u32>, want_h: Option<u32>) -> Option<(u32, u32)> {
    if ow <= 0 || oh <= 0 || (want_w.is_none() && want_h.is_none()) {
        return None;
    }
    let (ow, oh) = (ow as f64, oh as f64);
    let scale = match (want_w, want_h) {
        (Some(w), Some(h)) => (w as f64 / ow).min(h as f64 / oh),
        (Some(w), None) => w as f64 / ow,
        (None, Some(h)) => h as f64 / oh,
        (None, None) => return None,
    };
    let target = (ow * scale).ceil().max(1.0) as u32;
    let w = THUMB_STEPS.iter().copied().find(|s| *s >= target)?;
    if w as f64 >= ow {
        return None;
    }
    Some((w, ((oh * w as f64 / ow).round() as u32).max(1)))
}

pub fn can_thumbnail(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/svg+xml") || crate::media::tools().can_preview(mime)
}

/// Which picture of a file to make: a PDF page or a moment of a video.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct Variant {
    /// PDF page, 1-based (0 = first).
    pub page: u32,
    /// Video position in tenths of a second (`None` = automatic).
    pub time_ds: Option<u32>,
}

impl Variant {
    /// Part of the cache key and file name; 0 = default.
    pub fn key(&self) -> u32 {
        match self.time_ds {
            Some(t) => 1_000_000 + t.min(999_000_000),
            None => self.page.min(999_999) * u32::from(self.page > 1),
        }
    }
}

/// Rendition size for a vector image: any requested size is possible (rounded to a step,
/// at most `SVG_MAX_WIDTH`), also larger than the nominal size.
pub fn svg_thumb_size(ow: i64, oh: i64, want_w: Option<u32>, want_h: Option<u32>) -> Option<(u32, u32)> {
    let (ow, oh) = (if ow > 0 { ow as f64 } else { 300.0 }, if oh > 0 { oh as f64 } else { 150.0 });
    let scale = match (want_w, want_h) {
        (Some(w), Some(h)) => (w as f64 / ow).min(h as f64 / oh),
        (Some(w), None) => w as f64 / ow,
        (None, Some(h)) => h as f64 / oh,
        (None, None) => return None,
    };
    let target = (ow * scale).ceil().clamp(1.0, SVG_MAX_WIDTH as f64) as u32;
    let w = THUMB_STEPS.iter().copied().find(|s| *s >= target).unwrap_or(SVG_MAX_WIDTH).min(SVG_MAX_WIDTH);
    Some((w, ((oh * w as f64 / ow).round() as u32).clamp(1, 4 * SVG_MAX_WIDTH)))
}

const SVG_MAX_WIDTH: u32 = 2048;

/// Preview size of a PDF page, video frame or audio waveform: any width up to 2048 (rounded to
/// a step). The height follows the known aspect ratio (PDF: the first page; audio: 4:1).
fn media_preview_size(mime: &str, ow: i64, oh: i64, want_w: Option<u32>, want_h: Option<u32>) -> Option<(u32, u32)> {
    if !crate::media::tools().can_preview(mime) {
        return None;
    }
    let audio = mime.starts_with("audio/");
    let (ow, oh) = if audio || ow <= 0 || oh <= 0 { (400.0, 100.0) } else { (ow as f64, oh as f64) };
    let w = match (want_w, want_h) {
        (Some(w), Some(h)) => (w as f64).min(h as f64 * ow / oh),
        (Some(w), None) => w as f64,
        (None, Some(h)) => h as f64 * ow / oh,
        (None, None) => return None,
    };
    let target = w.ceil().clamp(16.0, SVG_MAX_WIDTH as f64) as u32;
    let w = THUMB_STEPS.iter().copied().find(|s| *s >= target).unwrap_or(SVG_MAX_WIDTH).min(SVG_MAX_WIDTH);
    let exact = oh * w as f64 / ow;
    // the same rounding the tools apply: ffmpeg keeps video heights even, pdftoppm rounds up
    let h = match crate::media::kind(mime) {
        Some(crate::media::Kind::Video) => ((exact / 2.0).round() * 2.0) as u32,
        Some(crate::media::Kind::Pdf) => (exact - 1e-6).ceil() as u32,
        _ => exact.round() as u32,
    };
    Some((w, h.max(1)))
}

/// The size a request for (`want_w`, `want_h`) is answered with; `None` = serve the original.
pub fn rendition(mime: &str, ow: i64, oh: i64, want_w: Option<u32>, want_h: Option<u32>) -> Option<(u32, u32)> {
    if mime == "image/svg+xml" {
        svg_thumb_size(ow, oh, want_w, want_h)
    } else if crate::media::kind(mime).is_some() {
        media_preview_size(mime, ow, oh, want_w, want_h)
    } else if can_thumbnail(mime) {
        thumb_size(ow, oh, want_w, want_h)
    } else {
        None
    }
}

fn fonts() -> std::sync::Arc<resvg::usvg::fontdb::Database> {
    static DB: std::sync::OnceLock<std::sync::Arc<resvg::usvg::fontdb::Database>> = std::sync::OnceLock::new();
    DB.get_or_init(|| {
        // The runtime image ships no fonts, so text in SVGs is drawn with embedded DejaVu.
        let mut db = resvg::usvg::fontdb::Database::new();
        for f in [dejavu::sans::regular(), dejavu::sans::bold(), dejavu::sans::oblique(), dejavu::sans_mono::regular()] {
            db.load_font_data(f.to_vec());
        }
        db.set_sans_serif_family("DejaVu Sans");
        db.set_serif_family("DejaVu Sans");
        db.set_cursive_family("DejaVu Sans");
        db.set_fantasy_family("DejaVu Sans");
        db.set_monospace_family("DejaVu Sans Mono");
        std::sync::Arc::new(db)
    })
    .clone()
}

/// Rasterise a (sanitised) SVG to a PNG of exactly `w` pixels wide.
fn render_svg(data: &[u8], w: u32, h: u32) -> Result<(&'static str, Vec<u8>), String> {
    use resvg::{tiny_skia, usvg};
    let mut opt = usvg::Options { font_family: "DejaVu Sans".into(), fontdb: fonts(), ..Default::default() };
    // Never read image files from disk; embedded data: images are decoded in memory.
    opt.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = usvg::Tree::from_data(data, &opt).map_err(|e| e.to_string())?;
    let size = tree.size();
    let h = if size.width() > 0.0 { ((size.height() * w as f32 / size.width()).round() as u32).clamp(1, 4 * SVG_MAX_WIDTH) } else { h };
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("invalid size")?;
    let ts = tiny_skia::Transform::from_scale(w as f32 / size.width(), h as f32 / size.height());
    resvg::render(&tree, ts, &mut pixmap.as_mut());
    Ok(("image/png", pixmap.encode_png().map_err(|e| e.to_string())?))
}

/// Encode scaled frames as an animated WebP (libwebp, lossy at quality 80, with alpha).
fn encode_animated_webp(frames: Vec<image::Frame>) -> Result<Vec<u8>, String> {
    use webp_animation::{Encoder, EncoderOptions, EncodingConfig};
    let first = frames.first().ok_or("no frames")?;
    let dims = (first.buffer().width(), first.buffer().height());
    let opts = EncoderOptions { encoding_config: Some(EncodingConfig::new_lossy(80.0)), ..Default::default() };
    let mut enc = Encoder::new_with_options(dims, opts).map_err(|e| format!("{e:?}"))?;
    let mut t: i64 = 0;
    for f in &frames {
        let buf = f.buffer();
        // every frame is the full canvas size after scaling; pad if a decoder gave less
        let img = if (buf.width(), buf.height()) == dims {
            std::borrow::Cow::Borrowed(buf)
        } else {
            let mut canvas = image::RgbaImage::new(dims.0, dims.1);
            image::imageops::overlay(&mut canvas, buf, 0, 0);
            std::borrow::Cow::Owned(canvas)
        };
        enc.add_frame(img.as_raw(), t.min(i32::MAX as i64) as i32).map_err(|e| format!("{e:?}"))?;
        let (num, den) = f.delay().numer_denom_ms();
        // browsers treat delays under 20 ms as 100 ms; keep what the original intended
        t += (num as i64 / den.max(1) as i64).max(20);
    }
    let data = enc.finalize(t.min(i32::MAX as i64) as i32).map_err(|e| format!("{e:?}"))?;
    Ok(data.to_vec())
}

/// Animated GIF/WebP: every frame is scaled; GIF stays GIF and WebP stays WebP.
/// `Ok(None)` = not animated (or too large to animate): use the still path.
fn render_animation(mime: &str, data: &[u8], w: u32, h: u32, max_pixels: u64) -> Result<Option<(&'static str, Vec<u8>)>, String> {
    use image::{codecs::gif::{GifDecoder, GifEncoder, Repeat}, codecs::webp::WebPDecoder, AnimationDecoder, Frame, ImageDecoder};
    const MAX_FRAMES: usize = 400;
    let cur = std::io::Cursor::new(data);
    let (dims, frames): ((u32, u32), image::Frames) = match mime {
        "image/gif" => {
            let d = GifDecoder::new(cur).map_err(|e| e.to_string())?;
            (d.dimensions(), d.into_frames())
        }
        "image/webp" => {
            let d = WebPDecoder::new(cur).map_err(|e| e.to_string())?;
            if !d.has_animation() {
                return Ok(None);
            }
            (d.dimensions(), d.into_frames())
        }
        _ => return Ok(None),
    };
    let per_frame = dims.0 as u64 * dims.1 as u64;
    if per_frame == 0 || per_frame > max_pixels {
        return Err("the image has too many pixels to be scaled on this server".into());
    }
    // Total decoding work is bounded: beyond it only the first frame is used.
    let budget = (max_pixels.saturating_mul(8) / per_frame).min(MAX_FRAMES as u64) as usize;
    let mut out_frames: Vec<Frame> = Vec::new();
    for f in frames {
        let f = f.map_err(|e| e.to_string())?;
        if out_frames.len() >= budget {
            return Ok(None);
        }
        let delay = f.delay();
        out_frames.push(Frame::from_parts(image::imageops::thumbnail(f.buffer(), w, h), 0, 0, delay));
    }
    if out_frames.len() < 2 {
        return Ok(None);
    }
    if mime == "image/webp" {
        // Animated WebP stays WebP: full colour and alpha, unlike GIF's 256 colours.
        return encode_animated_webp(out_frames).map(|b| Some(("image/webp", b)));
    }
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new_with_speed(&mut out, 10);
        enc.set_repeat(Repeat::Infinite).map_err(|e| e.to_string())?;
        enc.encode_frames(out_frames).map_err(|e| e.to_string())?;
    }
    Ok(Some(("image/gif", out)))
}

/// Decode, scale and re-encode. JPEG stays JPEG; everything else becomes PNG (keeps alpha).
fn render_thumb(mime: &str, data: &[u8], w: u32, h: u32, max_pixels: u64) -> Result<(&'static str, Vec<u8>), String> {
    use image::{ImageDecoder, ImageReader};
    if mime == "image/svg+xml" {
        return render_svg(data, w, h);
    }
    if let Some(anim) = render_animation(mime, data, w, h, max_pixels)? {
        return Ok(anim);
    }
    let mut reader = ImageReader::new(std::io::Cursor::new(data)).with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    // Hard ceiling on what a decoder may allocate, whatever the file header claims.
    limits.max_alloc = Some(max_pixels.saturating_mul(4).saturating_add(64 << 20));
    limits.max_image_width = Some(30_000);
    limits.max_image_height = Some(30_000);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let (dw, dh) = decoder.dimensions();
    if dw as u64 * dh as u64 > max_pixels {
        return Err("the image has too many pixels to be scaled on this server".into());
    }
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    let small = img.thumbnail(w, h);
    let mut out = std::io::Cursor::new(Vec::new());
    if mime == "image/jpeg" {
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        small.to_rgb8().write_with_encoder(enc).map_err(|e| e.to_string())?;
        Ok(("image/jpeg", out.into_inner()))
    } else {
        small.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
        Ok(("image/png", out.into_inner()))
    }
}

impl Wiki {
    /// Fill in duration, page count and frame size of uploaded media (in the background, on
    /// the primary; replicas receive the results by replication). Returns how many were done.
    pub async fn probe_pending(&self) -> usize {
        let t = crate::media::tools();
        if self.is_replica() || (t.ffprobe.is_none() && t.pdfinfo.is_none()) {
            return 0;
        }
        let todo: Vec<(i64, String)> = match self
            .read(|_, c| {
                let mut st = c.prepare(
                    "SELECT id, mime FROM file_version WHERE probed=0 AND mime IN ('application/pdf','video/mp4','video/webm','audio/mpeg','audio/ogg') ORDER BY id DESC LIMIT 20",
                )?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
        {
            Ok(v) => v,
            Err(_) => return 0,
        };
        let mut done = 0;
        for (id, mime) in todo {
            // Skip kinds whose tool is missing; they are probed once it is installed.
            let k = crate::media::kind(&mime);
            if (k == Some(crate::media::Kind::Pdf) && t.pdfinfo.is_none()) || (k != Some(crate::media::Kind::Pdf) && t.ffprobe.is_none()) {
                continue;
            }
            let data: Vec<u8> = match self.read(move |_, c| Ok(c.query_row("SELECT data FROM file_version WHERE id=?1", [id], |r| r.get(0))?)).await {
                Ok(d) => d,
                Err(_) => continue,
            };
            let m2 = mime.clone();
            let meta = tokio::task::spawn_blocking(move || crate::media::probe(&m2, &data)).await.unwrap_or_else(|e| Err(e.to_string()));
            let (m, status) = match meta {
                Ok(m) => (m, 1),
                Err(e) => {
                    tracing::info!(version = id, error = %e, "could not read media metadata");
                    (crate::media::Meta::default(), 2)
                }
            };
            let r = self
                .write_always(move |_, c| {
                    c.execute(
                        "UPDATE file_version SET duration=?1, pages=?2, width=CASE WHEN width=0 THEN ?3 ELSE width END, height=CASE WHEN height=0 THEN ?4 ELSE height END, probed=?5 WHERE id=?6",
                        params![m.duration, m.pages, m.width, m.height, status, id],
                    )?;
                    Ok(())
                })
                .await;
            if r.is_ok() {
                done += 1;
            }
        }
        done
    }
}

static THUMB_DISK_USED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Delete the least recently written thumbnails until at most `keep` bytes remain.
/// Returns the bytes left.
fn prune_thumb_dir(dir: &std::path::Path, keep: u64) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut files: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= keep {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
    total
}

static THUMB_JOBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Decoding is memory-hungry (4 bytes per pixel), so only a few images are scaled at once.
const MAX_THUMB_JOBS: usize = 2;

pub type Thumb = std::sync::Arc<(&'static str, Vec<u8>)>;

impl Wiki {
    /// The cached or freshly made rendition of one file version at exactly `w` x `h`.
    #[cfg(test)]
    pub fn thumbnail(&self, fd: &FileData, w: u32, h: u32) -> Res<Thumb> {
        self.thumbnail_of(fd, w, h, Variant::default())
    }

    /// The cached or freshly made rendition of one file version (and page / moment).
    pub fn thumbnail_of(&self, fd: &FileData, w: u32, h: u32, var: Variant) -> Res<Thumb> {
        use std::sync::atomic::Ordering;
        let key = (fd.version, w, var.key());
        if let Some(t) = self.thumbs.get(&key) {
            return Ok(t);
        }
        if let Some(t) = self.thumb_from_disk(fd.version, w, var.key()) {
            self.thumbs.insert(key, t.clone());
            return Ok(t);
        }
        if !can_thumbnail(&fd.mime) {
            return Err(ApiError::bad("nothumbnail", format!("{} files cannot be scaled", fd.mime)));
        }
        if THUMB_JOBS.fetch_add(1, Ordering::SeqCst) >= MAX_THUMB_JOBS {
            THUMB_JOBS.fetch_sub(1, Ordering::SeqCst);
            return Err(ApiError::new(axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy", "too many images are being scaled right now; retry shortly"));
        }
        let max_pixels = self.cfg.thumb_max_megapixels.saturating_mul(1_000_000);
        // A decoder panic must not leak the job slot or take the worker down.
        let res = std::panic::catch_unwind(|| match crate::media::kind(&fd.mime) {
            Some(_) => {
                let opts = crate::media::PreviewOpts { page: var.page, time: var.time_ds.map(|t| t as f64 / 10.0), duration: fd.duration };
                crate::media::preview(&fd.mime, &fd.data, w, opts).map(|png| ("image/png", png))
            }
            None => render_thumb(&fd.mime, &fd.data, w, h, max_pixels),
        });
        THUMB_JOBS.fetch_sub(1, Ordering::SeqCst);
        let (mime, bytes) = match res {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(ApiError::bad("thumbnailfailed", format!("cannot scale this image: {e}"))),
            Err(_) => return Err(ApiError::bad("thumbnailfailed", "cannot scale this image: the decoder failed")),
        };
        let t: Thumb = std::sync::Arc::new((mime, bytes));
        self.thumbs.insert(key, t.clone());
        self.thumb_to_disk(fd.version, w, var.key(), &t);
        Ok(t)
    }

    fn thumb_path(&self, version: i64, w: u32, var: u32, ext: &str) -> Option<std::path::PathBuf> {
        let name = if var == 0 { format!("{version}-{w}.{ext}") } else { format!("{version}-{w}-{var}.{ext}") };
        (!self.cfg.thumb_dir.is_empty()).then(|| std::path::Path::new(&self.cfg.thumb_dir).join(name))
    }

    fn thumb_from_disk(&self, version: i64, w: u32, var: u32) -> Option<Thumb> {
        for (ext, mime) in [("png", "image/png"), ("jpg", "image/jpeg"), ("gif", "image/gif"), ("webp", "image/webp")] {
            if let Ok(bytes) = std::fs::read(self.thumb_path(version, w, var, ext)?) {
                return Some(std::sync::Arc::new((mime, bytes)));
            }
        }
        None
    }

    /// Best effort: a failed write only means the thumbnail is made again after a restart.
    fn thumb_to_disk(&self, version: i64, w: u32, var: u32, t: &Thumb) {
        use std::sync::atomic::Ordering;
        let ext = match t.0 {
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "png",
        };
        let Some(path) = self.thumb_path(version, w, var, ext) else { return };
        let dir = std::path::Path::new(&self.cfg.thumb_dir);
        let tmp = dir.join(format!(".tmp-{}-{version}-{w}-{var}", std::process::id()));
        if std::fs::create_dir_all(dir).is_err() || std::fs::write(&tmp, &t.1).is_err() || std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        let total = THUMB_DISK_USED.fetch_add(t.1.len() as u64, Ordering::SeqCst) + t.1.len() as u64;
        if total > self.cfg.thumb_disk_bytes {
            THUMB_DISK_USED.store(prune_thumb_dir(dir, self.cfg.thumb_disk_bytes / 5 * 4), Ordering::SeqCst);
        }
    }

    /// Called once at startup: measure (and if needed trim) the thumbnail directory.
    pub fn init_thumb_dir(&self) {
        if !self.cfg.thumb_dir.is_empty() {
            let used = prune_thumb_dir(std::path::Path::new(&self.cfg.thumb_dir), self.cfg.thumb_disk_bytes);
            THUMB_DISK_USED.store(used, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

pub fn all_files(c: &Connection, prefix: &str, from: &str, mime: Option<&str>, limit: i64) -> Res<Vec<FileInfo>> {
    let like = format!("{}%", prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let mut st = c.prepare(&format!(
        "SELECT f.id,f.name,{VCOLS} FROM file f JOIN file_version v ON v.id=f.latest
         WHERE f.deleted=0 AND f.name LIKE ?1 ESCAPE '\\' AND f.name>=?2 AND (?3 IS NULL OR v.mime=?3) ORDER BY f.name LIMIT ?4"
    ))?;
    let rows = st
        .query_map(params![like, from, mime, limit], |r| Ok(FileInfo { id: r.get(0)?, name: r.get(1)?, latest: version(r, 2)? }))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn set_deleted(c: &Connection, name: &str, deleted: bool) -> Res<()> {
    c.execute("UPDATE file SET deleted=?1 WHERE name=?2", params![deleted as i64, name])?;
    Ok(())
}

pub fn version_json(v: &Version) -> Value {
    let mut o = json!({ "version": v.id, "timestamp": iso(v.ts), "user": v.user, "comment": v.comment, "mime": v.mime, "size": v.size,
            "width": v.width, "height": v.height, "sha256": v.sha256, "mediatype": mediatype(&v.mime) });
    if v.duration > 0.0 {
        o["duration"] = json!((v.duration * 1000.0).round() / 1000.0);
    }
    if v.pages > 0 {
        o["pages"] = json!(v.pages);
    }
    o["preview"] = json!(can_thumbnail(&v.mime));
    o
}

/// MediaWiki's media type names.
pub fn mediatype(mime: &str) -> &'static str {
    match mime {
        "image/svg+xml" => "DRAWING",
        m if m.starts_with("image/") => "BITMAP",
        "application/pdf" => "OFFICE",
        m if m.starts_with("video/") => "VIDEO",
        m if m.starts_with("audio/") => "AUDIO",
        _ => "UNKNOWN",
    }
}

pub fn info_json(w: &Wiki, f: &FileInfo) -> Value {
    let mut v = version_json(&f.latest);
    v["name"] = json!(f.name);
    v["title"] = json!(Title::new(NS_FILE, &f.name).full());
    v["url"] = json!(w.cfg.file_path.replace("$1", &crate::util::path_encode(&f.name)));
    v
}

pub struct UploadResult {
    pub file: FileInfo,
    pub new: bool,
    pub nochange: bool,
}

impl Wiki {
    #[allow(clippy::too_many_arguments)]
    pub fn upload(&self, c: &Connection, actor: &Actor, raw_name: &str, data: &[u8], comment: &str, description: Option<&str>, create_only: bool) -> Res<UploadResult> {
        actor.require("upload")?;
        if data.is_empty() {
            return Err(ApiError::bad("emptyfile", "the file is empty"));
        }
        if data.len() > self.cfg.upload_max_bytes {
            return Err(ApiError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "filetoobig",
                format!("files are limited to {} bytes", self.cfg.upload_max_bytes),
            ));
        }
        let title = file_title(raw_name)?;
        let mut svg: Option<crate::svg::Clean> = None;
        let (mime, exts): Kind = match sniff(data) {
            Some(k) => k,
            None if crate::svg::looks_like_svg(data) => {
                if !self.cfg.allow_svg {
                    return Err(ApiError::bad("filetypenotallowed", "SVG uploads are disabled on this wiki"));
                }
                // Only the sanitised re-serialisation is ever stored or served.
                svg = Some(crate::svg::sanitize(data).map_err(|e| ApiError::bad("badsvg", format!("the SVG was rejected: {e}")))?);
                ("image/svg+xml", &["svg"])
            }
            None => return Err(ApiError::bad("filetypenotallowed", "unsupported or unrecognised file type (allowed: png, jpeg, gif, webp, svg, pdf, ogg, webm, mp4, mp3)")),
        };
        let svg_dims = svg.as_ref().map(|c| (c.width, c.height));
        let svg_bytes = svg.map(|c| c.svg.into_bytes());
        let data: &[u8] = svg_bytes.as_deref().unwrap_or(data);
        let ext = title.name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        if !exts.contains(&ext.as_str()) {
            return Err(ApiError::bad("badextension", format!("the content is {mime}; the file name must end in .{}", exts.join(" or ."))));
        }
        let sha = sha256_hex(data);
        let existing = find_file(c, &title.name)?;
        if let Some(f) = &existing {
            if create_only {
                return Err(ApiError::conflict("fileexists", "a file with this name already exists"));
            }
            if f.latest.sha256 == sha {
                return Ok(UploadResult { file: f.clone(), new: false, nochange: true });
            }
        }
        let (w, h) = svg_dims.unwrap_or_else(|| dimensions(mime, data));
        let comment = truncate_chars(comment.trim(), 500);
        let ts = now();
        let file_id = match &existing {
            Some(f) => f.id,
            None => {
                c.execute(
                    "INSERT INTO file(name) VALUES(?1) ON CONFLICT(name) DO UPDATE SET deleted=0",
                    params![title.name],
                )?;
                c.query_row("SELECT id FROM file WHERE name=?1", params![title.name], |r| r.get(0))?
            }
        };
        c.execute(
            "INSERT INTO file_version(file,ts,user_id,user_name,comment,mime,size,width,height,sha256,data) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![file_id, ts, actor.user_id, actor.name, comment, mime, data.len() as i64, w, h, sha, data],
        )?;
        let vid = c.last_insert_rowid();
        c.execute("UPDATE file SET latest=?1 WHERE id=?2", params![vid, file_id])?;
        log_event(c, "upload", if existing.is_some() { "overwrite" } else { "upload" }, &actor.name, Some(&title), &comment, json!({ "sha256": sha, "size": data.len(), "mime": mime }))?;
        if find_page(c, &title)?.is_none() {
            let text = description.map(str::to_string).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Uploaded file.".into());
            // The upload itself succeeded; a missing description page is not fatal.
            let _ = self.edit(c, actor, EditReq { title: title.clone(), text, summary: "Uploaded file".into(), minor: false, base_rev: None, create_only: true, no_create: false });
        }
        let file = find_file(c, &title.name)?.ok_or_else(|| ApiError::internal("uploaded file vanished"))?;
        Ok(UploadResult { file, new: existing.is_none(), nochange: false })
    }

    pub fn delete_file(&self, c: &Connection, actor: &Actor, raw_name: &str, reason: &str) -> Res<()> {
        actor.require("deletefile")?;
        let title = file_title(raw_name)?;
        find_file(c, &title.name)?.ok_or_else(|| ApiError::not_found("missingfile", "the file does not exist"))?;
        set_deleted(c, &title.name, true)?;
        log_event(c, "upload", "file_delete", &actor.name, Some(&title), truncate_chars(reason.trim(), 500).as_str(), json!({}))?;
        if find_page(c, &title)?.is_some() {
            self.delete_page(c, actor, &title, reason)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::testutil::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut d = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        d.extend(w.to_be_bytes());
        d.extend(h.to_be_bytes());
        d.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        d
    }

    #[test]
    fn sniffing_and_dimensions() {
        assert_eq!(sniff(&png(3, 4)).unwrap().0, "image/png");
        assert_eq!(dimensions("image/png", &png(640, 480)), (640, 480));
        assert_eq!(sniff(b"GIF89a\x10\x00\x20\x00").unwrap().0, "image/gif");
        assert_eq!(dimensions("image/gif", b"GIF89a\x10\x00\x20\x00"), (16, 32));
        // JPEG: SOI, APP0 (len 4), SOF0 (h=0x0102, w=0x0304)
        let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 11, 8, 1, 2, 3, 4, 3, 0, 0, 0];
        assert_eq!(dimensions("image/jpeg", &jpg), (0x0304, 0x0102));
        assert!(sniff(b"<html><script>alert(1)</script>").is_none());
        assert!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_none());
        assert_eq!(thumb_size(1000, 500, Some(200), None), Some((200, 100)));
        assert_eq!(thumb_size(1000, 500, Some(201), None), Some((220, 110)), "rounded up to a step");
        assert_eq!(thumb_size(1000, 500, None, Some(100)), Some((200, 100)));
        assert_eq!(thumb_size(1000, 500, Some(400), Some(50)), Some((100, 50)), "fits inside the box");
        assert_eq!(thumb_size(300, 300, Some(300), None), None, "never scaled up");
        assert_eq!(thumb_size(300, 300, Some(5000), None), None);
        assert_eq!(thumb_size(0, 0, Some(100), None), None);
        assert!(sniff(b"MZ\x90\x00").is_none());
    }

    fn real_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn thumbnails_and_svg() {
        let t = make(false);
        let c = t.wiki.db.writer();
        t.wiki.upload(&c, &t.admin, "Big.png", &real_png(800, 400), "", None, false).unwrap();
        let fd = file_data(&c, "Big.png", None).unwrap();
        assert_eq!((fd.width, fd.height), (800, 400));
        let (w, h) = thumb_size(fd.width, fd.height, Some(150), None).unwrap();
        let th = t.wiki.thumbnail(&fd, w, h).unwrap();
        assert_eq!(th.0, "image/png");
        assert_eq!(dimensions("image/png", &th.1), (160, 80));
        assert!(th.1.len() < fd.data.len());
        assert!(std::sync::Arc::ptr_eq(&th, &t.wiki.thumbnail(&fd, w, h).unwrap()), "second request is served from the cache");
        // the rendition is also on disk and survives a restart (a new Wiki on the same directory)
        let on_disk = std::path::Path::new(&t.wiki.cfg.thumb_dir).join(format!("{}-{w}.png", fd.version));
        assert_eq!(std::fs::read(&on_disk).unwrap(), th.1);
        t.wiki.thumbs.invalidate_all();
        assert_eq!(t.wiki.thumbnail(&fd, w, h).unwrap().1, th.1);
        assert_eq!(prune_thumb_dir(std::path::Path::new(&t.wiki.cfg.thumb_dir), 0), 0);
        assert!(!on_disk.exists());
        // animated GIFs stay animated
        let gif = {
            use image::codecs::gif::{GifEncoder, Repeat};
            let mut out = Vec::new();
            {
                let mut enc = GifEncoder::new(&mut out);
                enc.set_repeat(Repeat::Infinite).unwrap();
                for i in 0..3u8 {
                    let img = image::RgbaImage::from_pixel(400, 200, image::Rgba([i * 80, 100, 200, 255]));
                    enc.encode_frame(image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(100, 1))).unwrap();
                }
            }
            out
        };
        t.wiki.upload(&c, &t.admin, "Anim.gif", &gif, "", None, false).unwrap();
        let fd2 = file_data(&c, "Anim.gif", None).unwrap();
        let (w2, h2) = rendition(&fd2.mime, fd2.width, fd2.height, Some(100), None).unwrap();
        let th2 = t.wiki.thumbnail(&fd2, w2, h2).unwrap();
        assert_eq!(th2.0, "image/gif");
        assert_eq!(dimensions("image/gif", &th2.1), (100, 50));
        {
            use image::AnimationDecoder;
            let frames = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&th2.1)).unwrap().into_frames().count();
            assert_eq!(frames, 3);
        }
        // animated WebP stays animated WebP, in full colour
        let webp = {
            use webp_animation::Encoder;
            let mut enc = Encoder::new((300, 150)).unwrap();
            for i in 0..4u8 {
                let img = image::RgbaImage::from_fn(300, 150, |x, _| image::Rgba([(x % 256) as u8, i * 60, 255 - (x % 256) as u8, 255]));
                enc.add_frame(img.as_raw(), i as i32 * 80).unwrap();
            }
            enc.finalize(320).unwrap().to_vec()
        };
        t.wiki.upload(&c, &t.admin, "Anim.webp", &webp, "", None, false).unwrap();
        let fd5 = file_data(&c, "Anim.webp", None).unwrap();
        assert_eq!((fd5.width, fd5.height), (300, 150));
        let (w5, h5) = rendition(&fd5.mime, fd5.width, fd5.height, Some(100), None).unwrap();
        let th5 = t.wiki.thumbnail(&fd5, w5, h5).unwrap();
        assert_eq!(th5.0, "image/webp");
        let dec = webp_animation::Decoder::new(&th5.1).unwrap();
        assert_eq!(dec.dimensions(), (100, 50));
        let frames: Vec<_> = dec.into_iter().collect();
        assert_eq!(frames.len(), 4);
        // libwebp reports each frame's end time: the 80 ms delays are kept
        assert_eq!(frames.iter().map(|f| f.timestamp()).collect::<Vec<_>>(), vec![80, 160, 240, 320]);
        // smooth gradients survive (a GIF would quantise them)
        let px = frames[0].data();
        let distinct: std::collections::HashSet<[u8; 3]> = px.chunks(4).map(|p| [p[0], p[1], p[2]]).collect();
        assert!(distinct.len() > 60, "{} colours", distinct.len());
        // a file that only pretends to be an image fails cleanly
        t.wiki.upload(&c, &t.admin, "Fake.png", &png(5000, 5000), "", None, false).unwrap();
        let fake = file_data(&c, "Fake.png", None).unwrap();
        assert_eq!(t.wiki.thumbnail(&fake, 100, 100).err().unwrap().code, "thumbnailfailed");
        // SVG: stored sanitised, with dimensions
        let r = t.wiki.upload(&c, &t.admin, "Logo.svg", b"<svg xmlns='http://www.w3.org/2000/svg' width='40' height='30' onload='alert(1)'><script>x()</script><rect width='4' height='3'/></svg>", "", None, false).unwrap();
        assert_eq!((r.file.latest.mime.as_str(), r.file.latest.width, r.file.latest.height), ("image/svg+xml", 40, 30));
        let stored = String::from_utf8(file_data(&c, "Logo.svg", None).unwrap().data).unwrap();
        assert!(stored.contains("<rect") && !stored.contains("script") && !stored.contains("onload"), "{stored}");
        // SVGs are rasterised on request (also larger than their nominal size), text included
        let fd3 = file_data(&c, "Logo.svg", None).unwrap();
        assert_eq!(rendition(&fd3.mime, fd3.width, fd3.height, Some(160), None), Some((160, 120)));
        assert_eq!(rendition(&fd3.mime, fd3.width, fd3.height, Some(99_999), None).unwrap().0, 2048);
        let th3 = t.wiki.thumbnail(&fd3, 160, 120).unwrap();
        assert_eq!((th3.0, dimensions("image/png", &th3.1)), ("image/png", (160, 120)));
        let px = image::load_from_memory(&th3.1).unwrap().to_rgba8();
        assert!(px.pixels().any(|p| p.0[3] > 0), "the rectangle was drawn");
        t.wiki.upload(&c, &t.admin, "Text.svg", b"<svg xmlns='http://www.w3.org/2000/svg' width='200' height='60'><text x='10' y='40' font-size='30' font-family='serif'>Hello</text></svg>", "", None, false).unwrap();
        let fd4 = file_data(&c, "Text.svg", None).unwrap();
        let th4 = t.wiki.thumbnail(&fd4, 200, 60).unwrap();
        let ink = image::load_from_memory(&th4.1).unwrap().to_rgba8().pixels().filter(|p| p.0[3] > 0).count();
        assert!(ink > 200, "text is rendered with the embedded font ({ink} px)");
        assert_eq!(t.wiki.upload(&c, &t.admin, "Bad.svg", b"<svg><g></svg>", "", None, false).err().unwrap().code, "badsvg");
        assert_eq!(t.wiki.upload(&c, &t.admin, "Logo.png", b"<svg xmlns='http://www.w3.org/2000/svg'/>", "", None, false).err().unwrap().code, "badextension");
    }

    #[test]
    fn upload_versions_dedupe_and_rules() {
        let t = make(false);
        let c = t.wiki.db.writer();
        let up = |name: &str, d: &[u8], create_only| t.wiki.upload(&c, &t.admin, name, d, "c", Some("A cat"), create_only);
        let r = up("cat.png", &png(10, 20), false).unwrap();
        assert!(r.new && r.file.latest.width == 10 && r.file.latest.height == 20 && r.file.name == "Cat.png");
        assert!(find_page(&c, &Title::new(NS_FILE, "Cat.png")).unwrap().is_some(), "description page created");
        assert!(up("cat.png", &png(10, 20), false).unwrap().nochange);
        assert_eq!(up("cat.png", &png(1, 1), true).err().unwrap().code, "fileexists");
        let r2 = up("Cat.png", &png(30, 40), false).unwrap();
        assert!(!r2.new && r2.file.latest.id != r.file.latest.id);
        assert_eq!(file_history(&c, r2.file.id, 10).unwrap().len(), 2);
        assert_eq!(file_data(&c, "Cat.png", Some(r.file.latest.id)).unwrap().data, png(10, 20));
        // the extension must match the real content
        assert_eq!(up("evil.html", &png(1, 1), false).err().unwrap().code, "badextension");
        assert_eq!(up("evil.png", b"<script>alert(1)</script>", false).err().unwrap().code, "filetypenotallowed");
        assert_eq!(up("evil.svg", b"<html><svg/><script>alert(1)</script></html>", false).err().unwrap().code, "filetypenotallowed");
        assert_eq!(up("x.png", b"", false).err().unwrap().code, "emptyfile");
        assert_eq!(up("Template:x.png", &png(1, 1), false).err().unwrap().code, "badfilename");
        assert_eq!(up("File:x.png", &png(1, 1), false).unwrap().file.name, "X.png");
        // anonymous users may not upload, even with anon edits on
        assert_eq!(t.wiki.upload(&c, &t.anon, "a.png", &png(1, 1), "", None, false).err().unwrap().status, 401);
        // delete hides the file and its page
        t.wiki.delete_file(&c, &t.admin, "Cat.png", "gone").unwrap();
        assert!(find_file(&c, "Cat.png").unwrap().is_none());
        assert!(find_page(&c, &Title::new(NS_FILE, "Cat.png")).unwrap().is_none());
        assert_eq!(file_data(&c, "Cat.png", None).err().map(|e| e.code), Some("missingfile"));
    }
}
