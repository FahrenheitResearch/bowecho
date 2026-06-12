//! Minimal read-only classic netCDF parser — just enough for CfRadial 1.x.
//!
//! Implements the netCDF classic file format per the Unidata "NetCDF Classic
//! Format Specification" (a.k.a. CDF-1) and its 64-bit-offset variant
//! (CDF-2): <https://docs.unidata.ucar.edu/netcdf-c/current/file_format_specifications.html>.
//! CDF-5 (64-bit data, `CDF\x05`) is detected and rejected with a clear
//! error — radar moment files do not use it. All values are big-endian.
//!
//! Supported: dimension/attribute/variable lists, the six classic types
//! (byte, char, short, int, float, double), fixed-size variables, and
//! record variables (unlimited dimension) including the single-record-
//! variable no-padding special case. netCDF-4/HDF5 files never reach this
//! module (they carry the HDF5 magic, not `CDF`).

use std::collections::BTreeMap;

use crate::{NexradError, Result};

const NC_DIMENSION: u32 = 0x0A;
const NC_VARIABLE: u32 = 0x0B;
const NC_ATTRIBUTE: u32 = 0x0C;

/// `true` for classic netCDF magic (`CDF\x01` or `CDF\x02`). CDF-5 sniffs
/// true as well so the decoder can reject it with a useful message instead
/// of the Level II decoder's.
pub fn looks_like_netcdf3_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..3] == b"CDF" && matches!(bytes[3], 1 | 2 | 5)
}

/// An attribute value (global or per-variable).
#[derive(Clone, Debug, PartialEq)]
pub enum NcValue {
    Str(String),
    Doubles(Vec<f64>),
    Ints(Vec<i64>),
}

impl NcValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Doubles(values) => values.first().copied(),
            Self::Ints(values) => values.first().map(|value| *value as f64),
            _ => None,
        }
    }
}

/// Variable data, decoded from big-endian storage.
#[derive(Clone, Debug)]
pub enum NcArray {
    I8(Vec<i8>),
    Char(Vec<u8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl NcArray {
    pub fn len(&self) -> usize {
        match self {
            Self::I8(values) => values.len(),
            Self::Char(values) => values.len(),
            Self::I16(values) => values.len(),
            Self::I32(values) => values.len(),
            Self::F32(values) => values.len(),
            Self::F64(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Element as f64 (numeric types only).
    pub fn get_f64(&self, index: usize) -> Option<f64> {
        match self {
            Self::I8(values) => values.get(index).map(|value| f64::from(*value)),
            Self::Char(_) => None,
            Self::I16(values) => values.get(index).map(|value| f64::from(*value)),
            Self::I32(values) => values.get(index).map(|value| f64::from(*value)),
            Self::F32(values) => values.get(index).map(|value| f64::from(*value)),
            Self::F64(values) => values.get(index).copied(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct NcVar {
    pub name: String,
    /// Dimension indices into [`Nc3File::dims`].
    pub dim_ids: Vec<usize>,
    pub attrs: BTreeMap<String, NcValue>,
    nc_type: u32,
    begin: u64,
}

impl NcVar {
    pub fn attr_str(&self, name: &str) -> Option<&str> {
        self.attrs.get(name).and_then(NcValue::as_str)
    }

    pub fn attr_f64(&self, name: &str) -> Option<f64> {
        self.attrs.get(name).and_then(NcValue::as_f64)
    }
}

/// Parsed header of a classic netCDF file plus the backing bytes.
pub struct Nc3File<'a> {
    bytes: &'a [u8],
    /// (name, length) — the record dimension stores its per-file length
    /// (`numrecs`), not zero.
    pub dims: Vec<(String, usize)>,
    pub record_dim: Option<usize>,
    pub numrecs: usize,
    pub gattrs: BTreeMap<String, NcValue>,
    pub vars: BTreeMap<String, NcVar>,
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
    offset64: bool,
}

impl<'a> Cursor<'a> {
    fn u32(&mut self) -> Result<u32> {
        let raw = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| truncated(self.at, 4, self.bytes.len()))?;
        self.at += 4;
        Ok(u32::from_be_bytes(raw.try_into().expect("4 bytes")))
    }

    fn offset(&mut self) -> Result<u64> {
        if self.offset64 {
            let raw = self
                .bytes
                .get(self.at..self.at + 8)
                .ok_or_else(|| truncated(self.at, 8, self.bytes.len()))?;
            self.at += 8;
            Ok(u64::from_be_bytes(raw.try_into().expect("8 bytes")))
        } else {
            Ok(u64::from(self.u32()?))
        }
    }

    fn name(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let raw = self
            .bytes
            .get(self.at..self.at + len)
            .ok_or_else(|| truncated(self.at, len, self.bytes.len()))?;
        self.at += len.div_ceil(4) * 4; // names pad to 4-byte boundaries
        Ok(String::from_utf8_lossy(raw).into_owned())
    }

    fn attrs(&mut self) -> Result<BTreeMap<String, NcValue>> {
        let tag = self.u32()?;
        let count = self.u32()? as usize;
        if tag != NC_ATTRIBUTE && (tag != 0 || count != 0) {
            return Err(invalid(self.at, "malformed attribute list tag"));
        }
        let mut attrs = BTreeMap::new();
        for _ in 0..count {
            let name = self.name()?;
            let nc_type = self.u32()?;
            let nelems = self.u32()? as usize;
            let elem_size = type_size(nc_type, self.at)?;
            let byte_len = nelems * elem_size;
            let raw = self
                .bytes
                .get(self.at..self.at + byte_len)
                .ok_or_else(|| truncated(self.at, byte_len, self.bytes.len()))?;
            self.at += byte_len.div_ceil(4) * 4;
            let value = match nc_type {
                2 => NcValue::Str(
                    String::from_utf8_lossy(raw)
                        .trim_end_matches('\0')
                        .to_owned(),
                ),
                1 => NcValue::Ints(raw.iter().map(|byte| i64::from(*byte as i8)).collect()),
                3 => NcValue::Ints(
                    raw.chunks_exact(2)
                        .map(|pair| i64::from(i16::from_be_bytes([pair[0], pair[1]])))
                        .collect(),
                ),
                4 => NcValue::Ints(
                    raw.chunks_exact(4)
                        .map(|quad| {
                            i64::from(i32::from_be_bytes(quad.try_into().expect("4 bytes")))
                        })
                        .collect(),
                ),
                5 => NcValue::Doubles(
                    raw.chunks_exact(4)
                        .map(|quad| {
                            f64::from(f32::from_be_bytes(quad.try_into().expect("4 bytes")))
                        })
                        .collect(),
                ),
                6 => NcValue::Doubles(
                    raw.chunks_exact(8)
                        .map(|oct| f64::from_be_bytes(oct.try_into().expect("8 bytes")))
                        .collect(),
                ),
                other => return Err(invalid(self.at, format!("attribute type {other}"))),
            };
            attrs.insert(name, value);
        }
        Ok(attrs)
    }
}

impl<'a> Nc3File<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        if !looks_like_netcdf3_bytes(bytes) {
            return Err(invalid(0, "missing netCDF classic magic"));
        }
        let version = bytes[3];
        if version == 5 {
            return Err(invalid(
                3,
                "CDF-5 (64-bit data) netCDF is unsupported; convert with `nccopy -k classic`",
            ));
        }
        let mut cursor = Cursor {
            bytes,
            at: 4,
            offset64: version == 2,
        };
        let numrecs = {
            let raw = cursor.u32()?;
            // 0xFFFFFFFF = STREAMING sentinel; treat as zero records.
            if raw == u32::MAX { 0 } else { raw as usize }
        };

        // Dimension list.
        let tag = cursor.u32()?;
        let dim_count = cursor.u32()? as usize;
        if tag != NC_DIMENSION && (tag != 0 || dim_count != 0) {
            return Err(invalid(cursor.at, "malformed dimension list tag"));
        }
        let mut dims = Vec::with_capacity(dim_count);
        let mut record_dim = None;
        for index in 0..dim_count {
            let name = cursor.name()?;
            let len = cursor.u32()? as usize;
            if len == 0 {
                record_dim = Some(index);
                dims.push((name, numrecs));
            } else {
                dims.push((name, len));
            }
        }

        let gattrs = cursor.attrs()?;

        // Variable list.
        let tag = cursor.u32()?;
        let var_count = cursor.u32()? as usize;
        if tag != NC_VARIABLE && (tag != 0 || var_count != 0) {
            return Err(invalid(cursor.at, "malformed variable list tag"));
        }
        let mut vars = BTreeMap::new();
        for _ in 0..var_count {
            let name = cursor.name()?;
            let ndims = cursor.u32()? as usize;
            let mut dim_ids = Vec::with_capacity(ndims);
            for _ in 0..ndims {
                dim_ids.push(cursor.u32()? as usize);
            }
            let attrs = cursor.attrs()?;
            let nc_type = cursor.u32()?;
            let _vsize = cursor.u32()?; // recomputed below; unreliable for big vars
            let begin = cursor.offset()?;
            vars.insert(
                name.clone(),
                NcVar {
                    name,
                    dim_ids,
                    attrs,
                    nc_type,
                    begin,
                },
            );
        }

        Ok(Self {
            bytes,
            dims,
            record_dim,
            numrecs,
            gattrs,
            vars,
        })
    }

    pub fn gattr_str(&self, name: &str) -> Option<&str> {
        self.gattrs.get(name).and_then(NcValue::as_str)
    }

    /// Resolved dimension lengths of a variable (record dim → numrecs).
    pub fn var_dims(&self, var: &NcVar) -> Vec<usize> {
        var.dim_ids
            .iter()
            .map(|id| self.dims.get(*id).map(|(_, len)| *len).unwrap_or(0))
            .collect()
    }

    fn is_record_var(&self, var: &NcVar) -> bool {
        matches!((var.dim_ids.first(), self.record_dim), (Some(first), Some(record)) if *first == record)
    }

    /// Per-record slab size in bytes for a record variable (or the full
    /// size for a fixed variable), before padding.
    fn slab_bytes(&self, var: &NcVar) -> Result<usize> {
        let elem = type_size(var.nc_type, 0)?;
        let skip_record = usize::from(self.is_record_var(var));
        let count: usize = var.dim_ids[skip_record..]
            .iter()
            .map(|id| self.dims.get(*id).map(|(_, len)| *len).unwrap_or(0))
            .product();
        Ok(count * elem)
    }

    /// Read the full data array of `name`, de-interleaving record slabs.
    pub fn read_var(&self, name: &str) -> Result<NcArray> {
        let var = self
            .vars
            .get(name)
            .ok_or_else(|| invalid(0, format!("netCDF variable '{name}' not found")))?;
        let slab = self.slab_bytes(var)?;
        let raw: Vec<u8> = if self.is_record_var(var) {
            // recsize = sum over record vars of their padded slabs; the
            // single-record-variable case is unpadded per the spec.
            let record_vars: Vec<&NcVar> = self
                .vars
                .values()
                .filter(|candidate| self.is_record_var(candidate))
                .collect();
            let recsize: usize = if record_vars.len() == 1 {
                slab
            } else {
                record_vars
                    .iter()
                    .map(|candidate| {
                        self.slab_bytes(candidate)
                            .map(|bytes| bytes.div_ceil(4) * 4)
                    })
                    .sum::<Result<usize>>()?
            };
            let mut raw = Vec::with_capacity(slab * self.numrecs);
            for record in 0..self.numrecs {
                let start = var.begin as usize + record * recsize;
                let chunk = self
                    .bytes
                    .get(start..start + slab)
                    .ok_or_else(|| truncated(start, slab, self.bytes.len()))?;
                raw.extend_from_slice(chunk);
            }
            raw
        } else {
            let start = var.begin as usize;
            self.bytes
                .get(start..start + slab)
                .ok_or_else(|| truncated(start, slab, self.bytes.len()))?
                .to_vec()
        };
        decode_array(&raw, var.nc_type)
    }
}

fn decode_array(raw: &[u8], nc_type: u32) -> Result<NcArray> {
    Ok(match nc_type {
        1 => NcArray::I8(raw.iter().map(|byte| *byte as i8).collect()),
        2 => NcArray::Char(raw.to_vec()),
        3 => NcArray::I16(
            raw.chunks_exact(2)
                .map(|pair| i16::from_be_bytes([pair[0], pair[1]]))
                .collect(),
        ),
        4 => NcArray::I32(
            raw.chunks_exact(4)
                .map(|quad| i32::from_be_bytes(quad.try_into().expect("4 bytes")))
                .collect(),
        ),
        5 => NcArray::F32(
            raw.chunks_exact(4)
                .map(|quad| f32::from_be_bytes(quad.try_into().expect("4 bytes")))
                .collect(),
        ),
        6 => NcArray::F64(
            raw.chunks_exact(8)
                .map(|oct| f64::from_be_bytes(oct.try_into().expect("8 bytes")))
                .collect(),
        ),
        other => return Err(invalid(0, format!("netCDF type {other} unsupported"))),
    })
}

fn type_size(nc_type: u32, offset: usize) -> Result<usize> {
    match nc_type {
        1 | 2 => Ok(1),
        3 => Ok(2),
        4 | 5 => Ok(4),
        6 => Ok(8),
        other => Err(invalid(offset, format!("netCDF type {other} unsupported"))),
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
        what: "netCDF structure",
        offset,
        needed,
        available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handcrafted CDF-1: dim x=3; gattr title="hi"; var v(short, dims [x])
    /// with attr f=1.5f, data [1, -2, 300].
    fn tiny_cdf1() -> Vec<u8> {
        let mut b: Vec<u8> = Vec::new();
        b.extend(b"CDF\x01");
        b.extend(0u32.to_be_bytes()); // numrecs
        // dim list
        b.extend(NC_DIMENSION.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(1u32.to_be_bytes()); // name len
        b.extend(b"x\0\0\0"); // padded
        b.extend(3u32.to_be_bytes()); // length
        // global attrs
        b.extend(NC_ATTRIBUTE.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(5u32.to_be_bytes());
        b.extend(b"title\0\0\0");
        b.extend(2u32.to_be_bytes()); // NC_CHAR
        b.extend(2u32.to_be_bytes()); // 2 chars
        b.extend(b"hi\0\0");
        // var list
        b.extend(NC_VARIABLE.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(b"v\0\0\0");
        b.extend(1u32.to_be_bytes()); // ndims
        b.extend(0u32.to_be_bytes()); // dim id 0
        b.extend(NC_ATTRIBUTE.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(b"f\0\0\0");
        b.extend(5u32.to_be_bytes()); // NC_FLOAT
        b.extend(1u32.to_be_bytes());
        b.extend(1.5f32.to_be_bytes());
        b.extend(3u32.to_be_bytes()); // NC_SHORT
        b.extend(8u32.to_be_bytes()); // vsize (3×2 padded to 8)
        let begin = (b.len() + 4) as u32;
        b.extend(begin.to_be_bytes());
        b.extend(1i16.to_be_bytes());
        b.extend((-2i16).to_be_bytes());
        b.extend(300i16.to_be_bytes());
        b.extend([0u8, 0]); // pad
        b
    }

    #[test]
    fn magic_sniffer_accepts_classic_versions() {
        assert!(looks_like_netcdf3_bytes(b"CDF\x01...."));
        assert!(looks_like_netcdf3_bytes(b"CDF\x02...."));
        assert!(looks_like_netcdf3_bytes(b"CDF\x05....")); // sniffed, then rejected
        assert!(!looks_like_netcdf3_bytes(b"CDF\x03...."));
        assert!(!looks_like_netcdf3_bytes(b"\x89HDF\r\n\x1a\n"));
    }

    #[test]
    fn parses_handcrafted_cdf1() {
        let bytes = tiny_cdf1();
        let file = Nc3File::open(&bytes).expect("open");
        assert_eq!(file.dims, vec![("x".to_owned(), 3)]);
        assert_eq!(file.gattr_str("title"), Some("hi"));
        let var = file.vars.get("v").expect("var v");
        assert_eq!(var.attr_f64("f"), Some(1.5));
        assert_eq!(file.var_dims(var), vec![3]);
        let data = file.read_var("v").expect("data");
        match data {
            NcArray::I16(values) => assert_eq!(values, vec![1, -2, 300]),
            other => panic!("unexpected array {other:?}"),
        }
    }

    #[test]
    fn cdf5_is_rejected_with_guidance() {
        let mut bytes = tiny_cdf1();
        bytes[3] = 5;
        let Err(err) = Nc3File::open(&bytes) else {
            panic!("CDF-5 must be rejected");
        };
        assert!(err.to_string().contains("CDF-5"));
    }
}
