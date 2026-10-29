use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Maximum coordinate value for 21-bit 3D and staggered 2D Morton codes (`2^21 - 1 = 2,097,151`).
pub const MAX_COORD_21: u32 = 0x001F_FFFF;

/// Spreads the lower 32 bits of `v` into the even bit positions of a `u64` (`0, 2, 4, ..., 62`).
#[inline]
pub fn spread_bits_2d(v: u32) -> u64 {
    let mut x = v as u64;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    x = (x | (x << 1)) & 0x5555_5555_5555_5555;
    x
}

/// Compacts the even bit positions of `x` (`0, 2, 4, ..., 62`) back into a `u32`.
#[inline]
pub fn compact_bits_2d(mut x: u64) -> u32 {
    x &= 0x5555_5555_5555_5555;
    x = (x | (x >> 1)) & 0x3333_3333_3333_3333;
    x = (x | (x >> 2)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x >> 4)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x >> 8)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x >> 16)) & 0x0000_0000_FFFF_FFFF;
    x as u32
}

/// Encodes a 2D coordinate `(x, y)` into a 64-bit Morton code (Z-order curve).
/// `x` is placed in even bits (`0, 2, ...`), `y` in odd bits (`1, 3, ...`).
#[inline]
pub fn morton_encode_2d(x: u32, y: u32) -> u64 {
    spread_bits_2d(x) | (spread_bits_2d(y) << 1)
}

/// Decodes a 64-bit 2D Morton code back into `(x, y)`.
#[inline]
pub fn morton_decode_2d(z: u64) -> (u32, u32) {
    (compact_bits_2d(z), compact_bits_2d(z >> 1))
}

/// Spreads the lower 21 bits of `v` into every 3rd bit of a `u64` (`0, 3, 6, ..., 60`).
#[inline]
pub fn spread_bits_3d(v: u32) -> u64 {
    let mut x = (v & MAX_COORD_21) as u64;
    x = (x | (x << 32)) & 0x001F_0000_0000_FFFF;
    x = (x | (x << 16)) & 0x001F_0000_FF00_00FF;
    x = (x | (x << 8)) & 0x100F_00F0_0F00_F00F;
    x = (x | (x << 4)) & 0x10C3_0C30_C30C_30C3;
    x = (x | (x << 2)) & 0x1249_2492_4924_9249;
    x
}

/// Compacts every 3rd bit of `x` (`0, 3, 6, ..., 60`) back into a 21-bit `u32`.
#[inline]
pub fn compact_bits_3d(mut x: u64) -> u32 {
    x &= 0x1249_2492_4924_9249;
    x = (x ^ (x >> 2)) & 0x10C3_0C30_C30C_30C3;
    x = (x ^ (x >> 4)) & 0x100F_00F0_0F00_F00F;
    x = (x ^ (x >> 8)) & 0x001F_0000_FF00_00FF;
    x = (x ^ (x >> 16)) & 0x001F_0000_0000_FFFF;
    x = (x ^ (x >> 32)) & 0x0000_0000_001F_FFFF;
    x as u32
}

/// Encodes a 3D coordinate `(x, y, z)` (21 bits each) into a 63-bit Morton code.
#[inline]
pub fn morton_encode_3d(x: u32, y: u32, z: u32) -> u64 {
    spread_bits_3d(x) | (spread_bits_3d(y) << 1) | (spread_bits_3d(z) << 2)
}

/// Decodes a 63-bit 3D Morton code back into `(x, y, z)` (21 bits each).
#[inline]
pub fn morton_decode_3d(m: u64) -> (u32, u32, u32) {
    (
        compact_bits_3d(m),
        compact_bits_3d(m >> 1),
        compact_bits_3d(m >> 2),
    )
}

/// Formats a 64-bit Morton code as a zero-padded 16-character lowercase hex string
/// suitable for lexicographical sorting in Redis `ZSET` (`ZRANGEBYLEX`).
#[inline]
pub fn format_morton_hex(z: u64) -> String {
    format!("{:016x}", z)
}

/// Parses a 16-character hex prefix from a Redis ZSET member `<16-hex>:<row_id>`.
#[inline]
pub fn parse_morton_member(member: &str) -> Option<(u64, u32)> {
    let (hex_part, id_part) = member.split_once(':')?;
    let z = u64::from_str_radix(hex_part, 16).ok()?;
    let row_id = id_part.parse::<u32>().ok()?;
    Some((z, row_id))
}

// ============================================================================
// Order-Preserving Coordinate Encoders for SQL Column Domains
// ============================================================================

/// Encodes `index` (`1..=2,097,151`) into a 21-bit integer.
#[inline]
pub fn encode_index_21(idx: u32) -> u32 {
    idx.min(MAX_COORD_21)
}

/// Converts a calendar date `(year, month, day)` to days since `2020-01-01`, clamped to `0..=1023` (10 bits).
/// Preserves chronological order across `2020-01-01` .. `2022-10-19`.
pub fn parse_date_days_since_2020(date_str: &str) -> Option<u32> {
    let mut parts = date_str.trim().split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;

    let days_from_2020 = days_since_civil_epoch(year, month, day) - days_since_civil_epoch(2020, 1, 1);
    Some(days_from_2020.clamp(0, 1023) as u32)
}

/// Howard Hinnant's civil date to days algorithm (exact, leap-year aware, strictly monotonic).
fn days_since_civil_epoch(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

/// Encodes a `YYYY-MM-DD` date into a 21-bit coordinate where the 10 active day bits
/// are placed at bits `19..10` (`days << 10`).
/// This aligns date resolution with the upper bits of `index` while leaving bits `9..0` zero.
#[inline]
pub fn encode_date_21(date_str: &str) -> u32 {
    let days = parse_date_days_since_2020(date_str).unwrap_or(0);
    (days & 0x3FF) << 10
}

/// Maps a single ASCII character (case-insensitive) to a 5-bit order-preserving alphabet code (`0..=28`):
/// - `0`: end of string
/// - `1`: space / punctuation `< 'a'`
/// - `2..=27`: `'a'..='z'`
/// - `28`: characters `> 'z'`
#[inline]
fn char_to_radix32(b: u8) -> u32 {
    let lower = b.to_ascii_lowercase();
    if lower >= b'a' && lower <= b'z' {
        (lower - b'a') as u32 + 2
    } else if lower < b'a' {
        1
    } else {
        28
    }
}

/// Encodes a string into a 15-bit order-preserving 3-character radix-32 integer (`0..=32767`).
/// If `pad_max` is true (used for upper bounds of prefix ranges), pads missing trailing chars with `31`.
pub fn encode_str_15bit(s: &str, pad_max: bool) -> u32 {
    let bytes = s.as_bytes();
    let pad = if pad_max { 31 } else { 0 };
    let c0 = bytes.first().map(|&b| char_to_radix32(b)).unwrap_or(pad);
    let c1 = bytes.get(1).map(|&b| char_to_radix32(b)).unwrap_or(pad);
    let c2 = bytes.get(2).map(|&b| char_to_radix32(b)).unwrap_or(pad);
    ((c0 & 0x1F) << 10) | ((c1 & 0x1F) << 5) | (c2 & 0x1F)
}

/// Encodes `country` for the 3D Morton index and staggered 2D `(country, city)` index:
/// Places the 15-bit 3-character prefix at bits `20..6` (`prefix15 << 6`).
#[inline]
pub fn encode_country_21(country: &str, pad_max: bool) -> u32 {
    encode_str_15bit(country, pad_max) << 6
}

/// Encodes `city` for the staggered 2D `(country, city)` Morton index:
/// Places the 15-bit 3-character prefix at bits `14..0`.
#[inline]
pub fn encode_city_21(city: &str, pad_max: bool) -> u32 {
    encode_str_15bit(city, pad_max)
}

// ============================================================================
// Z-Order Curve Range Decomposition (BIGMIN / LITMAX + Dyadic Block Pruning)
// ============================================================================

/// A contiguous interval `[start, end]` (inclusive) on the 1D Morton Z-order curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MortonInterval {
    pub start: u64,
    pub end: u64,
}

impl MortonInterval {
    #[inline]
    pub fn span(&self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubBox2D {
    min_x: u32,
    max_x: u32,
    min_y: u32,
    max_y: u32,
    z_min: u64,
    z_max: u64,
    split_bit: u32,
}

impl Ord for SubBox2D {
    fn cmp(&self, other: &Self) -> Ordering {
        self.split_bit
            .cmp(&other.split_bit)
            .then_with(|| (self.z_max - self.z_min).cmp(&(other.z_max - other.z_min)))
    }
}

impl PartialOrd for SubBox2D {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl SubBox2D {
    fn new(min_x: u32, max_x: u32, min_y: u32, max_y: u32) -> Self {
        let z_min = morton_encode_2d(min_x, min_y);
        let z_max = morton_encode_2d(max_x, max_y);
        let diff = z_min ^ z_max;
        let split_bit = if diff == 0 {
            0
        } else {
            64 - diff.leading_zeros()
        };
        Self {
            min_x,
            max_x,
            min_y,
            max_y,
            z_min,
            z_max,
            split_bit,
        }
    }

    /// Returns true if this 2D sub-box completely fills the Morton interval `[z_min, z_max]`
    /// with zero out-of-box points.
    #[inline]
    fn is_exact_dyadic(&self) -> bool {
        if self.z_min == self.z_max {
            return true;
        }
        let p = self.split_bit - 1;
        let mask = if p >= 63 { u64::MAX } else { (1u64 << (p + 1)) - 1 };
        (self.z_min & mask) == 0 && (self.z_max & mask) == mask
    }
}

/// Decomposes a 2D axis-aligned bounding box `[min_x..=max_x, min_y..=max_y]`
/// into at most `max_intervals` disjoint contiguous Morton intervals using
/// O(1) BIGMIN / LITMAX quadtree subdivision and dyadic block recognition.
pub fn decompose_morton_2d(
    min_x: u32,
    max_x: u32,
    min_y: u32,
    max_y: u32,
    max_intervals: usize,
) -> Vec<MortonInterval> {
    if min_x > max_x || min_y > max_y || max_intervals == 0 {
        return Vec::new();
    }

    let root = SubBox2D::new(min_x, max_x, min_y, max_y);
    if root.is_exact_dyadic() || max_intervals == 1 {
        return vec![MortonInterval {
            start: root.z_min,
            end: root.z_max,
        }];
    }

    let mut exact_intervals: Vec<MortonInterval> = Vec::new();
    let mut heap: BinaryHeap<SubBox2D> = BinaryHeap::new();
    heap.push(root);

    while let Some(cur) = heap.pop() {
        if cur.is_exact_dyadic() {
            exact_intervals.push(MortonInterval {
                start: cur.z_min,
                end: cur.z_max,
            });
            continue;
        }

        if exact_intervals.len() + heap.len() + 1 >= max_intervals {
            exact_intervals.push(MortonInterval {
                start: cur.z_min,
                end: cur.z_max,
            });
            break;
        }

        // Highest bit where z_min and z_max differ (0-indexed: 0..63)
        let p = cur.split_bit - 1;
        let dim = p % 2;
        let k = p / 2;
        let bit_mask = 1u32 << k;
        let low_mask = bit_mask - 1;

        if dim == 0 {
            // Split along X at bit k:
            // LITMAX is morton_encode_2d(split_low, cur.max_y)
            // BIGMIN is morton_encode_2d(split_high, cur.min_y)
            let split_high = cur.max_x & !low_mask;
            let split_low = split_high - 1;
            let left = SubBox2D::new(cur.min_x, split_low, cur.min_y, cur.max_y);
            let right = SubBox2D::new(split_high, cur.max_x, cur.min_y, cur.max_y);
            for child in [left, right] {
                if child.is_exact_dyadic() {
                    exact_intervals.push(MortonInterval {
                        start: child.z_min,
                        end: child.z_max,
                    });
                } else {
                    heap.push(child);
                }
            }
        } else {
            // Split along Y at bit k:
            // LITMAX is morton_encode_2d(cur.max_x, split_low)
            // BIGMIN is morton_encode_2d(cur.min_x, split_high)
            let split_high = cur.max_y & !low_mask;
            let split_low = split_high - 1;
            let left = SubBox2D::new(cur.min_x, cur.max_x, cur.min_y, split_low);
            let right = SubBox2D::new(cur.min_x, cur.max_x, split_high, cur.max_y);
            for child in [left, right] {
                if child.is_exact_dyadic() {
                    exact_intervals.push(MortonInterval {
                        start: child.z_min,
                        end: child.z_max,
                    });
                } else {
                    heap.push(child);
                }
            }
        }
    }

    for rem in heap {
        exact_intervals.push(MortonInterval {
            start: rem.z_min,
            end: rem.z_max,
        });
    }

    merge_intervals(exact_intervals)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubBox3D {
    min_x: u32,
    max_x: u32,
    min_y: u32,
    max_y: u32,
    min_z: u32,
    max_z: u32,
    z_min: u64,
    z_max: u64,
    split_bit: u32,
}

impl Ord for SubBox3D {
    fn cmp(&self, other: &Self) -> Ordering {
        self.split_bit
            .cmp(&other.split_bit)
            .then_with(|| (self.z_max - self.z_min).cmp(&(other.z_max - other.z_min)))
    }
}

impl PartialOrd for SubBox3D {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl SubBox3D {
    fn new(min_x: u32, max_x: u32, min_y: u32, max_y: u32, min_z: u32, max_z: u32) -> Self {
        let z_min = morton_encode_3d(min_x, min_y, min_z);
        let z_max = morton_encode_3d(max_x, max_y, max_z);
        let diff = z_min ^ z_max;
        let split_bit = if diff == 0 {
            0
        } else {
            64 - diff.leading_zeros()
        };
        Self {
            min_x,
            max_x,
            min_y,
            max_y,
            min_z,
            max_z,
            z_min,
            z_max,
            split_bit,
        }
    }

    #[inline]
    fn is_exact_dyadic(&self) -> bool {
        if self.z_min == self.z_max {
            return true;
        }
        let p = self.split_bit - 1;
        let mask = if p >= 63 { u64::MAX } else { (1u64 << (p + 1)) - 1 };
        (self.z_min & mask) == 0 && (self.z_max & mask) == mask
    }
}

/// Decomposes a 3D axis-aligned bounding box `[min_x..=max_x, min_y..=max_y, min_z..=max_z]`
/// (21-bit coordinates) into at most `max_intervals` disjoint contiguous Morton intervals
/// using O(1) BIGMIN / LITMAX octree subdivision.
pub fn decompose_morton_3d(
    min_x: u32,
    max_x: u32,
    min_y: u32,
    max_y: u32,
    min_z: u32,
    max_z: u32,
    max_intervals: usize,
) -> Vec<MortonInterval> {
    if min_x > max_x || min_y > max_y || min_z > max_z || max_intervals == 0 {
        return Vec::new();
    }

    let root = SubBox3D::new(min_x, max_x, min_y, max_y, min_z, max_z);
    if root.is_exact_dyadic() || max_intervals == 1 {
        return vec![MortonInterval {
            start: root.z_min,
            end: root.z_max,
        }];
    }

    let mut exact_intervals: Vec<MortonInterval> = Vec::new();
    let mut heap: BinaryHeap<SubBox3D> = BinaryHeap::new();
    heap.push(root);

    while let Some(cur) = heap.pop() {
        if cur.is_exact_dyadic() {
            exact_intervals.push(MortonInterval {
                start: cur.z_min,
                end: cur.z_max,
            });
            continue;
        }

        if exact_intervals.len() + heap.len() + 1 >= max_intervals {
            exact_intervals.push(MortonInterval {
                start: cur.z_min,
                end: cur.z_max,
            });
            break;
        }

        let p = cur.split_bit - 1;
        let dim = p % 3;
        let k = p / 3;
        let bit_mask = 1u32 << k;
        let low_mask = bit_mask - 1;

        let (left, right) = match dim {
            0 => {
                let split_high = cur.max_x & !low_mask;
                let split_low = split_high - 1;
                (
                    SubBox3D::new(cur.min_x, split_low, cur.min_y, cur.max_y, cur.min_z, cur.max_z),
                    SubBox3D::new(split_high, cur.max_x, cur.min_y, cur.max_y, cur.min_z, cur.max_z),
                )
            }
            1 => {
                let split_high = cur.max_y & !low_mask;
                let split_low = split_high - 1;
                (
                    SubBox3D::new(cur.min_x, cur.max_x, cur.min_y, split_low, cur.min_z, cur.max_z),
                    SubBox3D::new(cur.min_x, cur.max_x, split_high, cur.max_y, cur.min_z, cur.max_z),
                )
            }
            _ => {
                let split_high = cur.max_z & !low_mask;
                let split_low = split_high - 1;
                (
                    SubBox3D::new(cur.min_x, cur.max_x, cur.min_y, cur.max_y, cur.min_z, split_low),
                    SubBox3D::new(cur.min_x, cur.max_x, cur.min_y, cur.max_y, split_high, cur.max_z),
                )
            }
        };

        for child in [left, right] {
            if child.is_exact_dyadic() {
                exact_intervals.push(MortonInterval {
                    start: child.z_min,
                    end: child.z_max,
                });
            } else {
                heap.push(child);
            }
        }
    }

    for rem in heap {
        exact_intervals.push(MortonInterval {
            start: rem.z_min,
            end: rem.z_max,
        });
    }

    merge_intervals(exact_intervals)
}

/// Sorts and merges overlapping or directly adjacent Morton intervals.
fn merge_intervals(mut intervals: Vec<MortonInterval>) -> Vec<MortonInterval> {
    if intervals.len() <= 1 {
        return intervals;
    }
    intervals.sort_unstable_by_key(|iv| iv.start);
    let mut merged: Vec<MortonInterval> = Vec::with_capacity(intervals.len());
    for iv in intervals {
        if let Some(last) = merged.last_mut() {
            if iv.start <= last.end.saturating_add(1) {
                last.end = last.end.max(iv.end);
                continue;
            }
        }
        merged.push(iv);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_morton_2d_roundtrip() {
        let samples = [
            (0, 0),
            (1, 1),
            (42, 999),
            (123_456, 654_321),
            (u32::MAX, 0),
            (0, u32::MAX),
            (u32::MAX, u32::MAX),
            (2_000_000, 880 << 10),
        ];
        for (x, y) in samples {
            let z = morton_encode_2d(x, y);
            assert_eq!(morton_decode_2d(z), (x, y));
        }
    }

    #[test]
    fn test_morton_3d_roundtrip() {
        for bit in 0..21 {
            let v = 1u32 << bit;
            assert_eq!(morton_decode_3d(morton_encode_3d(v, 0, 0)), (v, 0, 0));
            assert_eq!(morton_decode_3d(morton_encode_3d(0, v, 0)), (0, v, 0));
            assert_eq!(morton_decode_3d(morton_encode_3d(0, 0, v)), (0, 0, v));
        }
        let samples = [
            (0, 0, 0),
            (1, 2, 3),
            (1_999_999, 850 << 10, encode_country_21("Costa Rica", false)),
            (MAX_COORD_21, MAX_COORD_21, MAX_COORD_21),
        ];
        for (x, y, z) in samples {
            let m = morton_encode_3d(x, y, z);
            assert_eq!(morton_decode_3d(m), (x, y, z));
        }
    }

    #[test]
    fn test_decompose_2d_completeness_and_pruning() {
        // Box [2..=5, 1..=3] in an 8x8 grid (0..=7, 0..=7)
        let intervals = decompose_morton_2d(2, 5, 1, 3, 64);
        for x in 0..8 {
            for y in 0..8 {
                let z = morton_encode_2d(x, y);
                let in_box = (2..=5).contains(&x) && (1..=3).contains(&y);
                let in_intervals = intervals.iter().any(|iv| z >= iv.start && z <= iv.end);
                assert_eq!(
                    in_box, in_intervals,
                    "Point ({x}, {y}) z={z} in_box={in_box} but in_intervals={in_intervals}"
                );
            }
        }
    }

    #[test]
    fn test_decompose_3d_completeness_and_pruning() {
        // Box [1..=4, 2..=5, 0..=3] in an 8x8x8 grid
        let intervals = decompose_morton_3d(1, 4, 2, 5, 0, 3, 128);
        for x in 0..8 {
            for y in 0..8 {
                for z_c in 0..8 {
                    let m = morton_encode_3d(x, y, z_c);
                    let in_box =
                        (1..=4).contains(&x) && (2..=5).contains(&y) && (0..=3).contains(&z_c);
                    let in_intervals = intervals.iter().any(|iv| m >= iv.start && m <= iv.end);
                    assert_eq!(
                        in_box, in_intervals,
                        "Point ({x}, {y}, {z_c}) m={m} mismatch"
                    );
                }
            }
        }
    }

    #[test]
    fn test_date_and_string_monotonicity() {
        let d1 = encode_date_21("2020-01-01");
        let d2 = encode_date_21("2020-11-29");
        let d3 = encode_date_21("2021-01-03");
        let d4 = encode_date_21("2022-05-29");
        assert!(d1 < d2 && d2 < d3 && d3 < d4);

        let c1 = encode_country_21("Afghanistan", false);
        let c2 = encode_country_21("Canada", false);
        let c3 = encode_country_21("Costa Rica", false);
        let c4 = encode_country_21("Denmark", false);
        let c5 = encode_country_21("Zimbabwe", false);
        assert!(c1 < c2 && c2 < c3 && c3 < c4 && c4 < c5);
    }
}

