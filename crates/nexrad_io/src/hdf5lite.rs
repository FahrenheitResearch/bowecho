//! Minimal read-only HDF5 parser — just enough for ODIM_H5 polar volumes.
//!
//! The workspace has no HDF5 dependency (the C library is a heavy, awkward
//! build input on Windows CI), and ODIM files exercise a small, stable
//! corner of the format: BALTRAD/rave, HL-HDF, and h5py (libver "earliest",
//! its default) all write version-0 superblocks, version-1 object headers,
//! old-style groups (symbol table + v1 B-tree + local heap), and contiguous
//! or chunked+deflate dataset layouts. This module implements exactly that
//! subset, byte-for-byte against the HDF5 File Format Specification
//! (The HDF Group, "HDF5 File Format Specification Version 3.0";
//! <https://support.hdfgroup.org/documentation/hdf5/latest/_f_m_t3.html>):
//!
//! - Superblock v0/v1 (v2/v3 — the 1.10+ "latest" layout — is detected and
//!   rejected with a clear error).
//! - Version 1 object headers, including continuation blocks.
//! - Version 2 object headers ("OHDR", with "OCHK" continuation blocks and
//!   Jenkins lookup3 checksum verification). AEMET/Spain writes ODIM H5rad
//!   2.4 files (IRIS 8.13/10.3 export, live in ORD since 2026-06-23) as a
//!   mixed dialect: superblock v0 and old-style groups, but v2 headers on
//!   the leaf metadata groups (`datasetN/{how,what,where}`,
//!   `datasetN/dataM/{how,what}`). Their attributes stay compact (message
//!   0x000C version 1) and their link-info fractal-heap addresses are
//!   undefined, so v2 B-trees, fractal heaps, and dense attribute storage
//!   remain out of scope below.
//! - Messages: dataspace (0x0001), datatype (0x0003), data layout (0x0008,
//!   v3 compact/contiguous/chunked), filter pipeline (0x000B, deflate id 1
//!   and shuffle id 2), attribute (0x000C, versions 1-3), header
//!   continuation (0x0010), symbol table (0x0011).
//! - Datatypes: fixed-point, IEEE float (f32/f64), fixed-length strings, and
//!   variable-length strings (global heap collections).
//! - Chunk index: v1 B-trees; raw chunks pass through the inverse filter
//!   pipeline (deflate, then unshuffle) and edge chunks are clipped.
//!
//! Everything else (fractal heaps, dense attributes, v2 B-trees, shared
//! messages, fill values beyond zero, named datatypes, ...) is out of scope
//! and produces an explicit error rather than silent misreads.
//!
//! Hostile input is assumed: recursion and decoded allocation are bounded so
//! a malformed file returns an error instead of overflowing a worker stack or
//! exhausting the process allocator.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use flate2::read::ZlibDecoder;

use crate::{NexradError, Result};

const SIGNATURE: [u8; 8] = [0x89, b'H', b'D', b'F', b'\r', b'\n', 0x1a, b'\n'];
/// Version 2 object header signature (HDF5 spec section IV.A.2).
const OHDR_SIGNATURE: &[u8; 4] = b"OHDR";
/// Version 2 object header continuation block signature.
const OCHK_SIGNATURE: &[u8; 4] = b"OCHK";
const UNDEFINED_ADDR: u64 = u64::MAX;
/// Defense against corrupt files: deepest group nesting we will walk.
const MAX_GROUP_DEPTH: usize = 16;
/// Defense against corrupt B-trees: most nodes visited per tree walk.
const MAX_BTREE_NODES: usize = 1 << 16;
/// Deepest chain of internal B-tree nodes either recursive walker accepts.
/// Node count alone does not prevent a single-child chain from exhausting a
/// worker thread's stack.
const MAX_BTREE_DEPTH: usize = 32;
/// Defense against corrupt/self-referencing v2 header continuations: most
/// header blocks (chunk 0 + OCHK continuations) per object header.
const MAX_HEADER_BLOCKS: usize = 1 << 10;
const MAX_OBJECT_MESSAGES: usize = 4096;
const MAX_OBJECT_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_GROUP_ENTRIES: usize = 1 << 20;
const MAX_DATA_CHUNKS: usize = 1 << 18;
const MAX_DATASPACE_RANK: usize = 32;
const MAX_DATASPACE_DIM: usize = 100 * 1024 * 1024;
const MAX_HDF5_DATASET_BYTES: usize = 256 * 1024 * 1024;
/// Aggregate declared dataset bytes that one HDF5 file view may materialize.
/// This bounds declaration bombs made from many individually valid planes,
/// including unwritten planes synthesized from fill values.
pub(crate) const MAX_HDF5_TOTAL_DATASET_BYTES: usize = 512 * 1024 * 1024;
const MAX_HDF5_ATTRIBUTE_BYTES: usize = 16 * 1024 * 1024;
const MAX_HDF5_FILTERS: usize = 32;
const MAX_HDF5_FILTER_VALUES: usize = 1024;

/// `true` when the buffer starts with the HDF5 superblock signature.
pub fn looks_like_hdf5_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= SIGNATURE.len() && bytes[..SIGNATURE.len()] == SIGNATURE
}

/// A decoded scalar or 1-D attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum H5Attr {
    Str(String),
    F64(f64),
    I64(i64),
    F64Array(Vec<f64>),
    I64Array(Vec<i64>),
}

impl H5Attr {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }

    /// Numeric view: integers widen to f64 (ODIM writers disagree about
    /// whether e.g. `nodata` is a long or a double).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::F64(value) => Some(*value),
            Self::I64(value) => Some(*value as f64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::I64(value) => Some(*value),
            Self::F64(value) => (value.fract() == 0.0).then_some(*value as i64),
            _ => None,
        }
    }
}

/// Raw dataset elements, converted from the on-disk datatype.
#[derive(Clone, Debug, PartialEq)]
pub enum H5Data {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl H5Data {
    pub fn len(&self) -> usize {
        match self {
            Self::U8(values) => values.len(),
            Self::U16(values) => values.len(),
            Self::F32(values) => values.len(),
            Self::F64(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A dataset: dimension sizes (row-major) plus the element array.
#[derive(Clone, Debug)]
pub struct H5Dataset {
    pub dims: Vec<usize>,
    pub data: H5Data,
}

/// Read-only HDF5 file view over a byte slice.
pub struct H5File<'a> {
    bytes: &'a [u8],
    offset_size: usize,
    length_size: usize,
    /// Absolute path ("/a/b") → object header address for every object
    /// reachable from the root group.
    objects: BTreeMap<String, u64>,
    budget_total: usize,
    budget_left: Cell<usize>,
}

impl<'a> H5File<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        Self::open_within_budget(bytes, MAX_HDF5_TOTAL_DATASET_BYTES)
    }

    /// Open with an explicit aggregate decode budget. Tests use a small value
    /// to exercise the ceiling without allocating hundreds of megabytes.
    pub(crate) fn open_within_budget(bytes: &'a [u8], budget: usize) -> Result<Self> {
        if !looks_like_hdf5_bytes(bytes) {
            return Err(invalid(0, "missing HDF5 superblock signature"));
        }
        let version = *bytes.get(8).ok_or_else(|| truncated(8, 1, bytes.len()))?;
        if version > 1 {
            // Real-world note: netCDF-4 files (modern CfRadial 1.x and all
            // CfRadial 2, written by Radx/netCDF) carry this superblock —
            // every public CfRadial sample checked in 2026 does. Point
            // those users at the conversion that actually works.
            return Err(invalid(
                8,
                format!(
                    "HDF5 superblock version {version} (1.10+ 'latest' layout) is unsupported. \
                     If this is a netCDF-4 CfRadial file, convert it to classic netCDF \
                     (`nccopy -k classic` or RadxConvert) and open the .nc; ODIM_H5 writers \
                     should use default/earliest library settings"
                ),
            ));
        }
        let offset_size = read_u8(bytes, 13)? as usize;
        let length_size = read_u8(bytes, 14)? as usize;
        if !(4..=8).contains(&offset_size) || !(4..=8).contains(&length_size) {
            return Err(invalid(13, "unsupported HDF5 offset/length sizes"));
        }
        // v0: fixed fields end at 24; v1 inserts 4 bytes (indexed-storage k).
        let addr_block = if version == 0 { 24 } else { 28 };
        // base, free-space, EOF, driver-info addresses; then the root group
        // symbol table entry, whose object header address is field 2.
        let root_entry = addr_block + 4 * offset_size;
        let root_header = read_offset(bytes, root_entry + offset_size, offset_size)?;
        let mut file = Self {
            bytes,
            offset_size,
            length_size,
            objects: BTreeMap::new(),
            budget_total: budget,
            budget_left: Cell::new(budget),
        };
        let header = file.parse_object_header(root_header)?;
        file.objects.insert("/".to_owned(), root_header);
        let mut visited_groups = BTreeSet::from([root_header]);
        file.walk_group("", &header, &mut visited_groups, 0)?;
        Ok(file)
    }

    fn charge_decode_budget(&self, bytes: usize, what: &str) -> Result<()> {
        let left = self.budget_left.get();
        if bytes > left {
            return Err(invalid(
                0,
                format!(
                    "HDF5 decode budget exhausted: {what} needs {bytes} bytes, {left} of the \
                     {} byte whole-file budget left",
                    self.budget_total
                ),
            ));
        }
        self.budget_left.set(left - bytes);
        Ok(())
    }

    /// Names of the direct children of `path` (groups and datasets).
    pub fn child_names(&self, path: &str) -> Vec<String> {
        let prefix = if path == "/" {
            "/".to_owned()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        self.objects
            .keys()
            .filter_map(|key| {
                let rest = key.strip_prefix(&prefix)?;
                (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_owned())
            })
            .collect()
    }

    pub fn has_object(&self, path: &str) -> bool {
        self.objects.contains_key(path)
    }

    /// Read one attribute of the object at `path`.
    pub fn attr(&self, path: &str, name: &str) -> Option<H5Attr> {
        let header = self.parse_object_header(*self.objects.get(path)?).ok()?;
        for message in &header.messages {
            if message.kind != 0x000C {
                continue;
            }
            if let Ok(Some(attr)) = self.parse_attribute(&message.body, name) {
                return Some(attr);
            }
        }
        None
    }

    /// Read the full dataset at `path`.
    pub fn dataset(&self, path: &str) -> Result<H5Dataset> {
        let address = *self
            .objects
            .get(path)
            .ok_or_else(|| invalid(0, format!("HDF5 object '{path}' not found")))?;
        let header = self.parse_object_header(address)?;
        let mut dims: Option<Vec<usize>> = None;
        let mut dtype: Option<Datatype> = None;
        let mut layout: Option<Layout> = None;
        let mut filters: Vec<Filter> = Vec::new();
        for message in &header.messages {
            match message.kind {
                0x0001 => dims = Some(self.parse_dataspace(&message.body)?),
                0x0003 => dtype = Some(self.parse_datatype(&message.body)?),
                0x0008 => layout = Some(self.parse_layout(&message.body)?),
                0x000B => filters = self.parse_filter_pipeline(&message.body)?,
                _ => {}
            }
        }
        let dims = dims.ok_or_else(|| invalid(0, format!("dataset '{path}' has no dataspace")))?;
        let dtype = dtype.ok_or_else(|| invalid(0, format!("dataset '{path}' has no datatype")))?;
        let layout = layout.ok_or_else(|| invalid(0, format!("dataset '{path}' has no layout")))?;
        let element_count = checked_product(&dims, "HDF5 dataset element count")?;
        let byte_len = checked_allocation_bytes(
            element_count,
            dtype.size,
            MAX_HDF5_DATASET_BYTES,
            "HDF5 dataset",
        )?;
        // Charge the declaration before every allocation path, including an
        // unwritten contiguous plane materialized entirely as fill values.
        self.charge_decode_budget(byte_len, &format!("dataset '{path}'"))?;
        let raw = match layout {
            Layout::Compact(data) => data,
            Layout::Contiguous { address, size } => {
                if address == UNDEFINED_ADDR {
                    vec![0u8; byte_len] // never written: fill value (zero)
                } else {
                    self.slice(address, (size as usize).min(byte_len))?.to_vec()
                }
            }
            Layout::Chunked {
                btree_address,
                chunk_dims,
            } => self.read_chunked(btree_address, &chunk_dims, &dims, dtype.size, &filters)?,
        };
        if raw.len() < byte_len {
            return Err(invalid(
                0,
                format!(
                    "dataset '{path}' raw stream too short: {} < {byte_len}",
                    raw.len()
                ),
            ));
        }
        let data = dtype.convert(&raw[..byte_len])?;
        Ok(H5Dataset { dims, data })
    }

    // ----- object graph -------------------------------------------------

    fn walk_group(
        &mut self,
        prefix: &str,
        header: &ObjectHeader,
        visited_groups: &mut BTreeSet<u64>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_GROUP_DEPTH {
            return Err(invalid(0, "HDF5 group nesting too deep"));
        }
        for message in &header.messages {
            if message.kind != 0x0011 {
                continue;
            }
            // Symbol table message: v1 B-tree of SNOD leaves + local heap.
            let btree = read_offset(&message.body, 0, self.offset_size)?;
            let heap = read_offset(&message.body, self.offset_size, self.offset_size)?;
            let heap_data = self.local_heap_data(heap)?;
            let mut entries = Vec::new();
            let mut visited_nodes = BTreeSet::new();
            self.collect_group_entries(btree, &mut entries, &mut visited_nodes, 0)?;
            for (name_offset, child_address) in entries {
                let name = heap_string(self.bytes, heap_data, name_offset)?;
                let path = format!("{prefix}/{name}");
                if self.objects.contains_key(&path) {
                    continue; // hard-link cycle guard
                }
                let child = self.parse_object_header(child_address)?;
                self.objects.insert(path.clone(), child_address);
                if visited_groups.insert(child_address) {
                    self.walk_group(&path, &child, visited_groups, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    fn collect_group_entries(
        &self,
        node_address: u64,
        out: &mut Vec<(u64, u64)>,
        visited: &mut BTreeSet<u64>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_BTREE_DEPTH {
            return Err(invalid(0, "HDF5 group B-tree nested too deep"));
        }
        if !visited.insert(node_address) {
            return Err(invalid(
                address_to_usize(node_address)?,
                "cycle in HDF5 group B-tree",
            ));
        }
        if visited.len() > MAX_BTREE_NODES {
            return Err(invalid(0, "HDF5 group B-tree too large"));
        }
        let node = self.slice(node_address, 8 + 2 * self.offset_size)?;
        if &node[..4] != b"TREE" {
            return Err(invalid(node_address as usize, "expected TREE signature"));
        }
        let level = node[5];
        let entries = u16::from_le_bytes([node[6], node[7]]) as usize;
        // keys/children alternate after the two sibling addresses.
        let mut cursor = address_to_usize(node_address)?
            .checked_add(8 + 2 * self.offset_size)
            .ok_or_else(|| invalid(0, "HDF5 group B-tree cursor overflow"))?;
        for _ in 0..entries {
            cursor += self.length_size; // key (heap offset) — unused here
            let child = read_offset(self.bytes, cursor, self.offset_size)?;
            cursor += self.offset_size;
            if level == 0 {
                self.read_snod(child, out)?;
            } else {
                self.collect_group_entries(child, out, visited, depth + 1)?;
            }
        }
        Ok(())
    }

    fn read_snod(&self, address: u64, out: &mut Vec<(u64, u64)>) -> Result<()> {
        let head = self.slice(address, 8)?;
        if &head[..4] != b"SNOD" {
            return Err(invalid(address as usize, "expected SNOD signature"));
        }
        let count = u16::from_le_bytes([head[6], head[7]]) as usize;
        let entry_size = 2 * self.offset_size + 8 + 16;
        let mut cursor = address_to_usize(address)?
            .checked_add(8)
            .ok_or_else(|| invalid(0, "HDF5 symbol-table address overflow"))?;
        for _ in 0..count {
            let name_offset = read_offset(self.bytes, cursor, self.length_size)?;
            let header = read_offset(self.bytes, cursor + self.offset_size, self.offset_size)?;
            if out.len() >= MAX_GROUP_ENTRIES {
                return Err(invalid(
                    address_to_usize(address)?,
                    "HDF5 group has too many entries",
                ));
            }
            out.push((name_offset, header));
            cursor = cursor
                .checked_add(entry_size)
                .ok_or_else(|| invalid(cursor, "HDF5 symbol-table cursor overflow"))?;
        }
        Ok(())
    }

    fn local_heap_data(&self, address: u64) -> Result<u64> {
        let head = self.slice(address, 8 + 2 * self.length_size + self.offset_size)?;
        if &head[..4] != b"HEAP" {
            return Err(invalid(address as usize, "expected HEAP signature"));
        }
        read_offset(head, 8 + 2 * self.length_size, self.offset_size)
    }

    fn parse_object_header(&self, address: u64) -> Result<ObjectHeader> {
        // Version 2 headers announce themselves with a signature; version 1
        // headers have none and start with the version byte.
        if self
            .slice(address, OHDR_SIGNATURE.len())
            .is_ok_and(|sig| sig == OHDR_SIGNATURE)
        {
            return self.parse_object_header_v2(address);
        }
        let head = self.slice(address, 16)?;
        if head[0] != 1 {
            return Err(invalid(
                address as usize,
                format!("object header version {} is unsupported", head[0]),
            ));
        }
        let total_messages = u16::from_le_bytes([head[2], head[3]]) as usize;
        if total_messages > MAX_OBJECT_MESSAGES {
            return Err(invalid(
                address_to_usize(address)?,
                format!(
                    "HDF5 object header declares {total_messages} messages (limit {MAX_OBJECT_MESSAGES})"
                ),
            ));
        }
        let block_size = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
        if block_size > MAX_OBJECT_MESSAGE_BYTES {
            return Err(invalid(
                address_to_usize(address)?,
                "HDF5 object-header message block is too large",
            ));
        }
        let mut messages = Vec::with_capacity(total_messages);
        // (start, length) message blocks; the first follows 4 pad bytes.
        let first_block = address_to_usize(address)?
            .checked_add(16)
            .ok_or_else(|| invalid(0, "HDF5 object-header address overflow"))?;
        let mut blocks = vec![(first_block, block_size)];
        let mut scheduled_blocks = BTreeSet::from([first_block]);
        let mut block_index = 0;
        let mut message_bytes = 0usize;
        while block_index < blocks.len() && messages.len() < total_messages {
            let (start, len) = blocks[block_index];
            block_index += 1;
            let mut cursor = start;
            let end = start
                .checked_add(len)
                .ok_or_else(|| invalid(start, "HDF5 object-header block overflow"))?;
            self.bytes
                .get(start..end)
                .ok_or_else(|| truncated(start, len, self.bytes.len()))?;
            while cursor
                .checked_add(8)
                .is_some_and(|header_end| header_end <= end)
                && messages.len() < total_messages
            {
                let header = self.slice(cursor as u64, 8)?;
                let kind = u16::from_le_bytes([header[0], header[1]]);
                let size = u16::from_le_bytes([header[2], header[3]]) as usize;
                let body_start = cursor
                    .checked_add(8)
                    .ok_or_else(|| invalid(cursor, "HDF5 message address overflow"))?;
                let body_end = body_start
                    .checked_add(size)
                    .ok_or_else(|| invalid(body_start, "HDF5 message size overflow"))?;
                if body_end > end {
                    return Err(truncated(cursor, 8 + size, end.saturating_sub(cursor)));
                }
                let body = self.slice(body_start as u64, size)?.to_vec();
                if kind == 0x0010 {
                    // Continuation: offset + length of the next block.
                    let offset = read_offset(&body, 0, self.offset_size)?;
                    let length = read_offset(&body, self.offset_size, self.length_size)?;
                    let offset = address_to_usize(offset)?;
                    let length = usize::try_from(length)
                        .map_err(|_| invalid(cursor, "HDF5 continuation length overflows usize"))?;
                    if length > MAX_OBJECT_MESSAGE_BYTES {
                        return Err(invalid(cursor, "HDF5 continuation block is too large"));
                    }
                    if blocks.len() >= MAX_HEADER_BLOCKS {
                        return Err(invalid(
                            address_to_usize(address)?,
                            "HDF5 object header has too many continuation blocks",
                        ));
                    }
                    if !scheduled_blocks.insert(offset) {
                        return Err(invalid(offset, "cycle in HDF5 object-header continuations"));
                    }
                    blocks.push((offset, length));
                } else {
                    message_bytes = message_bytes.checked_add(size).ok_or_else(|| {
                        invalid(cursor, "HDF5 object-header message size overflow")
                    })?;
                    if message_bytes > MAX_OBJECT_MESSAGE_BYTES {
                        return Err(invalid(
                            address_to_usize(address)?,
                            "HDF5 object header contains too much message data",
                        ));
                    }
                    messages.push(Message { kind, body });
                }
                cursor = body_end;
            }
        }
        Ok(ObjectHeader { messages })
    }

    /// Version 2 object header ("OHDR"), HDF5 spec section IV.A.2.
    ///
    /// Wire layout (all little-endian):
    /// `OHDR` (4) | version=2 (1) | flags (1) |
    /// [access/mod/change/birth times, 4×u32, when flags bit 5] |
    /// [max-compact/min-dense attribute counts, 2×u16, when flags bit 4] |
    /// size-of-chunk-0 (1/2/4/8 bytes per flags bits 0-1) | messages |
    /// checksum (u32, Jenkins lookup3 over the chunk from the signature on).
    ///
    /// Messages: type (u8 — v1 uses u16), size (u16), flags (u8),
    /// [creation order (u16) when header flags bit 2], body — with NO
    /// inter-message 8-byte alignment (v1 pads). A trailing gap smaller
    /// than one message header may precede the checksum. Continuation
    /// messages (0x0010) point at "OCHK" blocks: signature (4) | messages |
    /// checksum (u32), whose stored length INCLUDES signature and checksum.
    fn parse_object_header_v2(&self, address: u64) -> Result<ObjectHeader> {
        let head = self.slice(address, 6)?;
        let version = head[4];
        if version != 2 {
            return Err(invalid(
                address as usize,
                format!("OHDR object header version {version} unsupported (need 2)"),
            ));
        }
        let flags = head[5];
        let address = address_to_usize(address)?;
        let mut cursor = address
            .checked_add(6)
            .ok_or_else(|| invalid(address, "HDF5 v2 header address overflow"))?;
        if flags & 0x20 != 0 {
            cursor = cursor
                .checked_add(16)
                .ok_or_else(|| invalid(cursor, "HDF5 v2 timestamp fields overflow"))?;
        }
        if flags & 0x10 != 0 {
            cursor = cursor
                .checked_add(4)
                .ok_or_else(|| invalid(cursor, "HDF5 v2 attribute fields overflow"))?;
        }
        let size_width = 1usize << (flags & 0x03);
        let chunk0_size = usize::try_from(read_uint(self.bytes, cursor, size_width)?)
            .map_err(|_| invalid(cursor, "HDF5 v2 chunk size overflows usize"))?;
        if chunk0_size > MAX_OBJECT_MESSAGE_BYTES {
            return Err(invalid(cursor, "HDF5 v2 header message block is too large"));
        }
        cursor = cursor
            .checked_add(size_width)
            .ok_or_else(|| invalid(cursor, "HDF5 v2 header cursor overflow"))?;
        // Creation-order tracking widens every message header by 2 bytes.
        let message_header = if flags & 0x04 != 0 { 6 } else { 4 };
        let mut messages = Vec::new();
        // (message region start, message region length, chunk start for the
        // checksum). Chunk 0's checksummed span begins at the signature.
        let mut blocks = vec![(cursor, chunk0_size, address)];
        let mut scheduled_blocks = BTreeSet::from([address]);
        let mut block_index = 0;
        let mut message_bytes = 0usize;
        while block_index < blocks.len() {
            if blocks.len() > MAX_HEADER_BLOCKS {
                return Err(invalid(
                    address,
                    "HDF5 v2 header has too many continuation blocks",
                ));
            }
            let (start, len, chunk_start) = blocks[block_index];
            block_index += 1;
            let end = start
                .checked_add(len)
                .ok_or_else(|| invalid(start, "HDF5 v2 message block overflow"))?;
            if chunk_start > end {
                return Err(invalid(chunk_start, "invalid HDF5 v2 checksum span"));
            }
            let stored = self.slice(end as u64, 4)?;
            let stored = u32::from_le_bytes(stored.try_into().expect("4 bytes"));
            let computed = jenkins_lookup3(self.slice(chunk_start as u64, end - chunk_start)?);
            if stored != computed {
                return Err(invalid(
                    chunk_start,
                    format!(
                        "HDF5 v2 object header checksum mismatch (stored {stored:#010x}, computed {computed:#010x})"
                    ),
                ));
            }
            let mut cursor = start;
            // Stop on the trailing gap: any leftover space smaller than one
            // message header is padding before the checksum.
            while cursor
                .checked_add(message_header)
                .is_some_and(|header_end| header_end <= end)
            {
                let header = self.slice(cursor as u64, message_header)?;
                let kind = u16::from(header[0]);
                let size = u16::from_le_bytes([header[1], header[2]]) as usize;
                // header[3] = message flags; header[4..6] = creation order.
                let body_start = cursor
                    .checked_add(message_header)
                    .ok_or_else(|| invalid(cursor, "HDF5 v2 message address overflow"))?;
                let body_end = body_start
                    .checked_add(size)
                    .ok_or_else(|| invalid(body_start, "HDF5 v2 message size overflow"))?;
                if body_end > end {
                    return Err(truncated(cursor, message_header + size, end - cursor));
                }
                let body = self.slice(body_start as u64, size)?.to_vec();
                if kind == 0x0010 {
                    let offset = address_to_usize(read_offset(&body, 0, self.offset_size)?)?;
                    let length =
                        usize::try_from(read_uint(&body, self.offset_size, self.length_size)?)
                            .map_err(|_| invalid(cursor, "HDF5 v2 continuation length overflow"))?;
                    if length < 8 {
                        return Err(invalid(cursor, "HDF5 v2 continuation block too short"));
                    }
                    if length > MAX_OBJECT_MESSAGE_BYTES {
                        return Err(invalid(cursor, "HDF5 v2 continuation block is too large"));
                    }
                    if self.slice(offset as u64, 4)? != OCHK_SIGNATURE {
                        return Err(invalid(offset, "expected OCHK signature"));
                    }
                    if !scheduled_blocks.insert(offset) {
                        return Err(invalid(offset, "cycle in HDF5 v2 header continuations"));
                    }
                    let message_start = offset
                        .checked_add(4)
                        .ok_or_else(|| invalid(offset, "HDF5 v2 continuation address overflow"))?;
                    // Message region excludes the signature and checksum.
                    blocks.push((message_start, length - 8, offset));
                } else {
                    if messages.len() >= MAX_OBJECT_MESSAGES {
                        return Err(invalid(
                            address,
                            "HDF5 v2 object header has too many messages",
                        ));
                    }
                    message_bytes = message_bytes
                        .checked_add(size)
                        .ok_or_else(|| invalid(cursor, "HDF5 v2 message byte count overflow"))?;
                    if message_bytes > MAX_OBJECT_MESSAGE_BYTES {
                        return Err(invalid(
                            address,
                            "HDF5 v2 object header contains too much message data",
                        ));
                    }
                    messages.push(Message { kind, body });
                }
                cursor = body_end;
            }
        }
        Ok(ObjectHeader { messages })
    }

    // ----- messages -----------------------------------------------------

    fn parse_dataspace(&self, body: &[u8]) -> Result<Vec<usize>> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        let rank = *body.get(1).ok_or_else(|| truncated(1, 1, body.len()))? as usize;
        if rank > MAX_DATASPACE_RANK {
            return Err(invalid(
                1,
                format!("HDF5 dataspace rank {rank} exceeds {MAX_DATASPACE_RANK}"),
            ));
        }
        let dims_start: usize = match version {
            1 => 8, // version, rank, flags, reserved[5]
            2 => 4, // version, rank, flags, type
            other => {
                return Err(invalid(0, format!("dataspace version {other} unsupported")));
            }
        };
        let mut dims = Vec::with_capacity(rank);
        for index in 0..rank {
            let at = index
                .checked_mul(self.length_size)
                .and_then(|value| dims_start.checked_add(value))
                .ok_or_else(|| invalid(dims_start, "HDF5 dataspace cursor overflow"))?;
            let dim = usize::try_from(read_offset(body, at, self.length_size)?)
                .map_err(|_| invalid(at, "HDF5 dimension overflows usize"))?;
            if dim > MAX_DATASPACE_DIM {
                return Err(invalid(
                    at,
                    format!("HDF5 dimension {dim} exceeds {MAX_DATASPACE_DIM}"),
                ));
            }
            dims.push(dim);
        }
        Ok(dims)
    }

    fn parse_datatype(&self, body: &[u8]) -> Result<Datatype> {
        if body.len() < 8 {
            return Err(truncated(0, 8, body.len()));
        }
        let class = body[0] & 0x0F;
        let bits = u32::from_le_bytes([body[1], body[2], body[3], 0]);
        let size = u32::from_le_bytes([body[4], body[5], body[6], body[7]]) as usize;
        let big_endian = bits & 1 != 0;
        match class {
            0 if (1..=8).contains(&size) => Ok(Datatype {
                class: DtClass::Int {
                    signed: bits & (1 << 3) != 0,
                },
                size,
                big_endian,
            }),
            1 if matches!(size, 4 | 8) => Ok(Datatype {
                class: DtClass::Float,
                size,
                big_endian,
            }),
            3 if size <= MAX_HDF5_ATTRIBUTE_BYTES => Ok(Datatype {
                class: DtClass::FixedString,
                size,
                big_endian: false,
            }),
            9 if bits & 0x0F == 1 && size <= MAX_HDF5_ATTRIBUTE_BYTES => Ok(Datatype {
                class: DtClass::VlenString,
                size,
                big_endian: false,
            }),
            other => Err(invalid(
                0,
                format!("HDF5 datatype class {other} unsupported"),
            )),
        }
    }

    fn parse_layout(&self, body: &[u8]) -> Result<Layout> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        if version != 3 {
            return Err(invalid(
                0,
                format!("data layout message version {version} unsupported (need v3)"),
            ));
        }
        let class = *body.get(1).ok_or_else(|| truncated(1, 1, body.len()))?;
        match class {
            0 => {
                let size = usize::from(read_le_u16(body, 2)?);
                if size > MAX_HDF5_DATASET_BYTES {
                    return Err(invalid(2, "HDF5 compact dataset is too large"));
                }
                let end = 4usize
                    .checked_add(size)
                    .ok_or_else(|| invalid(4, "HDF5 compact layout size overflow"))?;
                let data = body
                    .get(4..end)
                    .ok_or_else(|| truncated(4, size, body.len()))?;
                Ok(Layout::Compact(data.to_vec()))
            }
            1 => Ok(Layout::Contiguous {
                address: read_offset(body, 2, self.offset_size)?,
                size: read_offset(body, 2 + self.offset_size, self.length_size)?,
            }),
            2 => {
                let dimensionality =
                    *body.get(2).ok_or_else(|| truncated(2, 1, body.len()))? as usize;
                if dimensionality == 0 || dimensionality > MAX_DATASPACE_RANK + 1 {
                    return Err(invalid(2, "invalid HDF5 chunk dimensionality"));
                }
                let btree_address = read_offset(body, 3, self.offset_size)?;
                let mut chunk_dims = Vec::with_capacity(dimensionality);
                for index in 0..dimensionality {
                    let at = index
                        .checked_mul(4)
                        .and_then(|value| 3usize.checked_add(self.offset_size)?.checked_add(value))
                        .ok_or_else(|| invalid(3, "HDF5 chunk-dimension cursor overflow"))?;
                    let dim = body
                        .get(at..at + 4)
                        .ok_or_else(|| truncated(at, 4, body.len()))?;
                    let dim = u32::from_le_bytes(dim.try_into().expect("4 bytes")) as usize;
                    if dim == 0 || dim > MAX_DATASPACE_DIM {
                        return Err(invalid(at, "invalid HDF5 chunk dimension"));
                    }
                    chunk_dims.push(dim);
                }
                // The trailing entry is the element size; drop it.
                chunk_dims.pop();
                Ok(Layout::Chunked {
                    btree_address,
                    chunk_dims,
                })
            }
            other => Err(invalid(0, format!("data layout class {other} unsupported"))),
        }
    }

    fn parse_filter_pipeline(&self, body: &[u8]) -> Result<Vec<Filter>> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        let count = *body.get(1).ok_or_else(|| truncated(1, 1, body.len()))? as usize;
        if count > MAX_HDF5_FILTERS {
            return Err(invalid(
                1,
                format!("HDF5 filter count {count} exceeds {MAX_HDF5_FILTERS}"),
            ));
        }
        let mut filters = Vec::with_capacity(count);
        let mut cursor = match version {
            1 => 8,
            2 => 2,
            other => {
                return Err(invalid(
                    0,
                    format!("filter pipeline version {other} unsupported"),
                ));
            }
        };
        for _ in 0..count {
            let id = read_le_u16(body, cursor)?;
            let has_name = version == 1 || id >= 256;
            let name_len = if has_name {
                usize::from(read_le_u16(
                    body,
                    cursor
                        .checked_add(2)
                        .ok_or_else(|| invalid(cursor, "HDF5 filter cursor overflow"))?,
                )?)
            } else {
                0
            };
            let after_id = cursor
                .checked_add(if has_name { 4 } else { 2 })
                .ok_or_else(|| invalid(cursor, "HDF5 filter cursor overflow"))?;
            let value_count = usize::from(read_le_u16(
                body,
                after_id
                    .checked_add(2)
                    .ok_or_else(|| invalid(after_id, "HDF5 filter cursor overflow"))?,
            )?);
            if value_count > MAX_HDF5_FILTER_VALUES {
                return Err(invalid(after_id, "HDF5 filter has too many client values"));
            }
            let mut at = after_id
                .checked_add(4)
                .ok_or_else(|| invalid(after_id, "HDF5 filter cursor overflow"))?;
            if name_len > 0 {
                let padded_name = if version == 1 {
                    name_len
                        .checked_add(7)
                        .map(|value| value / 8 * 8)
                        .ok_or_else(|| invalid(at, "HDF5 filter name length overflow"))?
                } else {
                    name_len
                };
                checked_range(body, at, padded_name)?;
                at = at
                    .checked_add(padded_name)
                    .ok_or_else(|| invalid(at, "HDF5 filter name cursor overflow"))?;
            }
            let mut client_values = Vec::with_capacity(value_count);
            for index in 0..value_count {
                let value_at = index
                    .checked_mul(4)
                    .and_then(|value| at.checked_add(value))
                    .ok_or_else(|| invalid(at, "HDF5 filter value cursor overflow"))?;
                let v = checked_range(body, value_at, 4)?;
                client_values.push(u32::from_le_bytes(v.try_into().expect("4 bytes")));
            }
            at = value_count
                .checked_mul(4)
                .and_then(|value| at.checked_add(value))
                .ok_or_else(|| invalid(at, "HDF5 filter value length overflow"))?;
            if version == 1 && value_count % 2 == 1 {
                checked_range(body, at, 4)?;
                at = at
                    .checked_add(4)
                    .ok_or_else(|| invalid(at, "HDF5 filter padding overflow"))?;
            }
            filters.push(Filter { id, client_values });
            cursor = at;
        }
        Ok(filters)
    }

    /// Parse one attribute message body; returns the value when the
    /// attribute's name matches.
    fn parse_attribute(&self, body: &[u8], wanted: &str) -> Result<Option<H5Attr>> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        if !(1..=3).contains(&version) {
            return Err(invalid(
                0,
                format!("attribute version {version} unsupported"),
            ));
        }
        let header_len = if version == 3 { 9 } else { 8 };
        if body.len() < header_len {
            return Err(truncated(0, header_len, body.len()));
        }
        let flags = body[1];
        if version >= 2 && flags & 0x03 != 0 {
            return Err(invalid(
                0,
                "shared attribute datatype/dataspace unsupported",
            ));
        }
        let name_size = usize::from(read_le_u16(body, 2)?);
        let dt_size = usize::from(read_le_u16(body, 4)?);
        let ds_size = usize::from(read_le_u16(body, 6)?);
        let mut cursor = header_len;
        let pad = |len: usize| -> Result<usize> {
            if version == 1 {
                len.checked_add(7)
                    .map(|value| value / 8 * 8)
                    .ok_or_else(|| invalid(0, "HDF5 attribute padding overflow"))
            } else {
                Ok(len)
            }
        };
        let name_bytes = checked_range(body, cursor, name_size)?;
        let name = name_bytes
            .split(|byte| *byte == 0)
            .next()
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        cursor = cursor
            .checked_add(pad(name_size)?)
            .ok_or_else(|| invalid(cursor, "HDF5 attribute name cursor overflow"))?;
        if name != wanted {
            return Ok(None);
        }
        let dtype = self.parse_datatype(checked_range(body, cursor, dt_size)?)?;
        cursor = cursor
            .checked_add(pad(dt_size)?)
            .ok_or_else(|| invalid(cursor, "HDF5 attribute datatype cursor overflow"))?;
        let dims = self.parse_dataspace(checked_range(body, cursor, ds_size)?)?;
        cursor = cursor
            .checked_add(pad(ds_size)?)
            .ok_or_else(|| invalid(cursor, "HDF5 attribute dataspace cursor overflow"))?;
        let count = checked_product(&dims, "HDF5 attribute element count")?.max(1);
        checked_allocation_bytes(
            count,
            dtype.size,
            MAX_HDF5_ATTRIBUTE_BYTES,
            "HDF5 attribute",
        )?;
        let data = body
            .get(cursor..)
            .ok_or_else(|| truncated(cursor, 0, body.len()))?;
        self.attr_value(&dtype, count, data).map(Some)
    }

    fn attr_value(&self, dtype: &Datatype, count: usize, data: &[u8]) -> Result<H5Attr> {
        match dtype.class {
            DtClass::FixedString => {
                let bytes = data.get(..dtype.size.min(data.len())).unwrap_or_default();
                let text = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
                Ok(H5Attr::Str(String::from_utf8_lossy(text).into_owned()))
            }
            DtClass::VlenString => {
                // Element: u32 byte length + global heap reference
                // (collection address + u32 object index).
                if data.len() < 4 + self.offset_size + 4 {
                    return Err(truncated(0, 4 + self.offset_size + 4, data.len()));
                }
                let collection = read_offset(data, 4, self.offset_size)?;
                let index = u32::from_le_bytes(
                    data[4 + self.offset_size..4 + self.offset_size + 4]
                        .try_into()
                        .expect("4 bytes"),
                );
                let object = self.global_heap_object(collection, index)?;
                let text = object.split(|byte| *byte == 0).next().unwrap_or_default();
                Ok(H5Attr::Str(String::from_utf8_lossy(text).into_owned()))
            }
            DtClass::Int { signed } => {
                let mut values = Vec::with_capacity(count);
                for index in 0..count {
                    let raw = data
                        .get(index * dtype.size..(index + 1) * dtype.size)
                        .ok_or_else(|| truncated(index * dtype.size, dtype.size, data.len()))?;
                    values.push(read_int(raw, signed, dtype.big_endian));
                }
                Ok(if count == 1 {
                    H5Attr::I64(values[0])
                } else {
                    H5Attr::I64Array(values)
                })
            }
            DtClass::Float => {
                let mut values = Vec::with_capacity(count);
                for index in 0..count {
                    let raw = data
                        .get(index * dtype.size..(index + 1) * dtype.size)
                        .ok_or_else(|| truncated(index * dtype.size, dtype.size, data.len()))?;
                    values.push(read_float(raw, dtype.big_endian)?);
                }
                Ok(if count == 1 {
                    H5Attr::F64(values[0])
                } else {
                    H5Attr::F64Array(values)
                })
            }
        }
    }

    fn global_heap_object(&self, collection: u64, index: u32) -> Result<Vec<u8>> {
        let head = self.slice(collection, 8 + self.length_size)?;
        if &head[..4] != b"GCOL" {
            return Err(invalid(collection as usize, "expected GCOL signature"));
        }
        let total = read_offset(head, 8, self.length_size)? as usize;
        let mut cursor = collection as usize + 8 + self.length_size;
        let end = collection as usize + total;
        while cursor + 8 + self.length_size <= end {
            let object_index = u16::from_le_bytes([self.bytes[cursor], self.bytes[cursor + 1]]);
            let size = read_offset(self.bytes, cursor + 8, self.length_size)? as usize;
            if object_index == 0 {
                break; // free space marker terminates the collection
            }
            let data_start = cursor + 8 + self.length_size;
            if object_index as u32 == index {
                return Ok(self.slice(data_start as u64, size)?.to_vec());
            }
            cursor = data_start + size.div_ceil(8) * 8;
        }
        Err(invalid(
            collection as usize,
            format!("global heap object {index} not found"),
        ))
    }

    // ----- chunked data -------------------------------------------------

    fn read_chunked(
        &self,
        btree_address: u64,
        chunk_dims: &[usize],
        dims: &[usize],
        element_size: usize,
        filters: &[Filter],
    ) -> Result<Vec<u8>> {
        if chunk_dims.len() != dims.len() {
            return Err(invalid(
                0,
                "HDF5 chunk dimensionality does not match dataset rank",
            ));
        }
        let elements = checked_product(dims, "HDF5 chunked dataset element count")?;
        let total = checked_allocation_bytes(
            elements,
            element_size,
            MAX_HDF5_DATASET_BYTES,
            "HDF5 chunked dataset",
        )?;
        let mut out = vec![0u8; total];
        if btree_address == UNDEFINED_ADDR {
            return Ok(out); // dataset never written
        }
        let mut chunks = Vec::new();
        let mut visited_nodes = BTreeSet::new();
        self.collect_chunks(
            btree_address,
            chunk_dims.len() + 1,
            &mut chunks,
            &mut visited_nodes,
            0,
        )?;
        let chunk_elements = checked_product(chunk_dims, "HDF5 chunk element count")?;
        let chunk_bytes = checked_allocation_bytes(
            chunk_elements,
            element_size,
            MAX_HDF5_DATASET_BYTES,
            "HDF5 chunk",
        )?;
        for chunk in chunks {
            if chunk.stored_size > MAX_HDF5_DATASET_BYTES {
                return Err(invalid(
                    address_to_usize(chunk.address)?,
                    "HDF5 stored chunk is too large",
                ));
            }
            let stored = self.slice(chunk.address, chunk.stored_size)?;
            let raw = apply_inverse_filters(
                stored,
                filters,
                chunk.filter_mask,
                element_size,
                chunk_bytes,
            )?;
            if raw.len() < chunk_bytes {
                return Err(invalid(
                    chunk.address as usize,
                    "decoded chunk shorter than chunk dimensions",
                ));
            }
            copy_chunk(
                &mut out,
                &raw,
                dims,
                chunk_dims,
                &chunk.offsets,
                element_size,
            );
        }
        Ok(out)
    }

    fn collect_chunks(
        &self,
        node_address: u64,
        key_dims: usize,
        out: &mut Vec<ChunkRef>,
        visited: &mut BTreeSet<u64>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_BTREE_DEPTH {
            return Err(invalid(0, "HDF5 chunk B-tree nested too deep"));
        }
        if !visited.insert(node_address) {
            return Err(invalid(
                address_to_usize(node_address)?,
                "cycle in HDF5 chunk B-tree",
            ));
        }
        if visited.len() > MAX_BTREE_NODES {
            return Err(invalid(0, "HDF5 chunk B-tree too large"));
        }
        let node = self.slice(node_address, 8 + 2 * self.offset_size)?;
        if &node[..4] != b"TREE" {
            return Err(invalid(node_address as usize, "expected TREE signature"));
        }
        if node[4] != 1 {
            return Err(invalid(node_address as usize, "expected chunk B-tree node"));
        }
        let level = node[5];
        let entries = u16::from_le_bytes([node[6], node[7]]) as usize;
        let key_size = 8 + 8 * key_dims;
        let mut cursor = address_to_usize(node_address)?
            .checked_add(8 + 2 * self.offset_size)
            .ok_or_else(|| invalid(0, "HDF5 chunk B-tree cursor overflow"))?;
        for _ in 0..entries {
            let key = self.slice(cursor as u64, key_size)?;
            let stored_size = u32::from_le_bytes(key[..4].try_into().expect("4 bytes")) as usize;
            let filter_mask = u32::from_le_bytes(key[4..8].try_into().expect("4 bytes"));
            let mut offsets = Vec::with_capacity(key_dims.saturating_sub(1));
            for dim in 0..key_dims.saturating_sub(1) {
                let at = 8 + dim * 8;
                let offset = u64::from_le_bytes(key[at..at + 8].try_into().expect("8 bytes"));
                offsets.push(usize::try_from(offset).map_err(|_| {
                    invalid(
                        address_to_usize(node_address).unwrap_or(0),
                        "HDF5 chunk offset overflows usize",
                    )
                })?);
            }
            cursor = cursor
                .checked_add(key_size)
                .ok_or_else(|| invalid(cursor, "HDF5 chunk B-tree key overflow"))?;
            let child = read_offset(self.bytes, cursor, self.offset_size)?;
            cursor = cursor
                .checked_add(self.offset_size)
                .ok_or_else(|| invalid(cursor, "HDF5 chunk B-tree child overflow"))?;
            if level == 0 {
                if out.len() >= MAX_DATA_CHUNKS {
                    return Err(invalid(0, "HDF5 dataset has too many chunks"));
                }
                out.push(ChunkRef {
                    address: child,
                    stored_size,
                    filter_mask,
                    offsets,
                });
            } else {
                self.collect_chunks(child, key_dims, out, visited, depth + 1)?;
            }
        }
        Ok(())
    }

    fn slice(&self, address: u64, len: usize) -> Result<&'a [u8]> {
        let start = address_to_usize(address)?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| invalid(start, "HDF5 byte range overflow"))?;
        self.bytes
            .get(start..end)
            .ok_or_else(|| truncated(start, len, self.bytes.len()))
    }
}

struct ObjectHeader {
    messages: Vec<Message>,
}

struct Message {
    kind: u16,
    body: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
enum DtClass {
    Int { signed: bool },
    Float,
    FixedString,
    VlenString,
}

#[derive(Clone, Copy, Debug)]
struct Datatype {
    class: DtClass,
    size: usize,
    big_endian: bool,
}

impl Datatype {
    /// Convert a raw element buffer into the closest [`H5Data`] storage.
    fn convert(&self, raw: &[u8]) -> Result<H5Data> {
        match self.class {
            DtClass::Int { signed: false } if self.size == 1 => Ok(H5Data::U8(raw.to_vec())),
            DtClass::Int { signed: false } if self.size == 2 => Ok(H5Data::U16(
                raw.chunks_exact(2)
                    .map(|pair| {
                        if self.big_endian {
                            u16::from_be_bytes([pair[0], pair[1]])
                        } else {
                            u16::from_le_bytes([pair[0], pair[1]])
                        }
                    })
                    .collect(),
            )),
            DtClass::Int { signed } => Ok(H5Data::F64(
                raw.chunks_exact(self.size)
                    .map(|chunk| read_int(chunk, signed, self.big_endian) as f64)
                    .collect(),
            )),
            DtClass::Float if self.size == 4 => Ok(H5Data::F32(
                raw.chunks_exact(4)
                    .map(|quad| {
                        let bits = if self.big_endian {
                            u32::from_be_bytes(quad.try_into().expect("4 bytes"))
                        } else {
                            u32::from_le_bytes(quad.try_into().expect("4 bytes"))
                        };
                        f32::from_bits(bits)
                    })
                    .collect(),
            )),
            DtClass::Float if self.size == 8 => Ok(H5Data::F64(
                raw.chunks_exact(8)
                    .map(|oct| {
                        let bits = if self.big_endian {
                            u64::from_be_bytes(oct.try_into().expect("8 bytes"))
                        } else {
                            u64::from_le_bytes(oct.try_into().expect("8 bytes"))
                        };
                        f64::from_bits(bits)
                    })
                    .collect(),
            )),
            _ => Err(invalid(0, "unsupported dataset element type")),
        }
    }
}

enum Layout {
    Compact(Vec<u8>),
    Contiguous {
        address: u64,
        size: u64,
    },
    Chunked {
        btree_address: u64,
        chunk_dims: Vec<usize>,
    },
}

struct Filter {
    id: u16,
    client_values: Vec<u32>,
}

struct ChunkRef {
    address: u64,
    stored_size: usize,
    filter_mask: u32,
    offsets: Vec<usize>,
}

/// Run the inverse filter pipeline over one stored chunk. Filters apply in
/// reverse pipeline order on read: deflate (id 1) inflates, shuffle (id 2)
/// de-interleaves byte planes. `filter_mask` bit N set = filter N skipped.
fn apply_inverse_filters(
    stored: &[u8],
    filters: &[Filter],
    filter_mask: u32,
    element_size: usize,
    max_output: usize,
) -> Result<Vec<u8>> {
    if stored.len() > MAX_HDF5_DATASET_BYTES {
        return Err(invalid(0, "HDF5 stored filter input is too large"));
    }
    let mut data = stored.to_vec();
    for (index, filter) in filters.iter().enumerate().rev() {
        if filter_mask & (1 << index) != 0 {
            continue;
        }
        match filter.id {
            1 => {
                // gzip/deflate (zlib stream per the HDF5 deflate filter).
                let mut decoder = ZlibDecoder::new(&data[..]);
                let mut inflated = Vec::new();
                let mut chunk = [0u8; 64 * 1024];
                loop {
                    let remaining = max_output.saturating_sub(inflated.len());
                    if remaining == 0 {
                        let mut probe = [0u8; 1];
                        let count = decoder
                            .read(&mut probe)
                            .map_err(|err| invalid(0, format!("HDF5 deflate chunk: {err}")))?;
                        if count != 0 {
                            return Err(invalid(
                                0,
                                format!("HDF5 deflate chunk expands beyond {max_output} bytes"),
                            ));
                        }
                        break;
                    }
                    let read_len = remaining.min(chunk.len());
                    let count = decoder
                        .read(&mut chunk[..read_len])
                        .map_err(|err| invalid(0, format!("HDF5 deflate chunk: {err}")))?;
                    if count == 0 {
                        break;
                    }
                    inflated.try_reserve(count).map_err(|err| {
                        invalid(0, format!("cannot reserve HDF5 deflate output: {err}"))
                    })?;
                    inflated.extend_from_slice(&chunk[..count]);
                }
                data = inflated;
            }
            2 => {
                let size = filter
                    .client_values
                    .first()
                    .copied()
                    .map(|v| v as usize)
                    .unwrap_or(element_size)
                    .max(1);
                data = unshuffle(&data, size);
            }
            other => {
                return Err(invalid(0, format!("HDF5 filter id {other} unsupported")));
            }
        }
        if data.len() > max_output {
            return Err(invalid(
                0,
                format!("HDF5 filter output exceeds {max_output} bytes"),
            ));
        }
    }
    if data.len() > max_output {
        return Err(invalid(
            0,
            format!("HDF5 filter output exceeds {max_output} bytes"),
        ));
    }
    Ok(data)
}

/// Inverse of the HDF5 shuffle filter: byte plane k holds byte k of every
/// element; re-interleave.
fn unshuffle(data: &[u8], element_size: usize) -> Vec<u8> {
    if element_size <= 1 || !data.len().is_multiple_of(element_size) {
        return data.to_vec();
    }
    let count = data.len() / element_size;
    let mut out = vec![0u8; data.len()];
    for plane in 0..element_size {
        for element in 0..count {
            out[element * element_size + plane] = data[plane * count + element];
        }
    }
    out
}

/// Copy one decoded chunk into the dataset buffer, clipping edge chunks.
fn copy_chunk(
    out: &mut [u8],
    chunk: &[u8],
    dims: &[usize],
    chunk_dims: &[usize],
    offsets: &[usize],
    element_size: usize,
) {
    // Treat the dataset as (outer, row) where row = innermost dimension —
    // sufficient for the 1-D/2-D arrays polar volumes use; higher ranks
    // copy via the same row loop with composite outer indices.
    let rank = dims.len();
    if rank == 0 || chunk_dims.len() != rank || offsets.len() < rank {
        return;
    }
    let row_len = dims[rank - 1];
    let chunk_row_len = chunk_dims[rank - 1];
    let row_offset = offsets[rank - 1];
    let copy_cols = chunk_row_len.min(row_len.saturating_sub(row_offset));
    if copy_cols == 0 {
        return;
    }
    // Number of rows in the chunk = product of all but the last chunk dim.
    let chunk_rows: usize = chunk_dims[..rank - 1].iter().product::<usize>().max(1);
    for chunk_row in 0..chunk_rows {
        // Decompose the chunk row into per-dimension indices.
        let mut remaining = chunk_row;
        let mut out_index = 0usize;
        let mut in_bounds = true;
        for dim in 0..rank - 1 {
            let stride: usize = chunk_dims[dim + 1..rank - 1]
                .iter()
                .product::<usize>()
                .max(1);
            let local = remaining / stride;
            remaining %= stride;
            let global = offsets[dim] + local;
            if global >= dims[dim] {
                in_bounds = false;
                break;
            }
            let out_stride: usize = dims[dim + 1..].iter().product();
            out_index += global * out_stride;
        }
        if !in_bounds {
            continue;
        }
        out_index += row_offset;
        let src = chunk_row * chunk_row_len * element_size;
        let dst = out_index * element_size;
        let len = copy_cols * element_size;
        if src + len <= chunk.len() && dst + len <= out.len() {
            out[dst..dst + len].copy_from_slice(&chunk[src..src + len]);
        }
    }
}

fn heap_string(bytes: &[u8], heap_data: u64, name_offset: u64) -> Result<String> {
    let start = (heap_data + name_offset) as usize;
    let tail = bytes
        .get(start..)
        .ok_or_else(|| truncated(start, 1, bytes.len()))?;
    let name = tail.split(|byte| *byte == 0).next().unwrap_or_default();
    Ok(String::from_utf8_lossy(name).into_owned())
}

fn read_u8(bytes: &[u8], at: usize) -> Result<u8> {
    bytes
        .get(at)
        .copied()
        .ok_or_else(|| truncated(at, 1, bytes.len()))
}

fn checked_range(bytes: &[u8], at: usize, len: usize) -> Result<&[u8]> {
    let end = at
        .checked_add(len)
        .ok_or_else(|| invalid(at, "HDF5 byte range overflow"))?;
    bytes
        .get(at..end)
        .ok_or_else(|| truncated(at, len, bytes.len()))
}

fn read_le_u16(bytes: &[u8], at: usize) -> Result<u16> {
    let raw = checked_range(bytes, at, 2)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn address_to_usize(address: u64) -> Result<usize> {
    usize::try_from(address).map_err(|_| invalid(0, "HDF5 address overflows usize"))
}

fn checked_product(values: &[usize], context: &'static str) -> Result<usize> {
    values.iter().try_fold(1usize, |product, value| {
        product
            .checked_mul(*value)
            .ok_or_else(|| invalid(0, format!("{context} overflow")))
    })
}

fn checked_allocation_bytes(
    count: usize,
    element_size: usize,
    limit: usize,
    context: &'static str,
) -> Result<usize> {
    let bytes = count
        .checked_mul(element_size)
        .ok_or_else(|| invalid(0, format!("{context} byte-size overflow")))?;
    if bytes > limit {
        return Err(invalid(
            0,
            format!("{context} requires {bytes} bytes (limit {limit})"),
        ));
    }
    Ok(bytes)
}

/// Little-endian unsigned integer of `size` bytes (HDF5 metadata is always
/// little-endian).
fn read_offset(bytes: &[u8], at: usize, size: usize) -> Result<u64> {
    let raw = checked_range(bytes, at, size)?;
    let mut value = 0u64;
    for (index, byte) in raw.iter().enumerate() {
        value |= u64::from(*byte) << (8 * index);
    }
    // Map a size-4 undefined address (all ones) to the canonical sentinel.
    if size < 8 && value == (1u64 << (8 * size)) - 1 {
        return Ok(UNDEFINED_ADDR);
    }
    Ok(value)
}

/// Little-endian unsigned integer of `size` bytes WITHOUT the undefined-
/// address sentinel mapping of [`read_offset`] — for sizes and lengths,
/// where an all-ones value is a value, not "undefined".
fn read_uint(bytes: &[u8], at: usize, size: usize) -> Result<u64> {
    let raw = checked_range(bytes, at, size)?;
    let mut value = 0u64;
    for (index, byte) in raw.iter().enumerate() {
        value |= u64::from(*byte) << (8 * index);
    }
    Ok(value)
}

/// Bob Jenkins' lookup3 `hashlittle` over little-endian words — the
/// H5_checksum_lookup3 metadata checksum used by v2 object headers and
/// their continuation blocks (and other 1.8+ structures).
fn jenkins_lookup3(data: &[u8]) -> u32 {
    let init = 0xdead_beef_u32.wrapping_add(data.len() as u32);
    let (mut a, mut b, mut c) = (init, init, init);
    let word = |chunk: &[u8]| u32::from_le_bytes(chunk.try_into().expect("4 bytes"));
    let mut rest = data;
    while rest.len() > 12 {
        a = a.wrapping_add(word(&rest[0..4]));
        b = b.wrapping_add(word(&rest[4..8]));
        c = c.wrapping_add(word(&rest[8..12]));
        // mix(a, b, c)
        a = a.wrapping_sub(c) ^ c.rotate_left(4);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a) ^ a.rotate_left(6);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b) ^ b.rotate_left(8);
        b = b.wrapping_add(a);
        a = a.wrapping_sub(c) ^ c.rotate_left(16);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a) ^ a.rotate_left(19);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b) ^ b.rotate_left(4);
        b = b.wrapping_add(a);
        rest = &rest[12..];
    }
    if rest.is_empty() {
        // hashlittle: a zero-length tail skips the final mix entirely.
        return c;
    }
    // The 1..=12 byte tail reads as three zero-padded words (the C switch
    // adds only the bytes present, which is the same thing).
    let mut tail = [0u8; 12];
    tail[..rest.len()].copy_from_slice(rest);
    a = a.wrapping_add(word(&tail[0..4]));
    b = b.wrapping_add(word(&tail[4..8]));
    c = c.wrapping_add(word(&tail[8..12]));
    // final(a, b, c)
    c = (c ^ b).wrapping_sub(b.rotate_left(14));
    a = (a ^ c).wrapping_sub(c.rotate_left(11));
    b = (b ^ a).wrapping_sub(a.rotate_left(25));
    c = (c ^ b).wrapping_sub(b.rotate_left(16));
    a = (a ^ c).wrapping_sub(c.rotate_left(4));
    b = (b ^ a).wrapping_sub(a.rotate_left(14));
    c = (c ^ b).wrapping_sub(b.rotate_left(24));
    c
}

fn read_int(raw: &[u8], signed: bool, big_endian: bool) -> i64 {
    let mut value = 0u64;
    if big_endian {
        for byte in raw {
            value = (value << 8) | u64::from(*byte);
        }
    } else {
        for (index, byte) in raw.iter().enumerate() {
            value |= u64::from(*byte) << (8 * index);
        }
    }
    if signed && !raw.is_empty() && raw.len() < 8 {
        let sign_bit = 1u64 << (8 * raw.len() - 1);
        if value & sign_bit != 0 {
            value |= !((1u64 << (8 * raw.len())) - 1);
        }
    }
    value as i64
}

fn read_float(raw: &[u8], big_endian: bool) -> Result<f64> {
    match raw.len() {
        4 => {
            let bits = if big_endian {
                u32::from_be_bytes(raw.try_into().expect("4 bytes"))
            } else {
                u32::from_le_bytes(raw.try_into().expect("4 bytes"))
            };
            Ok(f64::from(f32::from_bits(bits)))
        }
        8 => {
            let bits = if big_endian {
                u64::from_be_bytes(raw.try_into().expect("8 bytes"))
            } else {
                u64::from_le_bytes(raw.try_into().expect("8 bytes"))
            };
            Ok(f64::from_bits(bits))
        }
        other => Err(invalid(0, format!("float width {other} unsupported"))),
    }
}

fn invalid(offset: usize, reason: impl Into<String>) -> NexradError {
    NexradError::InvalidMessage {
        offset,
        reason: reason.into(),
    }
}

fn truncated(offset: usize, needed: usize, available: usize) -> NexradError {
    NexradError::Truncated {
        what: "HDF5 structure",
        offset,
        needed,
        available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser(bytes: &[u8]) -> H5File<'_> {
        H5File {
            bytes,
            offset_size: 8,
            length_size: 8,
            objects: BTreeMap::new(),
            budget_total: MAX_HDF5_TOTAL_DATASET_BYTES,
            budget_left: Cell::new(MAX_HDF5_TOTAL_DATASET_BYTES),
        }
    }

    #[test]
    fn magic_sniffer_matches_signature_only() {
        assert!(looks_like_hdf5_bytes(b"\x89HDF\r\n\x1a\nrest"));
        assert!(!looks_like_hdf5_bytes(b"\x89HDF\r\n\x1a"));
        assert!(!looks_like_hdf5_bytes(b"CDF\x01...."));
        assert!(!looks_like_hdf5_bytes(b"AR2V0006."));
    }

    #[test]
    fn unshuffle_reinterleaves_byte_planes() {
        // Two u16 elements 0x0201, 0x0403 shuffled = planes [01 03][02 04].
        let shuffled = [0x01, 0x03, 0x02, 0x04];
        assert_eq!(unshuffle(&shuffled, 2), vec![0x01, 0x02, 0x03, 0x04]);
        // Non-multiple lengths pass through untouched.
        assert_eq!(unshuffle(&[1, 2, 3], 2), vec![1, 2, 3]);
    }

    #[test]
    fn read_int_sign_extends_little_and_big_endian() {
        assert_eq!(read_int(&[0xFF], true, false), -1);
        assert_eq!(read_int(&[0xFF], false, false), 255);
        assert_eq!(read_int(&[0xFE, 0xFF], true, false), -2);
        assert_eq!(read_int(&[0xFF, 0xFE], true, true), -2);
        assert_eq!(read_int(&[0x2A, 0, 0, 0, 0, 0, 0, 0], false, false), 42);
    }

    #[test]
    fn undefined_addresses_normalize_across_offset_sizes() {
        assert_eq!(
            read_offset(&[0xFF, 0xFF, 0xFF, 0xFF], 0, 4).unwrap(),
            UNDEFINED_ADDR
        );
        assert_eq!(read_offset(&[0x10, 0, 0, 0], 0, 4).unwrap(), 0x10);
    }

    /// Unlike `read_offset`, `read_uint` must NOT map all-ones to the
    /// undefined sentinel — 0xFF is a legal 1-byte chunk size.
    #[test]
    fn read_uint_keeps_all_ones_values() {
        assert_eq!(read_uint(&[0xFF], 0, 1).unwrap(), 0xFF);
        assert_eq!(read_uint(&[0xFF, 0xFF], 0, 2).unwrap(), 0xFFFF);
        assert_eq!(read_uint(&[0x83, 0x01], 0, 2).unwrap(), 0x0183);
    }

    #[test]
    fn truncated_messages_return_errors_instead_of_indexing() {
        let file = parser(&[]);
        assert!(file.parse_attribute(&[1], "name").is_err());
        let Err(_) = file.parse_layout(&[3, 0]) else {
            panic!("truncated compact layout must fail");
        };
        let Err(_) = file.parse_filter_pipeline(&[1, 1]) else {
            panic!("truncated filter pipeline must fail");
        };
    }

    #[test]
    fn v1_object_header_rejects_continuation_cycle() {
        let mut bytes = vec![0u8; 40];
        bytes[0] = 1;
        bytes[2..4].copy_from_slice(&1u16.to_le_bytes());
        bytes[8..12].copy_from_slice(&24u32.to_le_bytes());
        bytes[16..18].copy_from_slice(&0x0010u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&16u16.to_le_bytes());
        bytes[24..32].copy_from_slice(&16u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&24u64.to_le_bytes());

        let file = parser(&bytes);
        let Err(err) = file.parse_object_header(0) else {
            panic!("continuation cycle must fail");
        };
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn btree_walks_reject_self_references() {
        let mut group = vec![0u8; 40];
        group[..4].copy_from_slice(b"TREE");
        group[5] = 1;
        group[6..8].copy_from_slice(&1u16.to_le_bytes());
        let file = parser(&group);
        let mut entries = Vec::new();
        let mut visited = BTreeSet::new();
        let err = file
            .collect_group_entries(0, &mut entries, &mut visited, 0)
            .expect_err("group B-tree cycle must fail");
        assert!(err.to_string().contains("cycle"));

        let mut chunks = vec![0u8; 48];
        chunks[..4].copy_from_slice(b"TREE");
        chunks[4] = 1;
        chunks[5] = 1;
        chunks[6..8].copy_from_slice(&1u16.to_le_bytes());
        let file = parser(&chunks);
        let mut refs = Vec::new();
        let mut visited = BTreeSet::new();
        let err = file
            .collect_chunks(0, 1, &mut refs, &mut visited, 0)
            .expect_err("chunk B-tree cycle must fail");
        assert!(err.to_string().contains("cycle"));
    }

    /// Build a chain of single-entry v1 internal B-tree nodes. This is the
    /// crafted shape for which node count does not bound recursion depth.
    fn btree_chain(node_type: u8, links: usize, key_size: usize) -> Vec<u8> {
        const OFFSET_SIZE: usize = 8;
        let header = 8 + 2 * OFFSET_SIZE;
        let stride = header + key_size + OFFSET_SIZE;
        let mut bytes = vec![0u8; stride * links];
        for index in 0..links {
            let at = index * stride;
            bytes[at..at + 4].copy_from_slice(b"TREE");
            bytes[at + 4] = node_type;
            bytes[at + 5] = 1;
            bytes[at + 6..at + 8].copy_from_slice(&1u16.to_le_bytes());
            let child = ((index + 1) * stride) as u64;
            let child_at = at + header + key_size;
            bytes[child_at..child_at + OFFSET_SIZE].copy_from_slice(&child.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn btree_walks_reject_crafted_depth() {
        let chunks = btree_chain(1, 200, 8 + 8);
        let file = parser(&chunks);
        let err = file
            .collect_chunks(0, 1, &mut Vec::new(), &mut BTreeSet::new(), 0)
            .expect_err("a deep chunk B-tree chain must fail");
        assert!(err.to_string().contains("too deep"), "{err}");

        let groups = btree_chain(0, 200, 8);
        let file = parser(&groups);
        let err = file
            .collect_group_entries(0, &mut Vec::new(), &mut BTreeSet::new(), 0)
            .expect_err("a deep group B-tree chain must fail");
        assert!(err.to_string().contains("too deep"), "{err}");
    }

    #[test]
    fn a_deep_btree_chain_cannot_overflow_a_small_stack() {
        let chain = btree_chain(1, 20_000, 8 + 8);
        let outcome = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let file = parser(&chain);
                file.collect_chunks(0, 1, &mut Vec::new(), &mut BTreeSet::new(), 0)
                    .map_err(|err| err.to_string())
            })
            .expect("spawn decoding thread")
            .join()
            .expect("the decoding thread must survive the file");
        assert!(
            outcome.is_err_and(|message| message.contains("too deep")),
            "a crafted chain must error, not recurse"
        );
    }

    #[test]
    fn the_decode_budget_bounds_the_sum_of_datasets() {
        const SYNTH: &[u8] = include_bytes!("../tests/data/odim_pvol_synth.h5");
        const PLANE_BYTES: usize = 36 * 25;

        let file = H5File::open_within_budget(SYNTH, PLANE_BYTES).expect("open");
        file.dataset("/dataset1/data1/data")
            .expect("the first plane fits the budget");
        let err = file
            .dataset("/dataset1/data2/data")
            .expect_err("the sum of planes must be bounded");
        assert!(err.to_string().contains("decode budget"), "{err}");

        let file = H5File::open(SYNTH).expect("open");
        let planes = file.child_names("/dataset1");
        assert!(planes.len() > 2, "fixture must have several planes");
        for plane in planes.iter().filter(|name| name.starts_with("data")) {
            file.dataset(&format!("/dataset1/{plane}/data"))
                .unwrap_or_else(|err| panic!("{plane} must decode: {err}"));
        }
    }

    /// Jenkins lookup3 (hashlittle) known-answer vectors. The 30-byte
    /// phrase with init 0 is the published lookup3 self-test value; the
    /// shorter vectors pin every tail-length branch class (empty, <4,
    /// exactly 12 = one full block, 13 = block + 1-byte tail) and were
    /// cross-checked against real HDF5 v2 header checksums (AEMET espdg
    /// PVOL fixture) with an independent Python implementation.
    #[test]
    fn jenkins_lookup3_matches_reference_vectors() {
        assert_eq!(jenkins_lookup3(b""), 0xdead_beef);
        assert_eq!(
            jenkins_lookup3(b"Four score and seven years ago"),
            0x1777_0551
        );
        assert_eq!(jenkins_lookup3(b"abc"), 0x0e39_7631);
        assert_eq!(jenkins_lookup3(b"0123456789ab"), 0x1065_e50a);
        assert_eq!(jenkins_lookup3(b"0123456789abc"), 0x7351_ce56);
    }
}
