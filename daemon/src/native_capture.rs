#![cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MIN_STUDIO_WINDOW_WIDTH: f64 = 320.0;
const MIN_STUDIO_WINDOW_HEIGHT: f64 = 200.0;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct LogicalRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureLimits {
    pub max_dimension: u32,
    pub max_pixels: u64,
    pub max_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct StudioWindow {
    #[serde(rename = "windowId")]
    pub window_id: u32,
    #[serde(rename = "ownerPid")]
    pub owner_pid: i32,
    pub owner: String,
    pub title: String,
    pub bounds: LogicalRect,
    #[serde(skip)]
    z_order: usize,
}

#[derive(Debug)]
pub struct NativeCaptureRequest<'a> {
    pub project_hint: Option<&'a str>,
    pub region: Option<CaptureRegion>,
    pub output_size: Option<[u32; 2]>,
    pub pixelated: bool,
    pub output: &'a Path,
    pub deadline: Instant,
    pub limits: CaptureLimits,
}

#[derive(Debug)]
pub struct NativeCaptureResult {
    pub output_path: PathBuf,
    pub size: usize,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub position: [f64; 2],
    pub window: StudioWindow,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct NativePermissionStatus {
    pub available: bool,
    pub authorized: bool,
}

pub fn screen_capture_permission_status() -> NativePermissionStatus {
    #[cfg(target_os = "macos")]
    {
        NativePermissionStatus {
            available: true,
            authorized: macos::preflight_screen_capture_access(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        NativePermissionStatus {
            available: false,
            authorized: false,
        }
    }
}

pub fn request_screen_capture_permission() -> Result<NativePermissionStatus, String> {
    #[cfg(target_os = "macos")]
    {
        let authorized = macos::preflight_screen_capture_access()
            || macos::request_screen_capture_access()
            || macos::preflight_screen_capture_access();
        Ok(NativePermissionStatus {
            available: true,
            authorized,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("native Roblox Studio window capture is only available on macOS".into())
    }
}

#[derive(Clone, Debug, PartialEq)]
struct RgbaImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PixelRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn create() -> Result<Self, String> {
        let base = std::env::temp_dir();
        for _ in 0..32 {
            let suffix = unique_suffix();
            let path = base.join(format!("rosync-native-capture-{suffix}"));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "create native capture temporary directory {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Err("could not allocate a unique native capture temporary directory".into())
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{counter}", std::process::id())
}

fn validate_dimensions(width: u32, height: u32, limits: CaptureLimits) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("native capture dimensions must be positive".into());
    }
    if width > limits.max_dimension || height > limits.max_dimension {
        return Err(format!(
            "native capture dimensions {width}x{height} exceed the {}px per-axis limit",
            limits.max_dimension
        ));
    }
    if u64::from(width) * u64::from(height) > limits.max_pixels {
        return Err(format!(
            "native capture dimensions {width}x{height} exceed the {}-pixel limit",
            limits.max_pixels
        ));
    }
    Ok(())
}

fn is_roblox_studio_owner(owner: &str) -> bool {
    owner
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        == "robloxstudio"
}

fn meaningful_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|token| {
            token.len() >= 2 && token != "roblox" && token != "studio" && token != "rbxl"
        })
        .collect()
}

fn window_project_match(window: &StudioWindow, project_hint: Option<&str>) -> usize {
    let Some(project_hint) = project_hint else {
        return 0;
    };
    let project_tokens = meaningful_tokens(project_hint);
    if project_tokens.is_empty() {
        return 0;
    }
    let title_tokens = meaningful_tokens(&window.title);
    project_tokens
        .iter()
        .filter(|token| title_tokens.contains(token))
        .count()
}

fn select_studio_window(
    windows: Vec<StudioWindow>,
    project_hint: Option<&str>,
) -> Result<StudioWindow, String> {
    let candidates = windows
        .into_iter()
        .filter(|window| {
            is_roblox_studio_owner(&window.owner)
                && window.owner_pid > 0
                && window.bounds.x.is_finite()
                && window.bounds.y.is_finite()
                && window.bounds.width.is_finite()
                && window.bounds.height.is_finite()
                && window.bounds.width >= MIN_STUDIO_WINDOW_WIDTH
                && window.bounds.height >= MIN_STUDIO_WINDOW_HEIGHT
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(
            "no visible Roblox Studio window was found; native fallback refuses to capture another application"
                .to_string()
        );
    }

    // Studio dialogs are separate CoreGraphics windows. Match the project at
    // the process level, then choose that process's largest window so a titled
    // modal never replaces the main Studio surface in the resulting capture.
    let selected_pid = candidates
        .iter()
        .map(|window| window.owner_pid)
        .max_by(|left_pid, right_pid| {
            let process_score = |pid: i32| {
                let windows = candidates.iter().filter(|window| window.owner_pid == pid);
                let project_match = windows
                    .clone()
                    .map(|window| window_project_match(window, project_hint))
                    .max()
                    .unwrap_or_default();
                let area = windows
                    .map(|window| window.bounds.width * window.bounds.height)
                    .max_by(f64::total_cmp)
                    .unwrap_or_default();
                (project_match, area)
            };
            let left = process_score(*left_pid);
            let right = process_score(*right_pid);
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.total_cmp(&right.1))
        })
        .expect("candidate list was checked as non-empty");
    Ok(candidates
        .into_iter()
        .filter(|window| window.owner_pid == selected_pid)
        .max_by(|left, right| {
            let left_area = left.bounds.width * left.bounds.height;
            let right_area = right.bounds.width * right.bounds.height;
            left_area
                .total_cmp(&right_area)
                // CGWindowListCopyWindowInfo is front-to-back. Prefer the earlier
                // window only when areas are otherwise equal.
                .then_with(|| right.z_order.cmp(&left.z_order))
        })
        .expect("selected process has at least one candidate window"))
}

fn pixel_crop_rect(
    image_width: u32,
    image_height: u32,
    window_bounds: LogicalRect,
    region: Option<CaptureRegion>,
) -> Result<(PixelRect, [f64; 2]), String> {
    if image_width == 0
        || image_height == 0
        || !window_bounds.width.is_finite()
        || !window_bounds.height.is_finite()
        || window_bounds.width <= 0.0
        || window_bounds.height <= 0.0
    {
        return Err("native capture returned invalid window geometry".into());
    }
    let Some(region) = region else {
        return Ok((
            PixelRect {
                x: 0,
                y: 0,
                width: image_width,
                height: image_height,
            },
            [window_bounds.x, window_bounds.y],
        ));
    };

    let region_x = f64::from(region.x);
    let region_y = f64::from(region.y);
    let region_right = region_x + f64::from(region.width);
    let region_bottom = region_y + f64::from(region.height);
    let window_right = window_bounds.x + window_bounds.width;
    let window_bottom = window_bounds.y + window_bounds.height;
    const EPSILON: f64 = 0.01;
    if region_x < window_bounds.x - EPSILON
        || region_y < window_bounds.y - EPSILON
        || region_right > window_right + EPSILON
        || region_bottom > window_bottom + EPSILON
    {
        return Err(format!(
            "requested logical screen region {},{},{},{} is outside the selected Roblox Studio window at {:.0},{:.0},{:.0},{:.0}",
            region.x,
            region.y,
            region.width,
            region.height,
            window_bounds.x,
            window_bounds.y,
            window_bounds.width,
            window_bounds.height
        ));
    }

    let scale_x = f64::from(image_width) / window_bounds.width;
    let scale_y = f64::from(image_height) / window_bounds.height;
    let left = ((region_x - window_bounds.x) * scale_x)
        .round()
        .clamp(0.0, f64::from(image_width)) as u32;
    let top = ((region_y - window_bounds.y) * scale_y)
        .round()
        .clamp(0.0, f64::from(image_height)) as u32;
    let right = ((region_right - window_bounds.x) * scale_x)
        .round()
        .clamp(0.0, f64::from(image_width)) as u32;
    let bottom = ((region_bottom - window_bounds.y) * scale_y)
        .round()
        .clamp(0.0, f64::from(image_height)) as u32;
    if right <= left || bottom <= top {
        return Err("requested logical region maps to an empty native pixel region".into());
    }
    Ok((
        PixelRect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        },
        [region_x, region_y],
    ))
}

fn decode_png(bytes: &[u8]) -> Result<RgbaImage, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("decode native capture PNG header: {error}"))?;
    let mut source = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut source)
        .map_err(|error| format!("decode native capture PNG: {error}"))?;
    let source = &source[..info.buffer_size()];
    let pixel_count = usize::try_from(u64::from(info.width) * u64::from(info.height))
        .map_err(|_| "native capture pixel count does not fit this platform")?;
    let capacity = pixel_count
        .checked_mul(4)
        .ok_or("native capture decoded size overflow")?;
    let mut pixels = Vec::with_capacity(capacity);
    match info.color_type {
        png::ColorType::Rgba => pixels.extend_from_slice(source),
        png::ColorType::Rgb => {
            for pixel in source.chunks_exact(3) {
                pixels.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for pixel in source.chunks_exact(2) {
                pixels.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for &value in source {
                pixels.extend_from_slice(&[value, value, value, 255]);
            }
        }
        png::ColorType::Indexed => {
            return Err("native capture PNG remained indexed after expansion".into());
        }
    }
    if pixels.len() != capacity {
        return Err(format!(
            "native capture decoded {} RGBA bytes, expected {capacity}",
            pixels.len()
        ));
    }
    Ok(RgbaImage {
        width: info.width,
        height: info.height,
        pixels,
    })
}

fn crop_image(image: &RgbaImage, crop: PixelRect) -> Result<RgbaImage, String> {
    let right = crop
        .x
        .checked_add(crop.width)
        .ok_or("native capture crop overflow")?;
    let bottom = crop
        .y
        .checked_add(crop.height)
        .ok_or("native capture crop overflow")?;
    if crop.width == 0 || crop.height == 0 || right > image.width || bottom > image.height {
        return Err("native capture crop lies outside the decoded image".into());
    }
    if crop.x == 0 && crop.y == 0 && crop.width == image.width && crop.height == image.height {
        return Ok(image.clone());
    }
    let row_bytes = usize::try_from(crop.width)
        .map_err(|_| "native capture crop width does not fit this platform")?
        .checked_mul(4)
        .ok_or("native capture crop row overflow")?;
    let capacity = row_bytes
        .checked_mul(
            usize::try_from(crop.height)
                .map_err(|_| "native capture crop height does not fit this platform")?,
        )
        .ok_or("native capture crop size overflow")?;
    let mut pixels = Vec::with_capacity(capacity);
    for y in crop.y..bottom {
        let start = (u64::from(y) * u64::from(image.width) + u64::from(crop.x)) * 4;
        let start = usize::try_from(start).map_err(|_| "native capture crop offset overflow")?;
        pixels.extend_from_slice(&image.pixels[start..start + row_bytes]);
    }
    Ok(RgbaImage {
        width: crop.width,
        height: crop.height,
        pixels,
    })
}

fn resize_nearest(image: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    let mut pixels = vec![0; width as usize * height as usize * 4];
    for y in 0..height {
        let source_y = (u64::from(y) * u64::from(image.height) / u64::from(height)) as u32;
        for x in 0..width {
            let source_x = (u64::from(x) * u64::from(image.width) / u64::from(width)) as u32;
            let source = ((source_y * image.width + source_x) * 4) as usize;
            let destination = ((y * width + x) * 4) as usize;
            pixels[destination..destination + 4].copy_from_slice(&image.pixels[source..source + 4]);
        }
    }
    RgbaImage {
        width,
        height,
        pixels,
    }
}

fn resize_bilinear(image: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    let mut pixels = vec![0; width as usize * height as usize * 4];
    let scale_x = f64::from(image.width) / f64::from(width);
    let scale_y = f64::from(image.height) / f64::from(height);
    for y in 0..height {
        let source_y =
            ((f64::from(y) + 0.5) * scale_y - 0.5).clamp(0.0, f64::from(image.height - 1));
        let y0 = source_y.floor() as u32;
        let y1 = (y0 + 1).min(image.height - 1);
        let fy = source_y - f64::from(y0);
        for x in 0..width {
            let source_x =
                ((f64::from(x) + 0.5) * scale_x - 0.5).clamp(0.0, f64::from(image.width - 1));
            let x0 = source_x.floor() as u32;
            let x1 = (x0 + 1).min(image.width - 1);
            let fx = source_x - f64::from(x0);
            let offsets = [
                ((y0 * image.width + x0) * 4) as usize,
                ((y0 * image.width + x1) * 4) as usize,
                ((y1 * image.width + x0) * 4) as usize,
                ((y1 * image.width + x1) * 4) as usize,
            ];
            let weights = [
                (1.0 - fx) * (1.0 - fy),
                fx * (1.0 - fy),
                (1.0 - fx) * fy,
                fx * fy,
            ];
            let destination = ((y * width + x) * 4) as usize;
            // Interpolate RGB in premultiplied-alpha space so transparent window
            // edges do not acquire a dark fringe when resized.
            let mut alpha = 0.0;
            let mut premultiplied = [0.0; 3];
            for (offset, weight) in offsets.into_iter().zip(weights) {
                let source_alpha = f64::from(image.pixels[offset + 3]) / 255.0;
                alpha += source_alpha * weight;
                for (channel, accumulated) in premultiplied.iter_mut().enumerate() {
                    *accumulated +=
                        f64::from(image.pixels[offset + channel]) * source_alpha * weight;
                }
            }
            if alpha > 0.0 {
                for (channel, accumulated) in premultiplied.iter().enumerate() {
                    pixels[destination + channel] =
                        (accumulated / alpha).round().clamp(0.0, 255.0) as u8;
                }
            }
            pixels[destination + 3] = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    RgbaImage {
        width,
        height,
        pixels,
    }
}

fn resize_image(image: &RgbaImage, width: u32, height: u32, pixelated: bool) -> RgbaImage {
    if image.width == width && image.height == height {
        return image.clone();
    }
    if pixelated {
        resize_nearest(image, width, height)
    } else {
        resize_bilinear(image, width, height)
    }
}

fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("encode native capture PNG header: {error}"))?;
        writer
            .write_image_data(&image.pixels)
            .map_err(|error| format!("encode native capture PNG: {error}"))?;
    }
    Ok(output)
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;

    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("read native capture metadata {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(format!(
            "native capture file size {} is outside the 1..={max_bytes} byte limit",
            metadata.len()
        ));
    }
    let expected = usize::try_from(metadata.len())
        .map_err(|_| "native capture file size does not fit this platform")?;
    let file = std::fs::File::open(path)
        .map_err(|error| format!("open native capture {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(expected);
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read native capture {}: {error}", path.display()))?;
    if bytes.len() != expected {
        return Err(format!(
            "native capture read {} bytes, expected {expected}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

fn write_output(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
    }
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("capture.png");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", unique_suffix()));
    let write_result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| format!("create {}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path).map_err(|error| {
            format!(
                "replace native capture output {} with {}: {error}",
                path.display(),
                temporary.display()
            )
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result?;
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

pub async fn capture_studio_window(
    request: NativeCaptureRequest<'_>,
) -> Result<NativeCaptureResult, String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = request;
        return Err("native Studio window capture is only available on macOS".into());
    }

    #[cfg(target_os = "macos")]
    {
        if !macos::preflight_screen_capture_access() {
            return Err(
                "macOS screen capture permission is not granted; run `rosync capture authorize` first"
                    .into(),
            );
        }
        let windows = macos::visible_windows()?;
        let window = select_studio_window(windows, request.project_hint)?;
        let temp = TempDirectory::create()?;
        let source_path = temp.path.join("studio-window.png");
        let remaining = request.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("native capture deadline expired before screencapture".into());
        }
        let window_argument = format!("-l{}", window.window_id);
        let child = tokio::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-o", "-a", &window_argument, "-tpng"])
            .arg(&source_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        let output = tokio::time::timeout(remaining, child)
            .await
            .map_err(|_| "native capture deadline expired during screencapture".to_string())?
            .map_err(|error| format!("launch /usr/sbin/screencapture: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                output.status.to_string()
            } else {
                format!("{}: {stderr}", output.status)
            };
            return Err(format!(
                "macOS could not capture the selected Roblox Studio window ({detail}). Grant Screen & System Audio Recording permission to the app running rosync, then retry"
            ));
        }
        if request
            .deadline
            .saturating_duration_since(Instant::now())
            .is_zero()
        {
            return Err("native capture deadline expired before image processing".into());
        }
        let source_bytes = read_bounded(&source_path, request.limits.max_bytes)?;
        let decoded = decode_png(&source_bytes)?;
        validate_dimensions(decoded.width, decoded.height, request.limits)?;
        let (crop, position) =
            pixel_crop_rect(decoded.width, decoded.height, window.bounds, request.region)?;
        validate_dimensions(crop.width, crop.height, request.limits)?;
        let cropped = crop_image(&decoded, crop)?;
        let [output_width, output_height] = request
            .output_size
            .unwrap_or([cropped.width, cropped.height]);
        validate_dimensions(output_width, output_height, request.limits)?;
        let resized = resize_image(&cropped, output_width, output_height, request.pixelated);
        if request
            .deadline
            .saturating_duration_since(Instant::now())
            .is_zero()
        {
            return Err("native capture deadline expired before PNG encoding".into());
        }
        let bytes = encode_png(&resized)?;
        let size = bytes.len();
        if size == 0 || u64::try_from(size).unwrap_or(u64::MAX) > request.limits.max_bytes {
            return Err(format!(
                "encoded native capture size {size} is outside the 1..={} byte limit",
                request.limits.max_bytes
            ));
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let output_path = write_output(request.output, &bytes)?;
        Ok(NativeCaptureResult {
            output_path,
            size,
            sha256,
            width: resized.width,
            height: resized.height,
            position,
            window,
        })
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{LogicalRect, StudioWindow};
    use std::ffi::{c_char, c_void, CStr};

    type CFTypeRef = *const c_void;
    type CFArrayRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFIndex = isize;
    type CFTypeId = usize;
    type Boolean = u8;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CF_NUMBER_SINT64_TYPE: i32 = 4;
    const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1;
    const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
        fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> CFTypeRef;
        fn CFDictionaryGetTypeID() -> CFTypeId;
        fn CFDictionaryGetValue(dictionary: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
        fn CFGetTypeID(value: CFTypeRef) -> CFTypeId;
        fn CFNumberGetTypeID() -> CFTypeId;
        fn CFNumberGetValue(number: CFTypeRef, number_type: i32, value: *mut c_void) -> Boolean;
        fn CFRelease(value: CFTypeRef);
        fn CFStringGetCString(
            string: CFStringRef,
            buffer: *mut c_char,
            buffer_size: CFIndex,
            encoding: u32,
        ) -> Boolean;
        fn CFStringGetLength(string: CFStringRef) -> CFIndex;
        fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
        fn CFStringGetTypeID() -> CFTypeId;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> Boolean;
        fn CGRequestScreenCaptureAccess() -> Boolean;
        fn CGWindowListCopyWindowInfo(options: u32, relative_to_window: u32) -> CFArrayRef;
        fn CGRectMakeWithDictionaryRepresentation(
            dictionary: CFDictionaryRef,
            rectangle: *mut CGRect,
        ) -> Boolean;
        static kCGWindowBounds: CFStringRef;
        static kCGWindowLayer: CFStringRef;
        static kCGWindowName: CFStringRef;
        static kCGWindowNumber: CFStringRef;
        static kCGWindowOwnerName: CFStringRef;
        static kCGWindowOwnerPID: CFStringRef;
    }

    struct OwnedCf(CFTypeRef);

    pub(super) fn preflight_screen_capture_access() -> bool {
        // SAFETY: This CoreGraphics function takes no arguments and has no memory
        // ownership contract; it only reads the current process permission.
        unsafe { CGPreflightScreenCaptureAccess() != 0 }
    }

    pub(super) fn request_screen_capture_access() -> bool {
        // SAFETY: This CoreGraphics function takes no arguments and may show the
        // system privacy prompt. Callers invoke it only from explicit authorize.
        unsafe { CGRequestScreenCaptureAccess() != 0 }
    }

    impl Drop for OwnedCf {
        fn drop(&mut self) {
            // SAFETY: OwnedCf is only constructed for a non-null value returned by
            // a Core Foundation Create/Copy function, so it owns one retain.
            unsafe { CFRelease(self.0) };
        }
    }

    unsafe fn dictionary_value(dictionary: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef {
        // SAFETY: The caller passes a live CFDictionary and one of CoreGraphics'
        // process-lifetime CFString keys.
        unsafe { CFDictionaryGetValue(dictionary, key) }
    }

    unsafe fn cf_i64(value: CFTypeRef) -> Option<i64> {
        if value.is_null() {
            return None;
        }
        // SAFETY: CFGetTypeID accepts any non-null CF object.
        if unsafe { CFGetTypeID(value) } != unsafe { CFNumberGetTypeID() } {
            return None;
        }
        let mut number = 0i64;
        // SAFETY: `number` is valid writable storage for kCFNumberSInt64Type.
        let converted = unsafe {
            CFNumberGetValue(
                value,
                K_CF_NUMBER_SINT64_TYPE,
                (&mut number as *mut i64).cast(),
            )
        };
        (converted != 0).then_some(number)
    }

    unsafe fn cf_string(value: CFTypeRef) -> Option<String> {
        if value.is_null() {
            return None;
        }
        // SAFETY: CFGetTypeID accepts any non-null CF object.
        if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            return None;
        }
        // SAFETY: Type checking above establishes that value is a CFString.
        let length = unsafe { CFStringGetLength(value) };
        // SAFETY: CoreFoundation computes an upper bound for this string/encoding.
        let maximum =
            unsafe { CFStringGetMaximumSizeForEncoding(length, K_CF_STRING_ENCODING_UTF8) };
        if maximum < 0 {
            return None;
        }
        let capacity = usize::try_from(maximum).ok()?.checked_add(1)?;
        let mut buffer = vec![0u8; capacity];
        // SAFETY: The buffer has `capacity` writable bytes and value is a CFString.
        if unsafe {
            CFStringGetCString(
                value,
                buffer.as_mut_ptr().cast(),
                capacity as CFIndex,
                K_CF_STRING_ENCODING_UTF8,
            )
        } == 0
        {
            return None;
        }
        // SAFETY: CFStringGetCString guarantees NUL termination on success.
        let c_string = unsafe { CStr::from_ptr(buffer.as_ptr().cast()) };
        Some(c_string.to_string_lossy().into_owned())
    }

    unsafe fn cf_rect(value: CFTypeRef) -> Option<LogicalRect> {
        if value.is_null() {
            return None;
        }
        // SAFETY: CFGetTypeID accepts any non-null CF object.
        if unsafe { CFGetTypeID(value) } != unsafe { CFDictionaryGetTypeID() } {
            return None;
        }
        let mut rectangle = CGRect::default();
        // SAFETY: Type checking above establishes a dictionary and rectangle is
        // valid writable storage for the CoreGraphics representation helper.
        if unsafe { CGRectMakeWithDictionaryRepresentation(value, &mut rectangle) } == 0 {
            return None;
        }
        Some(LogicalRect {
            x: rectangle.origin.x,
            y: rectangle.origin.y,
            width: rectangle.size.width,
            height: rectangle.size.height,
        })
    }

    pub(super) fn visible_windows() -> Result<Vec<StudioWindow>, String> {
        // SAFETY: CGWindowListCopyWindowInfo returns a retained CFArray or null.
        let array = unsafe {
            CGWindowListCopyWindowInfo(
                K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS,
                0,
            )
        };
        if array.is_null() {
            return Err("CoreGraphics returned no visible-window list".into());
        }
        let _owned = OwnedCf(array);
        // SAFETY: array is a live CFArray for the duration of this function.
        let count = unsafe { CFArrayGetCount(array) };
        let mut windows = Vec::new();
        for index in 0..count {
            // SAFETY: index is within the array count above.
            let dictionary = unsafe { CFArrayGetValueAtIndex(array, index) };
            if dictionary.is_null()
                // SAFETY: CFGetTypeID accepts any non-null CF object.
                || unsafe { CFGetTypeID(dictionary) } != unsafe { CFDictionaryGetTypeID() }
            {
                continue;
            }
            // SAFETY: dictionary is a live CFDictionary and each key is a
            // process-lifetime CoreGraphics CFString constant.
            let owner = unsafe { cf_string(dictionary_value(dictionary, kCGWindowOwnerName)) }
                .unwrap_or_default();
            // Filter before parsing the rest. This is the privacy boundary that
            // prevents fallback from ever selecting another application's window.
            if !super::is_roblox_studio_owner(&owner) {
                continue;
            }
            // SAFETY: Same dictionary/key preconditions as above.
            let layer =
                unsafe { cf_i64(dictionary_value(dictionary, kCGWindowLayer)) }.unwrap_or_default();
            if layer != 0 {
                continue;
            }
            // SAFETY: Same dictionary/key preconditions as above.
            let Some(window_id) =
                (unsafe { cf_i64(dictionary_value(dictionary, kCGWindowNumber)) })
                    .and_then(|value| u32::try_from(value).ok())
            else {
                continue;
            };
            // SAFETY: Same dictionary/key preconditions as above.
            let owner_pid = unsafe { cf_i64(dictionary_value(dictionary, kCGWindowOwnerPID)) }
                .and_then(|value| i32::try_from(value).ok())
                .unwrap_or_default();
            // SAFETY: Same dictionary/key preconditions as above.
            let title = unsafe { cf_string(dictionary_value(dictionary, kCGWindowName)) }
                .unwrap_or_default();
            // SAFETY: Same dictionary/key preconditions as above.
            let Some(bounds) = (unsafe { cf_rect(dictionary_value(dictionary, kCGWindowBounds)) })
            else {
                continue;
            };
            windows.push(StudioWindow {
                window_id,
                owner_pid,
                owner,
                title,
                bounds,
                z_order: index as usize,
            });
        }
        Ok(windows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(
        id: u32,
        owner: &str,
        title: &str,
        width: f64,
        height: f64,
        z_order: usize,
    ) -> StudioWindow {
        StudioWindow {
            window_id: id,
            owner_pid: 42,
            owner: owner.into(),
            title: title.into(),
            bounds: LogicalRect {
                x: 100.0,
                y: 50.0,
                width,
                height,
            },
            z_order,
        }
    }

    fn window_for_pid(
        id: u32,
        pid: i32,
        title: &str,
        width: f64,
        height: f64,
        z_order: usize,
    ) -> StudioWindow {
        let mut window = window(id, "Roblox Studio", title, width, height, z_order);
        window.owner_pid = pid;
        window
    }

    #[test]
    fn selection_is_constrained_to_roblox_studio_and_ignores_tiny_helpers() {
        let selected = select_studio_window(
            vec![
                window(1, "Other App", "Race Stars", 4000.0, 3000.0, 0),
                window(2, "Roblox Studio", "Window", 66.0, 20.0, 1),
                window(
                    3,
                    "Roblox Studio",
                    "Race Stars - Roblox Studio",
                    1512.0,
                    949.0,
                    2,
                ),
            ],
            Some("Race Stars 2"),
        )
        .unwrap();
        assert_eq!(selected.window_id, 3);
    }

    #[test]
    fn selection_prefers_project_title_then_area() {
        let selected = select_studio_window(
            vec![
                window_for_pid(
                    1,
                    100,
                    "Different Project - Roblox Studio",
                    2000.0,
                    1400.0,
                    0,
                ),
                window_for_pid(2, 200, "Race Stars - Roblox Studio", 1200.0, 800.0, 1),
            ],
            Some("Race Stars 2"),
        )
        .unwrap();
        assert_eq!(selected.window_id, 2);
    }

    #[test]
    fn selection_uses_project_dialog_to_identify_process_but_captures_main_window() {
        let selected = select_studio_window(
            vec![
                window_for_pid(1, 100, "Race Stars Save Dialog", 600.0, 400.0, 0),
                window_for_pid(2, 100, "Roblox Studio", 1500.0, 900.0, 1),
                window_for_pid(
                    3,
                    200,
                    "Different Project - Roblox Studio",
                    1800.0,
                    1100.0,
                    2,
                ),
            ],
            Some("Race Stars 2"),
        )
        .unwrap();
        assert_eq!(selected.window_id, 2);
    }

    #[test]
    fn selection_refuses_when_no_studio_window_exists() {
        let error = select_studio_window(
            vec![window(1, "Other App", "Roblox Studio", 1000.0, 800.0, 0)],
            None,
        )
        .unwrap_err();
        assert!(error.contains("refuses to capture another application"));
    }

    #[test]
    fn logical_region_maps_to_retina_pixels() {
        let (crop, position) = pixel_crop_rect(
            1000,
            800,
            LogicalRect {
                x: 100.0,
                y: 50.0,
                width: 500.0,
                height: 400.0,
            },
            Some(CaptureRegion {
                x: 150,
                y: 100,
                width: 100,
                height: 50,
            }),
        )
        .unwrap();
        assert_eq!(
            crop,
            PixelRect {
                x: 100,
                y: 100,
                width: 200,
                height: 100
            }
        );
        assert_eq!(position, [150.0, 100.0]);
    }

    #[test]
    fn logical_region_must_be_inside_selected_studio_window() {
        let error = pixel_crop_rect(
            1000,
            800,
            LogicalRect {
                x: 100.0,
                y: 50.0,
                width: 500.0,
                height: 400.0,
            },
            Some(CaptureRegion {
                x: 90,
                y: 50,
                width: 100,
                height: 50,
            }),
        )
        .unwrap_err();
        assert!(error.contains("outside the selected Roblox Studio window"));
    }

    #[test]
    fn nearest_and_bilinear_resize_have_exact_requested_size() {
        let image = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        };
        for pixelated in [false, true] {
            let resized = resize_image(&image, 5, 3, pixelated);
            assert_eq!((resized.width, resized.height), (5, 3));
            assert_eq!(resized.pixels.len(), 5 * 3 * 4);
            let encoded = encode_png(&resized).unwrap();
            let decoded = decode_png(&encoded).unwrap();
            assert_eq!((decoded.width, decoded.height), (5, 3));
        }
    }
}
