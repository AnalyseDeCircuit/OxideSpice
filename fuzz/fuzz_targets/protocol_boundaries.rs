#![no_main]

use libfuzzer_sys::fuzz_target;
use oxide_spice_protocol::{
    CopyBits, DataHeader, DrawAlphaBlend, DrawComposite, DrawCopy, DrawFill,
    DrawMaskedDestination, DrawOpaque, DrawRop3, DrawStroke, DrawText, DrawTransparent, Framing,
    SubMessageList,
};

fuzz_target!(|data: &[u8]| {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    match selector % 16 {
        0 => {
            if body.len() >= Framing::Full.header_len()
                && let Ok(header) = DataHeader::decode(
                    Framing::Full,
                    &body[..Framing::Full.header_len()],
                )
                && let Some(offset) = header.sub_list_offset.filter(|offset| *offset != 0)
            {
                let _ = SubMessageList::decode(&body[Framing::Full.header_len()..], offset);
            }
        }
        1 => {
            let _ = SubMessageList::decode(body, 0);
        }
        2 => {
            let _ = DrawFill::decode(body);
        }
        3 => {
            let _ = DrawOpaque::decode(body);
        }
        4 | 5 => {
            let _ = DrawCopy::decode(body);
        }
        6 | 7 | 8 => {
            let _ = DrawMaskedDestination::decode(body);
        }
        9 => {
            let _ = DrawRop3::decode(body);
        }
        10 => {
            let _ = DrawStroke::decode(body);
        }
        11 => {
            let _ = DrawText::decode(body);
        }
        12 => {
            let _ = DrawTransparent::decode(body);
        }
        13 => {
            let _ = DrawAlphaBlend::decode(body);
        }
        14 => {
            let _ = CopyBits::decode(body);
        }
        15 => {
            let _ = DrawComposite::decode(body);
        }
        _ => unreachable!("selector was reduced to the complete dispatch range"),
    }
});
