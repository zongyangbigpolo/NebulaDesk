//! Keeping two clipboards in step.
//!
//! Copying is announced, not pushed: the side that copied says what it has,
//! and the other side asks only if it wants it. A screenshot on a clipboard
//! is megabytes and most are never pasted, so pushing eagerly would spend
//! the session's bandwidth on content nobody asked for — and would spend it
//! at exactly the wrong moment, since people copy in the middle of doing
//! something.
//!
//! # Loops
//!
//! Both sides watch their own clipboard and both sides write to it, which is
//! a feedback loop unless something breaks it. What breaks it here is that a
//! write records the digest of what was written: the next poll sees content
//! it already knows about and stays quiet. Without that, one copy would
//! bounce between the two machines for the life of the session.
//!
//! # Testing
//!
//! The state machine accepts an abstract clipboard, keeping loops, stale
//! requests and policy refusals testable without a desktop session. The native
//! adapter and worker below connect those decisions to the system pasteboard.

use ndp_proto::{ClipboardDataHeader, ClipboardFormat, ClipboardOffer, ClipboardRequest};
use std::path::PathBuf;

/// How large a clipboard payload may be before it is refused.
///
/// 64 MB. Generous enough for any screenshot or document anyone pastes,
/// small enough that a peer cannot use the clipboard channel to make the
/// other side allocate without bound.
pub const MAX_CONTENTS: u64 = 64 * 1024 * 1024;

/// One representation of a clipboard's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contents {
    /// What the bytes are.
    pub format: ClipboardFormat,
    /// The contents themselves.
    pub bytes: Vec<u8>,
}

impl Contents {
    /// Plain UTF-8 text.
    #[must_use]
    pub fn text(text: &str) -> Self {
        Self {
            format: ClipboardFormat::Text,
            bytes: text.as_bytes().to_vec(),
        }
    }

    fn digest(&self) -> anyhow::Result<[u8; 32]> {
        anyhow::ensure!(
            self.bytes.len() as u64 <= MAX_CONTENTS,
            "clipboard contents too large to fingerprint"
        );
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[self.format.to_u8()]);
        if self.format == ClipboardFormat::Png {
            // Native clipboards can return TIFF converted back to PNG, or PNG
            // with different compression/metadata. Compare pixels, not encoding.
            let image = decode_png(&self.bytes)?;
            hasher.update(&(image.width as u64).to_le_bytes());
            hasher.update(&(image.height as u64).to_le_bytes());
            hasher.update(&image.bytes);
        } else {
            hasher.update(&self.bytes);
        }
        Ok(*hasher.finalize().as_bytes())
    }
}

/// A clipboard this machine can read and write.
pub trait ClipboardAccess: Send {
    /// Read the current contents, if there are any this can represent.
    fn read(&mut self) -> anyhow::Result<Option<Contents>>;

    /// Replace the contents.
    fn write(&mut self, contents: &Contents) -> anyhow::Result<()>;

    /// A cheap value that changes whenever the clipboard does.
    ///
    /// Platforms that expose one — macOS's `changeCount`, Windows's
    /// sequence number — let a poll skip reading the contents entirely,
    /// which matters when the contents are a 40 MB image. Returning `None`
    /// means every poll reads and compares, which is correct but costs more.
    fn generation(&mut self) -> Option<u64> {
        None
    }

    /// Explicitly copied local file paths, never paths requested by a peer.
    ///
    /// `Some`, including an empty list, identifies a file-copy gesture and
    /// prevents its text/image fallback from being shared as clipboard data.
    fn read_files(&mut self) -> anyhow::Result<Option<Vec<PathBuf>>> {
        Ok(None)
    }
}

/// What one side of the clipboard sync should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Announce that this side's clipboard changed.
    Offer(ClipboardOffer),
    /// Ask for the contents of an offer.
    Request(ClipboardRequest),
    /// Send contents that were asked for.
    Data(ClipboardDataHeader, Vec<u8>),
    /// A local copy gesture, routed to the file worker, never the clipboard wire.
    Files(Vec<PathBuf>),
}

/// Tracks one side of a two-way clipboard sync.
pub struct ClipboardSync<C> {
    clipboard: C,
    /// What this side last saw or wrote, so neither is announced back.
    known: Option<[u8; 32]>,
    /// The generation the last poll read at, when the platform has one.
    generation: Option<u64>,
    /// The offer this side most recently made, and what it was offering.
    outgoing: Option<(u64, Contents)>,
    /// The next offer id to hand out.
    next_offer: u64,
    /// Whether this side may share and accept clipboard content at all.
    enabled: bool,
    files_enabled: bool,
    known_files: Option<Vec<PathBuf>>,
}

impl<C: ClipboardAccess> ClipboardSync<C> {
    /// Start watching a clipboard.
    ///
    /// `enabled` comes from the session policy. A disabled sync does nothing
    /// in either direction — it is not merely quiet, it also refuses
    /// incoming content, because the entitlement is what decides this and
    /// not the peer.
    pub fn new(clipboard: C, enabled: bool) -> Self {
        Self::with_file_transfer(clipboard, enabled, false)
    }

    /// Watch file-copy gestures independently of the text/image permission.
    pub fn with_file_transfer(clipboard: C, enabled: bool, files_enabled: bool) -> Self {
        Self {
            clipboard,
            known: None,
            generation: None,
            outgoing: None,
            next_offer: 1,
            enabled,
            files_enabled,
            known_files: None,
        }
    }

    /// Adopt the current contents without announcing them.
    ///
    /// Called once when a session starts: whatever was already on the
    /// clipboard before anyone connected is not news, and announcing it
    /// would overwrite the other side's clipboard the moment they connect.
    pub fn prime(&mut self) {
        if !self.enabled && !self.files_enabled {
            return;
        }
        self.generation = self.clipboard.generation();
        // A native generation baseline avoids reading pre-session contents at all.
        if self.generation.is_some() {
            return;
        }
        match self.clipboard.read_files() {
            Ok(Some(files)) => {
                self.known_files = Some(files);
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "could not prime clipboard file-copy state");
                return;
            }
            Ok(None) => {}
        }
        if self.enabled {
            if let Ok(Some(contents)) = self.clipboard.read() {
                match contents.digest() {
                    Ok(digest) => self.known = Some(digest),
                    Err(error) => tracing::warn!(%error, "could not prime clipboard contents"),
                }
            }
        }
    }

    /// Look for a local change worth announcing.
    pub fn poll(&mut self) -> Option<Action> {
        if !self.enabled && !self.files_enabled {
            return None;
        }

        // The cheap check first, where the platform offers one.
        let generation = self.clipboard.generation();
        if let Some(generation) = generation {
            if self.generation == Some(generation) {
                return None;
            }
            self.generation = Some(generation);
        }

        match self.clipboard.read_files() {
            Ok(Some(mut files)) => {
                if generation.is_some() && self.clipboard.generation() != generation {
                    return None;
                }
                let mut seen = std::collections::HashSet::new();
                files.retain(|path| seen.insert(path.clone()));
                self.outgoing = None;
                self.known = None;
                let changed = generation.is_some() || self.known_files.as_ref() != Some(&files);
                self.known_files = Some(files.clone());
                return (self.files_enabled && changed && !files.is_empty())
                    .then_some(Action::Files(files));
            }
            Err(error) => {
                self.outgoing = None;
                tracing::warn!(%error, "copied files could not be read");
                return None;
            }
            Ok(None) => self.known_files = None,
        }
        if !self.enabled {
            return None;
        }

        let contents = match self.clipboard.read() {
            Ok(Some(contents)) => contents,
            Ok(None) => {
                self.outgoing = None;
                self.known = None;
                return None;
            }
            Err(error) => {
                self.outgoing = None;
                tracing::warn!(%error, "the clipboard could not be read");
                return None;
            }
        };
        if generation.is_some() && self.clipboard.generation() != generation {
            return None;
        }

        if contents.bytes.len() as u64 > MAX_CONTENTS {
            self.outgoing = None;
            tracing::warn!(
                bytes = contents.bytes.len(),
                "clipboard contents too large to share"
            );
            return None;
        }
        let digest = match contents.digest() {
            Ok(digest) => digest,
            Err(error) => {
                self.outgoing = None;
                tracing::warn!(%error, "clipboard contents could not be fingerprinted");
                return None;
            }
        };
        if self.known == Some(digest) {
            return None;
        }
        self.known = Some(digest);

        let offer_id = self.next_offer;
        self.next_offer += 1;
        let offer = ClipboardOffer {
            offer_id,
            formats: vec![contents.format],
            size_hint: contents.bytes.len() as u64,
        };
        self.outgoing = Some((offer_id, contents));
        Some(Action::Offer(offer))
    }

    /// Decide whether to fetch what the peer is offering.
    pub fn on_offer(&mut self, offer: &ClipboardOffer) -> Option<Action> {
        if !self.enabled {
            return None;
        }
        if offer.size_hint > MAX_CONTENTS {
            tracing::debug!(
                bytes = offer.size_hint,
                "refusing a clipboard offer that is too large"
            );
            return None;
        }
        // Best format first is the sender's ordering, and the first one this
        // side can represent is the one to ask for.
        let format = offer
            .formats
            .iter()
            .copied()
            .find(|f| matches!(f, ClipboardFormat::Text | ClipboardFormat::Png))?;
        Some(Action::Request(ClipboardRequest {
            offer_id: offer.offer_id,
            format,
        }))
    }

    /// Answer a request, if it is for the offer this side still holds.
    pub fn on_request(&mut self, request: &ClipboardRequest) -> Option<Action> {
        if !self.enabled {
            return None;
        }
        let (offer_id, contents) = self.outgoing.as_ref()?;
        // A request for an older offer is answered with nothing rather than
        // with the current clipboard: the peer asked for what they were
        // told about, and sending something else would paste the wrong
        // thing without either side being able to tell.
        if *offer_id != request.offer_id || contents.format != request.format {
            tracing::debug!(
                asked = request.offer_id,
                held = offer_id,
                "ignoring a request for a clipboard offer that has been superseded"
            );
            return None;
        }
        Some(Action::Data(
            ClipboardDataHeader {
                offer_id: *offer_id,
                format: contents.format,
            },
            contents.bytes.clone(),
        ))
    }

    /// Put contents the peer sent onto this machine's clipboard.
    pub fn on_data(&mut self, header: ClipboardDataHeader, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.enabled,
            "this session may not synchronise the clipboard"
        );
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_CONTENTS,
            "the peer sent {} bytes of clipboard content, over the {MAX_CONTENTS} byte limit",
            bytes.len()
        );

        let contents = Contents {
            format: header.format,
            bytes: bytes.to_vec(),
        };
        anyhow::ensure!(
            matches!(
                contents.format,
                ClipboardFormat::Text | ClipboardFormat::Png
            ),
            "unsupported clipboard format"
        );
        tracing::debug!(
            offer_id = header.offer_id,
            format = ?header.format,
            bytes = bytes.len(),
            "clipboard data received from peer"
        );
        let digest = contents.digest()?;
        // A generation read after write may already belong to a new local copy.
        // Read the next poll's actual contents instead of marking that edit seen.
        self.generation = None;
        self.clipboard.write(&contents)?;
        self.known = Some(digest);
        self.known_files = None;
        self.outgoing = None;
        tracing::debug!(
            offer_id = header.offer_id,
            format = ?header.format,
            bytes = bytes.len(),
            "peer clipboard data written successfully"
        );
        Ok(())
    }
}

/// The desktop clipboard of the machine this is running on.
///
/// Text and images use arboard, with native macOS PNG and file-copy support.
pub struct SystemClipboard {
    inner: arboard::Clipboard,
}

impl SystemClipboard {
    /// Open the desktop clipboard.
    pub fn open() -> anyhow::Result<Self> {
        Ok(Self {
            inner: arboard::Clipboard::new()?,
        })
    }
}

impl ClipboardAccess for SystemClipboard {
    fn read(&mut self) -> anyhow::Result<Option<Contents>> {
        // Text first: it is what is on a clipboard nearly all the time, and
        // it is far cheaper to read than an image.
        match self.inner.get_text() {
            Ok(text) => return Ok(Some(Contents::text(&text))),
            Err(arboard::Error::ContentNotAvailable) => {}
            Err(error) => return Err(error.into()),
        }

        #[cfg(target_os = "macos")]
        if let Some(bytes) = macos::png()? {
            return Ok(Some(Contents {
                format: ClipboardFormat::Png,
                bytes,
            }));
        }

        match self.inner.get_image() {
            Ok(image) => Ok(Some(Contents {
                format: ClipboardFormat::Png,
                bytes: encode_png(&image)?,
            })),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write(&mut self, contents: &Contents) -> anyhow::Result<()> {
        match contents.format {
            ClipboardFormat::Text | ClipboardFormat::Html => {
                let text = std::str::from_utf8(&contents.bytes)?;
                self.inner.set_text(text)?;
            }
            ClipboardFormat::Png => {
                self.inner.set_image(decode_png(&contents.bytes)?)?;
            }
            ClipboardFormat::FileList => {
                anyhow::bail!("file lists are transferred as files, not pasted")
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn generation(&mut self) -> Option<u64> {
        Some(macos::change_count())
    }

    #[cfg(target_os = "macos")]
    fn read_files(&mut self) -> anyhow::Result<Option<Vec<PathBuf>>> {
        macos::files()
    }
}

/// Encode a clipboard image as PNG.
///
/// PNG rather than each platform's native bitmap: it is the one format all
/// three read and write, it is lossless, and pasted images are usually
/// screenshots, which it compresses well.
fn encode_png(image: &arboard::ImageData<'_>) -> anyhow::Result<Vec<u8>> {
    let pixels = image.width.checked_mul(image.height);
    anyhow::ensure!(
        pixels.is_some_and(|pixels| pixels > 0 && pixels <= MAX_CONTENTS as usize / 4),
        "clipboard image is too large or empty"
    );
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width as u32, image.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&image.bytes)?;
    writer.finish()?;
    Ok(out)
}

/// Decode a PNG into the RGBA a clipboard wants.
fn decode_png(bytes: &[u8]) -> anyhow::Result<arboard::ImageData<'static>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.set_limits(png::Limits {
        bytes: MAX_CONTENTS as usize,
    });
    let mut reader = decoder.read_info()?;
    let pixels = u64::from(reader.info().width) * u64::from(reader.info().height);
    anyhow::ensure!(pixels <= MAX_CONTENTS / 4, "clipboard image is too large");
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer)?;
    buffer.truncate(info.buffer_size());

    // Clipboards want straight RGBA. Anything else is converted here rather
    // than handed over in a shape the platform will misread as garbage.
    let rgba = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 0xff])
            .collect(),
        png::ColorType::Grayscale => buffer.iter().flat_map(|&g| [g, g, g, 0xff]).collect(),
        png::ColorType::GrayscaleAlpha => buffer
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => {
            anyhow::bail!("indexed PNGs are expanded by the decoder, so this cannot happen")
        }
    };

    Ok(arboard::ImageData {
        width: info.width as usize,
        height: info.height as usize,
        bytes: rgba.into(),
    })
}

#[cfg(target_os = "macos")]
mod macos {
    //! `NSPasteboard.changeCount` — the cheap "did anything change" answer.
    //!
    //! Without it a poll would read the whole clipboard several times a
    //! second, which for a pasted screenshot means decoding megabytes for
    //! nothing. With it, an unchanged clipboard costs one message send.

    use objc2_app_kit::NSPasteboard;

    /// The current pasteboard generation.
    #[must_use]
    pub fn change_count() -> u64 {
        // Monotonic per boot, and bumped by any write from any process, so
        // it is exactly the "has anything happened" signal a poll wants.
        NSPasteboard::generalPasteboard().changeCount() as u64
    }

    pub fn files() -> anyhow::Result<Option<Vec<std::path::PathBuf>>> {
        let board = NSPasteboard::generalPasteboard();
        let has_files = board.types().is_some_and(|types| {
            types.iter().any(|kind| {
                // SAFETY: AppKit's immutable pasteboard type constant.
                &*kind == unsafe { objc2_app_kit::NSPasteboardTypeFileURL }
            })
        });
        if !has_files {
            return Ok(None);
        }
        let mut paths = Vec::new();
        if let Some(items) = board.pasteboardItems() {
            for item in items {
                if let Some(value) =
                    item.stringForType(unsafe { objc2_app_kit::NSPasteboardTypeFileURL })
                {
                    // Unlike arboard's NSURL.path extraction, preserve the URL's
                    // host until validation: file://other-host must never become /local/path.
                    match super::local_file_url(&value.to_string()) {
                        Ok(path) => paths.push(path),
                        Err(error) => tracing::warn!(%error, "ignoring an invalid copied file URL"),
                    }
                }
            }
        }
        Ok(Some(paths))
    }

    pub fn png() -> anyhow::Result<Option<Vec<u8>>> {
        // arboard's macOS reader accepts TIFF only; applications can publish PNG alone.
        let data = NSPasteboard::generalPasteboard()
            .dataForType(unsafe { objc2_app_kit::NSPasteboardTypePNG });
        data.map(|data| {
            anyhow::ensure!(
                data.len() as u64 <= super::MAX_CONTENTS,
                "clipboard PNG is too large"
            );
            Ok(data.to_vec())
        })
        .transpose()
    }
}

#[cfg(any(target_os = "macos", test))]
fn local_file_url(value: &str) -> anyhow::Result<PathBuf> {
    let url = reqwest::Url::parse(value)?;
    anyhow::ensure!(
        url.scheme() == "file"
            && url.host_str().is_none_or(|host| host == "localhost")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "copied file URL must identify a local file"
    );
    url.to_file_path()
        .map_err(|()| anyhow::anyhow!("copied file URL is not a local path"))
}

/// A clipboard held in memory.
///
/// Not only for tests: a machine with no desktop session — a headless
/// server, a login screen — has no clipboard to open, and sharing one that
/// exists only inside the session is better than failing to start.
#[derive(Clone, Default)]
pub struct MemoryClipboard {
    contents: std::sync::Arc<std::sync::Mutex<Option<Contents>>>,
}

impl MemoryClipboard {
    /// Read what is on it, from outside the sync.
    #[must_use]
    pub fn peek(&self) -> Option<Contents> {
        self.contents.lock().ok()?.clone()
    }

    /// Put something on it, as though a user had copied.
    pub fn put(&self, contents: Contents) {
        if let Ok(mut slot) = self.contents.lock() {
            *slot = Some(contents);
        }
    }
}

impl ClipboardAccess for Box<dyn ClipboardAccess> {
    fn read(&mut self) -> anyhow::Result<Option<Contents>> {
        (**self).read()
    }

    fn write(&mut self, contents: &Contents) -> anyhow::Result<()> {
        (**self).write(contents)
    }

    fn generation(&mut self) -> Option<u64> {
        (**self).generation()
    }

    fn read_files(&mut self) -> anyhow::Result<Option<Vec<PathBuf>>> {
        (**self).read_files()
    }
}

impl ClipboardAccess for MemoryClipboard {
    fn read(&mut self) -> anyhow::Result<Option<Contents>> {
        Ok(self.peek())
    }

    fn write(&mut self, contents: &Contents) -> anyhow::Result<()> {
        self.put(contents.clone());
        Ok(())
    }
}

/// How often a clipboard is checked for changes.
///
/// Fast enough that copy-then-paste feels immediate, slow enough that the
/// check costs nothing on a platform without a generation counter — and on
/// macOS, where there is one, an unchanged clipboard costs a single message
/// send per tick.
pub const POLL: std::time::Duration = std::time::Duration::from_millis(300);

/// Something the peer sent that the clipboard sync should act on.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// The peer's clipboard changed.
    Offer(ClipboardOffer),
    /// The peer wants contents this side offered.
    Request(ClipboardRequest),
    /// Contents this side asked for.
    Data(ClipboardDataHeader, Vec<u8>),
}

/// A running clipboard sync, on its own thread.
pub struct Worker {
    inbound: std::sync::mpsc::Sender<Inbound>,
    /// What to send to the peer.
    pub outbound: tokio::sync::mpsc::Receiver<Action>,
}

impl Worker {
    /// Hand the sync something the peer sent.
    ///
    /// Dropping on a closed channel rather than failing: a dead clipboard
    /// thread is a session without clipboard sync, not a session over.
    pub fn deliver(&self, message: Inbound) {
        let _ = self.inbound.send(message);
    }
}

/// Start a clipboard sync on a thread of its own.
///
/// A thread rather than a task because every clipboard API on every platform
/// is blocking, and reading a pasted screenshot can take tens of
/// milliseconds. On an async runtime that is not a slow clipboard, it is a
/// stalled session: the same executor is carrying the video.
pub fn spawn<C: ClipboardAccess + 'static>(clipboard: C, enabled: bool) -> Worker {
    spawn_with_files(clipboard, enabled, false)
}

/// Start the shared pasteboard watcher with independent clipboard/file policy.
pub fn spawn_with_files<C: ClipboardAccess + 'static>(
    clipboard: C,
    enabled: bool,
    files_enabled: bool,
) -> Worker {
    let (inbound, requests) = std::sync::mpsc::channel::<Inbound>();
    // Bounded: clipboard traffic is human-paced, so a backlog means the
    // session is gone rather than that the peer is slow.
    let (actions, outbound) = tokio::sync::mpsc::channel::<Action>(8);

    std::thread::Builder::new()
        .name("nebula-clipboard".into())
        .spawn(move || {
            let mut sync = ClipboardSync::with_file_transfer(clipboard, enabled, files_enabled);
            // Whatever was already copied is not news; announcing it would
            // overwrite the other side's clipboard on connect.
            sync.prime();

            loop {
                let action = match requests.recv_timeout(POLL) {
                    Ok(Inbound::Offer(offer)) => sync.on_offer(&offer),
                    Ok(Inbound::Request(request)) => sync.on_request(&request),
                    Ok(Inbound::Data(header, bytes)) => {
                        if let Err(error) = sync.on_data(header, &bytes) {
                            tracing::warn!(%error, "clipboard contents could not be applied");
                        }
                        None
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => sync.poll(),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                };

                if let Some(action) = action {
                    if actions.blocking_send(action).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawning a thread should not fail");

    Worker { inbound, outbound }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct NativeState {
        generation: u64,
        contents: Option<Contents>,
        files: Option<Vec<PathBuf>>,
        generation_reads: usize,
        text_reads: usize,
        file_reads: usize,
    }

    #[derive(Clone, Default)]
    struct NativeFake(Arc<Mutex<NativeState>>);

    impl NativeFake {
        fn copy(&self, contents: Option<Contents>, files: Option<Vec<PathBuf>>) {
            let mut state = self.0.lock().unwrap();
            state.generation += 1;
            state.contents = contents;
            state.files = files;
        }
    }

    impl ClipboardAccess for NativeFake {
        fn read(&mut self) -> anyhow::Result<Option<Contents>> {
            let mut state = self.0.lock().unwrap();
            state.text_reads += 1;
            Ok(state.contents.clone())
        }

        fn read_files(&mut self) -> anyhow::Result<Option<Vec<PathBuf>>> {
            let mut state = self.0.lock().unwrap();
            state.file_reads += 1;
            Ok(state.files.clone())
        }

        fn generation(&mut self) -> Option<u64> {
            let mut state = self.0.lock().unwrap();
            state.generation_reads += 1;
            Some(state.generation)
        }

        fn write(&mut self, contents: &Contents) -> anyhow::Result<()> {
            self.copy(Some(contents.clone()), None);
            Ok(())
        }
    }

    struct WriteThenCopy {
        board: NativeFake,
        replacement: Option<Contents>,
        files: Option<Vec<PathBuf>>,
        after_write: bool,
    }

    impl ClipboardAccess for WriteThenCopy {
        fn read(&mut self) -> anyhow::Result<Option<Contents>> {
            self.board.read()
        }

        fn read_files(&mut self) -> anyhow::Result<Option<Vec<PathBuf>>> {
            self.board.read_files()
        }

        fn write(&mut self, contents: &Contents) -> anyhow::Result<()> {
            let mut native = contents.clone();
            if native.format == ClipboardFormat::Png {
                native.bytes = encode_png(&decode_png(&native.bytes)?)?;
            }
            self.board.write(&native)?;
            self.after_write = true;
            Ok(())
        }

        fn generation(&mut self) -> Option<u64> {
            if self.after_write {
                self.after_write = false;
                if self.replacement.is_some() || self.files.is_some() {
                    self.board
                        .copy(self.replacement.clone(), self.files.clone());
                }
            }
            self.board.generation()
        }
    }

    fn palette_png() -> Contents {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(vec![255, 0, 0]);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0]).unwrap();
        writer.finish().unwrap();
        Contents {
            format: ClipboardFormat::Png,
            bytes,
        }
    }

    #[test]
    fn a_local_copy_between_remote_write_and_generation_query_is_not_consumed() {
        for replacement in [Contents::text("immediate reverse copy"), palette_png()] {
            let board = NativeFake::default();
            let clipboard = WriteThenCopy {
                board: board.clone(),
                replacement: Some(replacement.clone()),
                files: None,
                after_write: false,
            };
            let mut sync = ClipboardSync::with_file_transfer(clipboard, true, true);
            sync.prime();
            let generations_before = board.0.lock().unwrap().generation_reads;
            sync.on_data(
                ClipboardDataHeader {
                    offer_id: 7,
                    format: ClipboardFormat::Text,
                },
                b"peer copy",
            )
            .unwrap();
            assert_eq!(
                board.0.lock().unwrap().generation_reads,
                generations_before,
                "a post-write generation could already belong to a new local copy"
            );
            let Some(Action::Offer(offer)) = sync.poll() else {
                panic!("immediate copy was lost")
            };
            assert_eq!(offer.formats, vec![replacement.format]);
            let Some(Action::Data(_, bytes)) = sync.on_request(&ClipboardRequest {
                offer_id: offer.offer_id,
                format: replacement.format,
            }) else {
                panic!("the new local contents must be available")
            };
            assert_eq!(bytes, replacement.bytes);
            assert_eq!(sync.poll(), None);
        }
    }

    #[test]
    fn a_file_copy_immediately_after_a_remote_write_remains_a_local_gesture() {
        let paths = vec![PathBuf::from("/local/explicitly-copied.txt")];
        let clipboard = WriteThenCopy {
            board: NativeFake::default(),
            replacement: None,
            files: Some(paths.clone()),
            after_write: false,
        };
        let mut sync = ClipboardSync::with_file_transfer(clipboard, true, true);
        sync.prime();
        sync.on_data(
            ClipboardDataHeader {
                offer_id: 7,
                format: ClipboardFormat::Text,
            },
            b"peer copy",
        )
        .unwrap();
        assert_eq!(sync.poll(), Some(Action::Files(paths)));
        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn reencoded_native_png_is_read_back_without_echoing() {
        let original = palette_png();
        let board = NativeFake::default();
        let clipboard = WriteThenCopy {
            board: board.clone(),
            replacement: None,
            files: None,
            after_write: false,
        };
        let mut sync = ClipboardSync::with_file_transfer(clipboard, true, true);
        sync.prime();
        sync.on_data(
            ClipboardDataHeader {
                offer_id: 7,
                format: ClipboardFormat::Png,
            },
            &original.bytes,
        )
        .unwrap();
        let native = board.0.lock().unwrap().contents.clone().unwrap();
        assert_ne!(
            native.bytes, original.bytes,
            "native image representation changed"
        );
        assert_eq!(native.digest().unwrap(), original.digest().unwrap());
        assert_eq!(sync.poll(), None, "identical pixels must not be sent back");
        assert_eq!(
            board.0.lock().unwrap().text_reads,
            1,
            "the next poll must read back"
        );
        assert_eq!(sync.poll(), None);
        assert_eq!(
            board.0.lock().unwrap().text_reads,
            1,
            "unchanged polls remain cheap"
        );
    }

    #[test]
    fn png_fingerprints_include_dimensions_and_reject_malformed_images() {
        let pixels = [255, 0, 0, 255, 0, 255, 0, 255];
        let image = |width, height| Contents {
            format: ClipboardFormat::Png,
            bytes: encode_png(&arboard::ImageData {
                width,
                height,
                bytes: std::borrow::Cow::Borrowed(&pixels),
            })
            .unwrap(),
        };
        assert_ne!(image(1, 2).digest().unwrap(), image(2, 1).digest().unwrap());
        assert!(Contents {
            format: ClipboardFormat::Png,
            bytes: vec![1, 2, 3]
        }
        .digest()
        .is_err());
        let board = NativeFake::default();
        let mut sync = ClipboardSync::new(board.clone(), true);
        sync.prime();
        assert!(sync
            .on_data(
                ClipboardDataHeader {
                    offer_id: 7,
                    format: ClipboardFormat::Png,
                },
                &[1, 2, 3]
            )
            .is_err());
        assert!(board.0.lock().unwrap().contents.is_none());
    }

    #[test]
    fn native_prime_neither_reads_nor_sends_preexisting_text_images_or_files() {
        for contents in [
            Contents::text("private"),
            Contents {
                format: ClipboardFormat::Png,
                bytes: vec![1, 2, 3],
            },
        ] {
            let board = NativeFake::default();
            board.copy(Some(contents), Some(vec!["/private/local.txt".into()]));
            let mut sync = ClipboardSync::with_file_transfer(board.clone(), true, true);
            sync.prime();
            assert_eq!(sync.poll(), None);
            let state = board.0.lock().unwrap();
            assert_eq!(state.text_reads, 0);
            assert_eq!(state.file_reads, 0);
        }
    }

    #[test]
    fn native_files_have_independent_policy_and_never_downgrade_to_text() {
        for (clipboard, files) in [(false, false), (false, true), (true, false), (true, true)] {
            let board = NativeFake::default();
            let mut sync = ClipboardSync::with_file_transfer(board.clone(), clipboard, files);
            sync.prime();
            let paths = vec![PathBuf::from("/local/copied.txt")];
            board.copy(Some(Contents::text("copied.txt")), Some(paths.clone()));
            assert_eq!(sync.poll(), files.then_some(Action::Files(paths)));
            assert_eq!(sync.poll(), None);
            assert_eq!(board.0.lock().unwrap().text_reads, 0);
            if !clipboard && !files {
                assert_eq!(board.0.lock().unwrap().file_reads, 0);
            }
            assert!(sync
                .on_request(&ClipboardRequest {
                    offer_id: 1,
                    format: ClipboardFormat::Text,
                })
                .is_none());
        }
    }

    #[test]
    fn empty_multiple_and_repeated_file_copy_gestures_are_distinct() {
        let board = NativeFake::default();
        let mut sync = ClipboardSync::with_file_transfer(board.clone(), true, true);
        sync.prime();
        board.copy(Some(Contents::text("not fallback")), Some(vec![]));
        assert_eq!(sync.poll(), None);
        let paths = vec![PathBuf::from("/local/a"), PathBuf::from("/local/b")];
        for _ in 0..2 {
            let mut duplicated = paths.clone();
            duplicated.push(paths[0].clone());
            board.copy(None, Some(duplicated));
            assert_eq!(sync.poll(), Some(Action::Files(paths.clone())));
            assert_eq!(sync.poll(), None, "one generation must not send twice");
        }
    }

    #[test]
    fn native_remote_writes_do_not_echo_or_authorize_file_reads() {
        let board = NativeFake::default();
        let mut sync = ClipboardSync::with_file_transfer(board.clone(), true, true);
        sync.prime();
        let header = ClipboardDataHeader {
            offer_id: 3,
            format: ClipboardFormat::Text,
        };
        sync.on_data(header, b"file:///private/local.txt").unwrap();
        assert_eq!(sync.poll(), None);
        // Inspect the native types once without interpreting text as a file URL.
        assert_eq!(board.0.lock().unwrap().file_reads, 1);
        assert!(board.0.lock().unwrap().files.is_none());
        assert!(sync
            .on_data(
                ClipboardDataHeader {
                    offer_id: 4,
                    format: ClipboardFormat::FileList
                },
                b"/private/local.txt",
            )
            .is_err());
        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn copying_files_invalidates_an_outstanding_text_offer() {
        let board = NativeFake::default();
        let mut sync = ClipboardSync::with_file_transfer(board.clone(), true, false);
        sync.prime();
        board.copy(Some(Contents::text("old")), None);
        let Some(Action::Offer(offer)) = sync.poll() else {
            panic!("expected text offer")
        };
        board.copy(
            Some(Contents::text("file name")),
            Some(vec!["/local/file".into()]),
        );
        assert_eq!(sync.poll(), None);
        assert!(sync
            .on_request(&ClipboardRequest {
                offer_id: offer.offer_id,
                format: ClipboardFormat::Text,
            })
            .is_none());
    }

    #[test]
    fn boxed_native_clipboards_forward_file_copy_gestures() {
        let board = NativeFake::default();
        let boxed: Box<dyn ClipboardAccess> = Box::new(board.clone());
        let mut sync = ClipboardSync::with_file_transfer(boxed, false, true);
        sync.prime();
        board.copy(None, Some(vec!["/local/file".into()]));
        assert_eq!(sync.poll(), Some(Action::Files(vec!["/local/file".into()])));
    }

    #[test]
    fn copied_file_urls_preserve_unicode_and_reject_remote_or_nonfile_paths() {
        assert_eq!(
            local_file_url("file:///Users/local/a%20b-%E4%B8%AD.txt").unwrap(),
            PathBuf::from("/Users/local/a b-中.txt")
        );
        for value in [
            "file://other-host/Users/local/private.txt",
            "https://example.invalid/file.txt",
            "/Users/local/private.txt",
            "file:///Users/local/private.txt?query",
            "file:///Users/local/private.txt#fragment",
        ] {
            assert!(
                local_file_url(value).is_err(),
                "{value} is not a local file URL"
            );
        }
    }

    #[test]
    fn native_text_and_png_round_trips_do_not_echo() {
        let bytes = encode_png(&arboard::ImageData {
            width: 1,
            height: 1,
            bytes: std::borrow::Cow::Borrowed(&[20, 40, 60, 255]),
        })
        .unwrap();
        for contents in [
            Contents::text("two-way 中 🦀"),
            Contents {
                format: ClipboardFormat::Png,
                bytes,
            },
        ] {
            let left_board = NativeFake::default();
            let right_board = NativeFake::default();
            let mut left = ClipboardSync::with_file_transfer(left_board.clone(), true, true);
            let mut right = ClipboardSync::with_file_transfer(right_board.clone(), true, true);
            left.prime();
            right.prime();
            left_board.copy(Some(contents.clone()), None);
            let Some(Action::Offer(offer)) = left.poll() else {
                panic!("missing offer")
            };
            let Some(Action::Request(request)) = right.on_offer(&offer) else {
                panic!("missing request")
            };
            let Some(Action::Data(header, bytes)) = left.on_request(&request) else {
                panic!("missing data")
            };
            right.on_data(header, &bytes).unwrap();
            assert_eq!(
                right_board.0.lock().unwrap().contents,
                Some(contents.clone())
            );
            assert_eq!(left.poll(), None);
            assert_eq!(right.poll(), None);
            // Copy a different payload in the reverse direction.
            let reverse = if contents.format == ClipboardFormat::Png {
                Contents {
                    format: ClipboardFormat::Png,
                    bytes: encode_png(&arboard::ImageData {
                        width: 1,
                        height: 1,
                        bytes: std::borrow::Cow::Borrowed(&[100, 80, 60, 255]),
                    })
                    .unwrap(),
                }
            } else {
                Contents::text("reverse direction")
            };
            right_board.copy(Some(reverse.clone()), None);
            let Some(Action::Offer(offer)) = right.poll() else {
                panic!("missing reverse offer")
            };
            let Some(Action::Request(request)) = left.on_offer(&offer) else {
                panic!("missing reverse request")
            };
            let Some(Action::Data(header, bytes)) = right.on_request(&request) else {
                panic!("missing reverse data")
            };
            left.on_data(header, &bytes).unwrap();
            assert_eq!(left_board.0.lock().unwrap().contents, Some(reverse));
            assert_eq!(left.poll(), None);
            assert_eq!(right.poll(), None);
        }
    }

    #[test]
    fn png_decode_expands_palette_and_strips_sixteen_bit_samples() {
        for (color, depth, pixels) in [
            (png::ColorType::Indexed, png::BitDepth::Eight, vec![0]),
            (
                png::ColorType::Rgb,
                png::BitDepth::Sixteen,
                vec![255, 255, 0, 0, 0, 0],
            ),
        ] {
            let mut bytes = Vec::new();
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(color);
            encoder.set_depth(depth);
            if color == png::ColorType::Indexed {
                encoder.set_palette(vec![255, 0, 0]);
            }
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&pixels).unwrap();
            writer.finish().unwrap();
            let image = decode_png(&bytes).unwrap();
            assert_eq!(image.bytes.as_ref(), &[255, 0, 0, 255]);
        }
    }

    #[tokio::test]
    async fn copied_files_feed_real_workers_in_both_directions_with_exact_hashes() {
        use crate::files;
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left_board = NativeFake::default();
        let right_board = NativeFake::default();
        let mut left_sync = ClipboardSync::with_file_transfer(left_board.clone(), false, true);
        let mut right_sync = ClipboardSync::with_file_transfer(right_board.clone(), false, true);
        left_sync.prime();
        right_sync.prime();
        let mut left = files::spawn(left_dir.path().into(), true);
        let mut right = files::spawn(right_dir.path().into(), true);
        let mut expected = Vec::new();

        for (source, target, board, sync, worker, prefix) in [
            (
                left_dir.path(),
                right_dir.path(),
                &left_board,
                &mut left_sync,
                &left,
                "left",
            ),
            (
                right_dir.path(),
                left_dir.path(),
                &right_board,
                &mut right_sync,
                &right,
                "right",
            ),
        ] {
            let mut paths = Vec::new();
            // More than MAX_ACTIVE, including an empty file and a file beyond WINDOW.
            for i in 0..6 {
                let bytes = match i {
                    0 => vec![],
                    1 => vec![42; files::WINDOW as usize + 17],
                    _ => vec![i as u8; files::CHUNK + i],
                };
                let name = format!("{prefix}-{i}.bin");
                let path = source.join(&name);
                std::fs::write(&path, &bytes).unwrap();
                paths.push(path);
                expected.push((target.join(name), blake3::hash(&bytes)));
            }
            board.copy(None, Some(paths));
            let Some(Action::Files(paths)) = sync.poll() else {
                panic!("missing local gesture")
            };
            for path in paths {
                worker.deliver(files::Inbound::Send(path));
            }
            assert_eq!(sync.poll(), None);
        }

        fn relay(action: files::Action, worker: &files::Worker) {
            worker.deliver(match action {
                files::Action::Offer(offer) => files::Inbound::Offer(offer),
                files::Action::Chunk(header, bytes) => files::Inbound::Chunk(header, bytes),
                files::Action::Ack(ack) => files::Inbound::Ack(ack),
            });
        }

        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                tokio::select! {
                    action = left.outbound.recv() => relay(action.unwrap(), &right),
                    action = right.outbound.recv() => relay(action.unwrap(), &left),
                }
                if expected.iter().all(|(path, _)| path.is_file()) {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!(
                "bidirectional queued file copies should finish: {error}; missing {:?}",
                expected
                    .iter()
                    .filter(|(path, _)| !path.is_file())
                    .map(|(path, _)| path)
                    .collect::<Vec<_>>()
            );
        });
        for (path, expected_hash) in expected {
            assert_eq!(blake3::hash(&std::fs::read(path).unwrap()), expected_hash);
        }
        // File arrivals never write file URLs back to either clipboard.
        assert_eq!(left_sync.poll(), None);
        assert_eq!(right_sync.poll(), None);
    }

    #[tokio::test]
    async fn clipboard_worker_skips_existing_files_and_emits_the_next_local_copy_once() {
        let board = NativeFake::default();
        let paths = vec![PathBuf::from("/local/copied.txt")];
        board.copy(None, Some(paths.clone()));
        let mut worker = spawn_with_files(board.clone(), false, true);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while board.0.lock().unwrap().generation_reads == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(tokio::time::timeout(POLL * 2, worker.outbound.recv())
            .await
            .is_err());
        // A new Finder Copy of the very same path is a new explicit gesture.
        board.copy(None, Some(paths.clone()));
        let action = tokio::time::timeout(POLL * 4, worker.outbound.recv())
            .await
            .unwrap();
        assert_eq!(action, Some(Action::Files(paths)));
        assert!(tokio::time::timeout(POLL * 2, worker.outbound.recv())
            .await
            .is_err());
    }

    /// A clipboard in memory, standing in for a desktop's.
    #[derive(Clone, Default)]
    struct Fake {
        contents: Arc<Mutex<Option<Contents>>>,
    }

    impl Fake {
        fn set(&self, contents: Contents) {
            *self.contents.lock().unwrap() = Some(contents);
        }
    }

    impl ClipboardAccess for Fake {
        fn read(&mut self) -> anyhow::Result<Option<Contents>> {
            Ok(self.contents.lock().unwrap().clone())
        }

        fn write(&mut self, contents: &Contents) -> anyhow::Result<()> {
            *self.contents.lock().unwrap() = Some(contents.clone());
            Ok(())
        }
    }

    #[test]
    fn a_copy_is_announced_once() {
        let board = Fake::default();
        let mut sync = ClipboardSync::new(board.clone(), true);
        sync.prime();

        board.set(Contents::text("hello"));
        let Some(Action::Offer(offer)) = sync.poll() else {
            panic!("a copy should be announced")
        };
        assert_eq!(offer.formats, vec![ClipboardFormat::Text]);
        assert_eq!(offer.size_hint, 5);

        assert_eq!(sync.poll(), None, "the same contents are not news twice");
    }

    #[test]
    fn what_was_already_on_the_clipboard_is_not_news() {
        let board = Fake::default();
        board.set(Contents::text("copied before anyone connected"));

        let mut sync = ClipboardSync::new(board, true);
        sync.prime();
        assert_eq!(sync.poll(), None);
    }

    #[test]
    fn a_full_round_trip_moves_the_contents_and_then_stops() {
        let left_board = Fake::default();
        let right_board = Fake::default();
        let mut left = ClipboardSync::new(left_board.clone(), true);
        let mut right = ClipboardSync::new(right_board.clone(), true);
        left.prime();
        right.prime();

        left_board.set(Contents::text("shared"));
        let Some(Action::Offer(offer)) = left.poll() else {
            panic!("the copy should be announced")
        };
        let Some(Action::Request(request)) = right.on_offer(&offer) else {
            panic!("the other side should want it")
        };
        let Some(Action::Data(header, bytes)) = left.on_request(&request) else {
            panic!("the request should be answered")
        };
        right.on_data(header, &bytes).unwrap();

        assert_eq!(
            right_board.contents.lock().unwrap().clone(),
            Some(Contents::text("shared"))
        );

        // This is the loop that would otherwise run forever: the side that
        // was written to must not announce what it was just given.
        assert_eq!(
            right.poll(),
            None,
            "content that arrived from the peer must not be sent back"
        );
        assert_eq!(left.poll(), None);
    }

    #[test]
    fn a_superseded_request_is_answered_with_nothing() {
        let board = Fake::default();
        let mut sync = ClipboardSync::new(board.clone(), true);
        sync.prime();

        board.set(Contents::text("first"));
        let Some(Action::Offer(first)) = sync.poll() else {
            panic!()
        };
        board.set(Contents::text("second"));
        let Some(Action::Offer(second)) = sync.poll() else {
            panic!()
        };
        assert_ne!(first.offer_id, second.offer_id);

        // Answering this with "second" would paste something the peer never
        // asked for and cannot tell apart from what they did ask for.
        assert_eq!(
            sync.on_request(&ClipboardRequest {
                offer_id: first.offer_id,
                format: ClipboardFormat::Text,
            }),
            None
        );
    }

    #[test]
    fn a_session_without_the_entitlement_shares_nothing_in_either_direction() {
        let board = Fake::default();
        let mut sync = ClipboardSync::new(board.clone(), false);
        sync.prime();

        board.set(Contents::text("secret"));
        assert_eq!(sync.poll(), None, "nothing leaves");

        assert_eq!(
            sync.on_offer(&ClipboardOffer {
                offer_id: 1,
                formats: vec![ClipboardFormat::Text],
                size_hint: 4,
            }),
            None,
            "nothing is asked for"
        );

        // And a peer that sends data unasked is refused, because the policy
        // is enforced here rather than trusted to be respected there.
        assert!(sync
            .on_data(
                ClipboardDataHeader {
                    offer_id: 1,
                    format: ClipboardFormat::Text,
                },
                b"unwanted",
            )
            .is_err());
    }

    #[test]
    fn oversized_content_is_refused_at_both_ends() {
        let board = Fake::default();
        let mut sync = ClipboardSync::new(board, true);
        sync.prime();

        assert_eq!(
            sync.on_offer(&ClipboardOffer {
                offer_id: 1,
                formats: vec![ClipboardFormat::Text],
                size_hint: MAX_CONTENTS + 1,
            }),
            None
        );

        assert!(sync
            .on_data(
                ClipboardDataHeader {
                    offer_id: 1,
                    format: ClipboardFormat::Text,
                },
                &vec![0u8; MAX_CONTENTS as usize + 1],
            )
            .is_err());
    }

    #[test]
    fn a_format_this_side_cannot_represent_is_not_requested() {
        let board = Fake::default();
        let mut sync = ClipboardSync::new(board, true);
        sync.prime();

        assert_eq!(
            sync.on_offer(&ClipboardOffer {
                offer_id: 1,
                formats: vec![ClipboardFormat::FileList],
                size_hint: 10,
            }),
            None,
            "file lists are a file transfer, not a clipboard paste"
        );
    }
}
