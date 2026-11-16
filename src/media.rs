//! Previews and metadata for PDF, audio and video, made by external programs:
//! `pdftoppm` / `pdfinfo` (Poppler) and `ffmpeg` / `ffprobe`.
//!
//! The programs are optional. They are looked up once at start-up (in `PATH`, or at the paths
//! given by `PDFTOPPM_PATH`, `PDFINFO_PATH`, `FFMPEG_PATH`, `FFPROBE_PATH`); whatever is missing
//! simply produces no preview. Parsing uploaded media is risky, so every run:
//!
//! * works on a private copy of the file in a fresh temporary folder, with no stdin and a clean
//!   environment;
//! * is killed after a time limit, and on Unix also limited in CPU time, memory and output size;
//! * for ffmpeg, is told the exact container format (the file was already content-sniffed) and may
//!   only read local files, so playlists or concat lists cannot make it open other files or URLs.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

#[derive(Default, Clone, Debug)]
pub struct Tools {
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    pub pdftoppm: Option<PathBuf>,
    pub pdfinfo: Option<PathBuf>,
}

static TOOLS: OnceLock<Tools> = OnceLock::new();

/// Tools found at start-up (empty until `detect` ran, and in tests that do not call it).
pub fn tools() -> &'static Tools {
    TOOLS.get_or_init(Tools::default)
}

fn find_program(env_key: &str, name: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env_key) {
        let p = p.trim();
        if p.eq_ignore_ascii_case("off") || p.eq_ignore_ascii_case("none") {
            return None;
        }
        if !p.is_empty() {
            return Some(PathBuf::from(p)).filter(|p| p.is_file());
        }
    }
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// Look for the programs once; `MEDIA_PREVIEWS=off` disables all of them.
pub fn detect() -> &'static Tools {
    let off = std::env::var("MEDIA_PREVIEWS").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "false" | "0" | "no"));
    let t = if off {
        Tools::default()
    } else {
        // A program that cannot even print its version is treated as missing.
        let works = |p: Option<PathBuf>, arg: &str| p.filter(|p| run_simple(p, &[arg]).is_ok());
        Tools {
            ffmpeg: works(find_program("FFMPEG_PATH", "ffmpeg"), "-version"),
            ffprobe: works(find_program("FFPROBE_PATH", "ffprobe"), "-version"),
            pdftoppm: works(find_program("PDFTOPPM_PATH", "pdftoppm"), "-v"),
            pdfinfo: works(find_program("PDFINFO_PATH", "pdfinfo"), "-v"),
        }
    };
    let _ = TOOLS.set(t);
    tools()
}

impl Tools {
    pub fn can_preview(&self, mime: &str) -> bool {
        match kind(mime) {
            Some(Kind::Pdf) => self.pdftoppm.is_some(),
            Some(Kind::Video) | Some(Kind::Audio) => self.ffmpeg.is_some(),
            None => false,
        }
    }

    pub fn summary(&self) -> String {
        let pdf = self.pdftoppm.is_some();
        let av = self.ffmpeg.is_some();
        match (pdf, av) {
            (true, true) => "PDF, video and audio".into(),
            (true, false) => "PDF only (install ffmpeg for video and audio)".into(),
            (false, true) => "video and audio only (install Poppler for PDF)".into(),
            (false, false) => "off (install ffmpeg and Poppler to enable)".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Pdf,
    Video,
    Audio,
}

pub fn kind(mime: &str) -> Option<Kind> {
    match mime {
        "application/pdf" => Some(Kind::Pdf),
        "video/mp4" | "video/webm" => Some(Kind::Video),
        "audio/mpeg" | "audio/ogg" => Some(Kind::Audio),
        _ => None,
    }
}

/// The ffmpeg demuxer for a content-sniffed type (never let ffmpeg guess).
fn demuxer(mime: &str) -> &'static str {
    match mime {
        "video/mp4" => "mov",
        "video/webm" => "matroska",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        _ => "",
    }
}

fn extension(mime: &str) -> &'static str {
    match mime {
        "application/pdf" => "pdf",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        _ => "bin",
    }
}

/// A private temporary folder, removed when dropped.
struct Workdir(PathBuf);

impl Workdir {
    fn new() -> Result<Workdir, String> {
        let dir = std::env::temp_dir().join(format!("hoepedia-media-{}", crate::util::new_token()));
        std::fs::create_dir(&dir).map_err(|e| format!("cannot create a temporary folder: {e}"))?;
        Ok(Workdir(dir))
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const MAX_OUTPUT: u64 = 64 * 1024 * 1024;

fn run_simple(prog: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    run(prog, args, None, Duration::from_secs(5))
}

/// Run a program with a time limit; returns its stdout. On Unix the child also gets hard
/// limits on CPU time, address space and written file size.
fn run(prog: &Path, args: &[&str], cwd: Option<&Path>, limit: Duration) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(prog);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).env_clear();
    if let Some(p) = std::env::var_os("PATH") {
        cmd.env("PATH", p);
    }
    // Windows needs these to start programs and find the temporary folder.
    for k in ["SYSTEMROOT", "TEMP", "TMP"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let cpu = limit.as_secs().max(1) + 1;
        unsafe {
            cmd.pre_exec(move || {
                let set = |res, v: u64| {
                    let l = libc::rlimit { rlim_cur: v as libc::rlim_t, rlim_max: v as libc::rlim_t };
                    libc::setrlimit(res, &l);
                };
                set(libc::RLIMIT_CPU, cpu);
                set(libc::RLIMIT_AS, 2048 * 1024 * 1024);
                set(libc::RLIMIT_FSIZE, MAX_OUTPUT);
                set(libc::RLIMIT_CORE, 0);
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", prog.display()))?;
    // Read output on separate threads so a chatty program cannot block on a full pipe.
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let mut err = child.stderr.take().ok_or("no stderr")?;
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = (&mut out).take(MAX_OUTPUT).read_to_end(&mut v);
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = (&mut err).take(64 * 1024).read_to_end(&mut v);
        v
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} took longer than {}s", prog.file_name().unwrap_or_default().to_string_lossy(), limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    if !status.success() {
        let msg = String::from_utf8_lossy(&stderr);
        let line = msg.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim();
        return Err(format!("{}: {}", prog.file_name().unwrap_or_default().to_string_lossy(), line.chars().take(200).collect::<String>()));
    }
    Ok(stdout)
}

#[derive(Default, Debug, Clone, PartialEq)]
pub struct Meta {
    /// Seconds (audio, video).
    pub duration: f64,
    /// PDF page count.
    pub pages: i64,
    /// Video frame size, or the first PDF page in points (1/72 inch).
    pub width: i64,
    pub height: i64,
}

/// Duration, frame size or page count. Best effort: missing tools give an empty result.
pub fn probe(mime: &str, data: &[u8]) -> Result<Meta, String> {
    let t = tools();
    let k = kind(mime).ok_or("not a media file")?;
    let wd = Workdir::new()?;
    let input = wd.0.join(format!("in.{}", extension(mime)));
    std::fs::write(&input, data).map_err(|e| e.to_string())?;
    let mut m = Meta::default();
    match k {
        Kind::Pdf => {
            let Some(pi) = &t.pdfinfo else { return Ok(m) };
            let out = run(pi, &["-l", "1", "in.pdf"], Some(&wd.0), Duration::from_secs(15))?;
            for line in String::from_utf8_lossy(&out).lines() {
                if let Some(v) = line.strip_prefix("Pages:") {
                    m.pages = v.trim().parse().unwrap_or(0);
                }
                // "Page size:      612 x 792 pts (letter)" or "Page    1 size: ..."
                if line.starts_with("Page size:") || (line.starts_with("Page") && line.contains("size:") && m.width == 0) {
                    let after = line.split("size:").nth(1).unwrap_or("");
                    let nums: Vec<f64> = after.split_whitespace().filter_map(|w| w.parse().ok()).take(2).collect();
                    if nums.len() == 2 {
                        m.width = nums[0].round() as i64;
                        m.height = nums[1].round() as i64;
                    }
                }
            }
        }
        Kind::Video | Kind::Audio => {
            let Some(fp) = &t.ffprobe else { return Ok(m) };
            let name = format!("in.{}", extension(mime));
            let out = run(
                fp,
                &["-v", "error", "-protocol_whitelist", "file", "-f", demuxer(mime), "-show_entries", "format=duration:stream=codec_type,width,height", "-of", "json", &name],
                Some(&wd.0),
                Duration::from_secs(15),
            )?;
            let v: serde_json::Value = serde_json::from_slice(&out).map_err(|e| format!("unreadable ffprobe output: {e}"))?;
            m.duration = v["format"]["duration"].as_str().and_then(|d| d.parse().ok()).filter(|d: &f64| d.is_finite() && *d >= 0.0).unwrap_or(0.0);
            if let Some(s) = v["streams"].as_array().and_then(|a| a.iter().find(|s| s["codec_type"] == "video")) {
                // Audio files can carry a cover picture; it is not the size of the media.
                if k == Kind::Video {
                    m.width = s["width"].as_i64().unwrap_or(0);
                    m.height = s["height"].as_i64().unwrap_or(0);
                }
            }
        }
    }
    Ok(m)
}

/// Options of a preview request.
#[derive(Default, Clone, Copy, Debug)]
pub struct PreviewOpts {
    /// PDF page (1-based; 0 = first).
    pub page: u32,
    /// Video: second to take the frame from (`None` = a tenth into the video, at most 5 s).
    pub time: Option<f64>,
    /// Known duration, used for the default frame time.
    pub duration: f64,
}

/// A PNG preview `width` pixels wide.
pub fn preview(mime: &str, data: &[u8], width: u32, opts: PreviewOpts) -> Result<Vec<u8>, String> {
    let t = tools();
    let k = kind(mime).ok_or("not a media file")?;
    let width = width.clamp(16, 2048);
    let wd = Workdir::new()?;
    let name = format!("in.{}", extension(mime));
    std::fs::write(wd.0.join(&name), data).map_err(|e| e.to_string())?;
    let w = width.to_string();
    match k {
        Kind::Pdf => {
            let prog = t.pdftoppm.as_ref().ok_or("PDF previews need Poppler's pdftoppm, which is not installed")?;
            let page = opts.page.max(1).to_string();
            run(prog, &["-f", &page, "-l", &page, "-png", "-singlefile", "-scale-to-x", &w, "-scale-to-y", "-1", &name, "out"], Some(&wd.0), Duration::from_secs(30))?;
        }
        Kind::Video => {
            let prog = t.ffmpeg.as_ref().ok_or("video previews need ffmpeg, which is not installed")?;
            let at = opts.time.unwrap_or_else(|| (opts.duration * 0.1).min(5.0)).max(0.0);
            let vf = format!("scale={w}:-2");
            let grab = |ss: f64| {
                let ss = format!("{ss:.3}");
                run(
                    prog,
                    &[
                        "-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "2", "-protocol_whitelist", "file", "-ss", &ss, "-f", demuxer(mime), "-i", &name,
                        "-frames:v", "1", "-an", "-vf", &vf, "-f", "image2", "-c:v", "png", "-y", "out.png",
                    ],
                    Some(&wd.0),
                    Duration::from_secs(30),
                )
            };
            grab(at)?;
            // A time past the end gives no frame: fall back to the first one.
            if !wd.0.join("out.png").exists() && at > 0.0 {
                grab(0.0)?;
            }
        }
        Kind::Audio => {
            let prog = t.ffmpeg.as_ref().ok_or("audio previews need ffmpeg, which is not installed")?;
            let h = (width / 4).clamp(16, 512);
            let filter = format!("[0:a]aformat=channel_layouts=mono,showwavespic=s={w}x{h}:colors=#3366cc[v]");
            run(
                prog,
                &[
                    "-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "2", "-protocol_whitelist", "file", "-f", demuxer(mime), "-i", &name,
                    "-filter_complex", &filter, "-map", "[v]", "-frames:v", "1", "-f", "image2", "-c:v", "png", "-y", "out.png",
                ],
                Some(&wd.0),
                Duration::from_secs(30),
            )?;
        }
    }
    let png = std::fs::read(wd.0.join("out.png")).map_err(|_| "the preview program produced no image".to_string())?;
    if !png.starts_with(b"\x89PNG") {
        return Err("the preview program produced no image".into());
    }
    Ok(png)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Tests that need the external programs skip themselves where they are not installed.
    pub fn have(p: &Option<PathBuf>) -> bool {
        let ok = p.is_some();
        if !ok {
            eprintln!("(skipped: media tool not installed)");
        }
        ok
    }

    /// A small media file made with ffmpeg itself: `lavfi` sources need no input file.
    pub fn make_with_ffmpeg(args: &[&str], ext: &str) -> Option<Vec<u8>> {
        let ff = detect().ffmpeg.clone()?;
        let wd = Workdir::new().ok()?;
        let out = format!("out.{ext}");
        let mut a: Vec<&str> = vec!["-nostdin", "-hide_banner", "-loglevel", "error"];
        a.extend_from_slice(args);
        a.extend_from_slice(&["-y", &out]);
        run(&ff, &a, Some(&wd.0), Duration::from_secs(60)).ok()?;
        std::fs::read(wd.0.join(&out)).ok()
    }

    /// A minimal two-page PDF (hand-written, valid xref).
    pub fn two_page_pdf() -> Vec<u8> {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 5 0 R >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 6 0 R >>",
            "<< /Length 35 >>\nstream\n1 0 0 rg 20 20 260 160 re f\nendstream",
            "<< /Length 35 >>\nstream\n0 0 1 rg 20 20 260 160 re f\nendstream",
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offs {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    fn png_size(p: &[u8]) -> (u32, u32) {
        (u32::from_be_bytes([p[16], p[17], p[18], p[19]]), u32::from_be_bytes([p[20], p[21], p[22], p[23]]))
    }

    fn pixel(p: &[u8], x: u32, y: u32) -> [u8; 4] {
        image::load_from_memory(p).unwrap().to_rgba8().get_pixel(x, y).0
    }

    #[test]
    fn pdf_pages_and_metadata() {
        let t = detect();
        if !have(&t.pdftoppm) || !have(&t.pdfinfo) {
            return;
        }
        let pdf = two_page_pdf();
        let m = probe("application/pdf", &pdf).unwrap();
        assert_eq!((m.pages, m.width, m.height), (2, 300, 200));
        let p1 = preview("application/pdf", &pdf, 150, PreviewOpts::default()).unwrap();
        assert_eq!(png_size(&p1), (150, 100));
        assert!(pixel(&p1, 75, 50)[0] > 200 && pixel(&p1, 75, 50)[2] < 50, "page 1 is red");
        let p2 = preview("application/pdf", &pdf, 150, PreviewOpts { page: 2, ..Default::default() }).unwrap();
        assert!(pixel(&p2, 75, 50)[2] > 200 && pixel(&p2, 75, 50)[0] < 50, "page 2 is blue");
        assert!(preview("application/pdf", &pdf, 150, PreviewOpts { page: 9, ..Default::default() }).is_err());
        assert!(preview("application/pdf", b"%PDF-1.4 garbage", 150, PreviewOpts::default()).is_err());
    }

    #[test]
    fn video_and_audio() {
        let t = detect();
        if !have(&t.ffmpeg) || !have(&t.ffprobe) {
            return;
        }
        // 4 s of colour bars at 320x240
        let mp4 = make_with_ffmpeg(&["-f", "lavfi", "-i", "testsrc=size=320x240:rate=10:duration=4", "-pix_fmt", "yuv420p"], "mp4").unwrap();
        let m = probe("video/mp4", &mp4).unwrap();
        assert_eq!((m.width, m.height), (320, 240));
        assert!((m.duration - 4.0).abs() < 0.3, "{}", m.duration);
        let p = preview("video/mp4", &mp4, 160, PreviewOpts { duration: m.duration, ..Default::default() }).unwrap();
        assert_eq!(png_size(&p), (160, 120));
        let late = preview("video/mp4", &mp4, 160, PreviewOpts { time: Some(999.0), duration: m.duration, ..Default::default() }).unwrap();
        assert_eq!(png_size(&late), (160, 120), "past the end falls back to the first frame");
        let webm = make_with_ffmpeg(&["-f", "lavfi", "-i", "testsrc=size=64x48:rate=5:duration=1", "-c:v", "libvpx", "-b:v", "100k"], "webm");
        if let Some(webm) = webm {
            assert_eq!(png_size(&preview("video/webm", &webm, 64, PreviewOpts::default()).unwrap()), (64, 48));
        }
        // 2 s sine tone
        let mp3 = make_with_ffmpeg(&["-f", "lavfi", "-i", "sine=frequency=440:duration=2"], "mp3").unwrap();
        let m = probe("audio/mpeg", &mp3).unwrap();
        assert!((m.duration - 2.0).abs() < 0.3 && m.width == 0, "{m:?}");
        let wave = preview("audio/mpeg", &mp3, 400, PreviewOpts::default()).unwrap();
        assert_eq!(png_size(&wave), (400, 100));
        // garbage and wrong containers fail cleanly
        assert!(preview("video/mp4", b"\0\0\0\x18ftypmp42 garbage", 100, PreviewOpts::default()).is_err());
        assert!(preview("audio/mpeg", &mp4, 100, PreviewOpts::default()).is_err());
    }

    #[test]
    fn ffmpeg_cannot_be_tricked_into_reading_other_files() {
        let t = detect();
        if !have(&t.ffmpeg) {
            return;
        }
        // An HLS playlist pointing at a local file, uploaded with a forged mp3 header. The
        // forced demuxer and the protocol whitelist must stop ffmpeg from following it.
        let secret = std::env::temp_dir().join(format!("hoepedia-secret-{}.txt", crate::util::new_token()));
        std::fs::write(&secret, "TOP SECRET").unwrap();
        let evil = format!("ID3#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:10.0,\nfile://{}\n#EXT-X-ENDLIST\n", secret.display());
        let r = preview("audio/mpeg", evil.as_bytes(), 100, PreviewOpts::default());
        assert!(r.is_err(), "playlist must not be processed");
        let _ = std::fs::remove_file(secret);
    }

    #[test]
    fn hung_programs_are_killed() {
        if cfg!(windows) {
            return;
        }
        let t0 = Instant::now();
        let r = run(Path::new("/bin/sleep"), &["30"], None, Duration::from_millis(300));
        assert!(r.unwrap_err().contains("longer than"));
        assert!(t0.elapsed() < Duration::from_secs(3));
    }
}
