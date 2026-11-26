//! The compressed adjacency layout (#173): each node's neighbours sorted,
//! stored as the gaps between them, each gap a byte-aligned varint.
//!
//! A node's segment holds its neighbours in ascending order. The first is
//! stored as is and each later one as its distance from the one before, a
//! parallel edge being a gap of 0, so multiplicity is kept. Each gap is a
//! LEB128 varint: seven bits a byte, low bits first, the top bit set on
//! every byte but the last. A gap below 128 (most of them, once the nodes
//! are numbered for locality, #172) is one byte.
//!
//! [`Packed`] holds the bytes and, per node, the byte position its segment
//! starts at (`byte_offsets`, `n_nodes + 1` of them). The adjacency keeps
//! its slot offsets beside them, so a node's degree and its range of edge
//! ids are read without decoding (see [`Adjacency`]).
//!
//! Decoding streams: a kernel that reads a segment front to back
//! ([`Adjacency::for_each_neighbor`]) decodes it as it goes, and one that
//! needs a slice decodes it into a scratch buffer of its own
//! ([`Adjacency::neighbors_in`]).
//!
//! [`Adjacency`]: crate::topology::Adjacency
//! [`Adjacency::for_each_neighbor`]: crate::topology::Adjacency::for_each_neighbor
//! [`Adjacency::neighbors_in`]: crate::topology::Adjacency::neighbors_in

use arrow::buffer::ScalarBuffer;

/// The varint-coded, sorted neighbour segments of a compressed adjacency.
#[derive(Debug, Clone)]
pub struct Packed {
    /// Where node `u`'s segment starts in `bytes`; `n_nodes + 1` positions,
    /// from 0 to `bytes.len()`.
    pub byte_offsets: ScalarBuffer<u64>,
    /// The segments, one after another.
    pub bytes: ScalarBuffer<u8>,
}

impl Packed {
    /// Node `u`'s segment.
    #[inline]
    pub fn segment(&self, u: u32) -> &[u8] {
        let s = self.byte_offsets[u as usize] as usize;
        let e = self.byte_offsets[u as usize + 1] as usize;
        &self.bytes[s..e]
    }

    /// The bytes held, offsets included.
    pub fn heap_bytes(&self) -> usize {
        self.bytes.len() + 8 * self.byte_offsets.len()
    }
}

/// The bytes a varint of `x` takes.
#[inline]
pub fn varint_len(x: u32) -> usize {
    let bits = 32 - (x | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

/// Write `x` as a varint at `out[at..]`; the position after it.
#[inline]
pub fn put_varint(out: &mut [u8], mut at: usize, mut x: u32) -> usize {
    while x >= 0x80 {
        out[at] = (x as u8) | 0x80;
        x >>= 7;
        at += 1;
    }
    out[at] = x as u8;
    at + 1
}

/// The bytes of a segment of the sorted values `values`.
#[inline]
pub fn segment_len(values: &[u32]) -> usize {
    let mut prev = 0u32;
    values
        .iter()
        .map(|&v| {
            let len = varint_len(v.wrapping_sub(prev));
            prev = v;
            len
        })
        .sum()
}

/// Encode the sorted values `values` into `out`, which is
/// [`segment_len`]`(values)` bytes.
#[inline]
pub fn encode_segment(values: &[u32], out: &mut [u8]) {
    let mut prev = 0u32;
    let mut at = 0;
    for &v in values {
        at = put_varint(out, at, v.wrapping_sub(prev));
        prev = v;
    }
    debug_assert_eq!(at, out.len());
}

/// Call `f` with each value of the segment `bytes`, in order.
#[inline]
pub fn for_each(bytes: &[u8], mut f: impl FnMut(u32)) {
    let mut prev = 0u32;
    let mut acc = 0u32;
    let mut shift = 0u32;
    for &b in bytes {
        if b < 0x80 {
            // The common case: a one-byte gap, or the last byte of one.
            prev = prev.wrapping_add(acc | u32::from(b).wrapping_shl(shift));
            f(prev);
            acc = 0;
            shift = 0;
        } else {
            acc |= u32::from(b & 0x7f).wrapping_shl(shift);
            shift += 7;
        }
    }
}

/// Whether `f` holds for some value of the segment `bytes`, read in order
/// up to the first that does.
#[inline]
pub fn any(bytes: &[u8], mut f: impl FnMut(u32) -> bool) -> bool {
    let mut prev = 0u32;
    let mut acc = 0u32;
    let mut shift = 0u32;
    for &b in bytes {
        if b < 0x80 {
            prev = prev.wrapping_add(acc | u32::from(b).wrapping_shl(shift));
            if f(prev) {
                return true;
            }
            acc = 0;
            shift = 0;
        } else {
            acc |= u32::from(b & 0x7f).wrapping_shl(shift);
            shift += 7;
        }
    }
    false
}

/// Append the values of the segment `bytes` to `out`.
#[inline]
pub fn decode_into(bytes: &[u8], out: &mut Vec<u32>) {
    // At most one value a byte.
    out.reserve(bytes.len());
    for_each(bytes, |v| out.push(v));
}

/// The next value of a segment read from byte `at` of `bytes`, after the
/// value `prev`: the value and the byte after it.
#[inline]
pub fn next(bytes: &[u8], mut at: usize, prev: u32) -> (u32, usize) {
    let mut acc = 0u32;
    let mut shift = 0u32;
    loop {
        let b = bytes[at];
        at += 1;
        acc |= u32::from(b & 0x7f).wrapping_shl(shift);
        if b < 0x80 {
            return (prev.wrapping_add(acc), at);
        }
        shift += 7;
    }
}

/// Check that the segment `bytes` holds exactly `degree` values, each
/// below `n_nodes` and none below the one before, every varint at most
/// five bytes long and ending inside the segment.
pub fn check_segment(bytes: &[u8], degree: usize, n_nodes: usize) -> Result<(), &'static str> {
    let mut count = 0usize;
    let mut prev = 0u64;
    let mut acc = 0u64;
    let mut shift = 0u32;
    for &b in bytes {
        if shift > 28 {
            return Err("has a gap longer than five bytes");
        }
        acc |= u64::from(b & 0x7f) << shift;
        if b < 0x80 {
            prev += acc;
            if prev >= n_nodes as u64 {
                return Err("has a target out of range");
            }
            count += 1;
            acc = 0;
            shift = 0;
        } else {
            shift += 7;
        }
    }
    if shift != 0 {
        return Err("has a gap cut short at the end of a segment");
    }
    if count != degree {
        return Err("has a segment whose length is not its degree");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_round_trip() {
        for values in [
            vec![],
            vec![0],
            vec![0, 0, 0],
            vec![5, 5, 127, 128, 255, 16_383, 16_384, 2_097_151, 2_097_152],
            vec![1, 268_435_455, 268_435_456, u32::MAX - 1, u32::MAX, u32::MAX],
        ] {
            let mut bytes = vec![0u8; segment_len(&values)];
            encode_segment(&values, &mut bytes);
            let mut back = Vec::new();
            decode_into(&bytes, &mut back);
            assert_eq!(back, values);
            // Read one at a time.
            let (mut at, mut prev, mut one) = (0, 0, Vec::new());
            while at < bytes.len() {
                (prev, at) = next(&bytes, at, prev);
                one.push(prev);
            }
            assert_eq!(one, values);
            assert!(check_segment(&bytes, values.len(), u32::MAX as usize + 1).is_ok());
            assert_eq!(any(&bytes, |v| v == u32::MAX), values.contains(&u32::MAX));
        }
        assert_eq!(varint_len(0), 1);
        assert_eq!(varint_len(127), 1);
        assert_eq!(varint_len(128), 2);
        assert_eq!(varint_len(u32::MAX), 5);
    }

    #[test]
    fn malformed_segments_are_found() {
        assert!(check_segment(&[0x80], 1, 10).is_err());
        assert!(check_segment(&[3], 1, 3).is_err());
        assert!(check_segment(&[1, 1], 1, 10).is_err());
        assert!(check_segment(&[0xff, 0xff, 0xff, 0xff, 0xff, 0x01], 1, 10).is_err());
        assert!(check_segment(&[1, 1], 2, 10).is_ok());
    }
}
