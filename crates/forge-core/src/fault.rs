//! Test-only fault injection for copy paths (owned by the ingest/copy-robustness work).
//!
//! The app exposes arming only through its active UI automation hook. Production copies pay
//! one atomic flag check per read and take no lock while the seam is disarmed.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
enum Rule {
    SourceGone {
        prefix: PathBuf,
        after_bytes: u64,
        bytes_read: u64,
        triggered: bool,
    },
    CorruptBeforeVerify {
        prefix: PathBuf,
        fired: bool,
    },
}

static ARMED: AtomicBool = AtomicBool::new(false);
static RULE: Mutex<Option<Rule>> = Mutex::new(None);

/// Arm a simulated removable-device disconnect after `after_bytes` have been read under
/// `prefix`. Only the active automation hook calls this in the application.
pub fn arm_source_gone_after(prefix: &Path, after_bytes: u64) -> Result<(), String> {
    let Some(prefix) = non_empty_prefix(prefix)? else {
        return Err("the fault path prefix must not be empty".into());
    };
    set_rule(Rule::SourceGone {
        prefix,
        after_bytes,
        bytes_read: 0,
        triggered: false,
    });
    Ok(())
}

/// Arm a one-shot corruption of the temporary copy before it is read back for verification.
/// The final destination is never placed because the verification hash will differ.
pub fn arm_corrupt_before_verify(prefix: &Path) -> Result<(), String> {
    let Some(prefix) = non_empty_prefix(prefix)? else {
        return Err("the fault path prefix must not be empty".into());
    };
    set_rule(Rule::CorruptBeforeVerify {
        prefix,
        fired: false,
    });
    Ok(())
}

fn non_empty_prefix(prefix: &Path) -> Result<Option<PathBuf>, String> {
    if prefix.as_os_str().is_empty() {
        return Err("the fault path prefix must not be empty".into());
    }
    Ok(Some(prefix.to_path_buf()))
}

fn set_rule(rule: Rule) {
    *RULE.lock().unwrap_or_else(|p| p.into_inner()) = Some(rule);
    ARMED.store(true, Ordering::Release);
}

/// Disarm any current rule. Also used by automation after simulating a card reinsert.
pub fn disarm() {
    ARMED.store(false, Ordering::Release);
    *RULE.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// Whether a fault rule is armed. Callers should also ensure automation is active before
/// changing behavior around this seam.
pub fn is_armed() -> bool {
    ARMED.load(Ordering::Acquire)
}

/// Wraps the copy source reader. The ordinary path checks one atomic flag, then reads
/// directly from the original reader.
pub fn reader<R: Read>(reader: R, path: &Path) -> impl Read {
    FaultReader {
        inner: reader,
        path: path.to_path_buf(),
    }
}

struct FaultReader<R> {
    inner: R,
    path: PathBuf,
}

impl<R: Read> Read for FaultReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !ARMED.load(Ordering::Acquire) || buf.is_empty() {
            return self.inner.read(buf);
        }

        let mut rule = RULE.lock().unwrap_or_else(|p| p.into_inner());
        let Some(Rule::SourceGone {
            prefix,
            after_bytes,
            bytes_read,
            triggered,
        }) = rule.as_mut()
        else {
            return self.inner.read(buf);
        };
        if !self.path.starts_with(prefix) {
            return self.inner.read(buf);
        }
        if *triggered || *bytes_read >= *after_bytes {
            *triggered = true;
            return Err(device_gone());
        }

        let remaining = (*after_bytes - *bytes_read).min(buf.len() as u64) as usize;
        let read = self.inner.read(&mut buf[..remaining])?;
        *bytes_read = bytes_read.saturating_add(read as u64);
        if *bytes_read >= *after_bytes {
            *triggered = true;
        }
        Ok(read)
    }
}

/// Whether a simulated disconnected source covers `path`. Ancestor paths are included so a
/// recursive copy can stop the current source root as soon as one file detects the fault.
pub fn source_is_gone(path: &Path) -> bool {
    if !ARMED.load(Ordering::Acquire) {
        return false;
    }
    let rule = RULE.lock().unwrap_or_else(|p| p.into_inner());
    matches!(
        rule.as_ref(),
        Some(Rule::SourceGone {
            prefix,
            triggered: true,
            ..
        }) if path.starts_with(prefix) || prefix.starts_with(path)
    )
}

/// Corrupt one byte of the temp copy for a matching source, immediately before verification.
/// Returns `true` when the rule fired.
pub fn corrupt_before_verify(source: &Path, temp: &Path) -> io::Result<bool> {
    if !ARMED.load(Ordering::Acquire) {
        return Ok(false);
    }
    let fire = {
        let mut rule = RULE.lock().unwrap_or_else(|p| p.into_inner());
        match rule.as_mut() {
            Some(Rule::CorruptBeforeVerify { prefix, fired })
                if !*fired && source.starts_with(prefix.as_path()) =>
            {
                *fired = true;
                true
            }
            _ => false,
        }
    };
    if !fire {
        return Ok(false);
    }

    let mut file = OpenOptions::new().read(true).write(true).open(temp)?;
    let mut byte = [0];
    if file.read(&mut byte)? == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(0))?;
    byte[0] ^= 1;
    file.write_all(&byte)?;
    file.flush()?;
    Ok(true)
}

fn device_gone() -> io::Error {
    // ERROR_DEVICE_NOT_CONNECTED on Windows. The copy engine preserves the OS error in its
    // report so automated runs exercise the same error boundary as a pulled card.
    io::Error::from_raw_os_error(1167)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};

    struct DisarmOnDrop;

    impl Drop for DisarmOnDrop {
        fn drop(&mut self) {
            disarm();
        }
    }

    #[test]
    fn source_gone_rule_fails_after_the_shared_byte_threshold() {
        let _cleanup = DisarmOnDrop;
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("card");
        let first = prefix.join("a.bin");
        let second = prefix.join("b.bin");
        fs::create_dir_all(&prefix).unwrap();
        fs::write(&first, b"abcd").unwrap();
        fs::write(&second, b"efgh").unwrap();

        arm_source_gone_after(&prefix, 6).unwrap();
        let mut a = reader(File::open(&first).unwrap(), &first);
        let mut buf = [0; 8];
        assert_eq!(a.read(&mut buf).unwrap(), 4);
        let mut b = reader(File::open(&second).unwrap(), &second);
        assert_eq!(b.read(&mut buf).unwrap(), 2);
        let error = b.read(&mut buf).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(1167));
        assert!(source_is_gone(&prefix));
    }

    #[test]
    fn corruption_rule_changes_only_the_temp_copy_once() {
        let _cleanup = DisarmOnDrop;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let temp = dir.path().join(".forge-partial-test");
        fs::write(&source, b"original").unwrap();
        fs::write(&temp, b"original").unwrap();

        arm_corrupt_before_verify(&source).unwrap();
        assert!(corrupt_before_verify(&source, &temp).unwrap());
        assert!(!corrupt_before_verify(&source, &temp).unwrap());
        assert_eq!(fs::read(&source).unwrap(), b"original");
        assert_ne!(fs::read(&temp).unwrap(), b"original");
    }
}
