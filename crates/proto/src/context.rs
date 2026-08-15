//! Typed context references — what an `@` in the composer *is* (gh#424).
//!
//! A reference is **a path, resolved where the work happens**. Not a content
//! snapshot, not a hash, not an inlined file: the chat's checkout lives on its
//! host device, the turn runs there, and the only thing that means the same on
//! the laptop that typed it and the box that runs it is a path relative to that
//! checkout's root. A snapshot would additionally put file content into the
//! session doc, which the render-parts privacy policy exists to prevent.
//!
//! The consequences of that choice, all of them deliberate:
//!
//! - **Resolution is late.** `path` is checkout-relative and joined to the root
//!   by the host at execute time, so a reference picked on a phone against a
//!   worktree it has never seen still names the same file. It also means a
//!   reference can go stale between the pick and the turn — see
//!   [`ResolvedRef::exists`], which is reported rather than refused, because a
//!   file the agent is about to *create* is a legitimate thing to point at.
//! - **Containment is checked, not assumed.** A path that normalizes outside
//!   the checkout is not a reference at all (see `comet_engine::context_files`);
//!   the picker cannot produce one, so anything that does is a client bug or a
//!   forged command, and both are refused at the host.
//! - **A directory enumerates nothing.** It names a place to look. Expanding one
//!   would put a repo listing in the prompt and in the doc, stale by the time it
//!   is read, when the agent has better tools for looking inside a directory
//!   than a listing somebody's composer made earlier.

use serde::{Deserialize, Serialize};

/// What a reference points at. Only the distinction the agent (and the picker
/// row) actually acts on — a file is read, a directory is looked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContextRefKind {
    File,
    Directory,
}

impl ContextRefKind {
    pub fn is_dir(self) -> bool {
        matches!(self, ContextRefKind::Directory)
    }
}

/// One `@` reference, as it rides the durable command.
///
/// `path` is **relative to the chat's checkout root**, forward-slashed, with no
/// `.` or `..` segments — the normal form [`normalize`] produces and the host
/// re-checks before it resolves anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextRef {
    pub path: String,
    pub kind: ContextRefKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_id: Option<String>,
}

impl ContextRef {
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: ContextRefKind::File,
            checkout_id: None,
        }
    }

    pub fn directory(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: ContextRefKind::Directory,
            checkout_id: None,
        }
    }

    /// The token that stands for this reference in prompt text: `@path`, and
    /// `@path/` for a directory so the trailing slash says which it is on every
    /// surface, including the ones that draw no chip.
    pub fn token(&self) -> String {
        match self.kind {
            ContextRefKind::File => format!("@{}", self.path),
            ContextRefKind::Directory => format!("@{}/", self.path),
        }
    }
}

/// One row of the `@` picker: a path the host found under the checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextMatch {
    /// Checkout-relative path (the value a [`ContextRef`] is minted from).
    pub path: String,
    pub kind: ContextRefKind,
}

impl ContextMatch {
    /// The last path segment — what the picker puts in the strong column.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// Everything above the last segment, or `""` at the root.
    pub fn parent(&self) -> &str {
        match self.path.rfind('/') {
            Some(ix) => &self.path[..ix],
            None => "",
        }
    }

    pub fn to_ref(&self) -> ContextRef {
        ContextRef {
            path: self.path.clone(),
            kind: self.kind,
            checkout_id: None,
        }
    }
}

/// What the host's search answered, plus what it had to leave out.
///
/// `truncated` is not decoration: the walk is budget-bounded (a checkout can be
/// a million files and this runs per keystroke over a relay), and a picker that
/// showed 20 of thousands without saying so would read as "this is everything
/// there is".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSearch {
    #[serde(default)]
    pub matches: Vec<ContextMatch>,
    /// True when the scan hit its budget or the result cap — there are more.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_id: Option<String>,
}

/// A reference after the host resolved it against the real checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRef {
    /// The reference as queued (checkout-relative).
    pub reference: ContextRef,
    /// Absolute path on the host device.
    pub absolute: String,
    /// Whether it is there *now*, at execute time. A reference that went away
    /// between the pick and the turn is reported, never silently dropped and
    /// never fatal.
    pub exists: bool,
}

/// Normalize a checkout-relative path into the one form a reference may carry:
/// forward slashes, no leading `./`, no empty or `.` segments, no trailing
/// slash. `None` when the path escapes the checkout (`..`), is absolute, or is
/// empty — none of which is a reference, and all of which the host refuses.
///
/// Textual and total on purpose: it runs on the *sender* to mint the reference
/// and on the *host* to check it, and the host's copy must not depend on the
/// filesystem to reach its verdict (a symlinked `a/b` that resolves elsewhere is
/// caught separately, by canonicalizing against the root — see
/// `comet_engine::context_files::resolve`).
pub fn normalize(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    if path.starts_with('/') || path.chars().nth(1) == Some(':') {
        return None; // absolute — not relative to any checkout
    }
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return None, // escaping is refused, never resolved away
            other => parts.push(other),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_keeps_relative_paths_and_refuses_escapes() {
        assert_eq!(normalize("src/main.rs").as_deref(), Some("src/main.rs"));
        assert_eq!(normalize("./src/./main.rs").as_deref(), Some("src/main.rs"));
        assert_eq!(normalize("src//main.rs").as_deref(), Some("src/main.rs"));
        assert_eq!(normalize("src/").as_deref(), Some("src"));
        assert_eq!(normalize("src\\main.rs").as_deref(), Some("src/main.rs"));

        // Everything that is not a path inside the checkout.
        assert_eq!(normalize("../secrets"), None);
        assert_eq!(normalize("src/../../etc/passwd"), None);
        assert_eq!(normalize("/etc/passwd"), None);
        assert_eq!(normalize("C:/Windows"), None);
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("./"), None);
    }

    #[test]
    fn a_reference_reads_the_same_on_a_surface_that_draws_no_chip() {
        assert_eq!(ContextRef::file("src/main.rs").token(), "@src/main.rs");
        assert_eq!(ContextRef::directory("crates/ui").token(), "@crates/ui/");
    }

    #[test]
    fn match_columns_split_at_the_last_segment() {
        let m = ContextMatch {
            path: "crates/ui/src/composer.rs".into(),
            kind: ContextRefKind::File,
        };
        assert_eq!(m.name(), "composer.rs");
        assert_eq!(m.parent(), "crates/ui/src");
        let root = ContextMatch {
            path: "README.md".into(),
            kind: ContextRefKind::File,
        };
        assert_eq!(root.name(), "README.md");
        assert_eq!(root.parent(), "");
    }
}
