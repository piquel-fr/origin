//! Append-only files of checked frames, which the [record index](crate::index)
//! and the [op log](crate::oplog) are made of.
//!
//! A file is a header naming its format and version, then frames. A frame is
//! its payload's `u32` length, the payload, the first 8 bytes of the
//! payload's BLAKE3 hash, and the length again, so the last frame can be
//! checked from the end without reading the rest.
//!
//! Appenders hold an exclusive lock and write whole frames at the end. A
//! crash in the middle of an append leaves part of a frame at the end, a
//! partial tail: readers skip it, and the next append cuts it off first, so
//! a partial frame is only ever last. Anything else that fails its check is
//! damage, which each format handles its own way, including a length that
//! runs past the end of the file when a frame written in full ends it: an
//! append cut short never leaves one.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use origin_platform::Durability;
use origin_shared::Digest;
use origin_shared::object::DecodeError;

/// The format of a file of frames: its header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frames {
    header: &'static [u8],
}

/// How a file ends after its last intact frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tail {
    /// It ends there.
    Clean,
    /// With part of a frame, at this offset: an append in progress, or one a
    /// crash cut short.
    Partial(u64),
    /// With damage, or a header from another format or version.
    Damaged,
}

/// Why a frame cannot be read.
enum Broken {
    /// The bytes end before it does.
    Partial,
    /// It is not what was appended: it fails its check, or its payload
    /// doesn't decode. Each format handles damage the same way whatever
    /// the reason.
    Damaged,
}

impl Frames {
    /// The length of a frame's check.
    const CHECK: usize = 8;

    /// The bytes a frame adds to its payload: two lengths and a check.
    pub(crate) const FRAMING: usize = 4 + Self::CHECK + 4;

    /// The format whose files start with `header`.
    pub(crate) const fn new(header: &'static [u8]) -> Self {
        Self { header }
    }

    /// The header every file of the format starts with.
    pub(crate) fn header(self) -> &'static [u8] {
        self.header
    }

    /// Encodes the frame of the payload `write` writes.
    pub(crate) fn frame(write: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut out = Writer(vec![0; 4]);
        write(&mut out);
        let mut out = out.0;
        let len = u32::try_from(out.len() - 4)
            .expect("frames are under 4 GiB")
            .to_le_bytes();
        out[..4].copy_from_slice(&len);
        let check = Self::check(&out[4..]);
        out.extend_from_slice(&check);
        out.extend_from_slice(&len);
        out
    }

    fn check(payload: &[u8]) -> [u8; Self::CHECK] {
        Digest::of(payload).as_bytes()[..Self::CHECK]
            .try_into()
            .expect("a digest is longer than a check")
    }

    /// The frames of a file, decoded by `decode`, and their offsets, up to
    /// the first broken one, and how the file ends after them. A frame that
    /// doesn't decode is damage.
    pub(crate) fn parse<T>(
        self,
        bytes: &[u8],
        mut decode: impl FnMut(&[u8]) -> Result<T, DecodeError>,
    ) -> (Vec<(u64, T)>, Tail) {
        let mut frames = Vec::new();
        let Some(mut rest) = bytes.strip_prefix(self.header) else {
            return (frames, Tail::Damaged);
        };
        while !rest.is_empty() {
            let offset = (bytes.len() - rest.len()) as u64;
            let frame = Self::payload(rest).and_then(|(payload, len)| {
                let frame = decode(payload).map_err(|_| Broken::Damaged)?;
                Ok((frame, len))
            });
            match frame {
                Ok((frame, len)) => {
                    frames.push((offset, frame));
                    rest = &rest[len..];
                }
                Err(Broken::Partial) if !self.ends_with_frame(bytes, offset) => {
                    return (frames, Tail::Partial(offset));
                }
                Err(_) => return (frames, Tail::Damaged),
            }
        }
        (frames, Tail::Clean)
    }

    /// The frames at the end of a file, decoded by `decode`, oldest first:
    /// read backwards from the end, through the last frame `wanted` is true
    /// of. Returns `None` if a frame on the way is broken, or if the file
    /// has no such frame: then only [`Frames::parse`] can read it.
    pub(crate) fn parse_back<T>(
        self,
        bytes: &[u8],
        decode: impl FnMut(&[u8]) -> Result<T, DecodeError>,
        wanted: impl FnMut(&T) -> bool,
    ) -> Option<Vec<T>> {
        let (frames, unwanted) = self.back(bytes, decode, wanted);
        unwanted.then_some(frames)
    }

    /// The intact frames at the end of a file, decoded by `decode`, oldest
    /// first: read backwards from the end, up to a broken one or the
    /// header. They are those after damage that [`Frames::parse`] stops at.
    pub(crate) fn parse_tail<T>(
        self,
        bytes: &[u8],
        decode: impl FnMut(&[u8]) -> Result<T, DecodeError>,
    ) -> Vec<T> {
        self.back(bytes, decode, |_| true).0
    }

    /// Reads frames backwards from the end of `bytes`, oldest first, while
    /// `wanted` is true of them, up to a broken one or the header. Returns
    /// them, and whether it stopped at one `wanted` is false of.
    fn back<T>(
        self,
        bytes: &[u8],
        mut decode: impl FnMut(&[u8]) -> Result<T, DecodeError>,
        mut wanted: impl FnMut(&T) -> bool,
    ) -> (Vec<T>, bool) {
        let mut end = bytes.len();
        let mut frames = Vec::new();
        let unwanted = loop {
            let Some((start, frame)) = self.frame_before(bytes, end, &mut decode) else {
                break false;
            };
            if !wanted(&frame) {
                break true;
            }
            frames.push(frame);
            end = start;
        };
        frames.reverse();
        (frames, unwanted)
    }

    /// The frame of `bytes` that ends at `end`, decoded by `decode`, and
    /// where it starts, if it's intact and after the header.
    fn frame_before<T>(
        self,
        bytes: &[u8],
        end: usize,
        decode: &mut impl FnMut(&[u8]) -> Result<T, DecodeError>,
    ) -> Option<(usize, T)> {
        let (_, len) = bytes[..end].split_last_chunk::<4>()?;
        let start = end.checked_sub(u32::from_le_bytes(*len) as usize + Self::FRAMING)?;
        if start < self.header.len() {
            return None;
        }
        let (payload, _) = Self::payload(&bytes[start..end]).ok()?;
        Some((start, decode(payload).ok()?))
    }

    /// Whether `bytes` end with a frame written in full that starts at or
    /// after `offset`, whose trailing length and check are intact. An
    /// append cut short never leaves one after it, so a frame at `offset`
    /// that seems to run past the end has a damaged length instead. Its
    /// leading length isn't checked: it may be the damaged one.
    fn ends_with_frame(self, bytes: &[u8], offset: u64) -> bool {
        let Some((rest, len)) = bytes.split_last_chunk::<4>() else {
            return false;
        };
        let len = u32::from_le_bytes(*len) as usize;
        let Some(start) = bytes.len().checked_sub(len + Self::FRAMING) else {
            return false;
        };
        if (start as u64) < offset || start < self.header.len() {
            return false;
        }
        let (payload, check) = rest[start + 4..].split_at(len);
        *check == Self::check(payload)
    }

    /// Checks the frame `bytes` start with, returning its payload and its
    /// length.
    fn payload(bytes: &[u8]) -> Result<(&[u8], usize), Broken> {
        let (len, rest) = bytes.split_first_chunk::<4>().ok_or(Broken::Partial)?;
        let (payload, rest) = rest
            .split_at_checked(u32::from_le_bytes(*len) as usize)
            .ok_or(Broken::Partial)?;
        let (check, rest) = rest
            .split_first_chunk::<{ Self::CHECK }>()
            .ok_or(Broken::Partial)?;
        let (end, _) = rest.split_first_chunk::<4>().ok_or(Broken::Partial)?;
        if end != len || *check != Self::check(payload) {
            return Err(Broken::Damaged);
        }
        Ok((payload, payload.len() + Self::FRAMING))
    }

    /// The payload of the last frame of `file`, or `None` if it holds only
    /// the header, after cutting off a partial frame a crash left, durably.
    /// Reads only the last frame, backwards from its trailing length, unless
    /// it's broken.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::InvalidData`] if the file is damaged, or
    /// if it cannot be read or cut.
    pub(crate) fn last(
        self,
        platform: &impl Durability,
        file: &File,
    ) -> io::Result<Option<Vec<u8>>> {
        if let Some(last) = self.peek_last(file)? {
            return Ok(last);
        }
        let mut bytes = Vec::new();
        let mut reader = file;
        reader.seek(SeekFrom::Start(0))?;
        reader.read_to_end(&mut bytes)?;
        let (frames, tail) = self.parse(&bytes, |payload| Ok(payload.to_vec()));
        match tail {
            Tail::Clean => {}
            Tail::Partial(offset) => {
                file.set_len(offset)?;
                platform.sync_file(file)?;
            }
            Tail::Damaged => return Err(io::ErrorKind::InvalidData.into()),
        }
        Ok(frames.into_iter().last().map(|(_, payload)| payload))
    }

    /// The last frame's payload, `Some(None)` for a file holding only the
    /// header, or `None` if the file doesn't end with an intact frame. Reads
    /// only the last frame, and never writes.
    #[expect(
        clippy::option_option,
        reason = "a file with no frame, and one that doesn't end with one, differ"
    )]
    pub(crate) fn peek_last(self, mut file: &File) -> io::Result<Option<Option<Vec<u8>>>> {
        let len = file.metadata()?.len();
        let header = self.header.len() as u64;
        if len <= header {
            return Ok((len == header).then_some(None));
        }
        let mut end = [0; 4];
        file.seek(SeekFrom::Start(len - 4))?;
        file.read_exact(&mut end)?;
        let frame = u64::from(u32::from_le_bytes(end)) + Self::FRAMING as u64;
        if frame > len - header {
            return Ok(None);
        }
        let mut bytes = vec![0; usize::try_from(frame).map_err(io::Error::other)?];
        file.seek(SeekFrom::Start(len - frame))?;
        file.read_exact(&mut bytes)?;
        Ok(Self::payload(&bytes)
            .ok()
            .map(|(payload, _)| Some(payload.to_vec())))
    }

    /// Appends `frame` to the locked `file`, opened for appending, without
    /// syncing it, returning its offset. A partial frame a crash left is cut
    /// off first; damage is left for readers to find.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read, cut or written.
    pub(crate) fn append(
        self,
        platform: &impl Durability,
        file: &File,
        frame: &[u8],
    ) -> io::Result<u64> {
        match self.last(platform, file) {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
            result => {
                result?;
            }
        }
        let offset = file.metadata()?.len();
        if let Err(error) = (&*file).write_all(frame) {
            // Best effort: the next append cuts it off otherwise.
            let _ = file.set_len(offset);
            return Err(error);
        }
        Ok(offset)
    }
}

/// Builds a frame's payload, which a
/// [`Reader`](origin_shared::object::Reader) reads.
#[derive(Debug)]
pub(crate) struct Writer(Vec<u8>);

impl Writer {
    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }

    /// A `u32` count, of items or bytes.
    pub(crate) fn len(&mut self, len: usize) {
        self.bytes(&u32::try_from(len).expect("fewer than 2^32").to_le_bytes());
    }

    /// Bytes preceded by their length.
    pub(crate) fn prefixed(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.bytes(bytes);
    }

    /// A 0 byte for `None`, or a 1 byte and the id.
    pub(crate) fn option(&mut self, id: Option<&[u8; 32]>) {
        match id {
            None => self.u8(0),
            Some(id) => {
                self.u8(1);
                self.bytes(id);
            }
        }
    }
}
