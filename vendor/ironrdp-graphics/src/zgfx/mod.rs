//! ZGFX (RDP8) Bulk Data Compression

mod api;
mod compressor;
mod control_messages;
mod wrapper;

use std::io;
use std::sync::LazyLock;

pub use api::{CompressionMode, compress_and_wrap_egfx};
use bitvec::bits;
use bitvec::field::BitField as _;
use bitvec::order::Msb0;
use bitvec::slice::BitSlice;
pub use compressor::Compressor;
pub use wrapper::{wrap_compressed, wrap_uncompressed};

use self::control_messages::{BulkEncodedData, CompressionFlags, SegmentedDataPdu};

/// Sliding window size shared by compressor and decompressor.
pub(crate) const HISTORY_SIZE: usize = 2_500_000;

pub struct Decompressor {
    /// Everything decompressed, oldest first, back to at least `HISTORY_SIZE` bytes ago. It
    /// starts as `HISTORY_SIZE` zeros, as the protocol's history buffer does, and grows to
    /// twice that before the oldest bytes are dropped, so a match is one copy within it.
    history: Vec<u8>,
}

impl Decompressor {
    pub fn new() -> Self {
        let mut history = Vec::with_capacity(2 * HISTORY_SIZE + usize::from(u16::MAX));
        history.resize(HISTORY_SIZE, 0);
        Self { history }
    }

    pub fn decompress(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<usize, ZgfxError> {
        let segmented_data = SegmentedDataPdu::from_buffer(input)?;

        match segmented_data {
            SegmentedDataPdu::Single(segment) => self.handle_segment(&segment, output),
            SegmentedDataPdu::Multipart {
                uncompressed_size,
                segments,
            } => {
                let mut bytes_written = 0;
                for segment in segments {
                    let written = self.handle_segment(&segment, output)?;
                    bytes_written += written;
                }

                if bytes_written != uncompressed_size {
                    Err(ZgfxError::InvalidDecompressedSize {
                        decompressed_size: bytes_written,
                        uncompressed_size,
                    })
                } else {
                    Ok(bytes_written)
                }
            }
        }
    }

    fn handle_segment(&mut self, segment: &BulkEncodedData<'_>, output: &mut Vec<u8>) -> Result<usize, ZgfxError> {
        if !segment.data.is_empty() {
            if segment.compression_flags.contains(CompressionFlags::COMPRESSED) {
                self.decompress_segment(segment.data, output)
            } else {
                self.history.extend_from_slice(segment.data);
                self.trim_history();
                output.extend_from_slice(segment.data);

                Ok(segment.data.len())
            }
        } else {
            Ok(0)
        }
    }

    fn decompress_segment(&mut self, encoded_data: &[u8], output: &mut Vec<u8>) -> Result<usize, ZgfxError> {
        // The value of the last byte indicates the number of unused bits in the final byte
        let Some((&unused_bits, data)) = encoded_data.split_last() else {
            return Ok(0);
        };
        let end = (data.len() * 8)
            .checked_sub(usize::from(unused_bits))
            .ok_or(ZgfxError::UnexpectedEnd)?;
        let mut bits = BitReader { data, position: 0, end };
        let tokens = &*TOKEN_LOOKUP;
        let start = self.history.len();

        while bits.remaining() > 0 {
            let Some((prefix_length, token)) =
                tokens[usize::try_from(bits.peek() >> (32 - TOKEN_PREFIX_BITS)).expect("a 9-bit index fits usize")]
            else {
                return Err(ZgfxError::TokenBitsNotFound);
            };
            bits.skip(usize::from(prefix_length))?;

            match token {
                // The prefix value is encoded with a "0" prefix,
                // then read 8 bits containing the byte to output.
                DecodedToken::NullLiteral => {
                    let value = u8::try_from(bits.read(8)?).expect("8 bits fit u8");
                    self.history.push(value);
                }
                DecodedToken::Literal(value) => self.history.push(value),
                DecodedToken::Match {
                    distance_value_size,
                    distance_base,
                } => {
                    // Each token has been assigned a different base distance
                    // and number of additional value bits to be added to compute the full distance.
                    let distance = usize::try_from(distance_base + bits.read(usize::from(distance_value_size))?)
                        .map_err(|_| ZgfxError::InvalidIntegralConversion("token's full distance"))?;
                    if distance == 0 {
                        self.read_unencoded_bytes(&mut bits)?;
                    } else {
                        let length = read_match_length(&mut bits)?;
                        self.copy_match(distance, length)?;
                    }
                }
            }
        }

        let bytes_written = self.history.len() - start;
        output.extend_from_slice(&self.history[start..]);
        self.trim_history();

        Ok(bytes_written)
    }

    fn read_unencoded_bytes(&mut self, bits: &mut BitReader<'_>) -> Result<(), ZgfxError> {
        // A match distance of zero is a special case,
        // which indicates that an unencoded run of bytes follows.
        // The count of bytes is encoded as a 15-bit value, and the bytes start at the next byte boundary.
        let length = usize::try_from(bits.read(15)?).expect("15 bits fit usize");
        bits.skip(bits.position.next_multiple_of(8) - bits.position)?;
        let first = bits.position / 8;
        bits.skip(length * 8)?;
        self.history.extend_from_slice(&bits.data[first..first + length]);

        Ok(())
    }

    /// Appends `length` bytes from `distance` bytes back. When the match is longer than its
    /// distance, the bytes it copies repeat with that period.
    fn copy_match(&mut self, distance: usize, length: usize) -> Result<(), ZgfxError> {
        if distance > HISTORY_SIZE {
            return Err(ZgfxError::DistanceBeyondHistory { distance });
        }
        // The history always holds at least HISTORY_SIZE bytes.
        let from = self.history.len() - distance;
        let mut left = length;
        while left > 0 {
            let chunk = left.min(self.history.len() - from);
            self.history.extend_from_within(from..from + chunk);
            left -= chunk;
        }

        Ok(())
    }

    /// Drops the oldest bytes once the history has doubled, keeping `HISTORY_SIZE` of them.
    fn trim_history(&mut self) {
        if self.history.len() > 2 * HISTORY_SIZE {
            self.history.drain(..self.history.len() - HISTORY_SIZE);
        }
    }
}

impl Default for Decompressor {
    fn default() -> Self {
        Self::new()
    }
}

/// A match length prefix follows the token and indicates how many additional bits will be
/// needed to get the full length (the number of bytes to be copied).
fn read_match_length(bits: &mut BitReader<'_>) -> Result<usize, ZgfxError> {
    let length_token_size = bits.peek().leading_ones();
    // Longer prefixes can't describe a length that fits 32 bits.
    if length_token_size > 30 {
        return Err(ZgfxError::InvalidIntegralConversion("length of the token size"));
    }
    let length_token_size = usize::try_from(length_token_size).expect("at most 30");
    bits.skip(length_token_size + 1)?; // length token + zero bit

    if length_token_size == 0 {
        // special case
        return Ok(3);
    }
    let length = usize::try_from(bits.read(length_token_size + 1)?).expect("u32 fits usize");

    Ok((1 << (length_token_size + 1)) + length)
}

/// Reads a segment's bits, most significant first.
struct BitReader<'a> {
    data: &'a [u8],
    /// The next bit to read.
    position: usize,
    /// The number of bits in the stream: the rest of the last byte is padding.
    end: usize,
}

impl BitReader<'_> {
    fn remaining(&self) -> usize {
        self.end - self.position
    }

    /// The next 32 bits, MSB first. Bits past the end of the stream are padding or zeros:
    /// callers only use as many bits as remain.
    fn peek(&self) -> u32 {
        let byte = self.position / 8;
        let window = match self.data.get(byte..byte + 8) {
            Some(bytes) => u64::from_be_bytes(bytes.try_into().expect("8 bytes")),
            None => {
                let mut bytes = [0; 8];
                let tail = self.data.get(byte..).unwrap_or_default();
                bytes[..tail.len()].copy_from_slice(tail);
                u64::from_be_bytes(bytes)
            }
        };
        u32::try_from((window << (self.position % 8)) >> 32).expect("the upper 32 bits")
    }

    fn skip(&mut self, count: usize) -> Result<(), ZgfxError> {
        if count > self.remaining() {
            return Err(ZgfxError::UnexpectedEnd);
        }
        self.position += count;

        Ok(())
    }

    /// Reads `count` (at most 32) bits as a big-endian number.
    fn read(&mut self, count: usize) -> Result<u32, ZgfxError> {
        debug_assert!(count <= 32);
        let value = match count {
            0 => 0,
            _ => self.peek() >> (32 - count),
        };
        self.skip(count)?;

        Ok(value)
    }
}

/// Token prefixes are at most this long.
const TOKEN_PREFIX_BITS: u32 = 9;

#[derive(Clone, Copy)]
enum DecodedToken {
    NullLiteral,
    Literal(u8),
    Match {
        distance_value_size: u8,
        distance_base: u32,
    },
}

/// The token every 9-bit window starts with, and the length of its prefix; none when no
/// token's prefix matches. Built from `TOKEN_TABLE`, whose prefixes are a prefix code.
static TOKEN_LOOKUP: LazyLock<[Option<(u8, DecodedToken)>; 1 << TOKEN_PREFIX_BITS]> = LazyLock::new(|| {
    let mut lookup = [None; 1 << TOKEN_PREFIX_BITS];
    for token in TOKEN_TABLE.iter() {
        let length = token.prefix.len();
        let free_bits = usize::try_from(TOKEN_PREFIX_BITS).expect("9 fits usize") - length;
        let first = token.prefix.load_be::<usize>() << free_bits;
        let decoded = match token.ty {
            TokenType::NullLiteral => DecodedToken::NullLiteral,
            TokenType::Literal { literal_value } => DecodedToken::Literal(literal_value),
            TokenType::Match {
                distance_value_size,
                distance_base,
            } => DecodedToken::Match {
                distance_value_size: u8::try_from(distance_value_size).expect("at most 24 bits"),
                distance_base,
            },
        };
        let length = u8::try_from(length).expect("at most 9 bits");
        lookup[first..first + (1 << free_bits)].fill(Some((length, decoded)));
    }
    lookup
});

struct Token {
    prefix: &'static BitSlice<u8, Msb0>,
    ty: TokenType,
}

enum TokenType {
    NullLiteral,
    Literal {
        literal_value: u8,
    },
    Match {
        distance_value_size: usize,
        distance_base: u32,
    },
}

static TOKEN_TABLE: LazyLock<[Token; 40]> = LazyLock::new(|| {
    [
        Token {
            prefix: bits![static u8, Msb0; 0],
            ty: TokenType::NullLiteral,
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 0, 0, 0],
            ty: TokenType::Literal { literal_value: 0x00 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 0, 0, 1],
            ty: TokenType::Literal { literal_value: 0x01 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 0, 1, 0, 0],
            ty: TokenType::Literal { literal_value: 0x02 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 0, 1, 0, 1],
            ty: TokenType::Literal { literal_value: 0x03 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 0, 1, 1, 0],
            ty: TokenType::Literal { literal_value: 0x0ff },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 0, 1, 1, 1, 0],
            ty: TokenType::Literal { literal_value: 0x04 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 0, 1, 1, 1, 1],
            ty: TokenType::Literal { literal_value: 0x05 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 0, 0, 0, 0],
            ty: TokenType::Literal { literal_value: 0x06 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 0, 0, 0, 1],
            ty: TokenType::Literal { literal_value: 0x07 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 0, 0, 1, 0],
            ty: TokenType::Literal { literal_value: 0x08 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 0, 0, 1, 1],
            ty: TokenType::Literal { literal_value: 0x09 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 0, 1, 0, 0],
            ty: TokenType::Literal { literal_value: 0x0a },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 0, 1, 0, 1],
            ty: TokenType::Literal { literal_value: 0x0b },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 0, 1, 1, 0],
            ty: TokenType::Literal { literal_value: 0x3a },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 0, 1, 1, 1],
            ty: TokenType::Literal { literal_value: 0x3b },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 0, 0, 0],
            ty: TokenType::Literal { literal_value: 0x3c },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 0, 0, 1],
            ty: TokenType::Literal { literal_value: 0x3d },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 0, 1, 0],
            ty: TokenType::Literal { literal_value: 0x3e },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 0, 1, 1],
            ty: TokenType::Literal { literal_value: 0x3f },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 1, 0, 0],
            ty: TokenType::Literal { literal_value: 0x40 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 1, 0, 1],
            ty: TokenType::Literal { literal_value: 0x80 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 1, 1, 0, 0],
            ty: TokenType::Literal { literal_value: 0x0c },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 1, 1, 0, 1],
            ty: TokenType::Literal { literal_value: 0x38 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 1, 1, 1, 0],
            ty: TokenType::Literal { literal_value: 0x39 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 1, 1, 1, 1, 1, 1, 1],
            ty: TokenType::Literal { literal_value: 0x66 },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 0, 0, 1],
            ty: TokenType::Match {
                distance_value_size: 5,
                distance_base: 0,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 0, 1, 0],
            ty: TokenType::Match {
                distance_value_size: 7,
                distance_base: 32,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 0, 1, 1],
            ty: TokenType::Match {
                distance_value_size: 9,
                distance_base: 160,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 0, 0],
            ty: TokenType::Match {
                distance_value_size: 10,
                distance_base: 672,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 0, 1],
            ty: TokenType::Match {
                distance_value_size: 12,
                distance_base: 1_696,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 1, 0, 0],
            ty: TokenType::Match {
                distance_value_size: 14,
                distance_base: 5_792,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 1, 0, 1],
            ty: TokenType::Match {
                distance_value_size: 15,
                distance_base: 22_176,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 1, 1, 0, 0],
            ty: TokenType::Match {
                distance_value_size: 18,
                distance_base: 54_944,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 1, 1, 0, 1],
            ty: TokenType::Match {
                distance_value_size: 20,
                distance_base: 317_088,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 1, 1, 1, 0, 0],
            ty: TokenType::Match {
                distance_value_size: 20,
                distance_base: 1_365_664,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 1, 1, 1, 0, 1],
            ty: TokenType::Match {
                distance_value_size: 21,
                distance_base: 2_414_240,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 1, 1, 1, 1, 0, 0],
            ty: TokenType::Match {
                distance_value_size: 22,
                distance_base: 4_511_392,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 1, 1, 1, 1, 0, 1],
            ty: TokenType::Match {
                distance_value_size: 23,
                distance_base: 8_705_696,
            },
        },
        Token {
            prefix: bits![static u8, Msb0; 1, 0, 1, 1, 1, 1, 1, 1, 0],
            ty: TokenType::Match {
                distance_value_size: 24,
                distance_base: 17_094_304,
            },
        },
    ]
});

#[derive(Debug)]
pub enum ZgfxError {
    IOError(io::Error),
    InvalidCompressionType,
    InvalidSegmentedDescriptor,
    InvalidDecompressedSize {
        decompressed_size: usize,
        uncompressed_size: usize,
    },
    TokenBitsNotFound,
    InvalidIntegralConversion(&'static str),
    /// The compressed bits end inside a token.
    UnexpectedEnd,
    /// A match reaches further back than the history buffer.
    DistanceBeyondHistory {
        distance: usize,
    },
}

impl core::fmt::Display for ZgfxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::IOError(_error) => write!(f, "IO error"),
            Self::InvalidCompressionType => write!(f, "invalid compression type"),
            Self::InvalidSegmentedDescriptor => write!(f, "invalid segmented descriptor"),
            Self::InvalidDecompressedSize {
                decompressed_size,
                uncompressed_size,
            } => write!(
                f,
                "decompressed size of segments ({decompressed_size}) does not equal to uncompressed size ({uncompressed_size})",
            ),
            Self::TokenBitsNotFound => write!(f, "token bits not found"),
            Self::InvalidIntegralConversion(type_name) => {
                write!(f, "invalid `{type_name}`: out of range integral type conversion")
            }
            Self::UnexpectedEnd => write!(f, "compressed data ends inside a token"),
            Self::DistanceBeyondHistory { distance } => {
                write!(f, "match distance {distance} reaches beyond the history buffer")
            }
        }
    }
}

impl core::error::Error for ZgfxError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::IOError(error) => Some(error),
            Self::InvalidCompressionType => None,
            Self::InvalidSegmentedDescriptor => None,
            Self::InvalidDecompressedSize { .. } => None,
            Self::TokenBitsNotFound => None,
            Self::InvalidIntegralConversion(_) => None,
            Self::UnexpectedEnd => None,
            Self::DistanceBeyondHistory { .. } => None,
        }
    }
}

impl From<io::Error> for ZgfxError {
    fn from(err: io::Error) -> Self {
        Self::IOError(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODED_ZGFX_SINGLE: [&[u8]; 5] = [
        include_bytes!("test_assets/encoded.0.bin"),
        include_bytes!("test_assets/encoded.1.bin"),
        include_bytes!("test_assets/encoded.2.bin"),
        include_bytes!("test_assets/encoded.3.bin"),
        include_bytes!("test_assets/encoded.4.bin"),
    ];

    const DECODED_ZGFX_SINGLE: [&[u8]; 5] = [
        include_bytes!("test_assets/decoded.0.bin"),
        include_bytes!("test_assets/decoded.1.bin"),
        include_bytes!("test_assets/decoded.2.bin"),
        include_bytes!("test_assets/decoded.3.bin"),
        include_bytes!("test_assets/decoded.4.bin"),
    ];

    #[test]
    fn zgfx_decompresses_multiple_single_pdus() {
        let pairs = ENCODED_ZGFX_SINGLE
            .iter()
            .copied()
            .zip(DECODED_ZGFX_SINGLE.iter().copied());
        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(pairs.clone().map(|(_, d)| d.len()).max().unwrap());
        for (i, (encode, decode)) in pairs.enumerate() {
            let bytes_written = zgfx.decompress(encode.as_ref(), &mut decompressed).unwrap();
            assert_eq!(decode.len(), bytes_written);
            assert_eq!(decompressed, *decode, "Failed to decompress encoded PDU #{i}");
            decompressed.clear();
        }
    }

    #[test]
    fn zgfx_decompresses_only_one_literal() {
        let buffer = [0b1100_1000, 0x03];
        let expected = vec![0x01];

        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(expected.len());
        zgfx.decompress_segment(buffer.as_ref(), &mut decompressed).unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn zgfx_decompresses_one_literal_with_null_prefix() {
        let buffer = [0b0011_0010, 0b1000_0000, 0x07];
        let expected = vec![0x65];

        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(expected.len());
        zgfx.decompress_segment(buffer.as_ref(), &mut decompressed).unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn zgfx_decompresses_only_multiple_literals() {
        let buffer = [0b1100_1110, 0b1001_1011, 0b0001_1001, 0b0100_0000, 0x06];
        let expected = vec![0x01, 0x02, 0xff, 0x65];

        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(expected.len());
        zgfx.decompress_segment(buffer.as_ref(), &mut decompressed).unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn zgfx_decompresses_one_literal_with_one_match_distance_1() {
        let buffer = [0b0011_0010, 0b1100_0100, 0b0011_0000, 0x1];
        let expected = vec![0x65; 1 + 4]; // literal (1) + match repeated 4 (length) + 0 times

        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(expected.len());
        zgfx.decompress_segment(buffer.as_ref(), &mut decompressed).unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn zgfx_decompresses_three_literals_with_one_match_distance_3_length_57() {
        let buffer = [
            0b0010_0000,
            0b1001_0000,
            0b1000_1000,
            0b0111_0001,
            0b0001_1111,
            0b1011_0010,
            0x1,
        ];
        let expected = "ABC".repeat(20);
        let expected = expected.as_bytes();

        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(expected.len());
        zgfx.decompress_segment(buffer.as_ref(), &mut decompressed).unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn zgfx_decompresses_one_match_with_match_unencoded_bytes() {
        let expected = "The quick brown fox jumps over the lazy dog".as_bytes();
        let mut buffer = vec![0b1000_1000, 0b0000_0000, 0b00010101, 0b1000_0000];
        buffer.extend_from_slice(expected);
        buffer.extend_from_slice(&[0x00]); // no bits unused

        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(expected.len());
        zgfx.decompress_segment(buffer.as_ref(), &mut decompressed).unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn zgfx_decompresses_multiple_literals_with_match_in_center_with_not_compressed() {
        let buffer = [
            0xE1, // DEBLOCK_MULTIPART
            0x03, 0x00, // 3 segments
            0x2B, 0x00, 0x00, 0x00, // 0x0000002B total bytes uncompressed
            0x11, 0x00, 0x00, 0x00, // first segment is the next 17 bytes:
            0x04, // type 4, not PACKET_COMPRESSED
            0x54, 0x68, 0x65, 0x20, 0x71, 0x75, 0x69, 0x63, 0x6B, 0x20, 0x62, 0x72, 0x6F, 0x77, 0x6E,
            0x20, // "The quick brown "
            0x0E, 0x00, 0x00, 0x00, // second segment is the next 14 bytes:
            0x04, // type 4, not PACKET_COMPRESSED
            0x66, 0x6F, 0x78, 0x20, 0x6A, 0x75, 0x6D, 0x70, 0x73, 0x20, 0x6F, 0x76, 0x65, // "fox jumps ove"
            0x10, 0x00, 0x00, 0x00, // third segment is the next 16 bytes
            0x24, // type 4 + PACKET_COMPRESSED
            0x39, 0x08, 0x0E, 0x91, 0xF8, 0xD8, 0x61, 0x3D, 0x1E, 0x44, 0x06, 0x43, 0x79, 0x9C, // encoded:
            // 0 01110010 = literal 0x72 = "r"
            // 0 00100000 = literal 0x20 = " "
            // 0 01110100 = literal 0x74 = "t"
            //
            // 10001 11111 0 = match, distance = 31, length = 3 "he "
            //
            // 0 01101100 = literal 0x6C = "l"
            // 0 01100001 = literal 0x61 = "a"
            // 0 01111010 = literal 0x7A = "z"
            // 0 01111001 = literal 0x79 = "y"
            // 0 00100000 = literal 0x20 = " "
            // 0 01100100 = literal 0x64 = "d"
            // 0 01101111 = literal 0x6F = "o"
            // 0 01100111 = literal 0x67 = "g"
            0x02, // ignore last two bits of 0x9C byte
        ];
        let expected = "The quick brown fox jumps over the lazy dog".as_bytes();

        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(expected.len());
        let bytes_written = zgfx.decompress(buffer.as_ref(), &mut decompressed).unwrap();
        assert_eq!(expected.len(), bytes_written);
        assert_eq!(decompressed, expected, "\n{decompressed:x?} != \n{expected:x?}");
    }

    #[test]
    fn zgfx_decompresses_single_match_unencoded_block() {
        let buffer = [
            0xe0, 0x04, 0x13, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x01, 0x06, 0x0a, 0x00, 0x04, 0x00, 0x00, 0x00,
            0x20, 0x00, 0x00, 0x00,
        ];
        let expected = vec![
            0x13, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x01, 0x06, 0x0a, 0x00, 0x04, 0x00, 0x00, 0x00, 0x20, 0x00,
            0x00, 0x00,
        ];

        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(expected.len());
        let bytes_written = zgfx.decompress(buffer.as_ref(), &mut decompressed).unwrap();
        assert_eq!(expected.len(), bytes_written);
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn zgfx_decompresses_unencoded_block_without_padding() {
        let buffer = [0b1110_0101, 0b0001_0000, 0b0000_0000, 0b00000001, 0b1111_0000, 0x0];
        let expected = vec![0x08, 0xf0];

        let mut zgfx = Decompressor::new();
        let mut decompressed = Vec::with_capacity(expected.len());
        zgfx.decompress_segment(buffer.as_ref(), &mut decompressed).unwrap();
        assert_eq!(decompressed, expected);
    }
}
