// FILE: crates/cvtl-store/src/recovery.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Read a verified raw prefix
//   SCOPE: Validate complete frames, records and sequence
//   DEPENDS: record, writer, zstd
//   LINKS: M-CVTL-STORE
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   recover - release verified records in order
//   Recovery - prefix counts and framing result
//   Tail - clean boundary, incomplete or corrupt
// END_MODULE_MAP
//! Read-only verified prefix recovery. Records are released frame by frame.

use std::io::{self, BufReader, Cursor, Read};

use crate::{
    record::{decode_line, Record},
    writer::FRAME_BYTES,
};

const MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const CHECKSUM_FLAG: u8 = 0x04;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tail {
    /// Only a framing result. Historical completion still needs the owner's receipt.
    FrameBoundary,
    IncompleteFrame,
    CorruptFrame,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Recovery {
    pub frames: u64,
    pub records: u64,
    pub tail: Tail,
}

/// Release only records from complete, checksum-verified frames in the
/// same run/generation. Stop at the first damaged frame.
pub fn recover(reader: impl Read, mut record: impl FnMut(Record<'_>)) -> io::Result<Recovery> {
    let mut input = BufReader::new(reader);
    let mut result = Recovery {
        frames: 0,
        records: 0,
        tail: Tail::FrameBoundary,
    };
    let mut identity: Option<(String, u64)> = None;
    let mut last_time = 0;
    let mut decoded = Vec::with_capacity(FRAME_BYTES + 1);
    loop {
        let mut prefix = [0_u8; 5];
        let mut received = 0;
        while received < prefix.len() {
            match input.read(&mut prefix[received..]) {
                Ok(0) => break,
                Ok(count) => received += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if received == 0 {
            break;
        }
        if !MAGIC.starts_with(&prefix[..received.min(MAGIC.len())]) {
            result.tail = Tail::CorruptFrame;
            break;
        }
        if received < prefix.len() {
            result.tail = Tail::IncompleteFrame;
            break;
        }
        if prefix[4] & CHECKSUM_FLAG == 0 {
            result.tail = Tail::CorruptFrame;
            break;
        }

        let compressed_limit = zstd::zstd_safe::compress_bound(FRAME_BYTES) as u64 + 1;
        let frame = Cursor::new(prefix).chain(&mut input).take(compressed_limit);
        let mut decoder = zstd::stream::read::Decoder::with_buffer(frame)?.single_frame();
        decoder.window_log_max(18)?;
        decoded.clear();
        let read = decoder
            .by_ref()
            .take((FRAME_BYTES + 1) as u64)
            .read_to_end(&mut decoded);
        let remaining = decoder.finish().limit();
        match read {
            Ok(_) if decoded.len() <= FRAME_BYTES && remaining > 0 => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && remaining > 0 => {
                result.tail = Tail::IncompleteFrame;
                break;
            }
            _ => {
                result.tail = Tail::CorruptFrame;
                break;
            }
        }
        if decoded.is_empty() || decoded.last() != Some(&b'\n') {
            result.tail = Tail::CorruptFrame;
            break;
        }
        let mut frame_records = Vec::new();
        for line in decoded.split_inclusive(|byte| *byte == b'\n') {
            let next_seq = result.records.checked_add(frame_records.len() as u64 + 1);
            let Ok(row) = decode_line(line) else {
                result.tail = Tail::CorruptFrame;
                return Ok(result);
            };
            let (run, generation) =
                identity.get_or_insert_with(|| (row.run.into(), row.generation));
            if row.run != run
                || row.generation != *generation
                || Some(row.seq) != next_seq
                || row.accepted_ns < last_time
            {
                result.tail = Tail::CorruptFrame;
                return Ok(result);
            }
            last_time = row.accepted_ns;
            frame_records.push(row);
        }
        result.records += frame_records.len() as u64;
        result.frames += 1;
        for row in frame_records {
            record(row);
        }
    }
    Ok(result)
}
