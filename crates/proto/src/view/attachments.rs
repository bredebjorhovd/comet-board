//! The attachment-ref text transport — how a file staged on the host device
//! rides a user message, and how every surface reads it back out.
//!
//! Attachments are not a doc container: the composer uploads the bytes to the
//! **chat's host device** (`UploadChunk`/`UploadCommit` → a durable absolute
//! path there) and appends those paths to the prompt under a trailer. That
//! text is what persists in the session doc, what the agent reads, and what
//! each viewport parses back to draw thumbnails and file chips. `RunRequest::
//! attachments` carries the same paths as structured data so a harness can
//! inline image bytes, but the text is the transport of record — a harness
//! that inlines nothing still sees openable paths.
//!
//! It lives here rather than in `comet-ui` for the reason the module docs give
//! (gh#535): the phone parses the same trailer in Swift, and a viewport that
//! disagreed about where the body ends would render a user's prompt with the
//! machine trailer in it — or silently swallow a line of their text. One rule,
//! one test suite, and `apps/ios/Comet/Spec/attachments-spec.json` checks the
//! Swift half against this one.
//!
//! Two markers, because the two kinds read differently to a human and to an
//! agent: images say *open them to view*, everything else says *open them to
//! read*. Both parse; only the all-images case emits the image wording, so a
//! message produced before files were attachable is byte-identical to one
//! produced now.

use serde::{Deserialize, Serialize};

/// The body an image-only send carries (comet's `use-attachments.ts`).
pub const IMAGE_ATTACHMENT_ONLY_TEXT: &str = "See the attached image(s).";
/// The body a send with any non-image attachment carries.
pub const FILE_ATTACHMENT_ONLY_TEXT: &str = "See the attached file(s).";

const IMAGE_MARKER: &str = "Attached images (local files — open them to view):";
const FILE_MARKER: &str = "Attached files (local files — open them to read):";

/// Extensions every surface can decode and thumbnail. The engine's read-back
/// jail (`comet_engine::uploads::image_mime_by_ext`) serves exactly these from
/// outside the uploads dir, so a ref this says is an image is one a viewport
/// can actually fetch.
pub fn is_image_path(path: &str) -> bool {
    let ext = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "svg"
                | "bmp"
                | "tif"
                | "tiff"
                | "avif"
                | "heic"
        )
    )
}

/// How attachments ride the prompt (comet's `withAttachments`): plain local
/// paths appended to the text — the files are staged on the device that runs
/// the agent, so the agent opens them with its own tools, and the same text is
/// what persists as the user's doc entry.
pub fn with_attachments(text: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return text.to_string();
    }
    let all_images = paths.iter().all(|path| is_image_path(path));
    let body = if text.is_empty() {
        if all_images {
            IMAGE_ATTACHMENT_ONLY_TEXT
        } else {
            FILE_ATTACHMENT_ONLY_TEXT
        }
    } else {
        text
    };
    let marker = if all_images {
        IMAGE_MARKER
    } else {
        FILE_MARKER
    };
    let refs: Vec<String> = paths.iter().map(|path| format!("- {path}")).collect();
    format!("{body}\n\n{marker}\n{}", refs.join("\n"))
}

/// One attachment ref parsed back out of a user message's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserAttachmentRef {
    pub id: String,
    pub path: String,
    pub name: String,
    /// Whether this ref can be read back and drawn as a thumbnail. A `false`
    /// here is why a viewport shows a file chip instead of firing a
    /// `ReadAttachmentChunk` the host would refuse.
    pub is_image: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedUserMessage {
    /// The visible prompt, trailer stripped (empty for attachment-only sends).
    pub text: String,
    pub attachments: Vec<UserAttachmentRef>,
}

pub fn name_from_path(path: &str) -> String {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if name.is_empty() {
        "attachment".to_string()
    } else {
        name.to_string()
    }
}

/// Find the refs trailer: a blank line, then a line starting
/// (case-insensitive) with `Attached images (local files` or `Attached files
/// (local files` and ending `):`. Returns `(body_end, refs_start)` byte
/// offsets — the tolerant equivalent of comet's `ATTACHED_IMAGES_RE`.
fn find_refs_marker(content: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let mut best: Option<(usize, usize)> = None;
    for needle in [
        "\n\nattached images (local files",
        "\n\nattached files (local files",
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
                // The FIRST trailer in the message wins whichever marker it
                // used: a prompt that quotes the other wording further down is
                // still one message with one trailer.
                if best.is_none_or(|(prev, _)| gap < prev) {
                    best = Some((gap, refs_start));
                }
                break;
            }
            from = line_start;
        }
    }
    best
}

/// Split the visible prompt from its attachment-ref trailer (comet's
/// `parseUserMessageImages`, generalized past images).
pub fn parse_user_message_attachments(content: &str) -> ParsedUserMessage {
    let Some((body_end, refs_start)) = find_refs_marker(content) else {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments: Vec::new(),
        };
    };
    let body = content[..body_end].trim_end();
    let attachments: Vec<UserAttachmentRef> = content[refs_start..]
        .lines()
        .filter_map(|line| {
            let path = line.trim_start().strip_prefix("- ")?.trim();
            (!path.is_empty()).then(|| path.to_string())
        })
        .enumerate()
        .map(|(index, path)| UserAttachmentRef {
            id: format!("{index}:{path}"),
            name: name_from_path(&path),
            is_image: is_image_path(&path),
            path,
        })
        .collect();
    if attachments.is_empty() {
        // A marker line with nothing under it is somebody's prose, not a
        // trailer — hand back the message untouched.
        return ParsedUserMessage {
            text: content.to_string(),
            attachments,
        };
    }
    let body_is_placeholder =
        body.trim() == IMAGE_ATTACHMENT_ONLY_TEXT || body.trim() == FILE_ATTACHMENT_ONLY_TEXT;
    ParsedUserMessage {
        text: if body_is_placeholder {
            String::new()
        } else {
            body.to_string()
        },
        attachments,
    }
}

/// What the rail/sidebar shows for a user message: their words when they wrote
/// any, otherwise what they sent instead of words.
pub fn user_message_rail_text(content: &str) -> String {
    let parsed = parse_user_message_attachments(content);
    if !parsed.text.trim().is_empty() {
        return parsed.text;
    }
    let all_images = parsed.attachments.iter().all(|att| att.is_image);
    match (parsed.attachments.len(), all_images) {
        (0, _) => content.to_string(),
        (1, true) => "Attached image".to_string(),
        (1, false) => "Attached file".to_string(),
        (n, true) => format!("{n} attached images"),
        (n, false) => format!("{n} attached files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(items: &[&str]) -> Vec<String> {
        items.iter().map(|p| (*p).to_string()).collect()
    }

    #[test]
    fn images_keep_the_wording_they_have_always_had() {
        let rendered = with_attachments("what is this", &paths(&["/u/a.png"]));
        assert_eq!(
            rendered,
            "what is this\n\nAttached images (local files — open them to view):\n- /u/a.png"
        );
    }

    #[test]
    fn one_non_image_moves_the_whole_trailer_to_the_file_wording() {
        let rendered = with_attachments("", &paths(&["/u/a.png", "/u/log.txt"]));
        assert!(rendered.starts_with(FILE_ATTACHMENT_ONLY_TEXT));
        assert!(rendered.contains("Attached files (local files — open them to read):"));
    }

    #[test]
    fn both_markers_parse_and_classify() {
        let images = with_attachments("hi", &paths(&["/u/a.PNG"]));
        let files = with_attachments("hi", &paths(&["/u/spec.pdf", "/u/b.jpeg"]));
        let parsed_images = parse_user_message_attachments(&images);
        let parsed_files = parse_user_message_attachments(&files);
        assert_eq!(parsed_images.text, "hi");
        assert!(parsed_images.attachments[0].is_image);
        assert_eq!(parsed_files.text, "hi");
        assert_eq!(parsed_files.attachments.len(), 2);
        assert!(!parsed_files.attachments[0].is_image);
        assert!(parsed_files.attachments[1].is_image);
        assert_eq!(parsed_files.attachments[0].name, "spec.pdf");
    }

    #[test]
    fn an_attachment_only_send_has_no_visible_text() {
        let round = parse_user_message_attachments(&with_attachments("", &paths(&["/u/a.pdf"])));
        assert_eq!(round.text, "");
        assert_eq!(round.attachments.len(), 1);
    }

    #[test]
    fn a_marker_with_no_refs_under_it_is_prose() {
        let content = "hi\n\nAttached files (local files — open them to read):\nnothing";
        let parsed = parse_user_message_attachments(content);
        assert_eq!(parsed.text, content);
        assert!(parsed.attachments.is_empty());
    }

    #[test]
    fn the_first_trailer_wins_whichever_marker_it_used() {
        let content = "hi\n\nAttached files (local files — open them to read):\n- /u/a.pdf\n\n\
                       Attached images (local files — open them to view):\n- /u/b.png";
        let parsed = parse_user_message_attachments(content);
        assert_eq!(parsed.text, "hi");
        // Everything after the first marker is refs — including the second
        // marker line, which is not a `- ` line and so contributes nothing.
        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].path, "/u/a.pdf");
        assert_eq!(parsed.attachments[1].path, "/u/b.png");
    }

    #[test]
    fn rail_text_names_the_kind_that_was_sent() {
        assert_eq!(
            user_message_rail_text(&with_attachments("", &paths(&["/u/a.png"]))),
            "Attached image"
        );
        assert_eq!(
            user_message_rail_text(&with_attachments("", &paths(&["/u/a.pdf", "/u/b.txt"]))),
            "2 attached files"
        );
        assert_eq!(
            user_message_rail_text(&with_attachments("look", &paths(&["/u/a.png"]))),
            "look"
        );
        assert_eq!(user_message_rail_text("plain"), "plain");
    }

    #[test]
    fn extensions_decide_what_can_be_thumbnailed() {
        assert!(is_image_path("/u/a.HEIC"));
        assert!(is_image_path("a.jpg"));
        assert!(!is_image_path("/u/notes.md"));
        assert!(!is_image_path("/u/png"));
        assert!(!is_image_path("/u/archive.png.zip"));
    }
}

/// The cross-language fixture: this module's rules, written out so the phone's
/// Swift twin (`apps/ios/Comet/Composer/Attachments.swift`) can be checked
/// against them without linking Rust. Same discipline as `stats-spec.json` —
/// whichever side moves is the side that fails.
#[cfg(test)]
mod spec {
    use super::*;
    use serde_json::{Value, json};

    fn spec_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/ios/Comet/Spec/attachments-spec.json")
    }

    fn render_case(name: &str, text: &str, paths: &[&str]) -> Value {
        let paths: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
        json!({
            "name": name,
            "text": text,
            "paths": paths,
            "expect": with_attachments(text, &paths),
        })
    }

    fn parse_case(name: &str, content: &str) -> Value {
        json!({
            "name": name,
            "content": content,
            "expect": parse_user_message_attachments(content),
            "railText": user_message_rail_text(content),
        })
    }

    fn build() -> Value {
        let image_send = with_attachments("what is this", &["/u/uploads/a.png".to_string()]);
        let mixed_send = with_attachments(
            "",
            &[
                "/u/uploads/spec.pdf".to_string(),
                "/u/uploads/shot.png".to_string(),
            ],
        );
        json!({
            "note": "Generated by crates/proto/src/view/attachments.rs (mod spec). \
                     Do not hand-edit — run `UPDATE_ATTACHMENTS_SPEC=1 cargo test -p comet-proto attachments`.",
            "render": [
                render_case("no attachments is the text itself", "hello", &[]),
                render_case("images keep the original wording", "what is this", &["/u/uploads/a.png"]),
                render_case("an image-only send gets the placeholder body", "", &["/u/uploads/a.png"]),
                render_case(
                    "one non-image moves the whole trailer",
                    "read this",
                    &["/u/uploads/a.png", "/u/uploads/log.txt"],
                ),
                render_case("a file-only send gets the file placeholder", "", &["/u/uploads/spec.pdf"]),
            ],
            "parse": [
                parse_case("a plain message has no trailer", "just words"),
                parse_case("an image send round-trips", &image_send),
                parse_case("a mixed send round-trips", &mixed_send),
                parse_case(
                    "a marker with no refs under it is prose",
                    "hi\n\nAttached files (local files — open them to read):\nnothing",
                ),
                parse_case(
                    "the body keeps its own blank lines",
                    &with_attachments("one\n\ntwo", &["/u/uploads/a.pdf".to_string()]),
                ),
            ],
            "isImage": [
                json!({ "path": "/u/uploads/a.PNG", "expect": is_image_path("/u/uploads/a.PNG") }),
                json!({ "path": "/u/uploads/a.heic", "expect": is_image_path("/u/uploads/a.heic") }),
                json!({ "path": "/u/uploads/notes.md", "expect": is_image_path("/u/uploads/notes.md") }),
                json!({ "path": "/u/uploads/png", "expect": is_image_path("/u/uploads/png") }),
            ],
        })
    }

    #[test]
    fn the_cross_language_fixture_matches_this_module() {
        let built = build();
        let rendered = format!(
            "{}\n",
            serde_json::to_string_pretty(&built).expect("serializes")
        );
        let path = spec_path();
        if std::env::var("UPDATE_ATTACHMENTS_SPEC").is_ok() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).expect("fixture directory");
            }
            std::fs::write(&path, &rendered).expect("writes the fixture");
            return;
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "cannot read {}: {err}. Run \
                 `UPDATE_ATTACHMENTS_SPEC=1 cargo test -p comet-proto attachments`",
                path.display()
            )
        });
        let disk: Value = serde_json::from_str(&on_disk).expect("the fixture is JSON");
        assert_eq!(
            disk, built,
            "the checked-in attachment fixture no longer matches this module — the trailer rule \
             changed here, so the phone's copy in apps/ios/Comet/Composer/Attachments.swift is \
             now wrong too. Regenerate with `UPDATE_ATTACHMENTS_SPEC=1 cargo test -p comet-proto \
             attachments`, and run the Swift half (`scripts/ios-attachments-spec.sh`) in the same \
             change."
        );
    }
}
