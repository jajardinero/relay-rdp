//! ClearCodec's NSCodec subcodec (MS-RDPNSC 2.2.2 and 3.1.8.4).
//!
//! Four independently encoded AYCoCg planes, optional chroma subsampling and color loss.
//! ClearCodec uses top-to-bottom rows; its desktop output is opaque BGRA.

use ironrdp_core::{DecodeResult, ReadCursor, cast_length, ensure_size, invalid_field_err};

pub(super) fn decode(data: &[u8], width: u16, height: u16) -> DecodeResult<Vec<u8>> {
    let mut src = ReadCursor::new(data);
    ensure_size!(ctx: "NSCodec", in: src, size: 20);
    let mut lengths = [0usize; 4];
    for length in &mut lengths {
        *length = cast_length!("planeByteCount", src.read_u32())?;
    }
    let loss = src.read_u8();
    let subsampling = src.read_u8();
    src.read_u16(); // reserved
    if !(1..=7).contains(&loss) || subsampling > 1 {
        return Err(invalid_field_err!(
            "NSCodec",
            "invalid color loss or chroma subsampling level"
        ));
    }
    let (w, h) = (usize::from(width), usize::from(height));
    let stride = if subsampling != 0 {
        w.div_ceil(8) * 8
    } else {
        w
    };
    let chroma_size = if subsampling != 0 {
        stride / 2 * h.div_ceil(2)
    } else {
        w * h
    };
    let sizes = [stride * h, chroma_size, chroma_size, w * h];
    let mut planes: [Vec<u8>; 4] = core::array::from_fn(|_| Vec::new());
    for ((plane, length), size) in planes.iter_mut().zip(lengths).zip(sizes) {
        ensure_size!(ctx: "NSCodec plane", in: src, size: length);
        let encoded = src.read_slice(length);
        *plane = if length == 0 {
            vec![0xFF; size]
        } else if length < size {
            decode_rle(encoded, size)?
        } else {
            encoded[..size].to_vec()
        };
    }

    let shift = loss - 1;
    let mut bgra = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let ci = if subsampling != 0 {
                y / 2 * (stride / 2) + x / 2
            } else {
                y * w + x
            };
            let luma = i16::from(planes[0][y * stride + x]);
            // Chroma recovery wraps to a signed byte before conversion (MS-RDPEGDI 3.1.9.1).
            let co = i16::from(i8::from_ne_bytes([planes[1][ci] << shift]));
            let cg = i16::from(i8::from_ne_bytes([planes[2][ci] << shift]));
            for channel in [luma - co - cg, luma + cg, luma + co - cg] {
                bgra.push(u8::try_from(channel.clamp(0, 255)).expect("clamped to a byte"));
            }
            bgra.push(0xFF);
        }
    }
    Ok(bgra)
}

fn decode_rle(data: &[u8], size: usize) -> DecodeResult<Vec<u8>> {
    let mut src = ReadCursor::new(data);
    let mut plane = Vec::with_capacity(size);
    // The final four bytes are always literal. With five pixels left, the first is also
    // literal, even when it equals the next byte (MS-RDPNSC 2.2.2.1).
    while size.saturating_sub(plane.len()) > 4 {
        ensure_size!(ctx: "NSCodec RLE", in: src, size: 1);
        let value = src.read_u8();
        let left = size - plane.len();
        let count = if left == 5 {
            1
        } else {
            ensure_size!(ctx: "NSCodec RLE", in: src, size: 1);
            if src.peek_u8() == value {
                src.read_u8();
                ensure_size!(ctx: "NSCodec RLE run", in: src, size: 1);
                let count = src.read_u8();
                if count == 0xFF {
                    ensure_size!(ctx: "NSCodec RLE long run", in: src, size: 4);
                    cast_length!("runLength", src.read_u32())?
                } else {
                    usize::from(count) + 2
                }
            } else {
                1
            }
        };
        if count == 0 || count > left - 4 {
            return Err(invalid_field_err!(
                "NSCodec RLE",
                "run exceeds plane or its literal tail"
            ));
        }
        plane.resize(plane.len() + count, value);
    }
    if size.saturating_sub(plane.len()) != 4 {
        return Err(invalid_field_err!(
            "NSCodec RLE",
            "missing four-byte literal tail"
        ));
    }
    ensure_size!(ctx: "NSCodec RLE tail", in: src, size: 4);
    plane.extend_from_slice(src.read_slice(4));
    Ok(plane)
}
