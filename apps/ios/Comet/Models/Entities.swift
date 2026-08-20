// Entity model — Swift mirrors of the workspace/session doc rows
// (crates/doc/src/workspace.rs, schema.rs) and the derived display state
// (crates/ui/src/state.rs, entities.rs). Field names match the doc schema
// exactly; derivations (indicator, staleness, attention rank) are ports.

import Foundation

// MARK: - Workspace doc rows

struct DeviceRow: Identifiable, Hashable {
    var id: String
    var name: String
    var platform: String
    var lastSeenAt: Int64?
    var createdAt: Int64?
}

struct Space: Identifiable, Hashable {
    var id: String
    var deviceId: String
    var path: String
    var name: String?
    var gitDetected: Bool
    var gitCheckedAt: Int64?
    var checkoutId: String?
    var createdAt: Int64

    /// Display name: explicit name, else the folder's basename.
    var displayName: String {
        if let name, !name.isEmpty { return name }
        return (path as NSString).lastPathComponent
    }
}

struct ChatConfig: Hashable, Codable {
    var harness: String
    var model: String?
    var reasoning: String?
    var sandbox: String?
    /// The board-owned credential tuple. Both halves round-trip even though
    /// iOS does not configure them; dropping either can turn a dispatched chat
    /// into an ambient-credential chat on its next turn (gh#440).
    var pushRepo: String? = nil
    var pushContract: GithubPushContract? = nil
    /// Process-local tool servers stamped onto a dispatched chat (gh#273).
    /// Optional keeps old workspace rows decodable; edits preserve the list
    /// even though the iOS picker does not configure it.
    var mcpServers: [MCPServer]? = nil
}

struct GithubPushContract: Hashable, Codable {
    var contentsWrite: Bool
    var workflowsWrite: Bool
}

struct MCPServer: Hashable, Codable {
    var name: String
    var command: String
    var args: [String]
}

/// Where a chat came from, when it was forked from another one (gh#425 —
/// `comet_proto::ChatLineage`).
///
/// The phone cannot make a fork yet. It can very much be *reading* one, and a
/// shared-checkout fork is the case where not knowing is actively misleading:
/// the files under it are another session's too, and the edits arriving in them
/// are not all this agent's. So the row syncs and this renders it.
enum ForkCheckout: String, Codable { case shared, isolated }
enum ForkContext: String, Codable { case nativeResume, transcriptCopy }
enum TranscriptTruncation: String, Codable {
    case oldestMessagesDropped, newestMessageTruncated, oldestDroppedAndNewestTruncated
}

struct ChatLineage: Hashable {
    var sourceChatId: String
    var sourceMessageId: String?
    /// `shared` | `isolated`.
    var checkout: ForkCheckout
    /// `nativeResume` | `transcriptCopy`.
    var context: ForkContext
    var carriedMessages: Int?
    var truncated: Bool
    var truncation: TranscriptTruncation?

    var isShared: Bool { checkout == .shared }

    /// view/fork.rs `destination_tag` — the two must not share a word.
    var checkoutTag: String { isShared ? "shared read-only" : "own worktree" }

    /// view/fork.rs — `resumed` is legacy synced-row vocabulary; new forks
    /// always say `copy` because only the visible transcript came across.
    var contextTag: String { context == .nativeResume ? "resumed" : "copy" }

    var truncationExplanation: String {
        switch truncation {
        case .oldestMessagesDropped:
            return "Oldest messages dropped at the size budget."
        case .newestMessageTruncated:
            return "Newest message truncated at the size budget."
        case .oldestDroppedAndNewestTruncated:
            return "Oldest messages dropped and newest message truncated at the size budget."
        case nil:
            return truncated ? "Some transcript content was omitted at the size budget." : ""
        }
    }

    /// view/fork.rs `compact`.
    func compact(sourceTitle: String?) -> String {
        let source: String
        if let sourceTitle, !sourceTitle.trimmingCharacters(in: .whitespaces).isEmpty {
            source = "\u{201c}\(sourceTitle)\u{201d}"
        } else {
            source = "a deleted chat"
        }
        return "fork of \(source) · \(checkoutTag) · \(contextTag)"
    }

    /// The sentence behind the tag, for a surface with room for one.
    var explanation: String {
        let where_: String = isShared
            ? "Same folder as the session it came from: this fork is read-only, while source edits still appear here."
            : "Its own worktree and branch: nothing here can reach the source's files."
        let what: String = context == .nativeResume
            ? "The provider session was resumed, so this agent holds the whole conversation."
            : "The visible transcript was copied into its first prompt; hidden provider state was not."
        let truncationNote = truncationExplanation.isEmpty ? "" : " \(truncationExplanation)"
        return "\(where_) \(what)\(truncationNote)"
    }
}

struct Chat: Identifiable, Hashable {
    var id: String
    var deviceId: String
    var title: String?
    var archived: Bool
    var cwd: String?
    var branch: String?
    var checkoutId: String?
    var config: ChatConfig?
    var lastMessagePreview: String?
    var lastMessageAt: Int64?
    var createdAt: Int64
    var spaceId: String?
    var lastSeenAt: Int64?
    /// Defaulted so every existing construction (demo data, E2E fixtures) is
    /// untouched: a chat nobody forked has no lineage.
    var forkedFrom: ChatLineage? = nil

    var displayTitle: String {
        if let title, !title.isEmpty { return title }
        return "New session"
    }

    /// entities.rs:123 — unseen when a message arrived after the last seen mark.
    var unseen: Bool {
        guard let lastMessageAt else { return false }
        guard let lastSeenAt else { return true }
        return lastMessageAt > lastSeenAt
    }
}

enum SessionStatus: String {
    case idle, working, awaitingInput, errored
}

struct SessionRow: Hashable {
    var chatId: String
    var deviceId: String
    var status: SessionStatus
    var startedAt: Int64?
    var updatedAt: Int64
}

// MARK: - Derived display status (entities.rs / state.rs ports)

enum ChatIndicator: Int {
    case awaitingInput = 0
    case errored = 1
    case working = 2
    case completed = 3
    case idle = 4
}

/// state.rs:277 — a Working/AwaitingInput row older than this reads as stale
/// (a crashed backend never shows eternal "Working").
let sessionStaleMs: Int64 = 45_000
/// Freshness window for a device row's `lastSeenAt`. Only the DEMO dataset
/// reads it now: live presence is membership in `WorkspaceStore.presence` —
/// the room's own answer about its socket set — not the age of a timestamp
/// anybody wrote (gh#145).
let presenceFreshMs: Int64 = 45_000

func effectiveStatus(_ row: SessionRow?, now: Int64) -> SessionStatus? {
    guard let row else { return nil }
    switch row.status {
    case .working, .awaitingInput:
        let age = now - row.updatedAt
        // Negative ages (clock skew) are fresh.
        return age > sessionStaleMs ? nil : row.status
    case .errored, .idle:
        return row.status
    }
}

/// entities.rs:147 — live Working/AwaitingInput win; Errored only if unseen;
/// else unseen ⇒ Completed; else Idle.
func chatIndicator(chat: Chat, live: SessionStatus?) -> ChatIndicator {
    switch live {
    case .working: return .working
    case .awaitingInput: return .awaitingInput
    case .errored: return chat.unseen ? .errored : .idle
    default: return chat.unseen ? .completed : .idle
    }
}

/// The Sessions list order: PURE RECENCY, id tiebreak — a port of state.rs
/// `sort_active`. Status drives the dot, never the position.
///
/// This used to bucket by attention first, which is what the desktop did
/// before 55e1845: opening a completed session marks it seen (completed →
/// idle), and the row then dropped a bucket out from under the pointer. The
/// dots carry urgency instead, so the order never moves on its own.
func sortActive(_ chats: [Chat]) -> [Chat] {
    chats.sorted { a, b in
        let ta = a.lastMessageAt ?? a.createdAt, tb = b.lastMessageAt ?? b.createdAt
        if ta != tb { return ta > tb }
        return a.id < b.id
    }
}

// MARK: - Session doc entries

enum MessageRole: String {
    case user, assistant, system
}

enum MessageStatus: String {
    case streaming, complete, aborted
}

struct UserInputQuestion: Hashable, Codable {
    var id: String
    var header: String
    var question: String
    /// Plain labels — `proto::agent::UserInputQuestion.options` is a
    /// `Vec<String>`. This was modelled as `{label, description}` objects,
    /// which NEVER decoded: every question arrived empty, so the panel had no
    /// options to show and an unresolved request crashed the app.
    var options: [String]
    var multiSelect: Bool?
}

struct UserInputAnswer: Hashable, Codable {
    var questionId: String
    var labels: [String]
}

/// Render-only sanitized tool call (packages render-parts policy).
struct RenderToolCall: Hashable {
    var tag: String
    /// Loose payload — only render-relevant fields survive in the doc.
    var fields: [String: AnyHashable]

    var string: (String) -> String? { { key in self.fields[key] as? String } }
}

enum MessagePart: Hashable, Identifiable {
    case text(id: String, text: String)
    case tool(id: String, call: RenderToolCall, isError: Bool, resolved: Bool)
    case input(id: String, requestId: String, questions: [UserInputQuestion], resolved: Bool)
    case error(id: String, message: String)

    var id: String {
        switch self {
        case .text(let id, _), .tool(let id, _, _, _), .input(let id, _, _, _), .error(let id, _):
            return id
        }
    }
}

struct MessageEntry: Identifiable, Hashable {
    var id: String
    var role: MessageRole
    var parts: [MessagePart]
    var createdAt: Int64
    var deviceId: String
    var status: MessageStatus?
    var continuationOf: String?
}

// MARK: - Folder browsing (add-space palette data)

/// comet-proto FolderListing (entities.rs:225): the device's answer to
/// ListFolders. Dotfiles are pre-filtered and entries are capped at 500 by
/// the engine; the parent path is computed client-side.
struct FolderEntry: Codable, Hashable {
    var name: String
    var isDir: Bool
    var isRepo: Bool
}

struct FolderListing: Codable {
    var path: String
    var entries: [FolderEntry]
    var truncated: Bool

    var parent: String? {
        guard path.contains("/"), path != "/" else { return nil }
        let trimmed = String(path[..<(path.lastIndex(of: "/") ?? path.startIndex)])
        return trimmed.isEmpty ? "/" : trimmed
    }
}

/// pickers.rs CheckoutKind — where a new session runs. "Current worktree" is
/// NOT a third mode: it's `local` when the picked ref is already materialized
/// as a worktree (the session reuses that checkout's path).
enum CheckoutKind {
    case local
    case newWorktree
}

/// comet-proto RepoRef (entities.rs:193): one selectable ref from ListRefs.
struct RepoRef: Codable, Hashable, Identifiable {
    var name: String
    var current: Bool = false
    var worktreePath: String?

    var id: String { name }
}

enum ContextRefKind: String, Codable, Hashable {
    case file, directory
}

struct ContextRef: Codable, Hashable, Identifiable {
    var path: String
    var kind: ContextRefKind
    var checkoutId: String? = nil
    var id: String { "\(kind.rawValue):\(path)" }
}

struct ContextSearch: Codable {
    var matches: [ContextRef]
    var truncated: Bool = false
    var checkoutId: String? = nil
}

struct FollowupRow: Identifiable, Hashable {
    var id: String
    var prompt: String
    var context: [ContextRef]
    var messageId: String
    var edited: Bool
}

// MARK: - Command ledger (commands.rs port)

let commandDefaultTtlMs: Int64 = 86_400_000

/// comet-proto RunRequest (agent.rs:81). `reasoning` is lowercase
/// ("high"/"xhigh"/…), `sandbox` kebab-case ("workspace-write"), harness ids
/// kebab-case ("claude-code").
struct RunRequest: Codable {
    var prompt: String
    var model: String?
    var reasoning: String?
    var modelOptions: [String: String] = [:]
    var cwd: String
    var sandbox: String = "workspace-write"
    var autoApprove: Bool = true
    var resume: String?
    /// Absolute paths of attachments already committed on the RUN device —
    /// images or documents (gh#535), staged there by UploadChunk/UploadCommit.
    /// The same paths ride the prompt text as an `Attached images/files (local
    /// files …)` trailer, which is what persists in the doc and what the agent
    /// opens; this field additionally lets a harness inline image bytes.
    var attachments: [String] = []
}

enum SessionCommandPayload {
    case run(request: RunRequest, messageId: String)
    case steer(prompt: String, messageId: String?)
    case interrupt
    case respondInput(requestId: String, answers: [UserInputAnswer])

    var kind: String {
        switch self {
        case .run: return "run"
        case .steer: return "steer"
        case .interrupt: return "interrupt"
        case .respondInput: return "respondInput"
        }
    }
}

func nowMs() -> Int64 {
    Int64(Date().timeIntervalSince1970 * 1000)
}
