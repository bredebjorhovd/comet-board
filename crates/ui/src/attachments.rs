//! Attachments (feature-inventory §1.7/§1.8): the composer's staged images,
//! the chunked upload to the chat's host device, the plain-text attachment-ref
//! transport that rides the prompt, the transcript read-back cache, and the
//! full-size preview lightbox.
//!
//! Ports of comet's `composer/use-attachments.ts` (staging/upload),
//! `control/message-attachments.ts` (the `withAttachments` /
//! `parseUserMessageImages` text transport — attachment refs are embedded in
//! the user message's plain text, which is exactly what persists in the doc),
//! and `lib/transcript-attachment-cache.ts` (decoded-image cache keyed by
//! `(deviceId, path)`, seeded locally after a send so own bubbles never
//! round-trip).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use gpui::{
    AnyElement, BackgroundExecutor, Image, ImageFormat, ObjectFit, SharedString, Size,
    StyledImage as _, div, img, prelude::*, px,
};

use crate::state::EngineHandle;
use crate::theme::{Theme, white_alpha};
use comet_rpc::methods;

/// use-attachments.ts `MAX_ATTACHMENT_BYTES`.
pub const MAX_ATTACHMENT_BYTES: u64 = 24 * 1024 * 1024;
/// Base64 chars per `UploadChunk` (comet state.ts `UPLOAD_CHUNK` — sized for
/// the relay when the target device is remote).
pub const UPLOAD_CHUNK_B64_CHARS: usize = 60_000;
/// state.ts `MAX_ATTACHMENT_READ_CHUNKS` — bounds the read-back loop.
const MAX_READ_CHUNKS: usize = 1_000;

// ---------------------------------------------------------------------------
// Text transport (message-attachments.ts)
// ---------------------------------------------------------------------------

/// The body used for attachment-only sends (`use-attachments.ts`).
pub const ATTACHMENT_ONLY_TEXT: &str = "See the attached file(s).";

/// How attachments ride the prompt (use-attachments.ts `withAttachments`):
/// plain local paths appended to the text — the files are staged on the device
/// that runs the agent, so the agent can open them with its own tools; the
/// same text is what persists as the user doc entry.
pub fn with_attachments(text: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return text.to_string();
    }
    let refs: Vec<String> = paths.iter().map(|p| format!("- {p}")).collect();
    let body = if text.is_empty() {
        ATTACHMENT_ONLY_TEXT
    } else {
        text
    };
    format!(
        "{body}\n\nAttached files (local files — open them to view):\n{}",
        refs.join("\n")
    )
}

/// An attachment ref parsed back out of a user message's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserImageAttachment {
    pub id: String,
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUserMessage {
    /// The visible prompt (the refs trailer stripped; empty for image-only sends).
    pub text: String,
    pub attachments: Vec<UserImageAttachment>,
}

fn name_from_path(path: &str) -> String {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if name.is_empty() {
        "file".to_string()
    } else {
        name.to_string()
    }
}

/// Find the refs trailer: a blank line, then a line starting (case-insensitive)
/// with `Attached images (local files` or `Attached files (local files` and
/// ending `):`. Returns `(body_end, refs_start)` byte offsets — the tolerant
/// equivalent of comet's `ATTACHED_IMAGES_RE`.
fn find_refs_marker(content: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    for needle in [
        "\n\nattached files (local files",
        "\n\nattached images (local files",
    ] {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(needle) {
            let gap = from + rel;
            let line_start = gap + 2;
            let line_end = content[line_start..]
                .find('\n')
                .map(|p| line_start + p)
                .unwrap_or(content.len());
            let line = content[line_start..line_end].trim_end_matches('\r');
            if line.ends_with("):") {
                let refs_start = (line_end + 1).min(content.len());
                return Some((gap, refs_start));
            }
            from = line_start;
        }
    }
    None
}

/// message-attachments.ts `parseUserMessageImages`: split the visible prompt
/// from its attachment-ref trailer.
pub fn parse_user_message_images(content: &str) -> ParsedUserMessage {
    let Some((body_end, refs_start)) = find_refs_marker(content) else {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments: Vec::new(),
        };
    };
    let body = content[..body_end].trim_end();
    let attachments: Vec<UserImageAttachment> = content[refs_start..]
        .lines()
        .filter_map(|line| {
            let path = line.trim_start().strip_prefix("- ")?.trim();
            (!path.is_empty()).then(|| path.to_string())
        })
        .enumerate()
        .map(|(index, path)| UserImageAttachment {
            id: format!("{index}:{path}"),
            name: name_from_path(&path),
            path,
        })
        .collect();
    if attachments.is_empty() {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments,
        };
    }
    ParsedUserMessage {
        text: if body.trim() == ATTACHMENT_ONLY_TEXT {
            String::new()
        } else {
            body.to_string()
        },
        attachments,
    }
}

/// message-attachments.ts `userMessageRailText`: what the rail/sidebar shows
/// for a user message ("Attached file" / "N attached files" when file-only).
pub fn user_message_rail_text(content: &str) -> String {
    let parsed = parse_user_message_images(content);
    if !parsed.text.trim().is_empty() {
        return parsed.text;
    }
    match parsed.attachments.len() {
        0 => content.to_string(),
        1 => "Attached file".to_string(),
        n => format!("{n} attached files"),
    }
}

// ---------------------------------------------------------------------------
// Staging (use-attachments.ts intake)
// ---------------------------------------------------------------------------

/// An file staged in the composer, before upload. Raw bytes and MIME type are
/// always set; for recognised image formats the decoded [`Image`] is also
/// available (feeds thumbnails, the lightbox, and the post-send cache seed).
#[derive(Clone)]
pub struct StagedAttachment {
    pub id: String,
    /// File name with a type-matching extension (use-attachments.ts
    /// `ensureExtension` — agents sniff files by extension).
    pub name: String,
    pub image: Option<Arc<Image>>,
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

impl StagedAttachment {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// MIME type for a file extension (mirrors the engine's `mime_by_ext`). Returns
/// `application/octet-stream` for unrecognised extensions.
pub fn mime_by_ext(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        Some("tif") | Some("tiff") => "image/tiff",
        Some("avif") => "image/avif",
        Some("heic") => "image/heic",
        Some("txt") | Some("text") => "text/plain",
        Some("md") | Some("markdown") => "text/markdown",
        Some("csv") => "text/csv",
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("js") | Some("mjs") => "text/javascript",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("toml") => "application/toml",
        Some("rs") => "text/rust",
        Some("py") => "text/x-python",
        Some("rb") => "text/x-ruby",
        Some("c") => "text/x-c",
        Some("h") | Some("hpp") | Some("hxx") => "text/x-c-header",
        Some("cpp") | Some("cxx") | Some("cc") => "text/x-c++src",
        Some("ts") | Some("tsx") => "text/typescript",
        Some("jsx") => "text/jsx",
        Some("sh") | Some("bash") => "text/x-shellscript",
        Some("zsh") => "text/x-shellscript",
        Some("sql") => "text/x-sql",
        Some("diff") | Some("patch") => "text/x-diff",
        Some("log") => "text/x-log",
        Some("pdf") => "application/pdf",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("gz") | Some("tgz") => "application/gzip",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("flac") => "audio/flac",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("woff") | Some("woff2") => "font/woff",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Check whether a file extension maps to an image format.
pub fn is_image_extension(path: &Path) -> bool {
    matches!(mime_by_ext(path), "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/svg+xml" | "image/bmp" | "image/tiff" | "image/avif" | "image/heic")
}

/// MIME type string for a gpui `ImageFormat`.
pub fn mime_type_for_format(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::Webp => "image/webp",
        ImageFormat::Svg => "image/svg+xml",
        ImageFormat::Bmp => "image/bmp",
        ImageFormat::Tiff => "image/tiff",
        ImageFormat::Ico => "image/x-icon",
        ImageFormat::Pnm => "image/x-portable-anymap",
    }
}

/// Image formats the whole pipeline supports: intersection of gpui's decoders
/// and the engine's read-back.
pub fn format_by_mime(mime: &str) -> Option<ImageFormat> {
    match mime {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::Webp),
        "image/svg+xml" => Some(ImageFormat::Svg),
        "image/bmp" => Some(ImageFormat::Bmp),
        "image/tiff" => Some(ImageFormat::Tiff),
        _ => None,
    }
}

/// use-attachments.ts `ensureExtension`: pasted screenshots often arrive as a
/// bare "image" — make sure the staged name carries a type-matching extension.
pub fn ensure_extension(name: &str, ext: &str) -> String {
    let has_ext = name
        .rsplit_once('.')
        .map(|(stem, ext_part)| {
            !stem.is_empty()
                && (2..=5).contains(&ext_part.len())
                && ext_part.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or(false);
    if has_ext {
        name.to_string()
    } else {
        format!("{name}.{ext}")
    }
}

/// Stage a file from disk (picker / drop / pasted path). `Err` carries the
/// user-facing message (mirrors the old `onError` copy).
pub fn stage_file(path: &Path) -> Result<StagedAttachment, String> {
    let display_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let mime_type = mime_by_ext(path).to_string();
    let is_image = is_image_extension(path);
    let meta = std::fs::metadata(path).map_err(|_| format!("{display_name} could not be read."))?;
    if meta.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!("{display_name} is too large (24 MB max)."));
    }
    let bytes = std::fs::read(path).map_err(|_| format!("{display_name} could not be read."))?;
    let default_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let image = if is_image {
        match format_by_mime(&mime_type) {
            Some(fmt) => {
                let img = Image::from_bytes(fmt, bytes.clone());
                Some(Arc::new(img))
            }
            None => None,
        }
    } else {
        None
    };
    Ok(StagedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        name: ensure_extension(&display_name, default_ext),
        image,
        bytes,
        mime_type,
    })
}

/// Stage an image pasted from the clipboard.
pub fn stage_clipboard_image(image: Image) -> StagedAttachment {
    let format = image.format;
    let bytes = image.bytes.clone();
    let mime_type = mime_type_for_format(format).to_string();
    StagedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        name: ensure_extension("image", format.extension()),
        image: Some(Arc::new(image)),
        bytes,
        mime_type,
    }
}

// ---------------------------------------------------------------------------
// Upload (state.ts uploadAttachment) + read-back (state.ts readAttachmentImage)
// ---------------------------------------------------------------------------

fn with_target(mut params: serde_json::Value, target_device_id: Option<&str>) -> serde_json::Value {
    if let (Some(target), Some(map)) = (target_device_id, params.as_object_mut()) {
        map.insert("targetDeviceId".into(), target.into());
    }
    params
}

/// Per-call deadlines (desktop state.ts): a stalled-but-open relay link never
/// fails an RPC on its own, so every attachment call races a timer. The first
/// chunk gets 90s (a cold dial to a remote device), later chunks 30s; commit
/// 150s (it must outlast the engine's cross-device assemble); reads 20s.
const FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(90);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(150);
const READ_CHUNK_TIMEOUT: Duration = Duration::from_secs(20);

/// Race an RPC against `timeout` on the gpui background executor (these
/// futures run under `cx.spawn`, so tokio's timer reactor isn't available).
async fn call_with_timeout(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let call = engine.client().call(method, params);
    let timer = executor.timer(timeout);
    futures::pin_mut!(call);
    match futures::future::select(call, timer).await {
        futures::future::Either::Left((result, _)) => result.map_err(|e| e.to_string()),
        futures::future::Either::Right(_) => Err(format!("{method} timed out")),
    }
}

/// Chunked upload: base64 the bytes, `UploadChunk{uploadId,seq,data}` per 60KB
/// slice (positional `seq` makes the cheap retry idempotent), then
/// `UploadCommit{uploadId,fileName}` → the durable absolute path on the target
/// device. Errors return the raw cause (the composer shows friendly copy).
pub async fn upload_attachment(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    target_device_id: Option<&str>,
    attachment: &StagedAttachment,
) -> Result<String, String> {
    let b64 = BASE64.encode(attachment.bytes());
    let upload_id = uuid::Uuid::new_v4().to_string();
    let mut start = 0usize;
    let mut seq = 0u64;
    loop {
        let end = (start + UPLOAD_CHUNK_B64_CHARS).min(b64.len());
        let params = with_target(
            serde_json::json!({ "uploadId": upload_id, "seq": seq, "data": &b64[start..end] }),
            target_device_id,
        );
        let timeout = if seq == 0 {
            FIRST_CHUNK_TIMEOUT
        } else {
            CHUNK_TIMEOUT
        };
        // One transient blip must not abort a ~400-chunk upload; `seq` slots
        // are idempotent engine-side, so a blind re-send is safe (timeouts
        // retry too, like the original's per-chunk `withTimeout` + retry ×2).
        let mut attempt = 0u32;
        loop {
            match call_with_timeout(
                engine,
                executor,
                methods::UPLOAD_CHUNK,
                params.clone(),
                timeout,
            )
            .await
            {
                Ok(_) => break,
                Err(err) if attempt < 2 => {
                    attempt += 1;
                    tracing::debug!(error = %err, seq, "upload chunk retry");
                }
                Err(err) => return Err(err),
            }
        }
        start = end;
        seq += 1;
        if start >= b64.len() {
            break;
        }
    }
    let params = with_target(
        serde_json::json!({ "uploadId": upload_id, "fileName": attachment.name }),
        target_device_id,
    );
    let reply = call_with_timeout(
        engine,
        executor,
        methods::UPLOAD_COMMIT,
        params,
        COMMIT_TIMEOUT,
    )
    .await?;
    reply
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "upload commit returned no path".to_string())
}

/// A transcript attachment read back from the owning device.
pub struct LoadedAttachment {
    pub name: String,
    pub mime_type: String,
    pub image: Option<Arc<Image>>,
}

/// `ReadAttachmentChunk` loop: 45KB base64 chunks until `done` (bounded, with
/// the same stuck-offset guard as comet's `readAttachmentImage`).
pub async fn read_attachment(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    target_device_id: Option<&str>,
    path: &str,
) -> Option<LoadedAttachment> {
    let mut name = String::new();
    let mut mime = String::new();
    let mut b64 = String::new();
    let mut offset = 0u64;
    let mut done = false;
    for _ in 0..MAX_READ_CHUNKS {
        let params = with_target(
            serde_json::json!({ "path": path, "offset": offset }),
            target_device_id,
        );
        let chunk = call_with_timeout(
            engine,
            executor,
            methods::READ_ATTACHMENT_CHUNK,
            params,
            READ_CHUNK_TIMEOUT,
        )
        .await
        .ok()?;
        name = chunk.get("name")?.as_str()?.to_string();
        mime = chunk.get("mimeType")?.as_str()?.to_string();
        b64.push_str(chunk.get("data")?.as_str()?);
        done = chunk.get("done")?.as_bool()?;
        if done {
            break;
        }
        let next = chunk.get("nextOffset")?.as_u64()?;
        if next <= offset {
            return None;
        }
        offset = next;
    }
    if !done || b64.is_empty() {
        return None;
    }
    let bytes = BASE64.decode(b64.as_bytes()).ok()?;
    let image = format_by_mime(&mime)
        .map(|fmt| Arc::new(Image::from_bytes(fmt, bytes)));
    Some(LoadedAttachment {
        name: if name.is_empty() {
            name_from_path(path)
        } else {
            name
        },
        mime_type: mime,
        image,
    })
}

// ---------------------------------------------------------------------------
// Transcript image cache (transcript-attachment-cache.ts)
// ---------------------------------------------------------------------------

/// A decoded transcript attachment, ready for rendering.
#[derive(Clone)]
pub struct CachedAttachment {
    pub name: SharedString,
    pub mime_type: SharedString,
    pub image: Option<Arc<Image>>,
}

/// What a render pass sees for one `(deviceId, path)` source.
#[derive(Clone)]
pub enum AttachmentSnapshot {
    Loading,
    Loaded(CachedAttachment),
    /// Load failed; `retry_in` is how long until [`begin_load`] would hand out
    /// another attempt (the exponential 2s→15s ladder from user-attachments.tsx).
    Error {
        retry_in: Duration,
    },
}

enum CacheEntry {
    Loading { attempts: u32 },
    Loaded(CachedAttachment),
    Error { attempts: u32, at: Instant },
}

fn retry_delay(attempts: u32) -> Duration {
    Duration::from_millis((2_000u64 << attempts.min(3)).min(15_000))
}

fn cache() -> &'static Mutex<HashMap<(String, String), CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(device_id: &str, path: &str) -> (String, String) {
    (device_id.to_string(), path.to_string())
}

pub fn attachment_snapshot(device_id: &str, path: &str) -> AttachmentSnapshot {
    match cache().lock().unwrap().get(&key(device_id, path)) {
        Some(CacheEntry::Loaded(image)) => AttachmentSnapshot::Loaded(image.clone()),
        Some(CacheEntry::Error { attempts, at }) => AttachmentSnapshot::Error {
            retry_in: retry_delay(attempts.saturating_sub(1)).saturating_sub(at.elapsed()),
        },
        _ => AttachmentSnapshot::Loading,
    }
}

/// Claim the load for a source: `true` ⇒ the caller should start fetching now
/// (the entry is marked Loading so concurrent renders don't double-fetch).
/// Errored sources hand out a retry only after their backoff has elapsed.
pub fn begin_load(device_id: &str, path: &str) -> bool {
    let mut cache = cache().lock().unwrap();
    let entry = cache.entry(key(device_id, path));
    match entry {
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(CacheEntry::Loading { attempts: 0 });
            true
        }
        std::collections::hash_map::Entry::Occupied(mut o) => match o.get() {
            CacheEntry::Error { attempts, at }
                if at.elapsed() >= retry_delay(attempts.saturating_sub(1)) =>
            {
                let attempts = *attempts;
                o.insert(CacheEntry::Loading { attempts });
                true
            }
            _ => false,
        },
    }
}

pub fn store_loaded(
    device_id: &str,
    path: &str,
    name: SharedString,
    mime_type: SharedString,
    image: Option<Arc<Image>>,
) {
    cache().lock().unwrap().insert(
        key(device_id, path),
        CacheEntry::Loaded(CachedAttachment {
            name,
            mime_type,
            image,
        }),
    );
}

pub fn store_error(device_id: &str, path: &str) {
    let mut cache = cache().lock().unwrap();
    let attempts = match cache.get(&key(device_id, path)) {
        Some(CacheEntry::Loading { attempts }) => attempts + 1,
        Some(CacheEntry::Error { attempts, .. }) => *attempts,
        _ => 1,
    };
    cache.insert(
        key(device_id, path),
        CacheEntry::Error {
            attempts,
            at: Instant::now(),
        },
    );
}

/// Seed the cache after a successful upload (composer send path) so the just-
/// sent bubble's thumbnails render from local bytes instead of a round-trip.
pub fn seed_attachment(
    device_id: &str,
    path: &str,
    name: &str,
    mime_type: &str,
    image: Option<Arc<Image>>,
) {
    store_loaded(
        device_id,
        path,
        name.to_string().into(),
        mime_type.to_string().into(),
        image,
    );
}

// ---------------------------------------------------------------------------
// Preview lightbox (attachment-ui.tsx AttachmentPreviewDialog)
// ---------------------------------------------------------------------------

/// A full-size preview target (staged strip or transcript thumbnail).
#[derive(Clone)]
pub struct PreviewImage {
    pub name: SharedString,
    pub image: Arc<Image>,
}

/// The bare lightbox: dim scrim, the image at ≤85vh/90vw, the file name under
/// it. Any click closes (the whole dialog is the close button, as in the
/// original's `cursor-zoom-out` figure). Only valid for image attachments.
pub fn lightbox(
    viewport: Size<gpui::Pixels>,
    preview: &PreviewImage,
    on_close: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let max_h = px(f32::from(viewport.height) * 0.85);
    let max_w = px(f32::from(viewport.width) * 0.9);
    gpui::deferred(
        gpui::anchored()
            .position(gpui::point(px(0.0), px(0.0)))
            .child(
                div()
                    .id("attachment-lightbox")
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    .bg(gpui::hsla(0.0, 0.0, 0.0, 0.7))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.0))
                    .cursor_pointer()
                    .on_click(move |_, window, cx| on_close(window, cx))
                    .child(
                        img(preview.image.clone())
                            .object_fit(ObjectFit::Contain)
                            .max_h(max_h)
                            .max_w(max_w)
                            .rounded(px(Theme::RADIUS_CARD))
                            .shadow_2xl(),
                    )
                    .child(
                        div()
                            .max_w(max_w)
                            .overflow_hidden()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(white_alpha(0.45))
                            .child(preview.name.clone()),
                    ),
            ),
    )
    .priority(3)
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_attachments_round_trips_through_parse() {
        let paths = vec!["/data/uploads/ab-cat.png".to_string(), "/x/dog.jpg".into()];
        let content = with_attachments("look at these", &paths);
        let parsed = parse_user_message_images(&content);
        assert_eq!(parsed.text, "look at these");
        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].path, "/data/uploads/ab-cat.png");
        assert_eq!(parsed.attachments[0].name, "ab-cat.png");
        assert_eq!(parsed.attachments[1].name, "dog.jpg");
        assert_eq!(parsed.attachments[0].id, "0:/data/uploads/ab-cat.png");
    }

    #[test]
    fn image_only_send_hides_placeholder_body() {
        let content = with_attachments("", &["/a/b.png".to_string()]);
        assert!(content.starts_with(ATTACHMENT_ONLY_TEXT));
        let parsed = parse_user_message_images(&content);
        assert_eq!(parsed.text, "");
        assert_eq!(parsed.attachments.len(), 1);
    }

    #[test]
    fn plain_text_passes_through_unchanged() {
        assert_eq!(with_attachments("hello", &[]), "hello");
        let parsed = parse_user_message_images("hello\n\nno images here");
        assert!(parsed.attachments.is_empty());
        assert_eq!(parsed.text, "hello\n\nno images here");
    }

    #[test]
    fn marker_is_case_insensitive_and_requires_ref_lines() {
        // New-style "files" marker
        let parsed = parse_user_message_images(
            "hi\n\nATTACHED FILES (local files — open them to view):\n- /p/q.png",
        );
        assert_eq!(parsed.attachments.len(), 1);
        // Old-style "images" marker (legacy compatibility)
        let old = parse_user_message_images(
            "hi\n\nAttached images (local files — open them to view):\n- /p/q.png",
        );
        assert_eq!(old.attachments.len(), 1);
        // A trailer with no valid `- path` lines is left as plain text.
        let empty = parse_user_message_images(
            "hi\n\nAttached files (local files — open them to view):\nnothing",
        );
        assert!(empty.attachments.is_empty());
        assert!(empty.text.contains("Attached files"));
    }

    #[test]
    fn rail_text_summarizes_attachment_only_sends() {
        let one = with_attachments("", &["/a/b.png".to_string()]);
        assert_eq!(user_message_rail_text(&one), "Attached file");
        let two = with_attachments("", &["/a/b.png".to_string(), "/c/d.png".into()]);
        assert_eq!(user_message_rail_text(&two), "2 attached files");
        let with_text = with_attachments("fix this", &["/a/b.png".to_string()]);
        assert_eq!(user_message_rail_text(&with_text), "fix this");
        assert_eq!(user_message_rail_text("plain"), "plain");
    }

    #[test]
    fn ensure_extension_matches_browser_heuristic() {
        assert_eq!(ensure_extension("shot.png", "png"), "shot.png");
        assert_eq!(ensure_extension("image", "png"), "image.png");
        assert_eq!(ensure_extension("photo.j", "jpg"), "photo.j.jpg");
        assert_eq!(ensure_extension("archive.tar.gz", "png"), "archive.tar.gz");
    }

    #[test]
    fn mime_by_extension_covers_all() {
        assert_eq!(mime_by_ext(Path::new("f.png")), "image/png");
        assert_eq!(mime_by_ext(Path::new("f.JPG")), "image/jpeg");
        assert_eq!(mime_by_ext(Path::new("f.webp")), "image/webp");
        assert_eq!(mime_by_ext(Path::new("f.txt")), "text/plain");
        assert_eq!(mime_by_ext(Path::new("f.pdf")), "application/pdf");
        assert_eq!(mime_by_ext(Path::new("f.unknown")), "application/octet-stream");
        assert!(is_image_extension(Path::new("f.png")));
        assert!(is_image_extension(Path::new("f.JPG")));
        assert!(!is_image_extension(Path::new("f.txt")));
        assert!(!is_image_extension(Path::new("f.pdf")));
        assert!(!is_image_extension(Path::new("f.bin")));
    }

    #[test]
    fn retry_ladder_is_2s_doubling_capped_at_15s() {
        assert_eq!(retry_delay(0), Duration::from_millis(2_000));
        assert_eq!(retry_delay(1), Duration::from_millis(4_000));
        assert_eq!(retry_delay(2), Duration::from_millis(8_000));
        assert_eq!(retry_delay(3), Duration::from_millis(15_000));
        assert_eq!(retry_delay(9), Duration::from_millis(15_000));
    }
}
