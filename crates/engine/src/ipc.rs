//! The wire between the host and a renderer process (ADR 016).
//!
//! Length-prefixed binary frames, written and read by hand. The decoder is
//! the zero-trust boundary of the whole process model — the host reads what
//! a possibly compromised renderer wrote — so every length is checked
//! against what remains *and* against a cap before anything is allocated,
//! every enum tag is checked, every geometry value must be finite, and a
//! frame must be consumed exactly. A malformed message is an error; the
//! host then treats the renderer as crashed and never guesses.
//!
//! Fuzzed by the `ipc_decode` target (see [`fuzz_decode`]).

use crate::{EngineEvent, EngineInput, FrameStats, ImageLoading, LayoutWork, LoadRequest, PageCommand, PageId, Viewport};
use network::{Credentials, FetchResponse, NetworkError};
use rendering::display_list::GlyphInstance;
use rendering::{Damage, DisplayItem, ImageSampling};
use std::io::{Read, Write};

/// The largest message either side accepts (an image's pixels are the
/// largest payload; see [`MAX_IMAGE_BYTES`]).
pub const MAX_MESSAGE_BYTES: usize = 320 * 1024 * 1024;
/// The largest image a renderer sends (the GPU backend's image budget).
/// A larger one is not sent, and so not drawn.
pub const MAX_IMAGE_BYTES: usize = 256 * 1024 * 1024;
/// The largest image side the image decoders accept.
pub const MAX_IMAGE_SIDE: u32 = 16_384;
const MAX_SHORT_STRING: usize = 64 * 1024;
const MAX_EVENTS: usize = 4096;
const MAX_ITEMS: usize = 4 * 1024 * 1024;
const MAX_GLYPHS_PER_RUN: usize = 1024 * 1024;
const MAX_IMAGES_PER_FRAME: usize = 64 * 1024;
const MAX_HEADERS: usize = 1024;
/// The glyph atlas's widest (its width is fixed at 512 today).
const MAX_ATLAS_WIDTH: u32 = 4096;

/// Why a message could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

type Result<T> = std::result::Result<T, DecodeError>;

// ---- Primitives ----

#[derive(Default)]
pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }
    fn usize(&mut self, v: usize) {
        self.u64(v as u64);
    }
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }
    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.buf.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    /// A string that may be cut to `max` bytes (messages, not data).
    fn short_str(&mut self, s: &str) {
        let mut end = s.len().min(MAX_SHORT_STRING);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.str(&s[..end]);
    }
    fn opt_str(&mut self, s: Option<&str>) {
        match s {
            None => self.u8(0),
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
        }
    }
    fn opt_i64(&mut self, v: Option<i64>) {
        match v {
            None => self.u8(0),
            Some(v) => {
                self.u8(1);
                self.i64(v);
            }
        }
    }
}

pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(DecodeError("message ends early"));
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError("bad boolean")),
        }
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| DecodeError("u32"))?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| DecodeError("u64"))?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().map_err(|_| DecodeError("i64"))?))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    /// A finite `f32` (geometry: a NaN or infinity is never a position).
    fn finite(&mut self) -> Result<f32> {
        let v = self.f32()?;
        if v.is_finite() { Ok(v) } else { Err(DecodeError("a coordinate is not finite")) }
    }
    fn usize(&mut self) -> Result<usize> {
        usize::try_from(self.u64()?).map_err(|_| DecodeError("count too large"))
    }
    /// A count of items at least `min_size` bytes each: at most `max`, and no
    /// more than the rest of the message could hold — checked before
    /// anything is allocated for them.
    fn count(&mut self, max: usize, min_size: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        if n > max || n.saturating_mul(min_size.max(1)) > self.remaining() {
            return Err(DecodeError("count exceeds its limit or the message"));
        }
        Ok(n)
    }
    fn bytes(&mut self, max: usize) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(DecodeError("byte string exceeds its limit"));
        }
        self.take(n)
    }
    fn string(&mut self, max: usize) -> Result<String> {
        std::str::from_utf8(self.bytes(max)?).map(str::to_string).map_err(|_| DecodeError("string is not UTF-8"))
    }
    fn opt_string(&mut self, max: usize) -> Result<Option<String>> {
        match self.u8()? {
            0 => Ok(None),
            1 => self.string(max).map(Some),
            _ => Err(DecodeError("bad option tag")),
        }
    }
    fn opt_i64(&mut self) -> Result<Option<i64>> {
        match self.u8()? {
            0 => Ok(None),
            1 => self.i64().map(Some),
            _ => Err(DecodeError("bad option tag")),
        }
    }
    fn end(&self) -> Result<()> {
        if self.remaining() == 0 { Ok(()) } else { Err(DecodeError("trailing bytes")) }
    }
}

// ---- Framing ----

/// Writes one length-prefixed message.
pub(crate) fn write_frame(out: &mut impl Write, message: &[u8]) -> std::io::Result<()> {
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message too large"));
    }
    out.write_all(&(message.len() as u32).to_le_bytes())?;
    out.write_all(message)?;
    out.flush()
}

/// Reads one length-prefixed message; `Ok(None)` at a clean end of stream.
/// The claimed length is capped, and the buffer grows only as bytes really
/// arrive (a peer claiming a huge message and sending nothing costs
/// nothing).
pub(crate) fn read_frame(input: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match input.read(&mut len[got..])? {
            0 if got == 0 => return Ok(None),
            0 => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "truncated frame header")),
            n => got += n,
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message exceeds the size limit"));
    }
    let mut body = Vec::new();
    input.take(len as u64).read_to_end(&mut body)?;
    if body.len() != len {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "truncated frame"));
    }
    Ok(Some(body))
}

// ---- Messages to the renderer ----

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToRenderer {
    /// Milestone 35: `fonts` is the host's font catalog; the renderer asks
    /// for a font's bytes by its key ([`ToHost::FontRequest`]).
    /// `grants`: the capabilities the host granted the page, one bit each
    /// (`capability::CapabilityKind::bit`); the renderer re-creates them on
    /// its side (ADR 010 §2).
    /// `fingerprint` (Milestone 37): what the page's scripts may observe.
    Setup { viewport: Viewport, image_loading: ImageLoading, fonts: Vec<text::CatalogEntry>, grants: u8, fingerprint: privacy::FingerprintPolicy },
    Command(PageCommand),
    /// Advance to this time: nanoseconds from the host's epoch (negative:
    /// before it).
    Tick(i64),
    Shutdown,
    FetchReply { id: u64, result: std::result::Result<FetchResponse, NetworkError> },
    /// A font's bytes (`None`: the host would not or could not load it).
    FontReply { id: u64, bytes: Option<Vec<u8>> },
    /// `PageCommand::Grant` across the boundary (M36's gap): the kinds, one
    /// bit each, which the renderer mints again on its side (ADR 010 §2) —
    /// handles never cross a process boundary.
    Grant(u8),
    /// Milestone 41: the answer to a [`ToHost::Storage`] request.
    StorageReply { id: u64, answer: StorageAnswer },
}

/// Milestone 41 (ADR 025 §4): an operation on one origin's `localStorage`
/// area, which the browser process keeps and checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StorageOp {
    Get(String),
    Set(String, String),
    Remove(String),
    Clear,
    Length,
}

/// What the browser answers a storage operation with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StorageAnswer {
    Value(Option<String>),
    /// A write: `false` if it would exceed the area's quota.
    Written(bool),
    Length(u32),
    Done,
    /// The renderer may not reach this area (no storage grant, revoked, or
    /// an origin it never loaded).
    Refused,
}

/// The longest storage key or value either side sends: no area holds more.
const MAX_STORAGE_STRING: usize = storage::MAX_STORAGE_BYTES;

fn encode_storage_op(w: &mut Writer, op: &StorageOp) {
    match op {
        StorageOp::Get(name) => {
            w.u8(0);
            w.str(name);
        }
        StorageOp::Set(name, value) => {
            w.u8(1);
            w.str(name);
            w.str(value);
        }
        StorageOp::Remove(name) => {
            w.u8(2);
            w.str(name);
        }
        StorageOp::Clear => w.u8(3),
        StorageOp::Length => w.u8(4),
    }
}

fn decode_storage_op(r: &mut Reader) -> Result<StorageOp> {
    Ok(match r.u8()? {
        0 => StorageOp::Get(r.string(MAX_STORAGE_STRING)?),
        1 => StorageOp::Set(r.string(MAX_STORAGE_STRING)?, r.string(MAX_STORAGE_STRING)?),
        2 => StorageOp::Remove(r.string(MAX_STORAGE_STRING)?),
        3 => StorageOp::Clear,
        4 => StorageOp::Length,
        _ => return Err(DecodeError("bad storage operation")),
    })
}

/// Catalog entries a setup may carry, and the limits on their strings.
const MAX_CATALOG: usize = 8192;
const MAX_FONT_KEY: usize = 4096;
const MAX_FONT_NAMES: usize = 16;

fn encode_catalog(w: &mut Writer, fonts: &[text::CatalogEntry]) {
    let fonts = &fonts[..fonts.len().min(MAX_CATALOG)];
    w.len(fonts.len());
    for e in fonts {
        w.short_str(&e.key);
        w.u32(e.index);
        let list = |w: &mut Writer, v: &[String]| {
            let v = &v[..v.len().min(MAX_FONT_NAMES)];
            w.len(v.len());
            for s in v {
                w.short_str(s);
            }
        };
        list(w, &e.families);
        w.u32(u32::from(e.weight));
        w.u8(match e.style {
            text::FontStyle::Normal => 0,
            text::FontStyle::Italic => 1,
            text::FontStyle::Oblique => 2,
        });
        list(w, &e.langs);
    }
}

fn decode_catalog(r: &mut Reader) -> Result<Vec<text::CatalogEntry>> {
    let n = r.count(MAX_CATALOG, 16)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let key = r.string(MAX_FONT_KEY)?;
        let index = r.u32()?;
        let list = |r: &mut Reader| -> Result<Vec<String>> {
            let n = r.count(MAX_FONT_NAMES, 4)?;
            (0..n).map(|_| r.string(256)).collect()
        };
        let families = list(r)?;
        let weight = u16::try_from(r.u32()?).ok().filter(|w| (1..=1000).contains(w)).ok_or(DecodeError("bad font weight"))?;
        let style = match r.u8()? {
            0 => text::FontStyle::Normal,
            1 => text::FontStyle::Italic,
            2 => text::FontStyle::Oblique,
            _ => return Err(DecodeError("bad font style")),
        };
        let langs = list(r)?;
        out.push(text::CatalogEntry { key, index, families, weight, style, langs });
    }
    Ok(out)
}

impl ToRenderer {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            ToRenderer::Setup { viewport, image_loading, fonts, grants, fingerprint } => {
                w.u8(0);
                w.u8(*grants);
                w.u32(fingerprint.time_resolution_ms());
                w.u8(match fingerprint.timezone {
                    privacy::TimezonePolicy::Real => 0,
                    privacy::TimezonePolicy::Utc => 1,
                });
                w.u8(match fingerprint.fonts {
                    privacy::FontExposure::All => 0,
                    privacy::FontExposure::Standard => 1,
                });
                w.f32(viewport.width);
                w.f32(viewport.height);
                w.u8(match image_loading {
                    ImageLoading::Progressive => 0,
                    ImageLoading::BeforeFirstFrame => 1,
                });
                encode_catalog(&mut w, fonts);
            }
            ToRenderer::FontReply { id, bytes } => {
                w.u8(5);
                w.u64(*id);
                match bytes {
                    None => w.u8(0),
                    Some(b) => {
                        w.u8(1);
                        w.bytes(b);
                    }
                }
            }
            // A grant's handles stay on this side: the kinds go.
            ToRenderer::Command(PageCommand::Grant(grants)) => {
                w.u8(6);
                w.u8(grants.bits());
            }
            ToRenderer::Grant(bits) => {
                w.u8(6);
                w.u8(*bits);
            }
            ToRenderer::Command(command) => {
                w.u8(1);
                encode_command(&mut w, command);
            }
            ToRenderer::Tick(t) => {
                w.u8(2);
                w.i64(*t);
            }
            ToRenderer::StorageReply { id, answer } => {
                w.u8(7);
                w.u64(*id);
                match answer {
                    StorageAnswer::Value(None) => w.u8(0),
                    StorageAnswer::Value(Some(v)) => {
                        w.u8(1);
                        w.str(v);
                    }
                    StorageAnswer::Written(ok) => {
                        w.u8(2);
                        w.bool(*ok);
                    }
                    StorageAnswer::Length(n) => {
                        w.u8(3);
                        w.u32(*n);
                    }
                    StorageAnswer::Done => w.u8(4),
                    StorageAnswer::Refused => w.u8(5),
                }
            }
            ToRenderer::Shutdown => w.u8(3),
            ToRenderer::FetchReply { id, result } => {
                w.u8(4);
                w.u64(*id);
                match result {
                    Ok(r) => {
                        w.u8(0);
                        w.u32(u32::from(r.status));
                        w.str(&r.final_url);
                        w.len(r.headers.len());
                        for (k, v) in &r.headers {
                            w.short_str(k);
                            w.short_str(v);
                        }
                        w.bytes(&r.body);
                    }
                    Err(e) => {
                        w.u8(1);
                        let (tag, text) = match e {
                            NetworkError::InvalidUrl(t) => (0, t),
                            NetworkError::Transport(t) => (1, t),
                            NetworkError::Io(t) => (2, t),
                            NetworkError::Blocked(t) => (3, t),
                        };
                        w.u8(tag);
                        w.short_str(text);
                    }
                }
            }
        }
        w.into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<ToRenderer> {
        let mut r = Reader::new(bytes);
        let message = match r.u8()? {
            0 => {
                let grants = r.u8()?;
                let resolution = r.u32()?;
                let timezone = match r.u8()? {
                    0 => privacy::TimezonePolicy::Real,
                    1 => privacy::TimezonePolicy::Utc,
                    _ => return Err(DecodeError("bad timezone policy")),
                };
                let fonts = match r.u8()? {
                    0 => privacy::FontExposure::All,
                    1 => privacy::FontExposure::Standard,
                    _ => return Err(DecodeError("bad font exposure")),
                };
                let mut fingerprint = privacy::FingerprintPolicy::standard().with_time_resolution_ms(resolution);
                fingerprint.timezone = timezone;
                fingerprint.fonts = fonts;
                let viewport = Viewport::new(r.f32()?, r.f32()?);
                let image_loading = match r.u8()? {
                    0 => ImageLoading::Progressive,
                    1 => ImageLoading::BeforeFirstFrame,
                    _ => return Err(DecodeError("bad image-loading mode")),
                };
                ToRenderer::Setup { viewport, image_loading, fonts: decode_catalog(&mut r)?, grants, fingerprint }
            }
            5 => {
                let id = r.u64()?;
                let bytes = match r.u8()? {
                    0 => None,
                    1 => Some(r.bytes(text::MAX_FONT_BYTES)?.to_vec()),
                    _ => return Err(DecodeError("bad font reply")),
                };
                ToRenderer::FontReply { id, bytes }
            }
            1 => ToRenderer::Command(decode_command(&mut r)?),
            2 => ToRenderer::Tick(r.i64()?),
            3 => ToRenderer::Shutdown,
            6 => {
                let bits = r.u8()?;
                let known = capability::CapabilityKind::ALL.iter().fold(0u8, |b, k| b | k.bit());
                if bits & !known != 0 {
                    return Err(DecodeError("bad grant"));
                }
                ToRenderer::Grant(bits)
            }
            7 => {
                let id = r.u64()?;
                let answer = match r.u8()? {
                    0 => StorageAnswer::Value(None),
                    1 => StorageAnswer::Value(Some(r.string(MAX_STORAGE_STRING)?)),
                    2 => StorageAnswer::Written(r.bool()?),
                    3 => StorageAnswer::Length(r.u32()?),
                    4 => StorageAnswer::Done,
                    5 => StorageAnswer::Refused,
                    _ => return Err(DecodeError("bad storage answer")),
                };
                ToRenderer::StorageReply { id, answer }
            }
            4 => {
                let id = r.u64()?;
                let result = match r.u8()? {
                    0 => {
                        let status = u16::try_from(r.u32()?).map_err(|_| DecodeError("bad status"))?;
                        let final_url = r.string(origin::MAX_URL_BYTES)?;
                        let n = r.count(MAX_HEADERS, 8)?;
                        let mut headers = Vec::with_capacity(n);
                        for _ in 0..n {
                            headers.push((r.string(MAX_SHORT_STRING)?, r.string(MAX_SHORT_STRING)?));
                        }
                        let body = r.bytes(network::MAX_RESPONSE_BODY_BYTES as usize)?.to_vec();
                        Ok(FetchResponse { status, final_url, headers, body })
                    }
                    1 => {
                        let tag = r.u8()?;
                        let text = r.string(MAX_SHORT_STRING)?;
                        Err(match tag {
                            0 => NetworkError::InvalidUrl(text),
                            1 => NetworkError::Transport(text),
                            2 => NetworkError::Io(text),
                            3 => NetworkError::Blocked(text),
                            _ => return Err(DecodeError("bad network error")),
                        })
                    }
                    _ => return Err(DecodeError("bad fetch result")),
                };
                ToRenderer::FetchReply { id, result }
            }
            _ => return Err(DecodeError("unknown message to the renderer")),
        };
        r.end()?;
        Ok(message)
    }
}

fn encode_command(w: &mut Writer, command: &PageCommand) {
    match command {
        PageCommand::Load(LoadRequest::Url(url)) => {
            w.u8(0);
            w.str(url);
        }
        PageCommand::Load(LoadRequest::Html { html, url }) => {
            w.u8(1);
            w.str(html);
            w.opt_str(url.as_deref());
        }
        PageCommand::Input(input) => {
            w.u8(2);
            match input {
                EngineInput::PointerMove { x, y } => {
                    w.u8(0);
                    w.f32(*x);
                    w.f32(*y);
                }
                EngineInput::PointerDown { x, y } => {
                    w.u8(1);
                    w.f32(*x);
                    w.f32(*y);
                }
                EngineInput::Scroll { dx, dy } => {
                    w.u8(2);
                    w.f32(*dx);
                    w.f32(*dy);
                }
                EngineInput::Key { key } => {
                    w.u8(3);
                    w.short_str(key);
                }
                EngineInput::PointerUp { x, y } => {
                    w.u8(4);
                    w.f32(*x);
                    w.f32(*y);
                }
            }
        }
        PageCommand::Resize(v) => {
            w.u8(3);
            w.f32(v.width);
            w.f32(v.height);
        }
        PageCommand::SimulateCrash => w.u8(4),
        PageCommand::Revoke(kind) => {
            w.u8(5);
            w.u8(*kind as u8);
        }
        PageCommand::SetVisible(visible) => {
            w.u8(6);
            w.u8(u8::from(*visible));
        }
        // Sent as `ToRenderer::Grant` (above); a renderer never sends commands.
        PageCommand::Grant(grants) => {
            w.u8(7);
            w.u8(grants.bits());
        }
    }
}

/// Commands come from the host (trusted); they are still validated again by
/// the page loop inside the renderer, as in thread mode.
fn decode_command(r: &mut Reader) -> Result<PageCommand> {
    Ok(match r.u8()? {
        0 => PageCommand::Load(LoadRequest::Url(r.string(origin::MAX_URL_BYTES)?)),
        1 => {
            let html = r.string(crate::MAX_HTML_BYTES)?;
            let url = r.opt_string(origin::MAX_URL_BYTES)?;
            PageCommand::Load(LoadRequest::Html { html, url })
        }
        2 => PageCommand::Input(match r.u8()? {
            0 => EngineInput::PointerMove { x: r.f32()?, y: r.f32()? },
            1 => EngineInput::PointerDown { x: r.f32()?, y: r.f32()? },
            2 => EngineInput::Scroll { dx: r.f32()?, dy: r.f32()? },
            3 => EngineInput::Key { key: r.string(MAX_SHORT_STRING)? },
            4 => EngineInput::PointerUp { x: r.f32()?, y: r.f32()? },
            _ => return Err(DecodeError("bad input")),
        }),
        3 => PageCommand::Resize(Viewport::new(r.f32()?, r.f32()?)),
        4 => PageCommand::SimulateCrash,
        5 => {
            let tag = r.u8()?;
            PageCommand::Revoke(*capability::CapabilityKind::ALL.get(tag as usize).ok_or(DecodeError("bad capability"))?)
        }
        6 => PageCommand::SetVisible(match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(DecodeError("bad visibility")),
        }),
        _ => return Err(DecodeError("bad command")),
    })
}

// ---- Messages to the host ----

/// A request's initiator as a renderer states it — checked by the host
/// (ADR 016 §5), never trusted as is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireInitiator {
    pub url: Option<String>,
    /// `Some(id)` for an opaque origin: the renderer's own, process-local id.
    pub opaque: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireContext {
    pub initiator: Option<WireInitiator>,
    pub is_navigation: bool,
    pub credentials: Credentials,
    /// Milestone 37: what the request is for (the filter's type options).
    pub destination: network::Destination,
}

const DESTINATIONS: [network::Destination; 7] = [
    network::Destination::Document,
    network::Destination::Script,
    network::Destination::Image,
    network::Destination::Stylesheet,
    network::Destination::Font,
    network::Destination::XmlHttpRequest,
    network::Destination::Other,
];

/// A frame, minus what the host already has: glyph-atlas bands that did not
/// change, and image pixels it was sent before (their ids are listed).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WireFrame {
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub background: style::Color,
    pub items: Vec<DisplayItem>,
    pub atlas_width: u32,
    pub atlas_height: u32,
    pub atlas_generation: u64,
    /// `(band index, bytes)` for each band that changed.
    pub atlas_bands: Vec<(u32, Vec<u8>)>,
    /// `(id, width, height)` of every image the frame uses.
    pub images: Vec<(u64, u32, u32)>,
    pub damage: Damage,
    pub stats: FrameStats,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToHost {
    /// What one tick produced (ticks as nanoseconds from the host's epoch).
    Published { events: Vec<EngineEvent>, frame: Option<Box<WireFrame>>, next_wake: Option<i64>, completed_tick: i64 },
    /// An image's pixels, sent before the first frame that uses it.
    Image { id: u64, width: u32, height: u32, rgba: Vec<u8> },
    /// The page loop panicked (contained inside the renderer).
    Crashed { reason: String },
    FetchRequest { id: u64, url: String, context: WireContext },
    /// Whether the renderer is sandboxed, and how (a diagnostic line).
    Started { sandboxed: bool, detail: String },
    /// Milestone 35: the bytes of a catalog font (by key and face index).
    FontRequest { id: u64, key: String, index: u32 },
    /// Milestone 41: an operation on the `localStorage` area of `origin` (a
    /// serialised tuple origin; first-party — no frames exist yet). Checked
    /// by the host against the page's storage grant and the origins this
    /// renderer has loaded (ADR 025 §4).
    Storage { id: u64, origin: String, op: StorageOp },
}

impl ToHost {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            ToHost::Published { events, frame, next_wake, completed_tick } => {
                w.u8(0);
                let events: Vec<&EngineEvent> = events.iter().filter(|e| !matches!(e, EngineEvent::Frame { .. })).collect();
                w.len(events.len());
                for e in events {
                    encode_event(&mut w, e);
                }
                match frame {
                    None => w.u8(0),
                    Some(f) => {
                        w.u8(1);
                        encode_frame(&mut w, f);
                    }
                }
                w.opt_i64(*next_wake);
                w.i64(*completed_tick);
            }
            ToHost::Image { id, width, height, rgba } => {
                w.u8(1);
                w.u64(*id);
                w.u32(*width);
                w.u32(*height);
                w.bytes(rgba);
            }
            ToHost::Crashed { reason } => {
                w.u8(2);
                w.short_str(reason);
            }
            ToHost::FetchRequest { id, url, context } => {
                w.u8(3);
                w.u64(*id);
                w.str(url);
                match &context.initiator {
                    None => w.u8(0),
                    Some(i) => {
                        w.u8(1);
                        w.opt_str(i.url.as_deref());
                        match i.opaque {
                            None => w.u8(0),
                            Some(id) => {
                                w.u8(1);
                                w.u64(id);
                            }
                        }
                    }
                }
                w.bool(context.is_navigation);
                w.u8(match context.credentials {
                    Credentials::Include => 0,
                    Credentials::SameOrigin => 1,
                    Credentials::Omit => 2,
                });
                w.u8(DESTINATIONS.iter().position(|d| *d == context.destination).unwrap_or(DESTINATIONS.len() - 1) as u8);
            }
            ToHost::Started { sandboxed, detail } => {
                w.u8(4);
                w.bool(*sandboxed);
                w.short_str(detail);
            }
            ToHost::FontRequest { id, key, index } => {
                w.u8(5);
                w.u64(*id);
                w.short_str(key);
                w.u32(*index);
            }
            ToHost::Storage { id, origin, op } => {
                w.u8(6);
                w.u64(*id);
                w.str(origin);
                encode_storage_op(&mut w, op);
            }
        }
        w.into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<ToHost> {
        let mut r = Reader::new(bytes);
        let message = match r.u8()? {
            0 => {
                let n = r.count(MAX_EVENTS, 1)?;
                let mut events = Vec::with_capacity(n);
                for _ in 0..n {
                    events.push(decode_event(&mut r)?);
                }
                let frame = match r.u8()? {
                    0 => None,
                    1 => Some(Box::new(decode_frame(&mut r)?)),
                    _ => return Err(DecodeError("bad frame tag")),
                };
                ToHost::Published { events, frame, next_wake: r.opt_i64()?, completed_tick: r.i64()? }
            }
            1 => {
                let id = r.u64()?;
                let (width, height) = (r.u32()?, r.u32()?);
                if width == 0 || height == 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
                    return Err(DecodeError("image size out of range"));
                }
                let rgba = r.bytes(MAX_IMAGE_BYTES)?;
                if rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 {
                    return Err(DecodeError("image pixels do not match its size"));
                }
                ToHost::Image { id, width, height, rgba: rgba.to_vec() }
            }
            2 => ToHost::Crashed { reason: r.string(MAX_SHORT_STRING)? },
            3 => {
                let id = r.u64()?;
                let url = r.string(origin::MAX_URL_BYTES)?;
                let initiator = match r.u8()? {
                    0 => None,
                    1 => {
                        let url = r.opt_string(origin::MAX_URL_BYTES)?;
                        let opaque = match r.u8()? {
                            0 => None,
                            1 => Some(r.u64()?),
                            _ => return Err(DecodeError("bad opaque tag")),
                        };
                        Some(WireInitiator { url, opaque })
                    }
                    _ => return Err(DecodeError("bad initiator tag")),
                };
                let is_navigation = r.bool()?;
                let credentials = match r.u8()? {
                    0 => Credentials::Include,
                    1 => Credentials::SameOrigin,
                    2 => Credentials::Omit,
                    _ => return Err(DecodeError("bad credentials mode")),
                };
                let destination = *DESTINATIONS.get(r.u8()? as usize).ok_or(DecodeError("bad destination"))?;
                ToHost::FetchRequest { id, url, context: WireContext { initiator, is_navigation, credentials, destination } }
            }
            4 => ToHost::Started { sandboxed: r.bool()?, detail: r.string(MAX_SHORT_STRING)? },
            5 => ToHost::FontRequest { id: r.u64()?, key: r.string(MAX_FONT_KEY)?, index: r.u32()? },
            6 => ToHost::Storage { id: r.u64()?, origin: r.string(origin::MAX_URL_BYTES)?, op: decode_storage_op(&mut r)? },
            _ => return Err(DecodeError("unknown message to the host")),
        };
        r.end()?;
        Ok(message)
    }
}

// Page ids inside a renderer mean nothing to the host: events travel
// without them, and the host fills in its own.
fn encode_event(w: &mut Writer, event: &EngineEvent) {
    match event {
        EngineEvent::NavigationStarted { url, .. } => {
            w.u8(0);
            w.str(url);
        }
        EngineEvent::NavigationCommitted { url, .. } => {
            w.u8(1);
            w.opt_str(url.as_deref());
        }
        EngineEvent::NavigationFailed { url, error, .. } => {
            w.u8(2);
            w.str(url);
            w.short_str(error);
        }
        EngineEvent::PageCrashed { reason, .. } => {
            w.u8(3);
            w.short_str(reason);
        }
        EngineEvent::Frame { .. } => unreachable!("frames travel as `WireFrame`s"),
    }
}

fn decode_event(r: &mut Reader) -> Result<EngineEvent> {
    // A placeholder page id: the host replaces it.
    let page = PageId(0);
    Ok(match r.u8()? {
        0 => EngineEvent::NavigationStarted { page, url: r.string(origin::MAX_URL_BYTES)? },
        1 => EngineEvent::NavigationCommitted { page, url: r.opt_string(origin::MAX_URL_BYTES)? },
        2 => EngineEvent::NavigationFailed { page, url: r.string(origin::MAX_URL_BYTES)?, error: r.string(MAX_SHORT_STRING)? },
        3 => EngineEvent::PageCrashed { page, reason: r.string(MAX_SHORT_STRING)? },
        _ => return Err(DecodeError("bad event")),
    })
}

fn color(w: &mut Writer, c: style::Color) {
    w.buf.extend_from_slice(&[c.r, c.g, c.b, c.a]);
}

fn read_color(r: &mut Reader) -> Result<style::Color> {
    let b = r.take(4)?;
    Ok(style::Color { r: b[0], g: b[1], b: b[2], a: b[3] })
}

fn encode_frame(w: &mut Writer, f: &WireFrame) {
    w.f32(f.viewport_width);
    w.f32(f.viewport_height);
    color(w, f.background);
    w.len(f.items.len());
    for item in &f.items {
        match item {
            DisplayItem::Rect { x, y, width, height, color: c, radius } => {
                w.u8(0);
                for v in [x, y, width, height] {
                    w.f32(*v);
                }
                color(w, *c);
                w.f32(*radius);
            }
            DisplayItem::Shadow { x, y, width, height, color: c, radius, blur } => {
                w.u8(1);
                for v in [x, y, width, height] {
                    w.f32(*v);
                }
                color(w, *c);
                w.f32(*radius);
                w.f32(*blur);
            }
            DisplayItem::Glyphs { instances, atlas_generation } => {
                w.u8(2);
                w.u64(*atlas_generation);
                w.len(instances.len());
                for g in instances {
                    for v in [g.x, g.y, g.width, g.height] {
                        w.f32(v);
                    }
                    for v in [g.atlas_x, g.atlas_y, g.atlas_w, g.atlas_h] {
                        w.u32(v);
                    }
                    color(w, g.color);
                }
            }
            DisplayItem::Image { x, y, width, height, image_id, sampling } => {
                w.u8(3);
                for v in [x, y, width, height] {
                    w.f32(*v);
                }
                w.u64(*image_id);
                let s = ImageSampling::of(sampling);
                for v in s.uv {
                    w.f32(v);
                }
                w.f32(s.alpha);
            }
            DisplayItem::PushClip { rect, radius } => {
                w.u8(4);
                for v in rect {
                    w.f32(*v);
                }
                w.f32(*radius);
            }
            DisplayItem::PopClip => w.u8(5),
            DisplayItem::PushTransform(layer) => {
                w.u8(6);
                for v in layer.matrix.0.iter().chain(&layer.bounds).chain(&layer.origin) {
                    w.f32(*v);
                }
                match layer.element {
                    None => w.u8(0),
                    Some(e) => {
                        w.u8(1);
                        w.u64(e);
                    }
                }
                match layer.scroll {
                    None => w.u8(0),
                    Some(clip) => {
                        w.u8(1);
                        for v in clip {
                            w.f32(v);
                        }
                    }
                }
            }
            DisplayItem::PopTransform => w.u8(7),
            DisplayItem::PushEffect { opacity, bounds } => {
                w.u8(8);
                w.f32(*opacity);
                for v in bounds {
                    w.f32(*v);
                }
            }
            DisplayItem::PopEffect => w.u8(9),
        }
    }
    w.u32(f.atlas_width);
    w.u32(f.atlas_height);
    w.u64(f.atlas_generation);
    w.len(f.atlas_bands.len());
    for (index, bytes) in &f.atlas_bands {
        w.u32(*index);
        w.bytes(bytes);
    }
    w.len(f.images.len());
    for (id, width, height) in &f.images {
        w.u64(*id);
        w.u32(*width);
        w.u32(*height);
    }
    match f.damage {
        Damage::Full => w.u8(0),
        Damage::None => w.u8(1),
        Damage::Rect { x, y, width, height } => {
            w.u8(2);
            for v in [x, y, width, height] {
                w.f32(v);
            }
        }
    }
    let s = &f.stats;
    w.usize(s.restyled);
    w.bool(s.full_restyle);
    w.usize(s.animated);
    w.usize(s.composited);
    match s.layout {
        LayoutWork::Reused => w.u8(0),
        LayoutWork::Full { boxes } => {
            w.u8(1);
            w.usize(boxes);
        }
        LayoutWork::Incremental { laid_out, reused_boxes } => {
            w.u8(2);
            w.usize(laid_out);
            w.usize(reused_boxes);
        }
    }
    w.usize(s.display_items);
    w.usize(s.culled);
    w.bool(s.display_list_built);
    w.u32(s.animation_interval_ms);
}

fn decode_frame(r: &mut Reader) -> Result<WireFrame> {
    let viewport_width = r.finite()?;
    let viewport_height = r.finite()?;
    let background = read_color(r)?;
    // Every item is at least its one-byte tag: the count cannot outrun them.
    let n = r.count(MAX_ITEMS, 1)?;
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        let item = match r.u8()? {
            0 => DisplayItem::Rect { x: r.finite()?, y: r.finite()?, width: r.finite()?, height: r.finite()?, color: read_color(r)?, radius: r.finite()? },
            1 => DisplayItem::Shadow {
                x: r.finite()?,
                y: r.finite()?,
                width: r.finite()?,
                height: r.finite()?,
                color: read_color(r)?,
                radius: r.finite()?,
                blur: r.finite()?,
            },
            2 => {
                let atlas_generation = r.u64()?;
                let g = r.count(MAX_GLYPHS_PER_RUN, 36)?;
                let mut instances = Vec::with_capacity(g);
                for _ in 0..g {
                    instances.push(GlyphInstance {
                        x: r.finite()?,
                        y: r.finite()?,
                        width: r.finite()?,
                        height: r.finite()?,
                        atlas_x: r.u32()?,
                        atlas_y: r.u32()?,
                        atlas_w: r.u32()?,
                        atlas_h: r.u32()?,
                        color: read_color(r)?,
                    });
                }
                DisplayItem::Glyphs { instances, atlas_generation }
            }
            3 => {
                let (x, y, width, height) = (r.finite()?, r.finite()?, r.finite()?, r.finite()?);
                let image_id = r.u64()?;
                let uv = [r.finite()?, r.finite()?, r.finite()?, r.finite()?];
                let alpha = r.finite()?;
                DisplayItem::Image { x, y, width, height, image_id, sampling: ImageSampling::boxed(uv, alpha) }
            }
            4 => DisplayItem::PushClip { rect: [r.finite()?, r.finite()?, r.finite()?, r.finite()?], radius: r.finite()? },
            5 => DisplayItem::PopClip,
            6 => {
                let matrix = rendering::Affine([r.finite()?, r.finite()?, r.finite()?, r.finite()?, r.finite()?, r.finite()?]);
                let bounds = [r.finite()?, r.finite()?, r.finite()?, r.finite()?];
                let origin = [r.finite()?, r.finite()?];
                let element = match r.u8()? {
                    0 => None,
                    1 => Some(r.u64()?),
                    _ => return Err(DecodeError("bad element tag")),
                };
                let scroll = match r.u8()? {
                    0 => None,
                    1 => Some([r.finite()?, r.finite()?, r.finite()?, r.finite()?]),
                    _ => return Err(DecodeError("bad scroll tag")),
                };
                DisplayItem::PushTransform(Box::new(rendering::TransformLayer { matrix, bounds, element, origin, scroll }))
            }
            7 => DisplayItem::PopTransform,
            8 => DisplayItem::PushEffect { opacity: r.finite()?, bounds: [r.finite()?, r.finite()?, r.finite()?, r.finite()?] },
            9 => DisplayItem::PopEffect,
            _ => return Err(DecodeError("bad display item")),
        };
        items.push(item);
    }
    let atlas_width = r.u32()?;
    let atlas_height = r.u32()?;
    if atlas_width > MAX_ATLAS_WIDTH || atlas_height as usize > text::MAX_ATLAS_HEIGHT {
        return Err(DecodeError("glyph atlas size out of range"));
    }
    let atlas_generation = r.u64()?;
    let band_count = (atlas_height as usize).div_ceil(text::ATLAS_BAND_ROWS);
    let band_bytes = atlas_width as usize * text::ATLAS_BAND_ROWS;
    let n = r.count(band_count, 8)?;
    let mut atlas_bands = Vec::with_capacity(n);
    for _ in 0..n {
        let index = r.u32()?;
        let bytes = r.bytes(band_bytes)?;
        if index as usize >= band_count || bytes.len() != band_bytes {
            return Err(DecodeError("glyph atlas band out of range"));
        }
        atlas_bands.push((index, bytes.to_vec()));
    }
    let n = r.count(MAX_IMAGES_PER_FRAME, 16)?;
    let mut images = Vec::with_capacity(n);
    for _ in 0..n {
        images.push((r.u64()?, r.u32()?, r.u32()?));
    }
    let damage = match r.u8()? {
        0 => Damage::Full,
        1 => Damage::None,
        2 => Damage::Rect { x: r.finite()?, y: r.finite()?, width: r.finite()?, height: r.finite()? },
        _ => return Err(DecodeError("bad damage")),
    };
    let restyled = r.usize()?;
    let full_restyle = r.bool()?;
    let animated = r.usize()?;
    let composited = r.usize()?;
    let layout = match r.u8()? {
        0 => LayoutWork::Reused,
        1 => LayoutWork::Full { boxes: r.usize()? },
        2 => LayoutWork::Incremental { laid_out: r.usize()?, reused_boxes: r.usize()? },
        _ => return Err(DecodeError("bad layout work")),
    };
    let stats = FrameStats {
        restyled,
        full_restyle,
        animated,
        composited,
        layout,
        display_items: r.usize()?,
        culled: r.usize()?,
        display_list_built: r.bool()?,
        damage,
        animation_interval_ms: r.u32()?,
    };
    Ok(WireFrame {
        viewport_width,
        viewport_height,
        background,
        items,
        atlas_width,
        atlas_height,
        atlas_generation,
        atlas_bands,
        images,
        damage,
        stats,
    })
}

/// Decodes `bytes` as a message in each direction (the `ipc_decode` fuzz
/// target): must never panic, whatever the input.
#[doc(hidden)]
pub fn fuzz_decode(bytes: &[u8]) {
    let _ = ToHost::decode(bytes);
    let _ = ToRenderer::decode(bytes);
    let _ = read_frame(&mut &bytes[..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> WireFrame {
        let color = style::Color { r: 1, g: 2, b: 3, a: 4 };
        WireFrame {
            viewport_width: 800.0,
            viewport_height: 600.0,
            background: color,
            items: vec![
                DisplayItem::Rect { x: 1.0, y: 2.0, width: 3.0, height: 4.0, color, radius: 0.5 },
                DisplayItem::Shadow { x: 1.0, y: 2.0, width: 3.0, height: 4.0, color, radius: 0.5, blur: 6.0 },
                DisplayItem::Glyphs {
                    instances: vec![GlyphInstance { x: 1.0, y: 2.0, width: 3.0, height: 4.0, atlas_x: 5, atlas_y: 6, atlas_w: 7, atlas_h: 8, color }],
                    atlas_generation: 9,
                },
                DisplayItem::Image { x: 1.0, y: 2.0, width: 3.0, height: 4.0, image_id: 77, sampling: ImageSampling::boxed([0.0, 0.0, 2.0, 2.0], 0.5) },
                DisplayItem::PushEffect { opacity: 0.5, bounds: [0.0, 0.0, 9.0, 9.0] },
                DisplayItem::PushClip { rect: [1.0, 1.0, 5.0, 5.0], radius: 2.0 },
                DisplayItem::PushTransform(Box::new(rendering::TransformLayer {
                    matrix: rendering::Affine::rotate_deg(30.0),
                    bounds: [0.0, 0.0, 4.0, 4.0],
                    element: Some(12),
                    origin: [2.0, 2.0],
                    scroll: None,
                })),
                DisplayItem::PushTransform(Box::new(rendering::TransformLayer {
                    matrix: rendering::Affine::translate(0.0, -30.0),
                    bounds: [1.0, 1.0, 5.0, 5.0],
                    element: Some(13),
                    origin: [0.0, 0.0],
                    scroll: Some([1.0, 1.0, 5.0, 5.0]),
                })),
                DisplayItem::PopTransform,
                DisplayItem::PopTransform,
                DisplayItem::PopClip,
                DisplayItem::PopEffect,
            ],
            atlas_width: 512,
            atlas_height: 128,
            atlas_generation: 9,
            atlas_bands: vec![(1, vec![7; 512 * text::ATLAS_BAND_ROWS])],
            images: vec![(77, 2, 2)],
            damage: Damage::Rect { x: 0.0, y: 1.0, width: 2.0, height: 3.0 },
            stats: FrameStats {
                restyled: 1,
                full_restyle: true,
                animated: 2,
                composited: 1,
                layout: LayoutWork::Incremental { laid_out: 3, reused_boxes: 4 },
                display_items: 5,
                culled: 6,
                display_list_built: true,
                damage: Damage::Rect { x: 0.0, y: 1.0, width: 2.0, height: 3.0 },
                animation_interval_ms: 67,
            },
        }
    }

    #[test]
    fn every_message_round_trips() {
        let to_host = [
            ToHost::Published {
                events: vec![
                    EngineEvent::NavigationStarted { page: PageId(0), url: "https://a.example/".into() },
                    EngineEvent::NavigationCommitted { page: PageId(0), url: None },
                    EngineEvent::NavigationFailed { page: PageId(0), url: "x".into(), error: "e".into() },
                ],
                frame: Some(Box::new(frame())),
                next_wake: Some(-5),
                completed_tick: 42,
            },
            ToHost::Image { id: 3, width: 1, height: 2, rgba: vec![9; 8] },
            ToHost::Crashed { reason: "boom".into() },
            ToHost::FetchRequest {
                id: 8,
                url: "https://b.example/x".into(),
                context: WireContext {
                    initiator: Some(WireInitiator { url: Some("https://a.example/".into()), opaque: Some(4) }),
                    is_navigation: true,
                    credentials: Credentials::SameOrigin,
                    destination: network::Destination::Image,
                },
            },
            ToHost::Started { sandboxed: true, detail: "landlock+seccomp".into() },
            ToHost::Storage { id: 9, origin: "https://a.example".into(), op: StorageOp::Set("k".into(), "v".into()) },
            ToHost::Storage { id: 10, origin: "https://a.example".into(), op: StorageOp::Get("k".into()) },
            ToHost::Storage { id: 11, origin: "https://a.example".into(), op: StorageOp::Remove("k".into()) },
            ToHost::Storage { id: 12, origin: "https://a.example".into(), op: StorageOp::Clear },
            ToHost::Storage { id: 13, origin: "https://a.example".into(), op: StorageOp::Length },
        ];
        for m in to_host {
            assert_eq!(ToHost::decode(&m.encode()), Ok(m));
        }
        let to_renderer = [
            ToRenderer::Setup { viewport: Viewport::new(300.0, 200.0), image_loading: ImageLoading::BeforeFirstFrame, fonts: vec![text::CatalogEntry { key: "/f.ttf".into(), index: 0, families: vec!["x".into()], weight: 700, style: text::FontStyle::Italic, langs: vec!["hi".into()] }], grants: 0b11, fingerprint: privacy::FingerprintPolicy::private() },
            ToRenderer::FontReply { id: 3, bytes: Some(vec![1, 2, 3]) },
            ToRenderer::FontReply { id: 4, bytes: None },
            ToRenderer::Command(PageCommand::Load(LoadRequest::Html { html: "<p>".into(), url: Some("https://a/".into()) })),
            ToRenderer::Command(PageCommand::Load(LoadRequest::Url("https://a/".into()))),
            ToRenderer::Command(PageCommand::Input(EngineInput::Key { key: "Tab".into() })),
            ToRenderer::Command(PageCommand::Input(EngineInput::PointerUp { x: 3.0, y: 4.0 })),
            ToRenderer::Command(PageCommand::Input(EngineInput::Scroll { dx: 1.0, dy: -2.0 })),
            ToRenderer::Command(PageCommand::Resize(Viewport::new(10.0, 20.0))),
            ToRenderer::Command(PageCommand::SimulateCrash),
            ToRenderer::Command(PageCommand::Revoke(capability::CapabilityKind::Storage)),
            ToRenderer::Command(PageCommand::SetVisible(false)),
            ToRenderer::Command(PageCommand::SetVisible(true)),
            ToRenderer::Grant(0b11),
            ToRenderer::Tick(-7),
            ToRenderer::Shutdown,
            ToRenderer::FetchReply {
                id: 1,
                result: Ok(FetchResponse { status: 200, final_url: "https://a/".into(), headers: vec![("a".into(), "b".into())], body: vec![1, 2] }),
            },
            ToRenderer::FetchReply { id: 2, result: Err(NetworkError::Transport("down".into())) },
            ToRenderer::FetchReply { id: 3, result: Err(NetworkError::Blocked("trackers (||t.example^)".into())) },
            ToRenderer::StorageReply { id: 4, answer: StorageAnswer::Value(Some("v".into())) },
            ToRenderer::StorageReply { id: 5, answer: StorageAnswer::Value(None) },
            ToRenderer::StorageReply { id: 6, answer: StorageAnswer::Written(false) },
            ToRenderer::StorageReply { id: 7, answer: StorageAnswer::Length(3) },
            ToRenderer::StorageReply { id: 8, answer: StorageAnswer::Done },
            ToRenderer::StorageReply { id: 9, answer: StorageAnswer::Refused },
        ];
        for m in to_renderer {
            assert_eq!(ToRenderer::decode(&m.encode()), Ok(m));
        }
    }

    /// The committed fuzz seeds for messages M36, M37 and M38b changed still
    /// decode — a stale seed would only ever exercise the error path.
    #[test]
    fn the_committed_seeds_of_changed_messages_decode() {
        let seed = |name: &str| std::fs::read(format!("{}/../../fuzz/corpus/ipc_decode/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        assert!(matches!(ToHost::decode(&seed("04_fetch_request")), Ok(ToHost::FetchRequest { context: WireContext { destination: network::Destination::Image, .. }, .. })));
        match ToRenderer::decode(&seed("11_setup_with_grants")) {
            Ok(ToRenderer::Setup { grants: 3, fingerprint, .. }) => assert_eq!(fingerprint, privacy::FingerprintPolicy::private()),
            other => panic!("{other:?}"),
        }
        assert!(matches!(ToRenderer::decode(&seed("12_revoke_network")), Ok(ToRenderer::Command(PageCommand::Revoke(capability::CapabilityKind::Network)))));
        assert!(matches!(ToRenderer::decode(&seed("14_blocked_reply")), Ok(ToRenderer::FetchReply { result: Err(NetworkError::Blocked(_)), .. })));
        // Milestone 38b.
        assert!(matches!(ToRenderer::decode(&seed("15_set_hidden")), Ok(ToRenderer::Command(PageCommand::SetVisible(false)))));
        assert!(ToRenderer::decode(&seed("16_visibility_not_a_bool")).is_err());
        // The gap pass: a re-grant crosses as its kinds only.
        assert_eq!(ToRenderer::decode(&seed("17_grant_both")), Ok(ToRenderer::Grant(3)));
        assert!(ToRenderer::decode(&seed("18_grant_unknown_bit")).is_err());
        assert!(ToRenderer::decode(&seed("19_grant_as_command")).is_err(), "a grant is never a page command on the wire");
        assert_eq!(ToRenderer::decode(&seed("20_pointer_up")), Ok(ToRenderer::Command(PageCommand::Input(EngineInput::PointerUp { x: 3.0, y: 4.0 }))));
        // Milestone 41: storage served by the browser.
        assert_eq!(ToHost::decode(&seed("21_storage_set")), Ok(ToHost::Storage { id: 3, origin: "https://news.example".into(), op: StorageOp::Set("greeting".into(), "hello".into()) }));
        assert_eq!(ToHost::decode(&seed("22_storage_get")), Ok(ToHost::Storage { id: 4, origin: "https://news.example".into(), op: StorageOp::Get("greeting".into()) }));
        assert_eq!(ToRenderer::decode(&seed("23_storage_reply_value")), Ok(ToRenderer::StorageReply { id: 4, answer: StorageAnswer::Value(Some("hello".into())) }));
        assert_eq!(ToRenderer::decode(&seed("24_storage_reply_refused")), Ok(ToRenderer::StorageReply { id: 5, answer: StorageAnswer::Refused }));
        assert!(ToHost::decode(&seed("25_storage_unknown_op")).is_err());
        assert!(ToRenderer::decode(&seed("26_storage_huge_value")).is_err(), "longer than any area holds");
    }

    /// A page's re-grant leaves its handles on the host's side: the renderer
    /// receives the kinds and mints its own (ADR 010 §2).
    #[test]
    fn a_grant_command_crosses_as_its_kinds() {
        let authority = capability::Authority::new();
        let grants = capability::PageGrants { network: Some(authority.network(network::NetworkContext::new())), storage: None };
        let wire = ToRenderer::Command(PageCommand::Grant(grants)).encode();
        assert_eq!(ToRenderer::decode(&wire), Ok(ToRenderer::Grant(capability::CapabilityKind::Network.bit())));
    }

    #[test]
    fn hostile_messages_are_rejected_without_allocating_what_they_claim() {
        // A frame claiming four billion items in a few bytes.
        let mut w = Writer::default();
        w.u8(0);
        w.u32(0); // no events
        w.u8(1); // a frame
        w.f32(1.0);
        w.f32(1.0);
        w.buf.extend_from_slice(&[0, 0, 0, 0]);
        w.u32(u32::MAX);
        assert!(ToHost::decode(&w.into_bytes()).is_err());
        // An image whose pixels do not match its size, or is too big.
        assert!(ToHost::decode(&ToHost::Image { id: 1, width: 2, height: 2, rgba: vec![0; 3] }.encode()).is_err());
        assert!(ToHost::decode(&ToHost::Image { id: 1, width: MAX_IMAGE_SIDE + 1, height: 1, rgba: vec![] }.encode()).is_err());
        // A non-finite coordinate.
        let mut f = frame();
        f.items = vec![DisplayItem::Rect { x: f32::NAN, y: 0.0, width: 1.0, height: 1.0, color: f.background, radius: 0.0 }];
        let m = ToHost::Published { events: vec![], frame: Some(Box::new(f)), next_wake: None, completed_tick: 0 };
        assert!(ToHost::decode(&m.encode()).is_err());
        // An atlas band out of range.
        let mut f = frame();
        f.atlas_bands = vec![(99, vec![0; 512 * text::ATLAS_BAND_ROWS])];
        let m = ToHost::Published { events: vec![], frame: Some(Box::new(f)), next_wake: None, completed_tick: 0 };
        assert!(ToHost::decode(&m.encode()).is_err());
        // Trailing bytes, an unknown tag, every truncation of a valid message.
        let mut ok = ToHost::Crashed { reason: "x".into() }.encode();
        ok.push(0);
        assert!(ToHost::decode(&ok).is_err());
        assert!(ToHost::decode(&[250]).is_err());
        let full = ToHost::Published { events: vec![], frame: Some(Box::new(frame())), next_wake: Some(1), completed_tick: 2 }.encode();
        for cut in 0..full.len() {
            assert!(ToHost::decode(&full[..cut]).is_err(), "a message cut at {cut} bytes must not decode");
        }
    }

    /// The fuzz seeds (`fuzz/generate_seeds.py`) are real messages, so the
    /// fuzzer starts from valid protocol traffic.
    #[test]
    fn the_fuzz_seeds_are_valid_messages() {
        let host = [
            &include_bytes!("../../../fuzz/corpus/ipc_decode/01_crashed")[..],
            include_bytes!("../../../fuzz/corpus/ipc_decode/02_started"),
            include_bytes!("../../../fuzz/corpus/ipc_decode/03_image"),
            include_bytes!("../../../fuzz/corpus/ipc_decode/04_fetch_request"),
            include_bytes!("../../../fuzz/corpus/ipc_decode/05_published_frame"),
            include_bytes!("../../../fuzz/corpus/ipc_decode/10_compositing_frame"),
        ];
        for seed in host {
            ToHost::decode(seed).expect("a valid message to the host");
        }
        ToRenderer::decode(include_bytes!("../../../fuzz/corpus/ipc_decode/06_tick")).expect("a tick");
        ToRenderer::decode(include_bytes!("../../../fuzz/corpus/ipc_decode/07_fetch_reply")).expect("a fetch reply");
        let framed = read_frame(&mut &include_bytes!("../../../fuzz/corpus/ipc_decode/08_framed")[..]).unwrap().unwrap();
        ToHost::decode(&framed).expect("a framed message");
    }

    #[test]
    fn framing_refuses_oversized_and_truncated_frames() {
        let mut claim = Vec::new();
        claim.extend_from_slice(&(MAX_MESSAGE_BYTES as u32 + 1).to_le_bytes());
        assert!(read_frame(&mut &claim[..]).is_err());
        let mut short = Vec::new();
        short.extend_from_slice(&100u32.to_le_bytes());
        short.extend_from_slice(&[1, 2, 3]);
        assert!(read_frame(&mut &short[..]).is_err(), "a frame shorter than it claims");
        let mut good = Vec::new();
        write_frame(&mut good, b"hello").unwrap();
        let mut input = &good[..];
        assert_eq!(read_frame(&mut input).unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(read_frame(&mut input).unwrap(), None, "a clean end");
    }
}
