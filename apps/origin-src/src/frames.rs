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
//! damage, which each format handles its own way.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use origin_platform::Durability;
use origin_shared::{Digest, ObjectId, ObjectKind, RecordId, TreeId};

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
    /// It is not what was appended.
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
        mut decode: impl FnMut(&[u8]) -> Option<T>,
    ) -> (Vec<(u64, T)>, Tail) {
        let mut frames = Vec::new();
        let Some(mut rest) = bytes.strip_prefix(self.header) else {
            return (frames, Tail::Damaged);
        };
        while !rest.is_empty() {
            let offset = (bytes.len() - rest.len()) as u64;
            let frame = Self::payload(rest).and_then(|(payload, len)| {
                let frame = decode(payload).ok_or(Broken::Damaged)?;
                Ok((frame, len))
            });
            match frame {
                Ok((frame, len)) => {
                    frames.push((offset, frame));
                    rest = &rest[len..];
                }
                Err(Broken::Partial) => return (frames, Tail::Partial(offset)),
                Err(Broken::Damaged) => return (frames, Tail::Damaged),
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
        mut decode: impl FnMut(&[u8]) -> Option<T>,
        mut wanted: impl FnMut(&T) -> bool,
    ) -> Option<Vec<T>> {
        let header = self.header.len();
        let mut end = bytes.len();
        let mut frames = Vec::new();
        while end > header {
            let (_, len) = bytes[..end].split_last_chunk::<4>()?;
            let start = end.checked_sub(u32::from_le_bytes(*len) as usize + Self::FRAMING)?;
            if start < header {
                return None;
            }
            let (payload, _) = Self::payload(&bytes[start..end]).ok()?;
            let frame = decode(payload)?;
            if !wanted(&frame) {
                frames.reverse();
                return Some(frames);
            }
            frames.push(frame);
            end = start;
        }
        None
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
        let (frames, tail) = self.parse(&bytes, |payload| Some(payload.to_vec()));
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

/// Builds a frame's payload.
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
    pub(crate) fn sized(&mut self, bytes: &[u8]) {
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

    /// A record's kind and id.
    pub(crate) fn record(&mut self, record: RecordId) {
        self.u8(record.kind() as u8);
        self.bytes(ObjectId::from(record).as_bytes());
    }
}

/// A cursor over a frame's payload. Each read returns `None` if the payload
/// is malformed.
#[derive(Debug)]
pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl Reader<'_> {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (array, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*array)
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.array::<1>().map(|[byte]| byte)
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }

    /// What [`Writer::len`] wrote.
    pub(crate) fn len(&mut self) -> Option<usize> {
        usize::try_from(u32::from_le_bytes(self.array()?)).ok()
    }

    /// What [`Writer::sized`] wrote.
    pub(crate) fn sized(&mut self) -> Option<&[u8]> {
        let len = self.len()?;
        let (bytes, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        Some(bytes)
    }

    /// What [`Writer::option`] wrote.
    #[expect(
        clippy::option_option,
        reason = "like every read here, `None` is a malformed payload, and the value is an option"
    )]
    pub(crate) fn option<const N: usize>(&mut self) -> Option<Option<[u8; N]>> {
        match self.u8()? {
            0 => Some(None),
            1 => self.array().map(Some),
            _ => None,
        }
    }

    /// What [`Writer::record`] wrote.
    pub(crate) fn record(&mut self) -> Option<RecordId> {
        let kind = self.u8()?;
        let id = ObjectId::from_bytes(self.array()?);
        Some(match kind {
            k if k == ObjectKind::Tree as u8 => RecordId::Snapshot(TreeId::from(id)),
            k if k == ObjectKind::Revision as u8 => RecordId::Revision(id.into()),
            k if k == ObjectKind::Statement as u8 => RecordId::Statement(id.into()),
            k if k == ObjectKind::Acceptance as u8 => RecordId::Acceptance(id.into()),
            _ => return None,
        })
    }
}
