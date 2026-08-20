// Attachments — the phone's half of the attachment pipeline (gh#535): staging
// from Photos, Files and the pasteboard; the chunked upload to the device that
// HOSTS the chat; the plain-text ref trailer that rides the prompt; the
// transcript read-back cache; and the preview lightbox.
//
// Three things about this file are worth knowing before changing it.
//
// **The transport is text.** Committed absolute paths are appended to the
// prompt under an "Attached images/files (local files …):" trailer. That text
// is what persists in the session doc, what the agent opens, and what every
// viewport parses back out. The rule lives in `comet_proto::view::attachments`
// and the functions here are its Swift twin, checked against the fixture that
// module writes (`Spec/attachments-spec.json`, asserted by
// `AttachmentsSpecRunner`) — because a phone that disagreed about where the
// body ends would show somebody their own prompt with the machine trailer in
// it.
//
// **The upload goes to the chat's host, never "the local device".** The phone
// hosts nothing; a chat lives on a Mac or on the box, and the file has to land
// on whichever one runs the agent — `SessionStore.uploadAttachment` dials that
// device's relay room by `chat.deviceId`. This is the half of gh#535 that
// looked structural: an upload path that assumed the host was local would work
// against a laptop and quietly fail against the box.
//
// **A failed upload is never silent.** Each staged item carries its own
// status; a send that cannot get every file to the host sends NOTHING, keeps
// the draft and the files, and says why. Retrying re-uploads only what failed
// — a committed path is kept, so a five-photo send that lost one does not
// re-push the other four.

import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

// MARK: - Text transport (comet_proto::view::attachments)

/// The body an image-only send carries.
let imageAttachmentOnlyText = "See the attached image(s)."
/// The body a send with any non-image attachment carries.
let fileAttachmentOnlyText = "See the attached file(s)."

private let imageAttachmentMarker = "Attached images (local files — open them to view):"
private let fileAttachmentMarker = "Attached files (local files — open them to read):"

/// Extensions every surface can decode and thumbnail — the set the host's
/// read-back jail serves from outside its uploads dir.
private let imageExtensions: Set<String> = [
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "tif", "tiff", "avif", "heic",
]

func isImagePath(_ path: String) -> Bool {
    let name = path.split(whereSeparator: { $0 == "/" || $0 == "\\" }).last.map(String.init) ?? ""
    guard let dot = name.lastIndex(of: "."), dot != name.startIndex else { return false }
    return imageExtensions.contains(String(name[name.index(after: dot)...]).lowercased())
}

/// `withAttachments`: plain local paths appended to the text — the files are
/// staged on the device that runs the agent, so the agent opens them with its
/// own tools, and the same text is what persists as the user's doc entry.
func withAttachments(text: String, paths: [String]) -> String {
    guard !paths.isEmpty else { return text }
    let allImages = paths.allSatisfy(isImagePath)
    let body = text.isEmpty
        ? (allImages ? imageAttachmentOnlyText : fileAttachmentOnlyText)
        : text
    let marker = allImages ? imageAttachmentMarker : fileAttachmentMarker
    let refs = paths.map { "- \($0)" }.joined(separator: "\n")
    return "\(body)\n\n\(marker)\n\(refs)"
}

/// An attachment ref parsed back out of a user message's text.
struct UserAttachmentRef: Identifiable, Hashable {
    let id: String
    let path: String
    let name: String
    /// Whether this ref can be read back and drawn. `false` is why a viewport
    /// shows a file chip instead of firing a read the host would refuse.
    let isImage: Bool
}

struct ParsedUserMessage {
    var text: String
    var attachments: [UserAttachmentRef]
}

func attachmentName(fromPath path: String) -> String {
    let name = path.split(whereSeparator: { $0 == "/" || $0 == "\\" }).last.map(String.init) ?? ""
    return name.isEmpty ? "attachment" : name
}

/// Split the visible prompt from its attachment-ref trailer: a blank line,
/// then a marker line (case-insensitive) ending `):`, then `- path` lines.
func parseUserMessageAttachments(_ content: String) -> ParsedUserMessage {
    let lines = content.components(separatedBy: "\n")
    var markerIx: Int?
    for (ix, raw) in lines.enumerated() where ix > 0 {
        let line = raw.trimmingCharacters(in: .whitespaces)
        let lowered = line.lowercased()
        guard lines[ix - 1].trimmingCharacters(in: .whitespaces).isEmpty,
              lowered.hasPrefix("attached images (local files")
                  || lowered.hasPrefix("attached files (local files"),
              line.hasSuffix("):") else { continue }
        markerIx = ix
        break
    }
    guard let markerIx else {
        return ParsedUserMessage(text: content, attachments: [])
    }
    let attachments = lines[(markerIx + 1)...].compactMap { line -> String? in
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard trimmed.hasPrefix("- ") else { return nil }
        let path = String(trimmed.dropFirst(2)).trimmingCharacters(in: .whitespaces)
        return path.isEmpty ? nil : path
    }.enumerated().map { ix, path in
        UserAttachmentRef(id: "\(ix):\(path)", path: path,
                          name: attachmentName(fromPath: path), isImage: isImagePath(path))
    }
    guard !attachments.isEmpty else {
        // A marker line with nothing under it is somebody's prose.
        return ParsedUserMessage(text: content, attachments: [])
    }
    let body = lines[..<(markerIx - 1)].joined(separator: "\n")
        .trimmingCharacters(in: .whitespacesAndNewlines)
    let placeholder = body == imageAttachmentOnlyText || body == fileAttachmentOnlyText
    return ParsedUserMessage(text: placeholder ? "" : body, attachments: attachments)
}

/// What a rail/preview shows for a user message: their words when they wrote
/// any, otherwise what they sent instead of words.
func userMessageRailText(_ content: String) -> String {
    let parsed = parseUserMessageAttachments(content)
    if !parsed.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return parsed.text }
    let allImages = parsed.attachments.allSatisfy(\.isImage)
    switch (parsed.attachments.count, allImages) {
    case (0, _): return content
    case (1, true): return "Attached image"
    case (1, false): return "Attached file"
    case (let n, true): return "\(n) attached images"
    case (let n, false): return "\(n) attached files"
    }
}

// MARK: - Staging

/// Matches `crates/ui/src/attachments.rs` MAX_ATTACHMENT_BYTES; the engine's
/// own cap is 32MB, so this is the friendlier of the two limits.
let maxAttachmentBytes = 24 * 1024 * 1024
/// Base64 chars per `UploadChunk` — sized for the relay link.
let uploadChunkB64Chars = 60_000

/// Where one staged item is in its life. A staged send is not a fire-and-
/// forget: the composer holds these until every one reaches the host.
enum AttachmentUploadState: Equatable {
    case staged
    case uploading
    /// Committed on the host — this is the path the trailer will carry.
    case uploaded(path: String)
    case failed(reason: String)

    var isFailed: Bool { if case .failed = self { return true }; return false }
    var committedPath: String? { if case .uploaded(let path) = self { return path }; return nil }
}

/// Something picked in the composer, before (and during) upload. Bytes are what
/// uploads; `image` is non-nil only for types this phone can draw.
struct StagedAttachment: Identifiable, Hashable {
    let id: String
    /// File name with a type-matching extension (agents sniff by extension).
    let name: String
    let data: Data
    let image: UIImage?
    var state: AttachmentUploadState = .staged

    var isImage: Bool { image != nil }
    var byteLabel: String {
        ByteCountFormatter.string(fromByteCount: Int64(data.count), countStyle: .file)
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.id == rhs.id && lhs.state == rhs.state
    }

    func hash(into hasher: inout Hasher) { hasher.combine(id) }

    /// Stage picked photo bytes: keep the formats the host's read-back jail
    /// serves as-is; transcode everything else (HEIC camera shots, mainly) to
    /// JPEG so the thumbnail resolves on every surface.
    static func stagePhoto(data: Data, suggestedName: String? = nil) -> StagedAttachment? {
        var bytes = data
        var ext = sniffImageExtension(data)
        if ext == nil {
            guard let image = UIImage(data: data),
                  let jpeg = image.jpegData(compressionQuality: 0.9) else { return nil }
            bytes = jpeg
            ext = "jpg"
        }
        guard bytes.count <= maxAttachmentBytes, let ext,
              let image = UIImage(data: bytes) else { return nil }
        let id = UUID().uuidString.lowercased()
        let name = suggestedName.map { ensureExtension($0, ext) } ?? "photo-\(id.prefix(8)).\(ext)"
        return StagedAttachment(id: id, name: name, data: bytes, image: image)
    }

    /// Stage a document picked from Files (or pasted): any type, no transcode.
    /// An image among them still decodes, so a picked PNG thumbnails like a
    /// photo does.
    static func stageFile(data: Data, name: String) -> StagedAttachment? {
        guard !data.isEmpty, data.count <= maxAttachmentBytes else { return nil }
        let id = UUID().uuidString.lowercased()
        let cleaned = name.isEmpty ? "file-\(id.prefix(8))" : name
        let image = isImagePath(cleaned) ? UIImage(data: data) : nil
        return StagedAttachment(id: id, name: cleaned, data: data, image: image)
    }

    /// Pasted screenshots arrive as a bare "image" — make sure the staged name
    /// carries a type-matching extension (`ensureExtension`, desktop side).
    private static func ensureExtension(_ name: String, _ ext: String) -> String {
        if let dot = name.lastIndex(of: "."), dot != name.startIndex {
            let suffix = name[name.index(after: dot)...]
            if (2...5).contains(suffix.count), suffix.allSatisfy(\.isLetter) { return name }
        }
        return "\(name).\(ext)"
    }

    /// Magic-byte sniff for the formats both ends support.
    private static func sniffImageExtension(_ data: Data) -> String? {
        guard data.count >= 12 else { return nil }
        let b = [UInt8](data.prefix(12))
        if b[0] == 0x89, b[1] == 0x50, b[2] == 0x4E, b[3] == 0x47 { return "png" }
        if b[0] == 0xFF, b[1] == 0xD8, b[2] == 0xFF { return "jpg" }
        if b[0] == 0x47, b[1] == 0x49, b[2] == 0x46, b[3] == 0x38 { return "gif" }
        if b[0] == 0x52, b[1] == 0x49, b[2] == 0x46, b[3] == 0x46,
           b[8] == 0x57, b[9] == 0x45, b[10] == 0x42, b[11] == 0x50 { return "webp" }
        return nil
    }
}

/// Read the pasteboard into a staged attachment, if it holds anything we can
/// send. Images first (a screenshot copied out of another app), then any typed
/// item with bytes (a PDF copied out of Files), then plain text saved as .txt.
@MainActor
func stageFromPasteboard() -> StagedAttachment? {
    let board = UIPasteboard.general
    if board.hasImages,
       let image = board.image,
       let data = image.pngData() ?? image.jpegData(compressionQuality: 0.9) {
        return StagedAttachment.stagePhoto(data: data, suggestedName: "pasted-image")
    }
    for identifier in board.types {
        guard let type = UTType(identifier), !type.conforms(to: .text),
              let data = board.data(forPasteboardType: identifier), !data.isEmpty else { continue }
        let ext = type.preferredFilenameExtension ?? "bin"
        return StagedAttachment.stageFile(data: data, name: "pasted.\(ext)")
    }
    if let text = board.string, !text.isEmpty {
        return StagedAttachment.stageFile(data: Data(text.utf8), name: "pasted.txt")
    }
    return nil
}

// MARK: - Upload (to the chat's HOST device)

/// Chunked upload straight to the host device's relay room: base64 the bytes,
/// `UploadChunk {uploadId, seq, data}` per 60k-char slice (positional `seq`
/// makes retries idempotent), then `UploadCommit {uploadId, fileName}` → the
/// durable absolute path on that device.
func uploadAttachmentChunked(relay: DeviceRelayClient, name: String, data: Data) async throws -> String {
    struct OkReply: Decodable { var ok: Bool? }
    struct CommitReply: Decodable { var path: String }

    let b64 = data.base64EncodedString()
    let uploadId = UUID().uuidString.lowercased()
    var start = b64.startIndex
    var seq: UInt64 = 0
    while start < b64.endIndex {
        let end = b64.index(start, offsetBy: uploadChunkB64Chars, limitedBy: b64.endIndex) ?? b64.endIndex
        let params: [String: Any] = ["uploadId": uploadId, "seq": seq, "data": String(b64[start..<end])]
        // One transient blip must not abort a long upload; `seq` slots are
        // idempotent engine-side, so a blind re-send is safe.
        var attempt = 0
        while true {
            do {
                let _: OkReply = try await relay.call(method: "UploadChunk", params: params,
                                                      timeoutSeconds: seq == 0 ? 90 : 30)
                break
            } catch {
                attempt += 1
                if attempt > 2 { throw error }
            }
        }
        start = end
        seq += 1
    }
    // Commit outlasts the engine's assemble + best-effort edge mirror.
    let reply: CommitReply = try await relay.call(
        method: "UploadCommit",
        params: ["uploadId": uploadId, "fileName": name],
        timeoutSeconds: 150)
    return reply.path
}

// MARK: - Transcript image cache

/// Decoded transcript images keyed by `(deviceId, path)`, loaded over the
/// owning device's relay in 45KB base64 chunks, seeded locally after a send so
/// own bubbles never round-trip. Bounded by an encoded-byte LRU budget; failed
/// loads retry on the 2s→15s ladder.
@MainActor
@Observable
final class AttachmentImageCache {
    static let shared = AttachmentImageCache()

    enum Snapshot {
        case loading
        case loaded(name: String, image: UIImage)
        case error
    }

    private struct Key: Hashable {
        let deviceId: String
        let path: String
    }

    private enum Entry {
        case loading(attempts: Int)
        case loaded(name: String, image: UIImage, bytes: Int, lastUsed: UInt64)
        case error(attempts: Int, at: Date)
    }

    private static let budgetBytes = 64 * 1024 * 1024
    private static let maxReadChunks = 1_000

    private var entries: [Key: Entry] = [:]
    @ObservationIgnored private var tick: UInt64 = 0
    @ObservationIgnored private var loadedBytes = 0
    @ObservationIgnored private var config: AppConfig?
    @ObservationIgnored private var relays: [String: DeviceRelayClient] = [:]

    func configure(config: AppConfig) {
        if self.config !== config {
            self.config = config
            relays = [:]
        }
    }

    func snapshot(deviceId: String, path: String) -> Snapshot {
        switch entries[Key(deviceId: deviceId, path: path)] {
        case .loaded(let name, let image, _, _):
            return .loaded(name: name, image: image)
        case .error(let attempts, let at)
            where Date().timeIntervalSince(at) < Self.retryDelay(attempts):
            return .error
        case .error:
            return .loading  // ladder elapsed; the next load() attempt owns it
        case .loading, .none:
            return .loading
        }
    }

    /// Kick a load if this source isn't already loaded/loading (errored
    /// sources retry only after their backoff).
    func load(deviceId: String, path: String) {
        let key = Key(deviceId: deviceId, path: path)
        let attempts: Int
        switch entries[key] {
        case .loaded, .loading:
            return
        case .error(let n, let at):
            guard Date().timeIntervalSince(at) >= Self.retryDelay(n) else { return }
            attempts = n
        case .none:
            attempts = 0
        }
        guard let config else { return }
        entries[key] = .loading(attempts: attempts)
        let relay = relays[deviceId] ?? {
            let client = DeviceRelayClient(deviceId: deviceId, config: config)
            relays[deviceId] = client
            return client
        }()
        Task { @MainActor [weak self] in
            let loaded = await Self.readImage(relay: relay, path: path)
            guard let self else { return }
            if let loaded {
                self.store(key: key, name: loaded.name, image: loaded.image, bytes: loaded.bytes)
            } else {
                self.entries[key] = .error(attempts: attempts + 1, at: Date())
            }
        }
    }

    /// Seed after a successful upload (composer send path) so the just-sent
    /// bubble renders from local bytes instead of a round-trip.
    func seed(deviceId: String, path: String, name: String, data: Data) {
        guard let image = UIImage(data: data) else { return }
        store(key: Key(deviceId: deviceId, path: path), name: name, image: image, bytes: data.count)
    }

    private func store(key: Key, name: String, image: UIImage, bytes: Int) {
        tick += 1
        if case .loaded(_, _, let old, _)? = entries[key] {
            loadedBytes -= old
        }
        entries[key] = .loaded(name: name, image: image, bytes: bytes, lastUsed: tick)
        loadedBytes += bytes
        while loadedBytes > Self.budgetBytes {
            let oldest = entries.compactMap { entry -> (UInt64, Key, Int)? in
                guard entry.key != key,
                      case .loaded(_, _, let bytes, let used) = entry.value else { return nil }
                return (used, entry.key, bytes)
            }.min { $0.0 < $1.0 }
            guard let oldest else { break }
            entries.removeValue(forKey: oldest.1)
            loadedBytes -= oldest.2
        }
    }

    private static func retryDelay(_ attempts: Int) -> TimeInterval {
        min(Double(2 << min(max(attempts - 1, 0), 3)), 15)
    }

    /// `ReadAttachmentChunk` loop: 45KB base64 chunks until `done` (bounded,
    /// with a stuck-offset guard).
    private static func readImage(relay: DeviceRelayClient, path: String)
        async -> (name: String, image: UIImage, bytes: Int)? {
        struct Chunk: Decodable {
            var name: String
            var mimeType: String
            var data: String
            var nextOffset: UInt64
            var done: Bool
        }
        var name = ""
        var b64 = ""
        var offset: UInt64 = 0
        var done = false
        for _ in 0..<maxReadChunks {
            guard let chunk: Chunk = try? await relay.call(
                method: "ReadAttachmentChunk",
                params: ["path": path, "offset": offset],
                timeoutSeconds: 20) else { return nil }
            name = chunk.name
            b64 += chunk.data
            done = chunk.done
            if done { break }
            guard chunk.nextOffset > offset else { return nil }
            offset = chunk.nextOffset
        }
        guard done, let data = Data(base64Encoded: b64), let image = UIImage(data: data) else {
            return nil
        }
        return (name.isEmpty ? attachmentName(fromPath: path) : name, image, data.count)
    }
}

// MARK: - Composer staged strip

/// The staged row inside the composer pill: 56pt thumbs for images, a named
/// chip for everything else, each carrying its own upload state — a failed one
/// is red and tappable, which is the retry.
struct AttachmentStripView: View {
    let attachments: [StagedAttachment]
    let remove: (String) -> Void
    var retry: (String) -> Void = { _ in }

    @State private var preview: AttachmentPreview?

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(alignment: .top, spacing: 8) {
                ForEach(attachments) { att in
                    cell(att)
                        .overlay(alignment: .topTrailing) { removeButton(att) }
                        .padding(.top, 5)
                        .padding(.trailing, 5)
                }
            }
        }
        // One cover for the strip, not one per cell: every cell writes the same
        // `preview` state, and N covers bound to one item is how a tap presents
        // twice.
        .fullScreenCover(item: $preview) { AttachmentLightbox(preview: $0) }
    }

    @ViewBuilder
    private func cell(_ att: StagedAttachment) -> some View {
        Button {
            if att.state.isFailed {
                retry(att.id)
            } else if let image = att.image {
                preview = AttachmentPreview(name: att.name, image: image)
            }
        } label: {
            Group {
                if let image = att.image {
                    Image(uiImage: image)
                        .resizable()
                        .aspectRatio(contentMode: .fill)
                        .frame(width: 56, height: 56)
                        .clipShape(RoundedRectangle(cornerRadius: Theme.radiusRow))
                } else {
                    fileChip(att)
                }
            }
            .overlay { stateOverlay(att) }
            .overlay(RoundedRectangle(cornerRadius: Theme.radiusRow)
                .strokeBorder(att.state.isFailed ? Theme.danger : Theme.border, lineWidth: 1))
        }
        .buttonStyle(.plain)
        .accessibilityLabel(accessibilityLabel(att))
    }

    private func fileChip(_ att: StagedAttachment) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Image(systemName: "doc")
                .font(.system(size: 13, weight: .medium))
                .foregroundStyle(Theme.textMuted)
            Text(att.name)
                .font(Theme.sans(Theme.textCaption, weight: .medium))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
                .truncationMode(.middle)
            Text(att.byteLabel)
                .font(Theme.sans(Theme.textCaption))
                .foregroundStyle(Theme.textFaint)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .frame(width: 108, height: 56, alignment: .leading)
        .background(Theme.chip, in: RoundedRectangle(cornerRadius: Theme.radiusRow))
    }

    @ViewBuilder
    private func stateOverlay(_ att: StagedAttachment) -> some View {
        switch att.state {
        case .uploading:
            ZStack {
                // paint-ok: a scrim over somebody's photo, which is not a
                // surface the canvas has a tone for — it dims the picture.
                Color.black.opacity(0.45)
                ProgressView().controlSize(.small).tint(Theme.text)
            }
            .clipShape(RoundedRectangle(cornerRadius: Theme.radiusRow))
        case .failed:
            ZStack(alignment: .bottomLeading) {
                // paint-ok: the same photo scrim, under the retry glyph.
                Color.black.opacity(0.45)
                Image(systemName: "arrow.clockwise")
                    .font(.system(size: 11, weight: .bold))
                    .foregroundStyle(Theme.dangerText)
                    .padding(6)
            }
            .clipShape(RoundedRectangle(cornerRadius: Theme.radiusRow))
        case .staged, .uploaded:
            EmptyView()
        }
    }

    private func removeButton(_ att: StagedAttachment) -> some View {
        Button {
            remove(att.id)
        } label: {
            Image(systemName: "xmark")
                .font(.system(size: 8, weight: .bold))
                .foregroundStyle(Theme.text)
                .frame(width: 18, height: 18)
                // round-ok: the drawn cap that removes a thumbnail — an 18pt
                // badge sitting ON the picture.
                // paint-ok: it carries its own dark bed for the same reason;
                // no surface tone reads against an arbitrary photo.
                .background(.black.opacity(0.65), in: Circle())
                // round-ok: the same cap's hairline.
                .overlay(Circle().strokeBorder(Theme.borderStrong, lineWidth: 1))
        }
        .buttonStyle(.plain)
        .offset(x: 5, y: -5)
        .accessibilityLabel("Remove \(att.name)")
    }

    private func accessibilityLabel(_ att: StagedAttachment) -> String {
        switch att.state {
        case .failed(let reason):
            return "\(att.name) failed to upload: \(reason). Tap to retry the send."
        case .uploading: return "\(att.name), uploading"
        case .uploaded: return "\(att.name), uploaded"
        case .staged: return "\(att.name), attached"
        }
    }
}

// MARK: - Transcript strip (112×80 thumbs / file chips above the bubble)

struct UserAttachmentsStrip: View {
    let deviceId: String
    let attachments: [UserAttachmentRef]

    var body: some View {
        HStack(spacing: 8) {
            Spacer(minLength: 0)
            ForEach(attachments) { att in
                if att.isImage {
                    AttachmentThumbView(deviceId: deviceId, path: att.path)
                } else {
                    TranscriptFileChip(name: att.name)
                }
            }
        }
        // Fixed height: load-state flips never shift the transcript.
        .frame(height: 80)
        .frame(maxWidth: .infinity, alignment: .trailing)
        .clipped()
    }
}

/// A non-image ref: nothing to thumbnail, and the host serves those bytes only
/// out of its uploads dir — so say what it is instead of spinning on a read.
struct TranscriptFileChip: View {
    let name: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Image(systemName: "doc")
                .font(.system(size: 14))
                .foregroundStyle(Theme.textMuted)
            Text(name)
                .font(Theme.sans(Theme.textCaption, weight: .medium))
                .foregroundStyle(Theme.text)
                .lineLimit(2)
                .truncationMode(.middle)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .frame(width: 112, height: 80, alignment: .topLeading)
        .background(Theme.chip)
        .clipShape(RoundedRectangle(cornerRadius: Theme.radiusRow))
        .overlay(RoundedRectangle(cornerRadius: Theme.radiusRow)
            .strokeBorder(Theme.border, lineWidth: 1))
        .accessibilityLabel("Attached file \(name)")
    }
}

struct AttachmentThumbView: View {
    let deviceId: String
    let path: String

    private let cache = AttachmentImageCache.shared
    @State private var preview: AttachmentPreview?

    var body: some View {
        Group {
            switch cache.snapshot(deviceId: deviceId, path: path) {
            case .loaded(let name, let image):
                Button {
                    preview = AttachmentPreview(name: name, image: image)
                } label: {
                    Image(uiImage: image)
                        .resizable()
                        .aspectRatio(contentMode: .fill)
                        .frame(width: 112, height: 80)
                        .clipped()
                }
                .buttonStyle(.plain)
            case .loading:
                ProgressView()
                    .controlSize(.small)
                    .tint(Theme.textFaint)
                    .frame(width: 112, height: 80)
            case .error:
                // Tap retries once the backoff ladder allows it.
                Button {
                    cache.load(deviceId: deviceId, path: path)
                } label: {
                    Image(systemName: "photo.badge.exclamationmark")
                        .font(.system(size: 16))
                        .foregroundStyle(Theme.textFaint)
                        .frame(width: 112, height: 80)
                }
                .buttonStyle(.plain)
            }
        }
        .background(Theme.chip)
        .clipShape(RoundedRectangle(cornerRadius: Theme.radiusRow))
        .overlay(RoundedRectangle(cornerRadius: Theme.radiusRow)
            .strokeBorder(Theme.border, lineWidth: 1))
        .task(id: "\(deviceId)|\(path)") {
            cache.load(deviceId: deviceId, path: path)
        }
        .fullScreenCover(item: $preview) { AttachmentLightbox(preview: $0) }
    }
}

// MARK: - Lightbox (dim scrim, image ≤85vh/90vw, name under it, any tap closes)

struct AttachmentPreview: Identifiable {
    var id: String { name }
    let name: String
    let image: UIImage
}

struct AttachmentLightbox: View {
    let preview: AttachmentPreview
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        GeometryReader { geo in
            ZStack {
                // paint-ok: the lightbox scrim — a photo shown full-size is
                // shown against black, not against a UI surface.
                Color.black.opacity(0.9).ignoresSafeArea()
                VStack(spacing: 12) {
                    Image(uiImage: preview.image)
                        .resizable()
                        .aspectRatio(contentMode: .fit)
                        .frame(maxWidth: geo.size.width * 0.9, maxHeight: geo.size.height * 0.85)
                        .clipShape(RoundedRectangle(cornerRadius: Theme.radiusChip))
                    Text(preview.name)
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .contentShape(Rectangle())
            .onTapGesture { dismiss() }
        }
        .presentationBackground(.clear)
    }
}
