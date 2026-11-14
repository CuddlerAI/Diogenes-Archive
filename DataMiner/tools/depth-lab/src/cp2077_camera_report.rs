//! Disk/expansion bounds for CP2077 camera evidence, independent of admission budgets.
use std::io::{self, Write};

pub mod archive;
pub mod compact;

pub const ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
pub const ARCHIVE_ROW_BYTES: u64 = 1024 * 1024;
pub const LEGACY_REPORT_BYTES: u64 = 64 * 1024 * 1024;
// Physical legacy full-report bound. The v3 tail/metadata document remains <= 64 MiB.
pub const REPORT_BYTES: u64 = ARCHIVE_BYTES + LEGACY_REPORT_BYTES;
// Only the v3 archive's expansion + tail/metadata gets the larger budget.
// This is a byte ceiling, not preallocated memory or a change to evidence coverage.
pub const EXPANDED_REPORT_BYTES: u64 = 512 * 1024 * 1024;

/// Refuse a final report that its reader could not accept. No truncated write succeeds.
pub struct ReportWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W: Write> ReportWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            remaining: REPORT_BYTES,
        }
    }
}

impl<W: Write> Write for ReportWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(io::Error::other(
                "camera report exceeds its bounded disk budget",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_expansion_does_not_widen_physical_archive_or_legacy_bounds() {
        assert_eq!(REPORT_BYTES, 320 * 1024 * 1024);
        assert_eq!(EXPANDED_REPORT_BYTES, 512 * 1024 * 1024);
        assert_eq!(ARCHIVE_BYTES, 256 * 1024 * 1024);
        assert_eq!(LEGACY_REPORT_BYTES, 64 * 1024 * 1024);
        let mut writer = ReportWriter {
            inner: Vec::new(),
            remaining: 3,
        };
        writer.write_all(b"abc").unwrap();
        assert!(writer.write_all(b"d").is_err());
        assert_eq!(writer.inner, b"abc");
    }
}
