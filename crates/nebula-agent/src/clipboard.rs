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
//! Nothing in this file touches a real clipboard or a socket. It decides what
//! to send and what to store, and the caller does both, which is what makes
//! the interesting cases — the loop, a stale request, a refused format —
//! testable without a desktop session.

use ndp_proto::{ClipboardDataHeader, ClipboardFormat, ClipboardOffer, ClipboardRequest};

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

    fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[self.format.to_u8()]);
        hasher.update(&self.bytes);
        *hasher.finalize().as_bytes()
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
}

impl<C: ClipboardAccess> ClipboardSync<C> {
    /// Start watching a clipboard.
    ///
    /// `enabled` comes from the session policy. A disabled sync does nothing
    /// in either direction — it is not merely quiet, it also refuses
    /// incoming content, because the entitlement is what decides this and
    /// not the peer.
    pub fn new(clipboard: C, enabled: bool) -> Self {
        Self {
            clipboard,
            known: None,
            generation: None,
            outgoing: None,
            next_offer: 1,
            enabled,
        }
    }

    /// Adopt the current contents without announcing them.
    ///
    /// Called once when a session starts: whatever was already on the
    /// clipboard before anyone connected is not news, and announcing it
    /// would overwrite the other side's clipboard the moment they connect.
    pub fn prime(&mut self) {
        self.generation = self.clipboard.generation();
        if let Ok(Some(contents)) = self.clipboard.read() {
            self.known = Some(contents.digest());
        }
    }

    /// Look for a local change worth announcing.
    pub fn poll(&mut self) -> Option<Action> {
        if !self.enabled {
            return None;
        }

        // The cheap check first, where the platform offers one.
        if let Some(generation) = self.clipboard.generation() {
            if self.generation == Some(generation) {
                return None;
            }
            self.generation = Some(generation);
        }

        let contents = match self.clipboard.read() {
            Ok(Some(contents)) => contents,
            Ok(None) => return None,
            Err(error) => {
                tracing::debug!(%error, "the clipboard could not be read");
                return None;
            }
        };

        let digest = contents.digest();
        if self.known == Some(digest) {
            return None;
        }
        self.known = Some(digest);

        if contents.bytes.len() as u64 > MAX_CONTENTS {
            tracing::debug!(
                bytes = contents.bytes.len(),
                "clipboard contents too large to share"
            );
            return None;
        }

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
        // Recorded before the write, not after: on a platform with a
        // generation counter the write bumps it, and a poll racing in
        // between must still recognise its own content.
        self.known = Some(contents.digest());
        let result = self.clipboard.write(&contents);
        self.generation = self.clipboard.generation();
        result
    }
}

/// The desktop clipboard of the machine this is running on.
///
/// One implementation for all three platforms, because `arboard` already is
/// one and a clipboard is the rare place where the platforms genuinely agree
/// on the model: text or an image, replaced wholesale.
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
}

/// Encode a clipboard image as PNG.
///
/// PNG rather than each platform's native bitmap: it is the one format all
/// three read and write, it is lossless, and pasted images are usually
/// screenshots, which it compresses well.
fn encode_png(image: &arboard::ImageData<'_>) -> anyhow::Result<Vec<u8>> {
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
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info()?;
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
    let (inbound, requests) = std::sync::mpsc::channel::<Inbound>();
    // Bounded: clipboard traffic is human-paced, so a backlog means the
    // session is gone rather than that the peer is slow.
    let (actions, outbound) = tokio::sync::mpsc::channel::<Action>(8);

    std::thread::Builder::new()
        .name("nebula-clipboard".into())
        .spawn(move || {
            let mut sync = ClipboardSync::new(clipboard, enabled);
            // Whatever was already copied is not news; announcing it would
            // overwrite the other side's clipboard on connect.
            sync.prime();

            loop {
                let action = match requests.recv_timeout(POLL) {
                    Ok(Inbound::Offer(offer)) => sync.on_offer(&offer),
                    Ok(Inbound::Request(request)) => sync.on_request(&request),
                    Ok(Inbound::Data(header, bytes)) => {
                        if let Err(error) = sync.on_data(header, &bytes) {
                            tracing::debug!(%error, "clipboard contents could not be applied");
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
