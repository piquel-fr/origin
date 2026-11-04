//! Pack files: small objects gathered into a few large files, so a store of
//! a million statements holds a handful of files rather than a million.
//!
//! `objects/packs/` holds:
//!
//! - `<id>.pack`: [`Pack::MAGIC`], then each object as the `u32` length of
//!   its compressed encoding and the encoding, compressed as in a loose
//!   file. A pack can be read on its own, start to end.
//! - `<id>.idx`: [`Pack::INDEX_MAGIC`], the ids of the pack's objects in
//!   ascending order, then where each one's compressed encoding is in the
//!   pack and when this node stored the object: a `u64` offset, a `u32`
//!   length and a `u64` time in seconds since the Unix epoch, the
//!   modification time of the object's loose file. The pack's id is the
//!   hash of its object ids, which opening the pack verifies.
//! - `manifest`: the ids of the packs in use, one per line in hex, largest
//!   pack first. Replacing it publishes and retires packs at once.
//! - `lock`: held by the compactor, so only one ever runs.
//!
//! Integers are little endian.
//!
//! [`Store::compact`] gathers the small loose objects into a new pack, and
//! merges into it every pack smaller than twice what it holds so far, so a
//! store keeps a logarithmic number of packs and an object is rewritten a
//! logarithmic number of times. It writes the pack and its index durably,
//! then the manifest, and only then removes the loose files and the packs
//! it merged. A crash loses nothing: until the manifest is published, every
//! object is still where it was, and the next compaction removes the files
//! the manifest doesn't name.
//!
//! [`Store::compact_if_due`] compacts once enough loose objects pile up,
//! for a long-running process to call as it writes.
//!
//! Readers look in the packs they loaded last, and reload the manifest when
//! an object isn't there. An object in neither the loose objects nor the
//! current manifest's packs is not in the store, since a pack is published
//! before what it replaces is removed. A pack stays readable once opened,
//! even after it's retired, because an open file outlives its name, so
//! readers also reload the manifest every few seconds, to let go of the
//! packs it no longer names.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use origin_platform::{LockMode, LockedFile, Platform, ReadAt};
use origin_shared::{Digest, ObjectId};

use super::{Store, StoreError};

/// The packs a store loaded last, shared by its clones.
#[derive(Default)]
pub(super) struct Packs {
    manifest: RwLock<Manifest>,
    /// When the manifest was last read.
    read_at: Mutex<Option<Instant>>,
}

/// A manifest and the packs it names, open.
#[derive(Default)]
struct Manifest {
    /// The manifest's contents, or `None` until it's first read.
    bytes: Option<Vec<u8>>,
    packs: Vec<Arc<Pack>>,
}

impl Packs {
    /// How long a store goes by the manifest it read before its next lookup
    /// in the packs reads it again, so that a long-running process lets go
    /// of the packs another process's compaction retired.
    const RELOAD_AFTER: Duration = Duration::from_secs(10);

    /// The pack holding `id` among those loaded, and where it is.
    pub(super) fn find(&self, id: ObjectId) -> Option<(Arc<Pack>, Location)> {
        let manifest = self.manifest.read().unwrap_or_else(PoisonError::into_inner);
        manifest
            .packs
            .iter()
            .find_map(|pack| Some((Arc::clone(pack), pack.find(id)?)))
    }

    fn loaded(&self) -> Vec<Arc<Pack>> {
        let manifest = self.manifest.read().unwrap_or_else(PoisonError::into_inner);
        manifest.packs.clone()
    }

    /// Whether the manifest is due to be read again.
    fn stale(&self) -> bool {
        let read_at = self.read_at.lock().unwrap_or_else(PoisonError::into_inner);
        read_at.is_none_or(|at| at.elapsed() >= Self::RELOAD_AFTER)
    }
}

impl fmt::Debug for Packs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.loaded()).finish()
    }
}

/// One pack and its index, open for reading.
pub(super) struct Pack {
    id: Digest,
    /// The pack file's path, for error messages.
    path: PathBuf,
    file: File,
    /// The index file's contents.
    index: Vec<u8>,
}

/// Where a compressed encoding is in a pack, and when this node stored the
/// object, which packing keeps so retention counts from the first write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Location {
    offset: u64,
    len: u32,
    /// Seconds since the Unix epoch.
    stored: u64,
}

impl Location {
    /// The length of an encoded location.
    const LEN: usize = 8 + 4 + 8;

    fn encode(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.stored.to_le_bytes());
    }

    fn decode(bytes: &[u8; Self::LEN]) -> Self {
        let (offset, rest) = bytes.split_at(8);
        let (len, stored) = rest.split_at(4);
        Self {
            offset: u64::from_le_bytes(offset.try_into().expect("8 bytes")),
            len: u32::from_le_bytes(len.try_into().expect("4 bytes")),
            stored: u64::from_le_bytes(stored.try_into().expect("8 bytes")),
        }
    }

    /// When this node stored the object.
    pub(super) fn stored(self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.stored)
    }
}

impl Pack {
    /// How a pack file starts: its format and version.
    const MAGIC: [u8; 8] = *b"opack\0\0\x01";

    /// How an index file starts: its format and version.
    const INDEX_MAGIC: [u8; 8] = *b"oindex\0\x01";

    /// The bytes an object takes in an index.
    const ENTRY: usize = ObjectId::LEN + Location::LEN;

    /// The path of the pack `id`'s file with `extension`, `pack` or `idx`.
    fn file_path(dir: &Path, id: Digest, extension: &str) -> PathBuf {
        dir.join(format!("{id}.{extension}"))
    }

    /// The id of a pack holding `ids`, which are sorted.
    fn id_of(ids: &[u8]) -> Digest {
        Digest::of(ids)
    }

    /// Opens the pack `id` in `dir`, or returns `None` if its files are
    /// gone.
    fn open(platform: &impl ReadAt, dir: &Path, id: Digest) -> Result<Option<Self>, StoreError> {
        let index_path = Self::file_path(dir, id, "idx");
        let path = Self::file_path(dir, id, "pack");
        let (index, file) = match fs::read(&index_path).and_then(|index| {
            let file = File::open(&path)?;
            Ok((index, file))
        }) {
            Ok(opened) => opened,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::io(&index_path, source)),
        };
        let pack = Self {
            id,
            path,
            file,
            index,
        };
        if !pack.index_is_valid() {
            return Err(StoreError::Malformed(index_path));
        }
        let mut magic = [0; Self::MAGIC.len()];
        platform
            .read_exact_at(&pack.file, &mut magic, 0)
            .map_err(|source| StoreError::io(&pack.path, source))?;
        if magic != Self::MAGIC {
            return Err(StoreError::Malformed(pack.path));
        }
        Ok(Some(pack))
    }

    /// Whether the index has the right shape, ascending ids, and the ids
    /// the pack's id says.
    fn index_is_valid(&self) -> bool {
        let Some(tables) = self.index.strip_prefix(&Self::INDEX_MAGIC) else {
            return false;
        };
        tables.len().is_multiple_of(Self::ENTRY)
            && self.ids().is_sorted_by(|a, b| a < b)
            && Self::id_of(self.ids().as_flattened()) == self.id
    }

    /// How many objects the pack holds.
    fn len(&self) -> usize {
        (self.index.len() - Self::INDEX_MAGIC.len()) / Self::ENTRY
    }

    fn ids(&self) -> &[[u8; ObjectId::LEN]] {
        let start = Self::INDEX_MAGIC.len();
        self.index[start..start + self.len() * ObjectId::LEN]
            .as_chunks()
            .0
    }

    fn locations(&self) -> &[[u8; Location::LEN]] {
        let start = Self::INDEX_MAGIC.len() + self.len() * ObjectId::LEN;
        self.index[start..].as_chunks().0
    }

    /// Where the pack holds `id`, if it does.
    fn find(&self, id: ObjectId) -> Option<Location> {
        let index = self.ids().binary_search(id.as_bytes()).ok()?;
        Some(Location::decode(&self.locations()[index]))
    }

    /// Every object of the pack, in ascending order.
    fn entries(&self) -> impl Iterator<Item = (ObjectId, Location)> {
        self.ids()
            .iter()
            .zip(self.locations())
            .map(|(id, location)| (ObjectId::from_bytes(*id), Location::decode(location)))
    }

    /// Reads the compressed encoding at `location`.
    pub(super) fn read(
        &self,
        platform: &impl ReadAt,
        location: Location,
    ) -> Result<Vec<u8>, StoreError> {
        let mut bytes = vec![0; location.len as usize];
        platform
            .read_exact_at(&self.file, &mut bytes, location.offset)
            .map_err(|source| StoreError::io(&self.path, source))?;
        Ok(bytes)
    }

    /// The pack file's path.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for Pack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pack")
            .field("id", &self.id)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

/// Where an object written to a new pack comes from.
#[derive(Clone, Copy)]
enum Source {
    /// Its loose file.
    Loose,
    /// The merged pack at this position, at a location.
    Packed(usize, Location),
}

/// What a compaction did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compaction {
    /// Loose objects moved into the new pack.
    pub packed: usize,
    /// Packs merged into it.
    pub merged: usize,
    /// Loose objects removed because a pack already held them, left by a
    /// writer that didn't know of the pack yet, or by a crash.
    pub duplicates: usize,
}

impl<P: Platform> Store<P> {
    /// The largest loose file compaction packs. Larger objects are mostly
    /// file contents, which a pack merge would copy over and over.
    const PACKED_MAX: u64 = 16 * 1024;

    /// How many small loose objects make a compaction due, see
    /// [`Store::compact_if_due`]: about as many as git lets pile up.
    const COMPACT_AFTER: usize = 8 * 1024;

    /// How many objects a compaction reads at once, from every core, before
    /// appending them to the pack.
    const READ_CHUNK: usize = 16 * 1024;

    /// The packs directory's files besides the packs.
    const MANIFEST: &str = "manifest";
    const LOCK: &str = "lock";

    pub(super) fn packs_dir(&self) -> PathBuf {
        self.root.join("objects/packs")
    }

    /// The pack holding `id`, reloading the manifest if no pack loaded
    /// holds it, or if it was read too long ago.
    pub(super) fn find_packed(
        &self,
        id: ObjectId,
    ) -> Result<Option<(Arc<Pack>, Location)>, StoreError> {
        if self.packs.stale() {
            self.reload_packs()?;
        }
        if let Some(found) = self.packs.find(id) {
            return Ok(Some(found));
        }
        self.reload_packs()?;
        Ok(self.packs.find(id))
    }

    /// The id of every packed object, as of the current manifest.
    pub(super) fn packed_ids(&self) -> Result<Vec<ObjectId>, StoreError> {
        self.reload_packs()?;
        Ok(self
            .packs
            .loaded()
            .iter()
            .flat_map(|pack| pack.entries().map(|(id, _)| id))
            .collect())
    }

    /// Loads the manifest's packs, unless they're already loaded.
    pub(super) fn reload_packs(&self) -> Result<(), StoreError> {
        let dir = self.packs_dir();
        let path = dir.join(Self::MANIFEST);
        let mut missing_from = None;
        loop {
            *self
                .packs
                .read_at
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(source) => return Err(StoreError::io(&path, source)),
            };
            let current = |manifest: &Manifest| manifest.bytes.as_ref() == Some(&bytes);
            if current(
                &self
                    .packs
                    .manifest
                    .read()
                    .unwrap_or_else(PoisonError::into_inner),
            ) {
                return Ok(());
            }
            let mut manifest = self
                .packs
                .manifest
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if current(&manifest) {
                return Ok(());
            }
            if let Some(packs) = self.open_packs(&dir, &bytes, &manifest.packs)? {
                *manifest = Manifest {
                    bytes: Some(bytes),
                    packs,
                };
                return Ok(());
            }
            // A compactor retires packs only after publishing a manifest
            // without them, so if the manifest didn't change, the pack is
            // lost.
            if missing_from.as_ref() == Some(&bytes) {
                return Err(StoreError::Malformed(path));
            }
            missing_from = Some(bytes);
        }
    }

    /// Opens the packs a manifest names, reusing those already open, or
    /// returns `None` if one is gone.
    fn open_packs(
        &self,
        dir: &Path,
        manifest: &[u8],
        open: &[Arc<Pack>],
    ) -> Result<Option<Vec<Arc<Pack>>>, StoreError> {
        let malformed = || StoreError::Malformed(dir.join(Self::MANIFEST));
        let mut packs = Vec::new();
        for line in str::from_utf8(manifest).map_err(|_| malformed())?.lines() {
            let id: Digest = line.parse().map_err(|_| malformed())?;
            let pack = match open.iter().find(|pack| pack.id == id) {
                Some(pack) => Arc::clone(pack),
                None => match Pack::open(&self.platform, dir, id)? {
                    Some(pack) => Arc::new(pack),
                    None => return Ok(None),
                },
            };
            packs.push(pack);
        }
        Ok(Some(packs))
    }

    /// Moves the small loose objects into a new pack, merging packs into it
    /// as the [module documentation](self) describes. Readers and writers
    /// may use the store meanwhile, from any process, so a compaction can
    /// run in the background.
    ///
    /// Returns `None` without doing anything if another compaction is
    /// running.
    ///
    /// # Errors
    ///
    /// Fails if the store's files cannot be read, written or removed, or a
    /// pack is malformed. Whatever the failure, no object is lost.
    pub fn compact(&self) -> Result<Option<Compaction>, StoreError> {
        let Some(_lock) = self.lock_compactor()? else {
            return Ok(None);
        };
        self.reload_packs()?;
        self.compact_locked()
    }

    /// Compacts like [`Store::compact`] if the store holds more than
    /// [`Store::COMPACT_AFTER`] small loose objects, estimated from one
    /// shard of the 256, as git estimates its loose objects. Cheap enough to
    /// call after every few writes: it lists that one shard.
    ///
    /// Returns `None` if the compaction isn't due, or another is running.
    ///
    /// # Errors
    ///
    /// Fails like [`Store::compact`].
    pub fn compact_if_due(&self) -> Result<Option<Compaction>, StoreError> {
        let Some(_lock) = self.lock_compactor()? else {
            return Ok(None);
        };
        self.reload_packs()?;
        let (sampled, _) = self.small_loose(&self.packs.loaded(), &[0])?;
        if sampled.len() * 256 <= Self::COMPACT_AFTER {
            return Ok(None);
        }
        self.compact_locked()
    }

    /// Takes the compactor's lock, or returns `None` if another compactor
    /// holds it.
    fn lock_compactor(&self) -> Result<Option<LockedFile>, StoreError> {
        let lock = self.packs_dir().join(Self::LOCK);
        Ok(LockedFile::try_open(
            &self.platform,
            &lock,
            LockMode::Exclusive,
        )?)
    }

    /// [`Store::compact`], holding the lock, with the packs reloaded.
    fn compact_locked(&self) -> Result<Option<Compaction>, StoreError> {
        let dir = self.packs_dir();
        let packs = self.packs.loaded();
        Self::remove_unlisted(&dir, &packs)?;
        let shards: Vec<u8> = (0..=u8::MAX).collect();
        let (loose, duplicates) = self.small_loose(&packs, &shards)?;
        let mut compaction = Compaction {
            duplicates,
            ..Compaction::default()
        };
        if loose.is_empty() {
            return Ok(Some(compaction));
        }

        // Merge the packs smaller than twice the new pack, smallest first.
        let mut by_len = packs;
        by_len.sort_by_key(|pack| pack.len());
        let mut len = loose.len();
        let merging = by_len
            .iter()
            .take_while(|pack| {
                let merge = pack.len() < 2 * len;
                len += if merge { pack.len() } else { 0 };
                merge
            })
            .count();
        let (merged, kept) = by_len.split_at(merging);

        let mut sources: Vec<_> = loose.iter().map(|&id| (id, Source::Loose)).collect();
        for (position, pack) in merged.iter().enumerate() {
            sources.extend(
                pack.entries()
                    .map(|(id, location)| (id, Source::Packed(position, location))),
            );
        }
        sources.sort_unstable_by_key(|&(id, _)| id);
        sources.dedup_by_key(|&mut (id, _)| id);
        self.replace_packs(&dir, kept, merged, &sources)?;

        // Everything below is published in the new pack. A crash before
        // these files are gone leaves duplicates for the next compaction.
        Self::on_every_core(&loose, |ids| {
            ids.iter()
                .try_for_each(|&id| Self::remove_file(&self.path(id)))
        })
        .into_iter()
        .collect::<Result<(), _>>()?;
        compaction.packed = loose.len();
        compaction.merged = merged.len();
        Ok(Some(compaction))
    }

    /// Removes `ids` from the packs, rewriting each pack that holds one
    /// into a single new pack without them, and returns the bytes of
    /// compressed encodings that freed. Waits for a compaction running.
    ///
    /// # Errors
    ///
    /// Fails like [`Store::compact`].
    pub(super) fn remove_packed(&self, ids: &HashSet<ObjectId>) -> Result<u64, StoreError> {
        let dir = self.packs_dir();
        let _lock = LockedFile::open(&self.platform, &dir.join(Self::LOCK), LockMode::Exclusive)?;
        self.reload_packs()?;
        let (rewritten, kept): (Vec<_>, Vec<_>) = self
            .packs
            .loaded()
            .into_iter()
            .partition(|pack| pack.entries().any(|(id, _)| ids.contains(&id)));
        if rewritten.is_empty() {
            return Ok(0);
        }
        let mut freed = 0;
        let mut sources = Vec::new();
        for (position, pack) in rewritten.iter().enumerate() {
            for (id, location) in pack.entries() {
                if ids.contains(&id) {
                    freed += u64::from(location.len);
                } else {
                    sources.push((id, Source::Packed(position, location)));
                }
            }
        }
        sources.sort_unstable_by_key(|&(id, _)| id);
        sources.dedup_by_key(|&mut (id, _)| id);
        self.replace_packs(&dir, &kept, &rewritten, &sources)?;
        Ok(freed)
    }

    /// Publishes a manifest naming `kept` and a new pack of `sources`,
    /// sorted by id, unless there are none, then removes the files of
    /// `retired`. Only the compactor writes packs, so it must hold the
    /// lock.
    fn replace_packs(
        &self,
        dir: &Path,
        kept: &[Arc<Pack>],
        retired: &[Arc<Pack>],
        sources: &[(ObjectId, Source)],
    ) -> Result<(), StoreError> {
        let mut manifest: Vec<_> = kept.iter().map(|pack| (pack.len(), pack.id)).collect();
        if !sources.is_empty() {
            manifest.push((sources.len(), self.write_pack(dir, sources, retired)?));
        }
        manifest.sort_by_key(|&(len, _)| std::cmp::Reverse(len));
        let manifest: String = manifest.iter().map(|(_, id)| id.to_hex() + "\n").collect();
        let write = self.durable_write();
        write.stage(dir.join(Self::MANIFEST), manifest.as_bytes())?;
        write.commit()?;
        self.reload_packs()?;
        // A crash before these files are gone leaves them for the next
        // compaction.
        for pack in retired {
            for extension in ["idx", "pack"] {
                Self::remove_file(&Pack::file_path(dir, pack.id, extension))?;
            }
        }
        Ok(())
    }

    /// Writes a pack of `sources`, sorted by id, and its index, durably,
    /// returning its id.
    fn write_pack(
        &self,
        dir: &Path,
        sources: &[(ObjectId, Source)],
        merged: &[Arc<Pack>],
    ) -> Result<Digest, StoreError> {
        let mut index = Vec::with_capacity(Pack::INDEX_MAGIC.len() + sources.len() * Pack::ENTRY);
        index.extend_from_slice(&Pack::INDEX_MAGIC);
        for (id, _) in sources {
            index.extend_from_slice(id.as_bytes());
        }
        let id = Pack::id_of(&index[Pack::INDEX_MAGIC.len()..]);

        let write = self.durable_write();
        write.stage_with(Pack::file_path(dir, id, "pack"), |file| {
            file.write_all(&Pack::MAGIC)?;
            let mut offset = Pack::MAGIC.len() as u64;
            for chunk in sources.chunks(Self::READ_CHUNK) {
                let read = Self::on_every_core(chunk, |sources| {
                    sources
                        .iter()
                        .map(|&(id, source)| self.read_source(id, source, merged))
                        .collect::<Result<Vec<_>, _>>()
                });
                for read in read {
                    for (len, stored, bytes) in read? {
                        file.write_all(&len.to_le_bytes())?;
                        file.write_all(&bytes)?;
                        offset += 4;
                        Location {
                            offset,
                            len,
                            stored,
                        }
                        .encode(&mut index);
                        offset += u64::from(len);
                    }
                }
            }
            Ok::<_, StoreError>(())
        })?;
        write.stage(Pack::file_path(dir, id, "idx"), &index)?;
        write.commit()?;
        Ok(id)
    }

    /// Reads the compressed encoding of `id` from `source`, with its length
    /// and when this node stored it, in seconds since the Unix epoch.
    fn read_source(
        &self,
        id: ObjectId,
        source: Source,
        merged: &[Arc<Pack>],
    ) -> Result<(u32, u64, Vec<u8>), StoreError> {
        match source {
            Source::Loose => {
                let path = self.path(id);
                let io_error = |source| StoreError::io(&path, source);
                let mut file = File::open(&path).map_err(io_error)?;
                let modified = file.metadata().and_then(|metadata| metadata.modified());
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map_err(io_error)?;
                let len = u32::try_from(bytes.len())
                    .map_err(|_| io_error(io::ErrorKind::FileTooLarge.into()))?;
                // A time before the epoch counts as the epoch.
                let stored = modified
                    .map_err(io_error)?
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs());
                Ok((len, stored, bytes))
            }
            Source::Packed(pack, location) => Ok((
                location.len,
                location.stored,
                merged[pack].read(&self.platform, location)?,
            )),
        }
    }

    /// Removes the files of the packs directory that no pack of `packs`
    /// owns: those of compactions that crashed before publishing, and of
    /// packs retired by compactions that crashed before removing them. Only
    /// the compactor writes there, so it must hold the lock.
    fn remove_unlisted(dir: &Path, packs: &[Arc<Pack>]) -> Result<(), StoreError> {
        let mut keep = vec![Self::MANIFEST.to_owned(), Self::LOCK.to_owned()];
        for pack in packs {
            keep.extend(["idx", "pack"].map(|extension| format!("{}.{extension}", pack.id)));
        }
        for entry in fs::read_dir(dir).map_err(|source| StoreError::io(dir, source))? {
            let entry = entry.map_err(|source| StoreError::io(dir, source))?;
            if !keep.iter().any(|name| entry.file_name() == name.as_str()) {
                Self::remove_file(&entry.path())?;
            }
        }
        Ok(())
    }

    /// The small loose objects of `shards` that no pack of `packs` holds,
    /// and how many loose duplicates of packed objects it removed.
    fn small_loose(
        &self,
        packs: &[Arc<Pack>],
        shards: &[u8],
    ) -> Result<(Vec<ObjectId>, usize), StoreError> {
        let found = Self::on_every_core(shards, |shards| {
            let mut small = Vec::new();
            let mut duplicates = 0;
            for shard in shards {
                let dir = self.loose().join(format!("{shard:02x}"));
                let io_error = |source| StoreError::io(&dir, source);
                for entry in fs::read_dir(&dir).map_err(io_error)? {
                    let entry = entry.map_err(io_error)?;
                    // Skips temporary files, and any a user left there.
                    let Some(Ok(id)) = entry
                        .file_name()
                        .to_str()
                        .map(|name| format!("{shard:02x}{name}").parse::<ObjectId>())
                    else {
                        continue;
                    };
                    if packs.iter().any(|pack| pack.find(id).is_some()) {
                        Self::remove_file(&entry.path())?;
                        duplicates += 1;
                    } else if entry.metadata().map_err(io_error)?.len() <= Self::PACKED_MAX {
                        small.push(id);
                    }
                }
            }
            Ok::<_, StoreError>((small, duplicates))
        });
        found
            .into_iter()
            .try_fold((Vec::new(), 0), |(mut small, duplicates), found| {
                let (more, more_duplicates) = found?;
                small.extend(more);
                Ok((small, duplicates + more_duplicates))
            })
    }

    /// Removes a file, unless it's already gone.
    fn remove_file(path: &Path) -> Result<(), StoreError> {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(StoreError::io(path, error))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::slice;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    use origin_platform::{CloneFile, Durability, Error, FileLock, FileSystem, FsKind, Native, Op};
    use origin_shared::EncodedObject;

    use super::super::tests::{Recorder, Synced, store};
    use super::*;

    /// `count` distinct small objects, starting from `seed`.
    fn objects(seed: u32, count: u32) -> Vec<EncodedObject> {
        (seed..seed + count)
            .map(|i| EncodedObject::blob(format!("statement {i}").as_bytes()))
            .collect()
    }

    fn assert_readable<P: Platform>(store: &Store<P>, objects: &[EncodedObject]) {
        for object in objects {
            assert_eq!(&store.read(object.id()).unwrap(), object);
        }
    }

    /// The files of the packs directory, sorted.
    fn pack_files<P: Platform>(store: &Store<P>) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(store.packs_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn packs_small_objects_and_leaves_large_ones_loose() {
        let (_dir, store) = store();
        let small = objects(0, 100);
        // Hashes don't compress, so this stays above the limit.
        let large = EncodedObject::blob(
            &(0..1024_u32)
                .flat_map(|i| *Digest::of(&i.to_le_bytes()).as_bytes())
                .collect::<Vec<_>>(),
        );
        store.write_all(&small).unwrap();
        store.write_all(slice::from_ref(&large)).unwrap();

        let compaction = store.compact().unwrap().unwrap();
        assert_eq!(
            compaction,
            Compaction {
                packed: 100,
                merged: 0,
                duplicates: 0
            }
        );
        assert!(small.iter().all(|object| !store.path(object.id()).exists()));
        assert!(store.path(large.id()).exists(), "large objects stay loose");
        assert_eq!(
            pack_files(&store).len(),
            4,
            "a pack, its index, manifest, lock"
        );

        // A store opened afresh finds them too.
        let reopened = Store::new(&store.root, Native::default());
        assert_readable(&reopened, &small);
        assert!(reopened.contains(small[0].id()).unwrap());
        assert!(
            !reopened
                .contains(EncodedObject::blob(b"absent").id())
                .unwrap()
        );
        assert_eq!(
            reopened.write_all(&small).unwrap(),
            0,
            "packed objects aren't written again"
        );
        assert_eq!(reopened.read(large.id()).unwrap(), large);
    }

    #[test]
    fn merges_packs_geometrically() {
        let (_dir, store) = store();
        let compact = |seed, count| {
            store.write_all(&objects(seed, count)).unwrap();
            let compaction = store.compact().unwrap().unwrap();
            let packs: Vec<_> = store.packs.loaded().iter().map(|pack| pack.len()).collect();
            (compaction.merged, packs)
        };
        assert_eq!(compact(0, 10), (0, vec![10]));
        assert_eq!(compact(10, 6), (1, vec![16]), "10 < 2 × 6: merged");
        assert_eq!(compact(16, 3), (0, vec![16, 3]), "16 ≥ 2 × 3: kept");
        assert_eq!(compact(19, 2), (1, vec![16, 5]), "only the smaller one");
        assert_eq!(compact(21, 20), (2, vec![41]), "both");
        assert_readable(&store, &objects(0, 41));
        assert_eq!(pack_files(&store).len(), 4, "merged packs are removed");
    }

    #[test]
    fn removes_loose_duplicates_of_packed_objects() {
        let (_dir, store) = store();
        let objects = objects(0, 10);
        store.write_all(&objects).unwrap();
        store.compact().unwrap();

        // A writer that hasn't loaded the packs writes them again.
        let unaware = Store::new(&store.root, Native::default());
        assert_eq!(unaware.write_all(&objects[..3]).unwrap(), 3);
        let compaction = store.compact().unwrap().unwrap();
        assert_eq!(compaction.duplicates, 3);
        assert_eq!(compaction.packed, 0);
        assert!(!store.path(objects[0].id()).exists());
        assert_readable(&store, &objects);
    }

    #[test]
    fn lets_go_of_packs_another_process_retired() {
        let (_dir, store) = store();
        let first = objects(0, 10);
        store.write_all(&first).unwrap();
        store.compact().unwrap();
        let other = Store::new(&store.root, Native::default());
        assert_readable(&other, &first);
        store.write_all(&objects(10, 10)).unwrap();
        assert_eq!(store.compact().unwrap().unwrap().merged, 1);

        let ids =
            |store: &Store| -> Vec<_> { store.packs.loaded().iter().map(|pack| pack.id).collect() };
        assert_readable(&other, &first);
        assert_ne!(ids(&other), ids(&store), "still reading the retired pack");
        *other.packs.read_at.lock().unwrap() = Instant::now().checked_sub(Packs::RELOAD_AFTER);
        assert_readable(&other, &first);
        assert_eq!(ids(&other), ids(&store));
    }

    #[test]
    fn compacts_once_enough_loose_objects_pile_up() {
        let (_dir, store) = store();
        let few = objects(0, 100);
        store.write_all(&few).unwrap();
        assert_eq!(store.compact_if_due().unwrap(), None);
        // Enough for their estimate from one shard to be past the threshold.
        let many = objects(
            100,
            u32::try_from(3 * Store::<Native>::COMPACT_AFTER).unwrap(),
        );
        store.write_all(&many).unwrap();
        let compaction = store.compact_if_due().unwrap().unwrap();
        assert_eq!(compaction.packed, few.len() + many.len());
        assert_eq!(store.compact_if_due().unwrap(), None, "nothing left loose");
    }

    #[test]
    fn rewriting_a_packed_object_syncs_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Recorder::default());
        store.init().unwrap();
        let objects = objects(0, 10);
        store.write_all(&objects).unwrap();
        store.compact().unwrap();
        store.platform.0.lock().unwrap().clear();

        // The compactor may not have synced the manifest naming it yet.
        assert_eq!(store.write_all(&objects[..1]).unwrap(), 0);
        let packs = store.packs_dir();
        assert_eq!(
            store.platform.0.into_inner().unwrap(),
            [Synced::Batch(0), Synced::Dir(packs)]
        );
    }

    #[test]
    fn removes_packed_objects_and_keeps_when_each_was_stored() {
        let (_dir, store) = store();
        let first = objects(0, 10);
        store.write_all(&first).unwrap();
        let secs = |id| {
            store
                .stored_at(id)
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        let stored = secs(first[0].id());
        store.compact().unwrap();
        store.write_all(&objects(10, 10)).unwrap();
        assert_eq!(store.compact().unwrap().unwrap().merged, 1);
        assert_eq!(secs(first[0].id()), stored, "packing keeps it, merging too");

        let loose = EncodedObject::blob(b"loose");
        store.write_all(slice::from_ref(&loose)).unwrap();
        let doomed = [first[0].id(), first[1].id(), loose.id()];
        assert!(store.remove(&doomed).unwrap() > 0);
        let fresh = Store::new(&store.root, Native::default());
        for id in doomed {
            assert!(!fresh.contains(id).unwrap(), "{id}");
        }
        assert_readable(&fresh, &first[2..]);
        assert_readable(&fresh, &objects(10, 10));
        assert_eq!(pack_files(&store).len(), 4, "the old pack is removed");
        assert_eq!(store.remove(&doomed).unwrap(), 0, "already gone");
    }

    #[test]
    fn unreadable_objects_fail_the_compaction() {
        let (_dir, store) = store();
        let objects = objects(0, 10);
        store.write_all(&objects).unwrap();
        // Reading a directory fails.
        let unreadable = store.path(objects[3].id());
        fs::remove_file(&unreadable).unwrap();
        fs::create_dir(&unreadable).unwrap();

        let error = store.compact().unwrap_err();
        assert!(
            matches!(&error, StoreError::Io { path, .. } if *path == unreadable),
            "{error}"
        );
        assert!(store.packs.loaded().is_empty(), "nothing is published");
        assert_readable(&store, &objects[4..]);
    }

    #[test]
    fn one_compactor_at_a_time() {
        let (_dir, store) = store();
        let lock = store.packs_dir().join(Store::<Native>::LOCK);
        let _held = LockedFile::open(&store.platform, &lock, LockMode::Exclusive).unwrap();
        assert_eq!(store.compact().unwrap(), None);
    }

    #[test]
    fn rejects_corrupt_indexes() {
        let (_dir, store) = store();
        let objects = objects(0, 10);
        store.write_all(&objects).unwrap();
        store.compact().unwrap();
        let index = Pack::file_path(&store.packs_dir(), store.packs.loaded()[0].id, "idx");
        let mut bytes = fs::read(&index).unwrap();
        // Swap the first two ids, which keeps them valid but unsorted.
        let ids = Pack::INDEX_MAGIC.len()..Pack::INDEX_MAGIC.len() + 2 * ObjectId::LEN;
        bytes[ids].rotate_left(ObjectId::LEN);
        fs::write(&index, bytes).unwrap();

        let reopened = Store::new(&store.root, Native::default());
        let error = reopened.read(objects[0].id()).unwrap_err();
        assert!(
            matches!(&error, StoreError::Malformed(path) if *path == index),
            "{error}"
        );
    }

    /// The native platform, failing one batch sync once armed with
    /// [`Crashing::crash_after`], as if the process died there.
    #[derive(Default)]
    struct Crashing(Mutex<Option<usize>>);

    impl Crashing {
        /// Fails the batch sync after the next `batches`.
        fn crash_after(&self, batches: usize) {
            *self.0.lock().unwrap() = Some(batches);
        }
    }

    impl Durability for Crashing {
        fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
            let mut left = self.0.lock().unwrap();
            match *left {
                Some(0) => {
                    *left = None;
                    let source = io::Error::other("crash");
                    return Err(Error {
                        op: Op::Sync,
                        path: PathBuf::from("batch"),
                        source,
                    });
                }
                Some(batches) => *left = Some(batches - 1),
                None => {}
            }
            Native::default().sync_batch(files)
        }
    }

    impl FileSystem for Crashing {
        fn file_system(&self, path: &Path) -> io::Result<FsKind> {
            Native::default().file_system(path)
        }
    }

    impl FileLock for Crashing {}

    impl ReadAt for Crashing {}

    impl CloneFile for Crashing {
        fn cannot_clone(&self, error: &io::Error) -> bool {
            Native::default().cannot_clone(error)
        }
    }

    #[test]
    fn a_failed_compaction_loses_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Crashing::default());
        store.init().unwrap();
        let first = objects(0, 10);
        store.write_all(&first).unwrap();
        store.compact().unwrap();
        let second = objects(10, 10);
        store.write_all(&second).unwrap();

        // The new pack and its index are durable, the manifest isn't.
        store.platform.crash_after(1);
        assert!(store.compact().is_err());
        let fresh = Store::new(&store.root, Native::default());
        assert_readable(&fresh, &first);
        assert_readable(&fresh, &second);
        assert_eq!(pack_files(&store).len(), 6, "an unpublished pack is left");

        let compaction = store.compact().unwrap().unwrap();
        assert_eq!((compaction.packed, compaction.merged), (10, 1));
        assert_eq!(pack_files(&store).len(), 4, "and removed by the next one");
        assert_readable(&fresh, &first);
        assert_readable(&fresh, &second);
    }

    #[test]
    fn readers_never_miss_an_object_during_compaction() {
        let (_dir, store) = store();
        let rounds = 20;
        let written: Vec<_> = (0..rounds).map(|round| objects(round * 50, 50)).collect();
        let published = AtomicUsize::new(0);
        thread::scope(|scope| {
            // Readers in this process, sharing the compactor's packs, and
            // as another process would, with packs of their own.
            for reader in [store.clone(), Store::new(&store.root, Native::default())] {
                let (written, published) = (&written, &published);
                scope.spawn(move || {
                    while published.load(Ordering::SeqCst) < written.len() {
                        let available = published.load(Ordering::SeqCst);
                        for object in written[..available].iter().flatten() {
                            assert_eq!(&reader.read(object.id()).unwrap(), object);
                        }
                    }
                });
            }
            for objects in &written {
                store.write_all(objects).unwrap();
                published.fetch_add(1, Ordering::SeqCst);
                store.compact().unwrap();
            }
        });
        assert!(store.packs.loaded().len() < 6, "merged as it went");
    }

    #[test]
    fn listing_never_misses_an_object_during_compaction() {
        let (_dir, store) = store();
        let written = objects(0, 4000);
        store.write_all(&written).unwrap();
        let compacted = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                while !compacted.load(Ordering::SeqCst) {
                    assert_eq!(store.ids().unwrap().len(), written.len());
                }
            });
            store.compact().unwrap();
            compacted.store(true, Ordering::SeqCst);
        });
    }
}
