//! Choose a fresh run name before Depth can write StartRun/ACK evidence.
//! The caller serializes start operations; the recorder still enforces its
//! immutable-directory contract. Failed attempts are retained, never reused.
use std::io;
use std::path::Path;

pub(crate) fn next_available_run_seq(runs_root: &Path, first: u32) -> io::Result<u32> {
    let mut seq = first;
    for _ in 0..1024 {
        match std::fs::symlink_metadata(runs_root.join(format!("r{seq:04}"))) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(seq),
            Err(error) => return Err(error),
            Ok(_) => {
                seq = seq
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("run sequence exhausted"))?;
            }
        }
    }
    Err(io::Error::other(
        "too many occupied run names; existing evidence was preserved",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(label: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dm-run-slot-{label}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn failed_depth_attempts_keep_their_ack_files_and_get_new_run_names() {
        let root = root("retry");
        let old = root.join("r0002/depth-lab/probe-control/acks");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("0002-faulted.json"), b"original warmup failure").unwrap();
        assert_eq!(next_available_run_seq(&root, 2).unwrap(), 3);
        std::fs::create_dir(root.join("r0003")).unwrap();
        assert_eq!(next_available_run_seq(&root, 2).unwrap(), 4);
        assert_eq!(
            std::fs::read(old.join("0002-faulted.json")).unwrap(),
            b"original warmup failure"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_runs_and_occupied_files_do_not_overwrite_existing_artifacts() {
        let root = root("fresh");
        assert_eq!(next_available_run_seq(&root, 0).unwrap(), 0);
        std::fs::write(root.join("r0000"), b"keep").unwrap();
        assert_eq!(next_available_run_seq(&root, 0).unwrap(), 1);
        assert_eq!(std::fs::read(root.join("r0000")).unwrap(), b"keep");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exhausted_sequence_never_wraps_to_an_old_run() {
        let root = root("overflow");
        std::fs::create_dir(root.join(format!("r{}", u32::MAX))).unwrap();
        assert!(next_available_run_seq(&root, u32::MAX).is_err());
        assert!(!root.join("r0000").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
