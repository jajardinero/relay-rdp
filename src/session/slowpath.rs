//! Slow-path frames that carry more than one PDU.
//!
//! Windows Server 2008 packs several Share Control PDUs into one MCS Send Data Indication: a
//! Synchronize update followed by Bitmap updates, for instance. IronRDP decodes one PDU per
//! frame and reads the rest of the frame as part of it, so such frames are split first.

use std::borrow::Cow;

use ironrdp_pdu::mcs::{SendDataIndication, decode_send_data_indication};
use ironrdp_pdu::x224::X224;

/// totalLength, pduType and pduSource ([MS-RDPBCGR] 2.2.8.1.1.1.1).
const SHARE_CONTROL_HEADER_SIZE: usize = 6;

/// The frames to process in place of `frame`, one per PDU; `None` when it holds at most one.
pub fn split(frame: &[u8], io_channel_id: u16) -> Option<Vec<Vec<u8>>> {
    let data = decode_send_data_indication(frame).ok()?;
    if data.channel_id != io_channel_id {
        return None;
    }
    let pdus = share_control_pdus(data.user_data)?;
    if pdus.len() < 2 {
        return None;
    }
    pdus.into_iter()
        .map(|pdu| {
            let indication = SendDataIndication {
                initiator_id: data.initiator_id,
                channel_id: data.channel_id,
                user_data: Cow::Borrowed(pdu),
            };
            ironrdp_core::encode_vec(&X224(indication)).ok()
        })
        .collect()
}

/// The Share Control PDUs in `user_data`, by the length in each header; `None` unless they
/// fill it exactly. Other PDUs on the IO channel (Multitransport requests, flow control) don't
/// fit and are left whole.
fn share_control_pdus(mut rest: &[u8]) -> Option<Vec<&[u8]>> {
    let mut pdus = Vec::new();
    while !rest.is_empty() {
        let length = usize::from(u16::from_le_bytes(rest.get(..2)?.try_into().ok()?));
        if !(SHARE_CONTROL_HEADER_SIZE..=rest.len()).contains(&length) {
            return None;
        }
        let (pdu, tail) = rest.split_at(length);
        pdus.push(pdu);
        rest = tail;
    }
    Some(pdus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_pdu::rdp::headers::{IoChannelPdu, ShareDataPdu, decode_io_channel};

    const IO_CHANNEL: u16 = 1003;

    /// A Synchronize update as Windows Server 2008 sends it.
    const SYNCHRONIZE: &str = "16001700ea03ea03010085021600020000000300bdd6";

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    fn frame(channel_id: u16, user_data: &[u8]) -> Vec<u8> {
        let indication = SendDataIndication {
            initiator_id: 1002,
            channel_id,
            user_data: Cow::Borrowed(user_data),
        };
        ironrdp_core::encode_vec(&X224(indication)).unwrap()
    }

    fn is_update(frame: &[u8]) -> bool {
        let data = decode_send_data_indication(frame).unwrap();
        matches!(
            decode_io_channel(data),
            Ok(IoChannelPdu::Data(ctx)) if matches!(ctx.pdu, ShareDataPdu::Update(_))
        )
    }

    #[test]
    fn splits_packed_pdus_into_frames_the_session_decodes() {
        let pdu = hex(SYNCHRONIZE);
        let packed = frame(IO_CHANNEL, &[pdu.as_slice(), &pdu, &pdu].concat());
        assert!(!is_update(&packed));
        let frames = split(&packed, IO_CHANNEL).unwrap();
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|frame| is_update(frame)));
    }

    #[test]
    fn leaves_single_and_unrecognized_frames_whole() {
        let pdu = hex(SYNCHRONIZE);
        let packed = [pdu.as_slice(), &pdu].concat();
        assert_eq!(split(&frame(IO_CHANNEL, &pdu), IO_CHANNEL), None);
        assert_eq!(split(&frame(IO_CHANNEL + 1, &packed), IO_CHANNEL), None);
        assert_eq!(
            split(&frame(IO_CHANNEL, &packed[..packed.len() - 1]), IO_CHANNEL),
            None
        );
        // A Multitransport request: a security header, not a length.
        assert_eq!(
            split(&frame(IO_CHANNEL, &[2, 0, 0, 0, 1, 2, 3, 4]), IO_CHANNEL),
            None
        );
        assert_eq!(split(b"not a frame", IO_CHANNEL), None);
    }
}
