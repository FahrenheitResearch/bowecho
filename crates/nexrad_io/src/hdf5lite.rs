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
//! - Superblock v0/v1 (v2/v3 — the 1.10+ "latest" layout with v2 object
//!   headers — is detected and rejected with a clear error).
//! - Version 1 object headers, including continuation blocks.
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

use std::collections::BTreeMap;
use std::io::Read;

use flate2::read::ZlibDecoder;

use crate::{NexradError, Result};

const SIGNATURE: [u8; 8] = [0x89, b'H', b'D', b'F', b'\r', b'\n', 0x1a, b'\n'];
const UNDEFINED_ADDR: u64 = u64::MAX;
/// Defense against corrupt files: deepest group nesting we will walk.
const MAX_GROUP_DEPTH: usize = 16;
/// Defense against corrupt B-trees: most nodes visited per tree walk.
const MAX_BTREE_NODES: usize = 1 << 16;

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
}

impl<'a> H5File<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        if !looks_like_hdf5_bytes(bytes) {
            return Err(invalid(0, "missing HDF5 superblock signature"));
        }
        let version = *bytes.get(8).ok_or_else(|| truncated(8, 1, bytes.len()))?;
        if version > 1 {
            return Err(invalid(
                8,
                format!(
                    "HDF5 superblock version {version} (1.10+ 'latest' layout) is unsupported; \
                     rewrite the file with default/earliest library settings"
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
        };
        let header = file.parse_object_header(root_header)?;
        file.objects.insert("/".to_owned(), root_header);
        file.walk_group("", &header, 0)?;
        Ok(file)
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
        let element_count: usize = dims.iter().product();
        let byte_len = element_count * dtype.size;
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

    fn walk_group(&mut self, prefix: &str, header: &ObjectHeader, depth: usize) -> Result<()> {
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
            self.collect_group_entries(btree, &mut entries, 0)?;
            for (name_offset, child_address) in entries {
                let name = heap_string(self.bytes, heap_data, name_offset)?;
                let path = format!("{prefix}/{name}");
                if self.objects.contains_key(&path) {
                    continue; // hard-link cycle guard
                }
                let child = self.parse_object_header(child_address)?;
                self.objects.insert(path.clone(), child_address);
                self.walk_group(&path, &child, depth + 1)?;
            }
        }
        Ok(())
    }

    fn collect_group_entries(
        &self,
        node_address: u64,
        out: &mut Vec<(u64, u64)>,
        visited: usize,
    ) -> Result<()> {
        if visited > MAX_BTREE_NODES {
            return Err(invalid(0, "HDF5 group B-tree too large"));
        }
        let node = self.slice(node_address, 8 + 2 * self.offset_size)?;
        if &node[..4] != b"TREE" {
            return Err(invalid(node_address as usize, "expected TREE signature"));
        }
        let level = node[5];
        let entries = u16::from_le_bytes([node[6], node[7]]) as usize;
        // keys/children alternate after the two sibling addresses.
        let mut cursor = node_address as usize + 8 + 2 * self.offset_size;
        for _ in 0..entries {
            cursor += self.length_size; // key (heap offset) — unused here
            let child = read_offset(self.bytes, cursor, self.offset_size)?;
            cursor += self.offset_size;
            if level == 0 {
                self.read_snod(child, out)?;
            } else {
                self.collect_group_entries(child, out, visited + 1)?;
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
        let mut cursor = address as usize + 8;
        for _ in 0..count {
            let name_offset = read_offset(self.bytes, cursor, self.length_size)?;
            let header = read_offset(self.bytes, cursor + self.offset_size, self.offset_size)?;
            out.push((name_offset, header));
            cursor += entry_size;
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
        let head = self.slice(address, 16)?;
        if head[0] != 1 {
            return Err(invalid(
                address as usize,
                format!(
                    "object header version {} (v2/'latest' layout) is unsupported",
                    head[0]
                ),
            ));
        }
        let total_messages = u16::from_le_bytes([head[2], head[3]]) as usize;
        let block_size = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
        let mut messages = Vec::with_capacity(total_messages);
        // (start, length) message blocks; the first follows 4 pad bytes.
        let mut blocks = vec![(address as usize + 16, block_size)];
        let mut block_index = 0;
        while block_index < blocks.len() && messages.len() < total_messages {
            let (start, len) = blocks[block_index];
            block_index += 1;
            let mut cursor = start;
            let end = start + len;
            while cursor + 8 <= end && messages.len() < total_messages {
                let header = self.slice(cursor as u64, 8)?;
                let kind = u16::from_le_bytes([header[0], header[1]]);
                let size = u16::from_le_bytes([header[2], header[3]]) as usize;
                let body = self.slice(cursor as u64 + 8, size)?.to_vec();
                if kind == 0x0010 {
                    // Continuation: offset + length of the next block.
                    let offset = read_offset(&body, 0, self.offset_size)?;
                    let length = read_offset(&body, self.offset_size, self.length_size)?;
                    blocks.push((offset as usize, length as usize));
                } else {
                    messages.push(Message { kind, body });
                }
                cursor += 8 + size;
            }
        }
        Ok(ObjectHeader { messages })
    }

    // ----- messages -----------------------------------------------------

    fn parse_dataspace(&self, body: &[u8]) -> Result<Vec<usize>> {
        let version = *body.first().ok_or_else(|| truncated(0, 1, 0))?;
        let rank = *body.get(1).ok_or_else(|| truncated(1, 1, body.len()))? as usize;
        let dims_start = match version {
            1 => 8, // version, rank, flags, reserved[5]
            2 => 4, // version, rank, flags, type
            other => {
                return Err(invalid(0, format!("dataspace version {other} unsupported")));
            }
        };
        let mut dims = Vec::with_capacity(rank);
        for index in 0..rank {
            dims.push(read_offset(
                body,
                dims_start + index * self.length_size,
                self.length_size,
            )? as usize);
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
            0 => Ok(Datatype {
                class: DtClass::Int {
                    signed: bits & (1 << 3) != 0,
                },
                size,
                big_endian,
            }),
            1 => Ok(Datatype {
                class: DtClass::Float,
                size,
                big_endian,
            }),
            3 => Ok(Datatype {
                class: DtClass::FixedString,
                size,
                big_endian: false,
            }),
            9 if bits & 0x0F == 1 => Ok(Datatype {
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
                let size = u16::from_le_bytes([body[2], body[3]]) as usize;
                let data = body
                    .get(4..4 + size)
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
                let btree_address = read_offset(body, 3, self.offset_size)?;
                let mut chunk_dims = Vec::with_capacity(dimensionality);
                for index in 0..dimensionality {
                    let at = 3 + self.offset_size + index * 4;
                    let dim = body
                        .get(at..at + 4)
                        .ok_or_else(|| truncated(at, 4, body.len()))?;
                    chunk_dims.push(u32::from_le_bytes(dim.try_into().expect("4 bytes")) as usize);
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
            let id = u16::from_le_bytes([
                *body
                    .get(cursor)
                    .ok_or_else(|| truncated(cursor, 2, body.len()))?,
                *body
                    .get(cursor + 1)
                    .ok_or_else(|| truncated(cursor, 2, body.len()))?,
            ]);
            let has_name = version == 1 || id >= 256;
            let name_len = if has_name {
                u16::from_le_bytes([body[cursor + 2], body[cursor + 3]]) as usize
            } else {
                0
            };
            let after_id = if has_name { cursor + 4 } else { cursor + 2 };
            let value_count = u16::from_le_bytes([body[after_id + 2], body[after_id + 3]]) as usize;
            let mut at = after_id + 4;
            if name_len > 0 {
                at += if version == 1 {
                    name_len.div_ceil(8) * 8
                } else {
                    name_len
                };
            }
            let mut client_values = Vec::with_capacity(value_count);
            for index in 0..value_count {
                let v = body
                    .get(at + index * 4..at + index * 4 + 4)
                    .ok_or_else(|| truncated(at, 4, body.len()))?;
                client_values.push(u32::from_le_bytes(v.try_into().expect("4 bytes")));
            }
            at += value_count * 4;
            if version == 1 && value_count % 2 == 1 {
                at += 4;
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
        let flags = body[1];
        if version >= 2 && flags & 0x03 != 0 {
            return Err(invalid(
                0,
                "shared attribute datatype/dataspace unsupported",
            ));
        }
        let name_size = u16::from_le_bytes([body[2], body[3]]) as usize;
        let dt_size = u16::from_le_bytes([body[4], body[5]]) as usize;
        let ds_size = u16::from_le_bytes([body[6], body[7]]) as usize;
        let mut cursor = if version == 3 { 9 } else { 8 };
        let pad = |len: usize| {
            if version == 1 {
                len.div_ceil(8) * 8
            } else {
                len
            }
        };
        let name_bytes = body
            .get(cursor..cursor + name_size)
            .ok_or_else(|| truncated(cursor, name_size, body.len()))?;
        let name = name_bytes
            .split(|byte| *byte == 0)
            .next()
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        cursor += pad(name_size);
        if name != wanted {
            return Ok(None);
        }
        let dtype = self.parse_datatype(
            body.get(cursor..cursor + dt_size)
                .ok_or_else(|| truncated(cursor, dt_size, body.len()))?,
        )?;
        cursor += pad(dt_size);
        let dims = self.parse_dataspace(
            body.get(cursor..cursor + ds_size)
                .ok_or_else(|| truncated(cursor, ds_size, body.len()))?,
        )?;
        cursor += pad(ds_size);
        let count: usize = dims.iter().product::<usize>().max(1);
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
        let total: usize = dims.iter().product::<usize>() * element_size;
        let mut out = vec![0u8; total];
        if btree_address == UNDEFINED_ADDR {
            return Ok(out); // dataset never written
        }
        let mut chunks = Vec::new();
        self.collect_chunks(btree_address, chunk_dims.len() + 1, &mut chunks, 0)?;
        let chunk_elements: usize = chunk_dims.iter().product();
        for chunk in chunks {
            let stored = self.slice(chunk.address, chunk.stored_size)?;
            let raw = apply_inverse_filters(stored, filters, chunk.filter_mask, element_size)?;
            if raw.len() < chunk_elements * element_size {
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
        visited: usize,
    ) -> Result<()> {
        if visited > MAX_BTREE_NODES {
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
        let mut cursor = node_address as usize + 8 + 2 * self.offset_size;
        for _ in 0..entries {
            let key = self.slice(cursor as u64, key_size)?;
            let stored_size = u32::from_le_bytes(key[..4].try_into().expect("4 bytes")) as usize;
            let filter_mask = u32::from_le_bytes(key[4..8].try_into().expect("4 bytes"));
            let mut offsets = Vec::with_capacity(key_dims.saturating_sub(1));
            for dim in 0..key_dims.saturating_sub(1) {
                let at = 8 + dim * 8;
                offsets.push(
                    u64::from_le_bytes(key[at..at + 8].try_into().expect("8 bytes")) as usize,
                );
            }
            cursor += key_size;
            let child = read_offset(self.bytes, cursor, self.offset_size)?;
            cursor += self.offset_size;
            if level == 0 {
                out.push(ChunkRef {
                    address: child,
                    stored_size,
                    filter_mask,
                    offsets,
                });
            } else {
                self.collect_chunks(child, key_dims, out, visited + 1)?;
            }
        }
        Ok(())
    }

    fn slice(&self, address: u64, len: usize) -> Result<&'a [u8]> {
        let start =
            usize::try_from(address).map_err(|_| invalid(0, "HDF5 address overflows usize"))?;
        self.bytes
            .get(start..start + len)
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
) -> Result<Vec<u8>> {
    let mut data = stored.to_vec();
    for (index, filter) in filters.iter().enumerate().rev() {
        if filter_mask & (1 << index) != 0 {
            continue;
        }
        match filter.id {
            1 => {
                // gzip/deflate (zlib stream per the HDF5 deflate filter).
                let mut decoder = ZlibDecoder::new(&data[..]);
                let mut inflated = Vec::with_capacity(data.len() * 4);
                decoder
                    .read_to_end(&mut inflated)
                    .map_err(|err| invalid(0, format!("HDF5 deflate chunk: {err}")))?;
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

/// Little-endian unsigned integer of `size` bytes (HDF5 metadata is always
/// little-endian).
fn read_offset(bytes: &[u8], at: usize, size: usize) -> Result<u64> {
    let raw = bytes
        .get(at..at + size)
        .ok_or_else(|| truncated(at, size, bytes.len()))?;
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
}
