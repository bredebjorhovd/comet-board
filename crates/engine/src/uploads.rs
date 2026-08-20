//! Uploads — attachment staging + the content-addressed edge mirror
//! (feature-inventory §3.7 "Uploads"; port of comet's `uploads.ts`).
//!
//! The UI streams a file as base64 chunks (~60KB, sized for the relay when the
//! target device is remote); chunks stage on disk under `{data_dir}/uploads/tmp/
//! {uploadId}/{seq}.b64` (surviving an engine restart mid-upload, unlike comet's
//! in-memory buffers), and `commit` assembles them into
//! `{data_dir}/uploads/{id8}-{name}` and returns the absolute path, which the
//! composer appends to the prompt so the agent can read the file from disk.
//!
//! On commit the assembled bytes are also mirrored to the edge, best-effort:
//! `PUT {edge}/attachments/{sha256}` (bearer auth, content-addressed R2 —
//! `edge/src/index.ts`). A device that doesn't hold the file locally can fall
//! back to `GET {edge}/attachments/{sha256}` with the same bearer; native keeps
//! reads local-first (`read_chunk` proxies through the owning device), so the
//! GET fallback is the disaster path, not the hot path.
//!
//! `read_chunk` serves attachments back in 45KB base64 chunks. Path jail: only
//! files under the uploads dir or a workspace-known chat cwd are readable (the
//! RPC layer supplies the cwd roots). Inside the uploads dir any type reads
//! back — those bytes were handed to this device as an attachment, and since
//! gh#535 a phone can hand over a PDF or a log as easily as a photo. Outside
//! it, images only: a chat cwd is somebody's checkout, and this RPC must not
//! become a way to read source (or `.env`) off another device.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::EngineError;
use crate::doc_host::EdgeConfig;
use crate::repos::hex;

/// A pending upload must finish within this window (covers slow mesh links).
const STAGING_TTL: Duration = Duration::from_secs(10 * 60);
/// Hard cap on an assembled file (matches the edge's 32MB attachment cap).
const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Multiple of 3 so independent base64 chunks concatenate losslessly.
const READ_CHUNK_BYTES: u64 = 45_000;

/// `ReadAttachmentChunk` reply.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentChunk {
    pub name: String,
    pub mime_type: String,
    /// Base64 of this chunk's byte range.
    pub data: String,
    pub next_offset: u64,
    pub done: bool,
}

struct UploadsInner {
    /// Durable home for committed attachments (`{data_dir}/uploads`).
    dir: PathBuf,
    /// Chunk staging (`{data_dir}/uploads/tmp/{uploadId}/`).
    tmp: PathBuf,
    edge: Option<EdgeConfig>,
    http: reqwest::Client,
}

#[derive(Clone)]
pub struct Uploads {
    inner: Arc<UploadsInner>,
}

impl Uploads {
    pub fn new(data_dir: &Path, edge: Option<EdgeConfig>) -> Self {
        let dir = data_dir.join("uploads");
        Self {
            inner: Arc::new(UploadsInner {
                tmp: dir.join("tmp"),
                dir,
                edge,
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
            }),
        }
    }

    /// The durable uploads dir (a path-jail root).
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Stage one base64 chunk. Positional (`seq`) writes are IDEMPOTENT: a client
    /// retrying a chunk whose ack was lost overwrites the same slot instead of
    /// double-appending. Callers without `seq` get append-only behavior.
    pub fn append(&self, upload_id: &str, data: &str, seq: Option<u64>) -> Result<(), EngineError> {
        let dir = self.staging_dir(upload_id)?;
        self.sweep();
        std::fs::create_dir_all(&dir)?;
        let at = match seq {
            Some(seq) => seq,
            None => next_free_seq(&dir)?,
        };
        if at > 1_000_000 {
            return Err(EngineError::Other("Invalid chunk index".into()));
        }
        // Base64 inflates by ~4/3; bound the staged payload against the file cap.
        let staged: u64 = chunk_files(&dir)?
            .iter()
            .filter(|(seq, _)| *seq != at)
            .map(|(_, path)| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
            .sum();
        if (staged + data.len() as u64) * 3 / 4 > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::write(dir.join(format!("{at:06}.b64")), data)?;
        Ok(())
    }

    /// Assemble the staged chunks into a durable file and return its absolute
    /// path. Also mirrors the bytes to the edge (content-addressed), best-effort.
    pub fn commit(&self, upload_id: &str, file_name: &str) -> Result<String, EngineError> {
        let dir = self.staging_dir(upload_id)?;
        let mut parts = chunk_files(&dir)?;
        if parts.is_empty() {
            return Err(EngineError::Other("Unknown or expired upload".into()));
        }
        parts.sort_by_key(|(seq, _)| *seq);
        // Positional appends may leave holes if a chunk never arrived — joining
        // around them would silently corrupt the file.
        let mut joined = String::new();
        for (i, (seq, path)) in parts.iter().enumerate() {
            if *seq != i as u64 {
                return Err(EngineError::Other("Upload is missing a chunk".into()));
            }
            joined.push_str(std::fs::read_to_string(path)?.trim());
        }
        let bytes = BASE64
            .decode(joined.as_bytes())
            .map_err(|e| EngineError::Other(format!("upload is not valid base64: {e}")))?;
        if bytes.len() as u64 > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::create_dir_all(&self.inner.dir)?;
        let name = sanitize(file_name);
        let id8: String = upload_id.chars().take(8).collect();
        let path = self.inner.dir.join(format!("{id8}-{name}"));
        std::fs::write(&path, &bytes)?;
        let _ = std::fs::remove_dir_all(&dir);
        self.mirror_to_edge(&path, bytes);
        Ok(path.to_string_lossy().to_string())
    }

    /// Read one 45KB chunk of an attachment. `extra_roots` are the workspace's
    /// known chat cwds — together with the uploads dir they form the path jail.
    pub fn read_chunk(
        &self,
        path: &str,
        offset: u64,
        extra_roots: &[PathBuf],
    ) -> Result<AttachmentChunk, EngineError> {
        use std::io::{Read, Seek};
        let file = self.inspect(path, extra_roots)?;
        let size = file.size;
        let start = offset.min(size);
        let next_offset = (start + READ_CHUNK_BYTES).min(size);
        // Read ONLY this chunk's byte range — never the whole file per chunk.
        let mut buf = vec![0u8; (next_offset - start) as usize];
        let mut handle = std::fs::File::open(&file.resolved)?;
        handle.seek(std::io::SeekFrom::Start(start))?;
        let mut read = 0usize;
        while read < buf.len() {
            let n = handle.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(AttachmentChunk {
            name: file.name,
            mime_type: file.mime_type,
            data: BASE64.encode(&buf),
            next_offset,
            done: next_offset >= size,
        })
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn staging_dir(&self, upload_id: &str) -> Result<PathBuf, EngineError> {
        // The id becomes a directory name — jail it to a safe charset.
        let ok = !upload_id.is_empty()
            && upload_id.len() <= 64
            && upload_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        if !ok {
            return Err(EngineError::Other("Invalid upload id".into()));
        }
        Ok(self.inner.tmp.join(upload_id))
    }

    /// Reclaim staging dirs whose newest chunk is older than the TTL (an upload
    /// abandoned mid-stream must not hold up to 32MB forever).
    fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.inner.tmp) else {
            return;
        };
        for entry in entries.flatten() {
            let newest = std::fs::read_dir(entry.path())
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|f| f.metadata().ok()?.modified().ok())
                .max();
            let expired = match newest {
                Some(at) => at.elapsed().map(|age| age > STAGING_TTL).unwrap_or(false),
                None => true, // empty dir — reclaim
            };
            if expired {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn inspect(&self, path: &str, extra_roots: &[PathBuf]) -> Result<InspectedFile, EngineError> {
        let outside = || EngineError::Other("Attachment is outside the upload cache".into());
        // Canonicalize BOTH sides so `..` segments and symlinks can't escape.
        let resolved = std::fs::canonicalize(path).map_err(|_| outside())?;
        let allowed = std::iter::once(&self.inner.dir)
            .chain(extra_roots.iter())
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| resolved.starts_with(&root) && resolved != root);
        if !allowed {
            return Err(outside());
        }
        let meta = std::fs::metadata(&resolved)?;
        if !meta.is_file() {
            return Err(EngineError::Other("Attachment is not a file".into()));
        }
        if meta.len() > MAX_BYTES {
            return Err(EngineError::Other("Attachment is too large".into()));
        }
        // Two jails, not one. Anything COMMITTED here — a PDF, a log, a design
        // file the phone sent (gh#535) — is a file this device was handed for
        // this purpose, so any type reads back. A file merely sitting inside a
        // chat's checkout is not: it is somebody's source tree, and the widened
        // type table must not turn a thumbnail RPC into `cat ~/repo/.env`.
        // Images only, out there, exactly as before.
        let in_uploads = std::fs::canonicalize(&self.inner.dir)
            .map(|dir| resolved.starts_with(&dir))
            .unwrap_or(false);
        let mime_type = if in_uploads {
            mime_by_ext(&resolved).unwrap_or("application/octet-stream")
        } else {
            image_mime_by_ext(&resolved)
                .ok_or_else(|| EngineError::Other("Attachment is not a supported image".into()))?
        };
        Ok(InspectedFile {
            name: resolved
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "attachment".into()),
            mime_type: mime_type.to_string(),
            size: meta.len(),
            resolved,
        })
    }

    /// Best-effort content-addressed mirror (`PUT /attachments/{sha256}`, bearer
    /// auth). Failures only log — local commit already succeeded.
    fn mirror_to_edge(&self, path: &Path, bytes: Vec<u8>) {
        let Some(edge) = self.inner.edge.clone() else {
            return;
        };
        let sha = hex(&Sha256::digest(&bytes));
        let mime = mime_by_ext(path)
            .unwrap_or("application/octet-stream")
            .to_string();
        let url = format!("{}/attachments/{sha}", edge.url.trim_end_matches('/'));
        let http = self.inner.http.clone();
        tokio::spawn(async move {
            // Fresh bearer per request — never the boot-time snapshot.
            let Some(bearer) = edge.bearer().await else {
                tracing::warn!(sha = %sha, "attachment mirror skipped: signed out");
                return;
            };
            let sent = http
                .put(&url)
                .bearer_auth(&bearer)
                .header("content-type", mime)
                .body(bytes)
                .send()
                .await;
            match sent {
                Ok(res) if res.status().is_success() => {
                    tracing::debug!(sha = %sha, "attachment mirrored to edge");
                }
                Ok(res) => {
                    tracing::warn!(sha = %sha, status = %res.status(), "edge attachment mirror rejected");
                }
                Err(err) => {
                    tracing::warn!(sha = %sha, error = %err, "edge attachment mirror failed");
                }
            }
        });
    }
}

struct InspectedFile {
    resolved: PathBuf,
    name: String,
    mime_type: String,
    size: u64,
}

fn chunk_files(dir: &Path) -> Result<Vec<(u64, PathBuf)>, EngineError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let seq = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(seq) = seq
            && path.extension().and_then(|e| e.to_str()) == Some("b64")
        {
            files.push((seq, path));
        }
    }
    Ok(files)
}

fn next_free_seq(dir: &Path) -> Result<u64, EngineError> {
    Ok(chunk_files(dir)?
        .iter()
        .map(|(seq, _)| seq + 1)
        .max()
        .unwrap_or(0))
}

fn sanitize(file_name: &str) -> String {
    let base = Path::new(file_name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let tail: String = cleaned
        .chars()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        "upload".into()
    } else {
        tail
    }
}

/// The image types every viewport can decode — the set
/// `comet_proto::view::attachments::is_image_path` calls thumbnailable, and the
/// only set readable from outside the uploads dir.
pub fn image_mime_by_ext(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        "avif" => Some("image/avif"),
        "heic" => Some("image/heic"),
        _ => None,
    }
}

/// Content type for an attachment of any kind: images first, then the document
/// types a phone can hand over (gh#535), then `None` — callers inside the
/// uploads dir fall back to `application/octet-stream`, since the bytes are
/// already ours and the extension is only a hint to whoever opens them.
fn mime_by_ext(path: &Path) -> Option<&'static str> {
    if let Some(image) = image_mime_by_ext(path) {
        return Some(image);
    }
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "pdf" => Some("application/pdf"),
        "txt" | "log" | "text" => Some("text/plain"),
        "md" | "markdown" => Some("text/markdown"),
        "csv" => Some("text/csv"),
        "html" | "htm" => Some("text/html"),
        "json" => Some("application/json"),
        "xml" => Some("application/xml"),
        "yml" | "yaml" => Some("application/yaml"),
        "toml" => Some("application/toml"),
        "zip" => Some("application/zip"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("my photo (1).png"), "my_photo__1_.png");
        assert_eq!(sanitize(""), "upload");
    }

    fn uploads_in(dir: &Path) -> Uploads {
        Uploads::new(dir, None)
    }

    /// The phone's half of gh#535: a PDF uploaded from iOS commits, and reads
    /// back with a content type that says what it is.
    #[test]
    fn a_committed_document_commits_and_reads_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let uploads = uploads_in(tmp.path());
        let bytes = b"%PDF-1.7\nnot really a pdf, but the bytes are ours\n";
        uploads
            .append("upload1", &BASE64.encode(bytes), Some(0))
            .expect("append");
        let path = uploads.commit("upload1", "spec.pdf").expect("commit");
        let chunk = uploads.read_chunk(&path, 0, &[]).expect("read");
        assert_eq!(chunk.mime_type, "application/pdf");
        assert!(chunk.done);
        assert_eq!(BASE64.decode(chunk.data).expect("base64"), bytes);
    }

    /// An extension nobody has a type for still reads back — the bytes are in
    /// the uploads dir, which is the whole authorization.
    #[test]
    fn an_unknown_extension_inside_the_uploads_dir_is_octet_stream() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let uploads = uploads_in(tmp.path());
        uploads
            .append("upload2", &BASE64.encode(b"blob"), Some(0))
            .expect("append");
        let path = uploads.commit("upload2", "design.dc.html").expect("commit");
        let chunk = uploads.read_chunk(&path, 0, &[]).expect("read");
        assert_eq!(chunk.mime_type, "text/html");
        let path = {
            uploads
                .append("upload3", &BASE64.encode(b"blob"), Some(0))
                .expect("append");
            uploads.commit("upload3", "thing.qqq").expect("commit")
        };
        assert_eq!(
            uploads.read_chunk(&path, 0, &[]).expect("read").mime_type,
            "application/octet-stream"
        );
    }

    /// The line the widened type table must not cross: a chat's checkout is
    /// readable for images (transcript thumbnails) and nothing else.
    #[test]
    fn a_checkout_root_still_serves_images_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let uploads = uploads_in(&tmp.path().join("data"));
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).expect("checkout dir");
        let secret = checkout.join(".env");
        std::fs::write(&secret, b"TOKEN=hunter2").expect("write");
        let shot = checkout.join("shot.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\n").expect("write");
        let roots = vec![checkout.clone()];

        let refused = uploads
            .read_chunk(&secret.to_string_lossy(), 0, &roots)
            .expect_err("a checkout secret is not an attachment");
        assert!(refused.to_string().contains("not a supported image"));
        // A .pdf sitting in the checkout is refused for the same reason.
        let doc = checkout.join("notes.pdf");
        std::fs::write(&doc, b"%PDF").expect("write");
        assert!(
            uploads
                .read_chunk(&doc.to_string_lossy(), 0, &roots)
                .is_err()
        );
        assert_eq!(
            uploads
                .read_chunk(&shot.to_string_lossy(), 0, &roots)
                .expect("images still read")
                .mime_type,
            "image/png"
        );
    }
}
