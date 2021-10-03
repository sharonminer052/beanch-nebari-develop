use std::{
    collections::HashMap,
    convert::TryFrom,
    fmt::{Debug, Display},
};

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

use super::{
    btree_entry::{BTreeEntry, Reducer},
    read_chunk, BinarySerialization, PagedWriter,
};
use crate::{
    chunk_cache::CacheEntry, io::ManagedFile, tree::btree_entry::NodeInclusion, AbortError, Buffer,
    ChunkCache, Error, Vault,
};

#[derive(Clone, Debug)]
pub struct Interior<I, R> {
    // The key with the highest sort value within.
    pub key: Buffer<'static>,
    /// The location of the node.
    pub position: Pointer<I, R>,
    /// The reduced statistics.
    pub stats: R,
}

impl<I, R> From<BTreeEntry<I, R>> for Interior<I, R>
where
    I: Clone + Debug + BinarySerialization + 'static,
    R: Reducer<I> + Clone + Debug + BinarySerialization + 'static,
{
    fn from(entry: BTreeEntry<I, R>) -> Self {
        let key = entry.max_key().clone();
        let stats = entry.stats();

        Self {
            key,
            stats,
            position: Pointer::Loaded {
                previous_location: None,
                entry: Box::new(entry),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub enum Pointer<I, R> {
    OnDisk(u64),
    Loaded {
        previous_location: Option<u64>,
        entry: Box<BTreeEntry<I, R>>,
    },
}

impl<
        I: BinarySerialization + Debug + Clone + 'static,
        R: Reducer<I> + BinarySerialization + Debug + Clone + 'static,
    > Pointer<I, R>
{
    pub fn load<F: ManagedFile>(
        &mut self,
        file: &mut F,
        validate_crc: bool,
        vault: Option<&dyn Vault>,
        cache: Option<&ChunkCache>,
        current_order: usize,
    ) -> Result<(), Error> {
        match self {
            Pointer::OnDisk(position) => {
                let entry = match read_chunk(*position, validate_crc, file, vault, cache)? {
                    CacheEntry::Buffer(mut buffer) => {
                        // It's worthless to store this node in the cache
                        // because if we mutate, we'll be rewritten.
                        Box::new(BTreeEntry::deserialize_from(&mut buffer, current_order)?)
                    }
                    CacheEntry::Decoded(node) => node
                        .as_ref()
                        .as_any()
                        .downcast_ref::<Box<BTreeEntry<I, R>>>()
                        .unwrap()
                        .clone(),
                };
                *self = Self::Loaded {
                    entry,
                    previous_location: Some(*position),
                };
            }
            Pointer::Loaded { .. } => {}
        }
        Ok(())
    }

    pub fn get_mut(&mut self) -> Option<&mut BTreeEntry<I, R>> {
        match self {
            Pointer::OnDisk(_) => None,
            Pointer::Loaded { entry, .. } => Some(entry.as_mut()),
        }
    }

    pub fn position(&self) -> Option<u64> {
        match self {
            Pointer::OnDisk(location) => Some(*location),
            Pointer::Loaded {
                previous_location, ..
            } => *previous_location,
        }
    }

    pub fn map_loaded_entry<
        Output,
        E: Display + Debug,
        F: ManagedFile,
        Cb: FnOnce(&BTreeEntry<I, R>, &mut F) -> Result<Output, AbortError<E>>,
    >(
        &self,
        file: &mut F,
        vault: Option<&dyn Vault>,
        cache: Option<&ChunkCache>,
        current_order: usize,
        callback: Cb,
    ) -> Result<Output, AbortError<E>> {
        match self {
            Pointer::OnDisk(position) => match read_chunk(*position, false, file, vault, cache)? {
                CacheEntry::Buffer(mut buffer) => {
                    let decoded = BTreeEntry::deserialize_from(&mut buffer, current_order)?;

                    let result = callback(&decoded, file);
                    if let (Some(cache), Some(file_id)) = (cache, file.id()) {
                        cache.replace_with_decoded(file_id, *position, Box::new(decoded));
                    }
                    result
                }
                CacheEntry::Decoded(value) => {
                    let entry = value
                        .as_ref()
                        .as_any()
                        .downcast_ref::<Box<BTreeEntry<I, R>>>()
                        .unwrap();
                    callback(entry, file)
                }
            },
            Pointer::Loaded { entry, .. } => callback(entry, file),
        }
    }
}

impl<
        I: Clone + BinarySerialization + Debug + 'static,
        R: Reducer<I> + Clone + BinarySerialization + Debug + 'static,
    > Interior<I, R>
{
    #[allow(clippy::too_many_arguments)]
    pub fn copy_data_to<F, Callback>(
        &mut self,
        include_nodes: NodeInclusion,
        file: &mut F,
        copied_chunks: &mut HashMap<u64, u64>,
        writer: &mut PagedWriter<'_, F>,
        vault: Option<&dyn Vault>,
        scratch: &mut Vec<u8>,
        index_callback: &mut Callback,
    ) -> Result<bool, Error>
    where
        F: ManagedFile,
        Callback: FnMut(
            &Buffer<'static>,
            &mut I,
            &mut F,
            &mut HashMap<u64, u64>,
            &mut PagedWriter<'_, F>,
            Option<&dyn Vault>,
        ) -> Result<bool, Error>,
    {
        self.position.load(file, true, vault, None, 0)?;
        let node = self.position.get_mut().unwrap();
        let mut any_data_copied = node.copy_data_to(
            include_nodes,
            file,
            copied_chunks,
            writer,
            vault,
            scratch,
            index_callback,
        )?;

        // Serialize if we are supposed to
        let position = if include_nodes.should_include() {
            any_data_copied = true;
            scratch.clear();
            node.serialize_to(scratch, writer)?;
            Some(writer.write_chunk(scratch, false)?)
        } else {
            self.position.position()
        };

        // Remove the node from memory to save RAM during the compaction process.
        if let Some(position) = position {
            self.position = Pointer::OnDisk(position);
        }

        Ok(any_data_copied)
    }
}

impl<
        I: Clone + BinarySerialization + Debug + 'static,
        R: Reducer<I> + Clone + BinarySerialization + Debug + 'static,
    > BinarySerialization for Interior<I, R>
{
    fn serialize_to<F: ManagedFile>(
        &mut self,
        writer: &mut Vec<u8>,
        paged_writer: &mut PagedWriter<'_, F>,
    ) -> Result<usize, Error> {
        let mut pointer = Pointer::OnDisk(0);
        std::mem::swap(&mut pointer, &mut self.position);
        let location_on_disk = match pointer {
            Pointer::OnDisk(position) => position,
            Pointer::Loaded {
                mut entry,
                previous_location,
            } => match (entry.dirty, previous_location) {
                // Serialize if dirty, or if this node hasn't been on-disk before.
                (true, _) | (_, None) => {
                    entry.dirty = false;
                    let old_writer_length = writer.len();
                    entry.serialize_to(writer, paged_writer)?;
                    let position = paged_writer
                        .write_chunk(&writer[old_writer_length..writer.len()], false)?;
                    writer.truncate(old_writer_length);
                    if let (Some(cache), Some(file_id)) = (paged_writer.cache, paged_writer.id()) {
                        cache.replace_with_decoded(file_id, position, entry);
                    }
                    position
                }
                (false, Some(position)) => position,
            },
        };
        self.position = Pointer::OnDisk(location_on_disk);
        let mut bytes_written = 0;
        // Write the key
        let key_len = u16::try_from(self.key.len()).map_err(|_| Error::KeyTooLarge)?;
        writer.write_u16::<BigEndian>(key_len)?;
        writer.extend_from_slice(&self.key);
        bytes_written += 2 + key_len as usize;

        writer.write_u64::<BigEndian>(location_on_disk)?;
        bytes_written += 8;

        bytes_written += self.stats.serialize_to(writer, paged_writer)?;

        Ok(bytes_written)
    }

    fn deserialize_from(reader: &mut Buffer<'_>, current_order: usize) -> Result<Self, Error> {
        let key_len = reader.read_u16::<BigEndian>()? as usize;
        if key_len > reader.len() {
            return Err(Error::data_integrity(format!(
                "key length {} found but only {} bytes remaining",
                key_len,
                reader.len()
            )));
        }
        let key = reader.read_bytes(key_len)?.to_owned();

        let position = reader.read_u64::<BigEndian>()?;
        let stats = R::deserialize_from(reader, current_order)?;

        Ok(Self {
            key,
            position: Pointer::OnDisk(position),
            stats,
        })
    }
}
