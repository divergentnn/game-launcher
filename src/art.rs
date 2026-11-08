// SPDX-License-Identifier: GPL-3.0-or-later
//! Poster/backdrop cache. Source art is resized once into
//! `~/.cache/game-launcher`, so later starts only decode small JPEGs.
//! All work happens on one background thread; the UI thread never decodes
//! large images.

use crate::library::{home, Art};
use image::{imageops::FilterType, DynamicImage, ImageReader};
use slint::{Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

pub const POSTER_W: u32 = 352;
pub const POSTER_H: u32 = 528;
const BACKDROP_W: u32 = 960;
const BACKDROP_H: u32 = 540;
const ICON_SIZE: u32 = 144;
const FAILED_RETRY: Duration = Duration::from_secs(3 * 24 * 3600);

pub struct Job {
    pub key: String,
    pub art: Art,
}

pub enum Msg {
    Poster(String, SharedPixelBuffer<Rgb8Pixel>),
    Icon(String, SharedPixelBuffer<Rgba8Pixel>),
    BackdropReady(String),
}

pub fn cache_dir() -> PathBuf {
    home().join(".cache/game-launcher")
}

fn safe(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect()
}

pub fn poster_cache(key: &str) -> PathBuf {
    cache_dir().join("posters").join(format!("{}.jpg", safe(key)))
}

pub fn backdrop_cache(key: &str) -> PathBuf {
    cache_dir().join("backdrops").join(format!("{}.jpg", safe(key)))
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The cached file is current if it exists and is not older than its local source.
fn fresh(cache: &Path, source: Option<&Path>) -> bool {
    match (mtime(cache), source.and_then(mtime)) {
        (Some(c), Some(s)) => c >= s,
        (Some(_), None) => true,
        _ => false,
    }
}

fn decode(path: &Path) -> Option<DynamicImage> {
    ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()
}

fn save_jpeg(image: &DynamicImage, dest: &Path) -> Option<()> {
    fs::create_dir_all(dest.parent()?).ok()?;
    let tmp = dest.with_extension("tmp");
    let file = fs::File::create(&tmp).ok()?;
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 88);
    encoder.encode_image(&image.to_rgb8()).ok()?;
    drop(encoder);
    fs::rename(&tmp, dest).ok()
}

/// Download once into the cache. Failures are remembered for a few days.
fn download(urls: &[String], key: &str, kind: &str) -> Option<PathBuf> {
    if urls.is_empty() {
        return None;
    }
    let dir = cache_dir().join("downloads");
    let dest = dir.join(format!("{}-{kind}", safe(key)));
    if dest.is_file() {
        return Some(dest);
    }
    let failed = dir.join(format!("{}-{kind}.failed", safe(key)));
    if let Some(when) = mtime(&failed) {
        if SystemTime::now().duration_since(when).unwrap_or_default() < FAILED_RETRY {
            return None;
        }
    }
    fs::create_dir_all(&dir).ok()?;
    let tmp = dir.join(format!("{}-{kind}.part", safe(key)));
    for url in urls {
        let ok = Command::new("curl")
            .args(["-fsSL", "--max-time", "25", "-o"])
            .arg(&tmp)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && decode(&tmp).is_some() {
            fs::rename(&tmp, &dest).ok()?;
            let _ = fs::remove_file(&failed);
            return Some(dest);
        }
    }
    let _ = fs::remove_file(&tmp);
    let _ = fs::write(&failed, b"");
    None
}

fn rgb_buffer(image: &DynamicImage) -> SharedPixelBuffer<Rgb8Pixel> {
    let rgb = image.to_rgb8();
    SharedPixelBuffer::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height())
}

pub fn load_rgb(path: &Path) -> Option<SharedPixelBuffer<Rgb8Pixel>> {
    decode(path).map(|image| rgb_buffer(&image))
}

fn make_poster(source: &Path, dest: &Path) -> Option<DynamicImage> {
    let image = decode(source)?.resize_to_fill(POSTER_W, POSTER_H, FilterType::Triangle);
    save_jpeg(&image, dest)?;
    Some(image)
}

fn make_backdrop(source: &Path, dest: &Path) -> Option<()> {
    // Blur at low resolution (fast), then upscale; the blur hides the scaling.
    let small = decode(source)?.resize_to_fill(BACKDROP_W / 4, BACKDROP_H / 4, FilterType::Triangle);
    let blurred = small.blur(3.5).resize_exact(BACKDROP_W, BACKDROP_H, FilterType::Triangle);
    save_jpeg(&blurred, dest)
}

fn local_poster(art: &Art) -> Option<PathBuf> {
    art.poster_files.iter().find(|p| p.is_file()).cloned()
}

fn local_backdrop(art: &Art) -> Option<PathBuf> {
    art.backdrop_files.iter().find(|p| p.is_file()).cloned()
}

fn send_icon(job: &Job, send: &impl Fn(Msg)) {
    if let Some(image) = job.art.icon_file.as_deref().and_then(decode) {
        let image = image.resize(ICON_SIZE, ICON_SIZE, FilterType::Triangle).to_rgba8();
        let buffer = SharedPixelBuffer::clone_from_slice(image.as_raw(), image.width(), image.height());
        send(Msg::Icon(job.key.clone(), buffer));
    }
}

/// Process jobs in display order on one low-priority background thread.
pub fn spawn_worker(jobs: Vec<Job>, send: impl Fn(Msg) + Send + 'static) {
    std::thread::spawn(move || {
        unsafe {
            libc::nice(10);
        }
        let mut need_download = Vec::new();
        // 1. Posters available locally (or already cached): fast, shown first.
        for job in &jobs {
            let cache = poster_cache(&job.key);
            let local = local_poster(&job.art);
            let image = if fresh(&cache, local.as_deref()) {
                decode(&cache)
            } else if let Some(source) = &local {
                make_poster(source, &cache)
            } else {
                None
            };
            match image {
                Some(image) => send(Msg::Poster(job.key.clone(), rgb_buffer(&image))),
                None => {
                    send_icon(job, &send);
                    need_download.push(job);
                }
            }
        }
        // 2. Backdrops from local art (falling back to the local poster).
        let mut backdrop_download = Vec::new();
        for job in &jobs {
            let cache = backdrop_cache(&job.key);
            let source = local_backdrop(&job.art).or_else(|| local_poster(&job.art));
            if fresh(&cache, source.as_deref()) {
                continue;
            }
            match source {
                Some(source) if make_backdrop(&source, &cache).is_some() => send(Msg::BackdropReady(job.key.clone())),
                _ => backdrop_download.push(job),
            }
        }
        // 3. Remote art, once; cached for every later start.
        for job in need_download {
            let Some(source) = download(&job.art.poster_urls, &job.key, "poster") else { continue };
            if let Some(image) = make_poster(&source, &poster_cache(&job.key)) {
                send(Msg::Poster(job.key.clone(), rgb_buffer(&image)));
            }
        }
        for job in backdrop_download {
            let source = download(&job.art.backdrop_urls, &job.key, "backdrop")
                .or_else(|| download(&job.art.poster_urls, &job.key, "poster"));
            if let Some(source) = source {
                if make_backdrop(&source, &backdrop_cache(&job.key)).is_some() {
                    send(Msg::BackdropReady(job.key.clone()));
                }
            }
        }
    });
}
