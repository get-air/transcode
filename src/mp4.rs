//! Minimal, allocation-free ISO BMFF validation for generated CMAF artifacts.
//!
//! This is deliberately not a media engine. `GStreamer` performs all parsing,
//! muxing, and transcoding; these helpers reject truncated or malformed output
//! before it reaches a player.

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BoxHeader {
    kind: [u8; 4],
    payload_start: usize,
    end: usize,
}

/// Validates the required top-level boxes in a CMAF initialization segment.
///
/// # Errors
///
/// Returns an error for truncated boxes or missing `ftyp`/`moov` boxes.
pub fn validate_init_segment(data: &[u8]) -> Result<()> {
    let boxes = top_level_boxes(data)?;
    let has_ftyp = boxes.iter().any(|header| header.kind == *b"ftyp");
    let has_moov = boxes.iter().any(|header| header.kind == *b"moov");
    if has_ftyp && has_moov {
        Ok(())
    } else {
        Err(Error::Pipeline(
            "generated CMAF init segment is missing ftyp or moov".to_owned(),
        ))
    }
}

/// Validates the required boxes in a CMAF media segment.
///
/// # Errors
///
/// Returns an error for truncated boxes or missing `styp`, `moof`, `tfdt`, or
/// `mdat` boxes.
pub fn validate_media_segment(data: &[u8]) -> Result<()> {
    let boxes = top_level_boxes(data)?;
    let has_styp = boxes.iter().any(|header| header.kind == *b"styp");
    let has_moof = boxes.iter().any(|header| header.kind == *b"moof");
    let has_mdat = boxes.iter().any(|header| header.kind == *b"mdat");
    if has_styp && has_moof && has_mdat && decode_time(data).is_some() {
        Ok(())
    } else {
        Err(Error::Pipeline(
            "generated CMAF media segment is missing styp, moof, tfdt, or mdat".to_owned(),
        ))
    }
}

/// Returns the first track fragment decode time from a CMAF media segment.
#[must_use]
pub fn decode_time(data: &[u8]) -> Option<u64> {
    find_tfdt(data, 0, data.len(), 0)
}

fn find_tfdt(data: &[u8], start: usize, end: usize, depth: u8) -> Option<u64> {
    if depth > 8 || start >= end || end > data.len() {
        return None;
    }
    let mut cursor = start;
    while cursor < end {
        let header = parse_header(data, cursor, end).ok()?;
        if header.kind == *b"tfdt" {
            let payload = data.get(header.payload_start..header.end)?;
            let version = *payload.first()?;
            return if version == 1 {
                read_u64(payload.get(4..12)?)
            } else {
                read_u32(payload.get(4..8)?).map(u64::from)
            };
        }
        if matches!(&header.kind, b"moof" | b"traf")
            && let Some(value) = find_tfdt(
                data,
                header.payload_start,
                header.end,
                depth.saturating_add(1),
            )
        {
            return Some(value);
        }
        cursor = header.end;
    }
    None
}

fn top_level_boxes(data: &[u8]) -> Result<Vec<BoxHeader>> {
    let mut boxes = Vec::new();
    let mut cursor = 0;
    while cursor < data.len() {
        let header = parse_header(data, cursor, data.len()).map_err(Error::Pipeline)?;
        cursor = header.end;
        boxes.push(header);
    }
    Ok(boxes)
}

fn parse_header(data: &[u8], start: usize, limit: usize) -> std::result::Result<BoxHeader, String> {
    let basic = data
        .get(start..start.saturating_add(8))
        .ok_or_else(|| "truncated ISO BMFF box header".to_owned())?;
    let short_size = read_u32(&basic[..4]).ok_or_else(|| "invalid box size".to_owned())?;
    let kind = [basic[4], basic[5], basic[6], basic[7]];
    let (size, header_size) = match short_size {
        0 => (limit.saturating_sub(start), 8),
        1 => {
            let extended = data
                .get(start.saturating_add(8)..start.saturating_add(16))
                .and_then(read_u64)
                .ok_or_else(|| "truncated extended ISO BMFF box size".to_owned())?;
            (
                usize::try_from(extended).map_err(|_| "box is too large".to_owned())?,
                16,
            )
        }
        value => (
            usize::try_from(value).map_err(|_| "box is too large".to_owned())?,
            8,
        ),
    };
    if size < header_size {
        return Err("ISO BMFF box is smaller than its header".to_owned());
    }
    let end = start
        .checked_add(size)
        .ok_or_else(|| "ISO BMFF box size overflow".to_owned())?;
    if end > limit || end > data.len() {
        return Err("truncated ISO BMFF box payload".to_owned());
    }
    Ok(BoxHeader {
        kind,
        payload_start: start + header_size,
        end,
    })
}

fn read_u32(bytes: &[u8]) -> Option<u32> {
    let value: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(value))
}

fn read_u64(bytes: &[u8]) -> Option<u64> {
    let value: [u8; 8] = bytes.get(..8)?.try_into().ok()?;
    Some(u64::from_be_bytes(value))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn rejects_truncated_boxes() {
        let malformed = [0, 0, 0, 32, b'f', b't', b'y', b'p'];
        assert!(validate_init_segment(&malformed).is_err());
        assert!(validate_media_segment(&malformed).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_input_never_panics(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = validate_init_segment(&data);
            let _ = validate_media_segment(&data);
            let _ = decode_time(&data);
        }
    }
}
