//! Shared bounded archive traversal for writer verification and offline readers.
use super::{ARCHIVE_BYTES, ARCHIVE_ROW_BYTES, EXPANDED_REPORT_BYTES, REPORT_BYTES, compact};
use serde_json::value::RawValue;
use std::io::{BufRead, Read};

#[derive(Default, Debug, PartialEq)]
pub struct Counts {
    pub rows: u64,
    pub blocks: u64,
    pub bytes: u64,
    pub expanded_bytes: u64,
}

/// Hash each encoded line in `encoded`; visit full draw tokens in `draw`.
/// Expansion is bounded independently of the encoded 256 MiB disk budget.
pub fn visit(
    mut reader: impl BufRead,
    compact_format: bool,
    mut encoded: impl FnMut(&[u8]),
    mut draw: impl FnMut(&RawValue) -> Result<(), String>,
) -> Result<Counts, String> {
    let mut counts = Counts::default();
    let bound = if compact_format {
        compact::BLOCK_BYTES
    } else {
        ARCHIVE_ROW_BYTES
    };
    let expanded_bound = if compact_format {
        EXPANDED_REPORT_BYTES
    } else {
        REPORT_BYTES
    };
    loop {
        let mut line = Vec::new();
        let n = reader
            .by_ref()
            .take(bound + 1)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        counts.bytes += n as u64;
        if n as u64 > bound || line.last() != Some(&b'\n') || counts.bytes > ARCHIVE_BYTES {
            return Err("camera archive incomplete or exceeds disk/record bound".into());
        }
        encoded(&line);
        let rows = if compact_format {
            let block: compact::Block = serde_json::from_slice(&line).map_err(|e| e.to_string())?;
            counts.blocks += 1;
            block.decode()?
        } else {
            vec![serde_json::from_slice::<Box<RawValue>>(&line).map_err(|e| e.to_string())?]
        };
        for row in rows {
            counts.expanded_bytes += row.get().len() as u64 + 1;
            if !row.get().starts_with('{') || counts.expanded_bytes > expanded_bound {
                return Err(
                    "camera archive expansion exceeds report bound or row is not object".into(),
                );
            }
            draw(&row)?;
            counts.rows += 1;
        }
    }
    Ok(counts)
}
