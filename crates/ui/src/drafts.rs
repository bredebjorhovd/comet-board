//! Per-chat composer drafts, persisted device-locally (gh#536).
//!
//! A half-written prompt is not transcript content and not a setting: it is
//! the user's own unsent text, and the only two things that may clear it are
//! sending it and deleting it. Everything else — switching panes, switching
//! chats, collapsing a sidebar, quitting the app — must hand it back exactly
//! as it was left, caret included.
//!
//! **Local only.** Drafts live in `{data_dir}/composer-drafts.json` and never
//! enter the workspace or session docs: they are not conversation, they are
//! not synced, and they must not appear on another device. (The file sits
//! beside `ui-settings.json` rather than inside it because the two have
//! different write rates — a draft changes on every keystroke — and because a
//! corrupt settings file should never be able to eat somebody's typing.)

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "composer-drafts.json";

/// Debounce for draft writes after a keystroke. Shorter than the settings
/// debounce: what is being protected here is the user's own words, and the
/// window between the last keystroke and a crash is exactly what gets lost.
pub const SAVE_DEBOUNCE_MS: u64 = 250;

/// How many chats keep a draft on disk. Far above any plausible number of
/// half-written prompts; the cap exists so a long-lived install with thousands
/// of dead chats cannot grow the file without bound. Eviction is oldest-first.
pub const MAX_DRAFTS: usize = 200;

/// One chat's unsent text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Draft {
    pub text: String,
    /// Caret byte offset within `text` (the selection's active end). Restored
    /// on return so a draft comes back where it was left, not at its end.
    pub caret: usize,
    /// Last write, epoch millis — eviction order only, never displayed.
    pub updated_at: i64,
}

/// Every chat's draft, keyed by the composer's draft key: a chat id, or
/// `new:{space_id}` for a space's new-chat canvas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DraftStore {
    drafts: HashMap<String, Draft>,
}

impl DraftStore {
    /// Load from `{data_dir}/composer-drafts.json`; an unreadable or corrupt
    /// file yields an empty store rather than an error — a draft file is a
    /// convenience, and refusing to boot over one would be worse than the
    /// loss it was meant to prevent.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(store) => store.healed(),
                Err(err) => {
                    tracing::warn!(error = %err, "composer drafts corrupt; starting empty");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never
    /// corrupts what was already saved.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    pub fn get(&self, key: &str) -> Option<&Draft> {
        self.drafts.get(key)
    }

    pub fn text(&self, key: &str) -> &str {
        self.drafts.get(key).map(|d| d.text.as_str()).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.drafts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.drafts.len()
    }

    /// Record `key`'s draft. Empty text is not a draft — it is the absence of
    /// one — so it removes the entry instead of storing a blank. Returns
    /// whether anything changed, so the caller can skip a write.
    pub fn set(&mut self, key: &str, text: &str, caret: usize, now_ms: i64) -> bool {
        if text.is_empty() {
            return self.remove(key);
        }
        let caret = clamp_caret(text, caret);
        if let Some(existing) = self.drafts.get(key)
            && existing.text == text
            && existing.caret == caret
        {
            return false;
        }
        self.drafts.insert(
            key.to_string(),
            Draft {
                text: text.to_string(),
                caret,
                updated_at: now_ms,
            },
        );
        self.evict();
        true
    }

    /// Drop `key`'s draft — sending it, or the user emptying it. Returns
    /// whether there was one.
    pub fn remove(&mut self, key: &str) -> bool {
        self.drafts.remove(key).is_some()
    }

    /// Repair a hand-edited or partially-written file: no empty drafts, every
    /// caret inside its text and on a character boundary, and the cap applied.
    fn healed(mut self) -> Self {
        self.drafts.retain(|_, d| !d.text.is_empty());
        for draft in self.drafts.values_mut() {
            draft.caret = clamp_caret(&draft.text, draft.caret);
        }
        self.evict();
        self
    }

    /// Keep the [`MAX_DRAFTS`] most recently touched.
    fn evict(&mut self) {
        if self.drafts.len() <= MAX_DRAFTS {
            return;
        }
        let mut keys: Vec<(i64, String)> = self
            .drafts
            .iter()
            .map(|(k, d)| (d.updated_at, k.clone()))
            .collect();
        // Oldest first; ties broken by key so eviction is deterministic.
        keys.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (_, key) in keys.into_iter().take(self.drafts.len() - MAX_DRAFTS) {
            self.drafts.remove(&key);
        }
    }
}

/// The caret, forced inside `text` and onto a character boundary — a stored
/// offset can outlive the text it indexed (a hand-edited file, a draft written
/// by a newer build), and a byte offset mid-character would panic the layout.
fn clamp_caret(text: &str, caret: usize) -> usize {
    if caret >= text.len() {
        return text.len();
    }
    let mut caret = caret;
    while caret > 0 && !text.is_char_boundary(caret) {
        caret -= 1;
    }
    caret
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(entries: &[(&str, &str, usize)]) -> DraftStore {
        let mut store = DraftStore::default();
        for (i, (key, text, caret)) in entries.iter().enumerate() {
            store.set(key, text, *caret, i as i64);
        }
        store
    }

    #[test]
    fn round_trip_keeps_text_and_caret_per_chat() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(&[
            ("chat-a", "three\nlines\nhere", 6),
            ("new:space-1", "hi", 1),
        ]);
        store.save(dir.path()).unwrap();
        let loaded = DraftStore::load(dir.path());
        assert_eq!(loaded, store);
        assert_eq!(loaded.text("chat-a"), "three\nlines\nhere");
        assert_eq!(loaded.get("chat-a").unwrap().caret, 6);
        assert_eq!(loaded.text("new:space-1"), "hi");
        // Two chats, two drafts, neither wearing the other's text.
        assert_ne!(loaded.text("chat-a"), loaded.text("new:space-1"));
    }

    #[test]
    fn missing_and_corrupt_files_start_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(DraftStore::load(dir.path()).is_empty());
        std::fs::write(DraftStore::path(dir.path()), "{not json").unwrap();
        assert!(DraftStore::load(dir.path()).is_empty());
    }

    #[test]
    fn emptying_a_draft_removes_it() {
        let mut store = store_with(&[("chat-a", "typed", 5)]);
        assert!(store.set("chat-a", "", 0, 1));
        assert!(store.get("chat-a").is_none());
        // Removing what was never there is not a change worth a write.
        assert!(!store.set("chat-b", "", 0, 2));
        assert!(!store.remove("chat-b"));
    }

    #[test]
    fn an_unchanged_draft_reports_no_write() {
        let mut store = store_with(&[("chat-a", "typed", 5)]);
        assert!(!store.set("chat-a", "typed", 5, 9));
        assert!(store.set("chat-a", "typed", 2, 9), "the caret moved");
        assert!(store.set("chat-a", "typed more", 2, 9));
    }

    #[test]
    fn a_caret_past_the_end_or_mid_character_is_pulled_back() {
        let mut store = DraftStore::default();
        store.set("chat-a", "hei", 99, 0);
        assert_eq!(store.get("chat-a").unwrap().caret, 3);
        // "é" is two bytes: an offset inside it must not survive to the layout.
        store.set("chat-b", "café", 4, 0);
        assert_eq!(store.get("chat-b").unwrap().caret, 3);
    }

    #[test]
    fn a_hand_edited_file_is_healed_on_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            DraftStore::path(dir.path()),
            r#"{"drafts":{"a":{"text":"café","caret":4},"b":{"text":"","caret":0}}}"#,
        )
        .unwrap();
        let loaded = DraftStore::load(dir.path());
        assert_eq!(loaded.get("a").unwrap().caret, 3);
        assert!(loaded.get("b").is_none(), "an empty draft is no draft");
    }

    #[test]
    fn the_oldest_drafts_are_evicted_past_the_cap() {
        let mut store = DraftStore::default();
        for i in 0..(MAX_DRAFTS + 5) {
            store.set(&format!("chat-{i:04}"), "x", 1, i as i64);
        }
        assert_eq!(store.len(), MAX_DRAFTS);
        assert!(store.get("chat-0000").is_none(), "oldest evicted");
        assert!(
            store.get(&format!("chat-{:04}", MAX_DRAFTS + 4)).is_some(),
            "newest kept"
        );
    }
}
