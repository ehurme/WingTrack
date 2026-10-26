//! Video export: frames captured from the map (+ legend) and timeline / sensor-chart panels,
//! encoded to MP4 (native, via an `ffmpeg` on PATH) or animated GIF (pure Rust, native and web).
//!
//! The app drives recording deterministically: it sets the clock for a frame, asks egui for a
//! screenshot, waits for it, crops it with [`prepare`] and hands it to a [`Sink`], then advances
//! the clock by a fixed amount of tracking time. Render speed therefore never affects the output.

use egui::ColorImage;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Mp4,
    Gif,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Mp4 => "MP4 (H.264, via ffmpeg)",
            Format::Gif => "Animated GIF",
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            Format::Mp4 => "mp4",
            Format::Gif => "gif",
        }
    }
}

pub struct Settings {
    pub format: Format,
    pub fps: u32,
    /// Index into the app's playback speeds (tracking time per video second).
    pub speed_idx: usize,
    pub from_cursor: bool,
    /// Integer downscale factor (1 = full resolution).
    pub downscale: usize,
    pub include_timeline: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            // Switched to MP4 when the export dialog first finds ffmpeg.
            format: Format::Gif,
            fps: 30,
            speed_idx: 3,
            from_cursor: false,
            downscale: 1,
            include_timeline: true,
        }
    }
}

impl Settings {
    /// GIF frame delays are in centiseconds, and most viewers clamp anything under 2 cs.
    pub fn effective_fps(&self) -> u32 {
        match self.format {
            Format::Mp4 => self.fps.clamp(1, 60),
            Format::Gif => self.fps.clamp(1, 25),
        }
    }
}

/// An RGBA frame ready for encoding.
pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

/// Crop `[x, y, w, h]` (pixels) out of a screenshot, box-filter it down by `k`, trim to even
/// dimensions (H.264 needs them) and force it opaque.
pub fn prepare(img: &ColorImage, [x0, y0, cw, ch]: [usize; 4], k: usize) -> Result<Frame, String> {
    if x0 + cw > img.size[0] || y0 + ch > img.size[1] {
        return Err("the window got smaller during recording".into());
    }
    let k = k.max(1);
    let w = (cw / k) & !1;
    let h = (ch / k) & !1;
    if w == 0 || h == 0 {
        return Err("capture area is too small".into());
    }
    let n = (k * k) as u32;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0u32; 3];
            for dy in 0..k {
                let row = (y0 + y * k + dy) * img.size[0] + x0 + x * k;
                for p in &img.pixels[row..row + k] {
                    acc[0] += p.r() as u32;
                    acc[1] += p.g() as u32;
                    acc[2] += p.b() as u32;
                }
            }
            rgba.extend_from_slice(&[
                (acc[0] / n) as u8,
                (acc[1] / n) as u8,
                (acc[2] / n) as u8,
                255,
            ]);
        }
    }
    Ok(Frame { w, h, rgba })
}

#[cfg(not(target_arch = "wasm32"))]
type GifOut = std::io::BufWriter<std::fs::File>;
#[cfg(target_arch = "wasm32")]
type GifOut = Vec<u8>;

enum Inner {
    #[cfg(not(target_arch = "wasm32"))]
    Ffmpeg {
        child: std::process::Child,
        stdin: std::process::ChildStdin,
    },
    Gif(gif::Encoder<GifOut>),
}

/// Single-threaded encoder; opened lazily on the first frame, when the size is known.
struct Encoder {
    format: Format,
    fps: u32,
    #[cfg(not(target_arch = "wasm32"))]
    path: std::path::PathBuf,
    inner: Option<Inner>,
    size: (usize, usize),
}

impl Encoder {
    fn open(&mut self, w: usize, h: usize) -> Result<Inner, String> {
        match self.format {
            #[cfg(not(target_arch = "wasm32"))]
            Format::Mp4 => {
                use std::process::{Command, Stdio};
                let mut cmd = Command::new("ffmpeg");
                cmd.args([
                    "-y",
                    "-loglevel",
                    "error",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgba",
                ])
                .args([
                    "-s",
                    &format!("{w}x{h}"),
                    "-r",
                    &self.fps.to_string(),
                    "-i",
                    "-",
                ])
                .args(["-c:v", "libx264", "-preset", "medium", "-crf", "18"])
                .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"])
                .arg(&self.path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
                no_console(&mut cmd);
                let mut child = cmd
                    .spawn()
                    .map_err(|e| format!("could not start ffmpeg: {e}"))?;
                let stdin = child.stdin.take().ok_or("ffmpeg stdin unavailable")?;
                Ok(Inner::Ffmpeg { child, stdin })
            }
            #[cfg(target_arch = "wasm32")]
            Format::Mp4 => Err("MP4 export needs the desktop app (ffmpeg)".into()),
            Format::Gif => {
                #[cfg(not(target_arch = "wasm32"))]
                let out = std::io::BufWriter::new(
                    std::fs::File::create(&self.path).map_err(|e| e.to_string())?,
                );
                #[cfg(target_arch = "wasm32")]
                let out = Vec::new();
                let (gw, gh) = (u16::try_from(w), u16::try_from(h));
                let (Ok(gw), Ok(gh)) = (gw, gh) else {
                    return Err("frame too large for GIF".into());
                };
                let mut enc = gif::Encoder::new(out, gw, gh, &[]).map_err(|e| e.to_string())?;
                enc.set_repeat(gif::Repeat::Infinite)
                    .map_err(|e| e.to_string())?;
                Ok(Inner::Gif(enc))
            }
        }
    }

    fn push(&mut self, mut f: Frame) -> Result<(), String> {
        if self.inner.is_none() {
            self.inner = Some(self.open(f.w, f.h)?);
            self.size = (f.w, f.h);
        }
        if self.size != (f.w, f.h) {
            return Err("frame size changed during recording (window resized?)".into());
        }
        match self.inner.as_mut().expect("opened above") {
            #[cfg(not(target_arch = "wasm32"))]
            Inner::Ffmpeg { stdin, .. } => {
                use std::io::Write;
                stdin.write_all(&f.rgba).map_err(|e| format!("ffmpeg: {e}"))
            }
            Inner::Gif(enc) => {
                let mut gf = gif::Frame::from_rgba_speed(f.w as u16, f.h as u16, &mut f.rgba, 10);
                gf.delay = (100.0 / self.fps as f32).round().max(2.0) as u16;
                enc.write_frame(&gf).map_err(|e| e.to_string())
            }
        }
    }

    /// Flush and close. Native returns nothing extra; web returns the encoded file.
    fn finish(self) -> Result<Vec<u8>, String> {
        match self.inner {
            None => Err("no frames were captured".into()),
            #[cfg(not(target_arch = "wasm32"))]
            Some(Inner::Ffmpeg { child, stdin }) => {
                drop(stdin);
                let out = child.wait_with_output().map_err(|e| e.to_string())?;
                if out.status.success() {
                    Ok(Vec::new())
                } else {
                    Err(format!(
                        "ffmpeg failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ))
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            Some(Inner::Gif(enc)) => {
                use std::io::Write;
                let mut w = enc.into_inner().map_err(|e| e.to_string())?;
                w.flush().map_err(|e| e.to_string())?;
                Ok(Vec::new())
            }
            #[cfg(target_arch = "wasm32")]
            Some(Inner::Gif(enc)) => enc.into_inner().map_err(|e| e.to_string()),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn no_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Is an `ffmpeg` executable on PATH?
pub fn ffmpeg_available() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        no_console(&mut cmd);
        cmd.status().is_ok_and(|s| s.success())
    }
    #[cfg(target_arch = "wasm32")]
    false
}

/// Where encoded frames go. Native encodes on a worker thread so the UI keeps rendering;
/// the web build encodes inline (no threads).
pub struct Sink {
    #[cfg(not(target_arch = "wasm32"))]
    tx: Option<std::sync::mpsc::SyncSender<Frame>>,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<std::thread::JoinHandle<Result<(), String>>>,
    #[cfg(target_arch = "wasm32")]
    enc: Encoder,
}

impl Sink {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(format: Format, fps: u32, path: std::path::PathBuf) -> Sink {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Frame>(4);
        let worker = std::thread::spawn(move || {
            let mut enc = Encoder {
                format,
                fps,
                path,
                inner: None,
                size: (0, 0),
            };
            for f in rx {
                if let Err(e) = enc.push(f) {
                    // ffmpeg's own message (from finish) is more useful than "broken pipe".
                    return Err(enc.finish().err().unwrap_or(e));
                }
            }
            enc.finish().map(|_| ())
        });
        Sink {
            tx: Some(tx),
            worker: Some(worker),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn new(format: Format, fps: u32) -> Sink {
        Sink {
            enc: Encoder {
                format,
                fps,
                inner: None,
                size: (0, 0),
            },
        }
    }

    pub fn push(&mut self, f: Frame) -> Result<(), String> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let sent = self.tx.as_ref().is_some_and(|tx| tx.send(f).is_ok());
            if sent {
                Ok(())
            } else {
                Err(self
                    .join()
                    .err()
                    .unwrap_or_else(|| "encoder stopped".into()))
            }
        }
        #[cfg(target_arch = "wasm32")]
        self.enc.push(f)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn join(&mut self) -> Result<(), String> {
        self.tx = None;
        match self.worker.take() {
            Some(h) => h
                .join()
                .map_err(|_| "encoder thread panicked".to_string())?,
            None => Ok(()),
        }
    }

    /// Close the file. Native blocks until the encoder has flushed (call off the UI thread);
    /// web returns the encoded bytes for download.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn finish(mut self) -> Result<(), String> {
        self.join()
    }

    #[cfg(target_arch = "wasm32")]
    pub fn finish(self) -> Result<Vec<u8>, String> {
        self.enc.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Color32;

    #[test]
    fn prepare_crops_downscales_and_evens() {
        // 7×5 image, left half red, right half blue.
        let px: Vec<Color32> = (0..35)
            .map(|i| {
                if i % 7 < 4 {
                    Color32::RED
                } else {
                    Color32::BLUE
                }
            })
            .collect();
        let img = ColorImage::new([7, 5], px);
        let f = prepare(&img, [0, 0, 7, 5], 1).unwrap();
        assert_eq!((f.w, f.h), (6, 4));
        let f = prepare(&img, [0, 0, 4, 4], 2).unwrap();
        assert_eq!((f.w, f.h), (2, 2));
        assert_eq!(&f.rgba[0..4], &[255, 0, 0, 255]);
        assert!(prepare(&img, [4, 0, 4, 5], 1).is_err(), "out of bounds");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn gif_roundtrip() {
        let path = std::env::temp_dir().join("wingtrack_video_test.gif");
        let mut sink = Sink::new(Format::Gif, 10, path.clone());
        for i in 0..3u8 {
            sink.push(Frame {
                w: 4,
                h: 2,
                rgba: [i * 80, 0, 0, 255].repeat(8),
            })
            .unwrap();
        }
        sink.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"GIF89a"));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn mp4_via_ffmpeg() {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        }
        let path = std::env::temp_dir().join("wingtrack_video_test.mp4");
        let mut sink = Sink::new(Format::Mp4, 30, path.clone());
        for i in 0..30u8 {
            sink.push(Frame {
                w: 64,
                h: 48,
                rgba: [i * 8, 100, 200, 255].repeat(64 * 48),
            })
            .unwrap();
        }
        sink.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[4..8], b"ftyp", "MP4 container");
        let _ = std::fs::remove_file(path);
    }
}
