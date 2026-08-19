//! Safe reconstruction of a session document whose local CRDT history can no
//! longer be imported into the edge's shallow snapshot.
//!
//! The edge snapshot is the compatible CRDT base. Locally committed semantic
//! entries are replayed onto that base only when the edge transcript is an
//! exact prefix of the local transcript; any other shape is refused so a
//! recovery cannot silently choose between competing histories.

use comet_doc::{DocError, SessionCommandEntry, SessionCommandStatus, SessionDoc};
use loro::{ExportMode, LoroDoc, LoroEncodeError, LoroError};
use std::collections::{HashMap, HashSet};

#[derive(Debug, thiserror::Error)]
pub enum SessionRecoveryError {
    #[error("loro: {0}")]
    Loro(#[from] LoroError),
    #[error("loro export: {0}")]
    LoroExport(#[from] LoroEncodeError),
    #[error("session document: {0}")]
    Doc(#[from] DocError),
    #[error("{which} snapshot import is incomplete")]
    IncompleteSnapshot { which: &'static str },
    #[error("recovery update import is incomplete")]
    IncompleteRecoveryUpdate,
    #[error("{which} {kind} contains duplicate stable id {id}")]
    DuplicateId {
        which: &'static str,
        kind: &'static str,
        id: String,
    },
    #[error("the authoritative {kind} is not an exact prefix of the local {kind}")]
    NotExactPrefix { kind: &'static str },
    #[error("the authoritative {kind} is not an ordered subsequence of the local {kind}")]
    NotOrderedSubsequence { kind: &'static str },
    #[error("command {id} has conflicting immutable content")]
    CommandContentConflict { id: String },
    #[error("command {id} has an unsafe outcome transition from {authoritative:?} to {local:?}")]
    CommandOutcomeConflict {
        id: String,
        authoritative: SessionCommandStatus,
        local: SessionCommandStatus,
    },
    #[error("recovered {kind} did not exactly match the local {kind}")]
    VerificationFailed { kind: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecoveryManifest {
    pub authoritative_entries: usize,
    pub local_entries: usize,
    pub replayed_entry_ids: Vec<String>,
    pub authoritative_commands: usize,
    pub local_commands: usize,
    pub replayed_command_ids: Vec<String>,
}

pub struct SessionRecovery {
    pub manifest: SessionRecoveryManifest,
    /// Full snapshot of the compatible reconstructed document.
    pub snapshot: Vec<u8>,
    /// Only the semantic replay operations missing from the authoritative VV.
    pub update: Vec<u8>,
}

/// Rebuild a stale local session on top of an authoritative shallow snapshot.
///
/// This function is pure with respect to external state: it reads byte slices,
/// constructs fresh in-memory documents, and returns bytes for the caller to
/// inspect or persist. It never opens a store or connects to a room.
pub fn build_session_recovery(
    authoritative_snapshot: &[u8],
    local_snapshot: &[u8],
) -> Result<SessionRecovery, SessionRecoveryError> {
    let authoritative = import_complete(authoritative_snapshot, "authoritative")?;
    let local = import_complete(local_snapshot, "local")?;
    let authoritative_session = SessionDoc::from_doc(authoritative.clone());
    let local_session = SessionDoc::from_doc(local);
    let authoritative_entries = authoritative_session.read_entries()?;
    let local_entries = local_session.read_entries()?;
    let authoritative_commands = authoritative_session.read_commands()?;
    let local_commands = local_session.read_commands()?;

    require_unique_ids(
        "authoritative",
        "transcript",
        authoritative_entries.iter().map(|entry| &entry.id),
    )?;
    require_unique_ids(
        "local",
        "transcript",
        local_entries.iter().map(|entry| &entry.id),
    )?;
    require_unique_ids(
        "authoritative",
        "command ledger",
        authoritative_commands.iter().map(|command| &command.id),
    )?;
    require_unique_ids(
        "local",
        "command ledger",
        local_commands.iter().map(|command| &command.id),
    )?;
    if authoritative_entries.len() > local_entries.len()
        || authoritative_entries != local_entries[..authoritative_entries.len()]
    {
        return Err(SessionRecoveryError::NotExactPrefix { kind: "transcript" });
    }
    let mut next_authoritative = 0;
    for local_command in &local_commands {
        if authoritative_commands
            .get(next_authoritative)
            .is_some_and(|command| command.id == local_command.id)
        {
            next_authoritative += 1;
        }
    }
    if next_authoritative != authoritative_commands.len() {
        return Err(SessionRecoveryError::NotOrderedSubsequence {
            kind: "command ledger",
        });
    }

    let authoritative_vv = authoritative.oplog_vv();
    let replayed_entry_ids = local_entries[authoritative_entries.len()..]
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    for entry in &local_entries[authoritative_entries.len()..] {
        authoritative_session.push_message(entry)?;
    }
    let local_by_id = local_commands
        .iter()
        .map(|command| (command.id.as_str(), command))
        .collect::<HashMap<_, _>>();
    let mut replayed_command_ids = Vec::new();
    for authoritative_command in &authoritative_commands {
        let local_command = local_by_id[authoritative_command.id.as_str()];
        if !same_command_content(authoritative_command, local_command) {
            return Err(SessionRecoveryError::CommandContentConflict {
                id: authoritative_command.id.clone(),
            });
        }
        if authoritative_command.status != local_command.status
            || authoritative_command.resolution != local_command.resolution
        {
            if authoritative_command.status != SessionCommandStatus::Pending
                || local_command.status == SessionCommandStatus::Pending
            {
                return Err(SessionRecoveryError::CommandOutcomeConflict {
                    id: authoritative_command.id.clone(),
                    authoritative: authoritative_command.status,
                    local: local_command.status,
                });
            }
            authoritative_session.set_command_status(
                &local_command.id,
                local_command.status,
                local_command.resolution.as_deref(),
            )?;
            replayed_command_ids.push(local_command.id.clone());
        }
    }
    let authoritative_ids = authoritative_commands
        .iter()
        .map(|command| command.id.as_str())
        .collect::<HashSet<_>>();
    for (index, command) in local_commands.iter().enumerate() {
        if !authoritative_ids.contains(command.id.as_str()) {
            authoritative_session.insert_recovered_command(index, command)?;
            replayed_command_ids.push(command.id.clone());
        }
    }

    if authoritative_session.read_entries()? != local_entries {
        return Err(SessionRecoveryError::VerificationFailed { kind: "transcript" });
    }
    if authoritative_session.read_commands()? != local_commands {
        return Err(SessionRecoveryError::VerificationFailed {
            kind: "command ledger",
        });
    }

    let snapshot = authoritative.export(ExportMode::Snapshot)?;
    let update = authoritative.export(ExportMode::updates(&authoritative_vv))?;
    verify_update(
        authoritative_snapshot,
        &update,
        &local_entries,
        &local_commands,
    )?;

    Ok(SessionRecovery {
        manifest: SessionRecoveryManifest {
            authoritative_entries: authoritative_entries.len(),
            local_entries: local_entries.len(),
            replayed_entry_ids,
            authoritative_commands: authoritative_commands.len(),
            local_commands: local_commands.len(),
            replayed_command_ids,
        },
        snapshot,
        update,
    })
}

fn verify_update(
    authoritative_snapshot: &[u8],
    update: &[u8],
    expected_entries: &[comet_doc::SessionMessageEntry],
    expected_commands: &[SessionCommandEntry],
) -> Result<(), SessionRecoveryError> {
    let verifier = import_complete(authoritative_snapshot, "verification")?;
    let status = verifier.import(update)?;
    if status.pending.is_some() {
        return Err(SessionRecoveryError::IncompleteRecoveryUpdate);
    }
    let session = SessionDoc::from_doc(verifier);
    if session.read_entries()? != expected_entries {
        return Err(SessionRecoveryError::VerificationFailed { kind: "transcript" });
    }
    if session.read_commands()? != expected_commands {
        return Err(SessionRecoveryError::VerificationFailed {
            kind: "command ledger",
        });
    }
    Ok(())
}

fn same_command_content(left: &SessionCommandEntry, right: &SessionCommandEntry) -> bool {
    left.id == right.id
        && left.payload == right.payload
        && left.context == right.context
        && left.issued_by == right.issued_by
        && left.issued_at == right.issued_at
        && left.based_on == right.based_on
        && left.expires_at == right.expires_at
}

fn import_complete(snapshot: &[u8], which: &'static str) -> Result<LoroDoc, SessionRecoveryError> {
    let doc = LoroDoc::new();
    let status = doc.import(snapshot)?;
    if status.pending.is_some() {
        return Err(SessionRecoveryError::IncompleteSnapshot { which });
    }
    Ok(doc)
}

fn require_unique_ids<'a>(
    which: &'static str,
    kind: &'static str,
    ids: impl Iterator<Item = &'a String>,
) -> Result<(), SessionRecoveryError> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(SessionRecoveryError::DuplicateId {
                which,
                kind,
                id: id.clone(),
            });
        }
    }
    Ok(())
}
