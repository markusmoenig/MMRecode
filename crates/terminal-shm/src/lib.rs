//! Small audited POSIX shared-memory boundary for local terminal graphics.
//!
//! The main editor forbids unsafe code. Mapping a POSIX shared-memory descriptor necessarily
//! requires an unsafe call, so that operation lives here behind a narrow safe API.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{fs::File, io};

use memmap2::MmapOptions;
use rustix::{fs, shm};

/// Creates a new named shared-memory object and fills it with `bytes`.
///
/// `name` must follow the POSIX shared-memory naming rules. The caller transfers ownership of the
/// name to the terminal and remains responsible for unlinking it if the terminal does not.
///
/// # Errors
///
/// Returns an operating-system error when the name cannot be created, sized, or mapped.
pub fn create(name: &str, bytes: &[u8]) -> io::Result<()> {
    let fd = shm::open(
        name,
        shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR,
        fs::Mode::RUSR | fs::Mode::WUSR,
    )
    .map_err(io::Error::from)?;
    fs::ftruncate(
        &fd,
        u64::try_from(bytes.len())
            .map_err(|_| io::Error::other("shared-memory payload is too large"))?,
    )
    .map_err(io::Error::from)?;
    let file = File::from(fd);
    // SAFETY: `file` is a newly created, exclusively owned POSIX shared-memory object whose
    // length was set to exactly `bytes.len()`. No other mapping exists until this function returns.
    let mut mapping = unsafe { MmapOptions::new().len(bytes.len()).map_mut(&file)? };
    mapping.copy_from_slice(bytes);
    Ok(())
}

/// Removes a POSIX shared-memory name.
///
/// # Errors
///
/// Returns an operating-system error when the name does not exist or cannot be removed.
pub fn unlink(name: &str) -> io::Result<()> {
    shm::unlink(name).map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn created_object_contains_bytes_and_can_be_unlinked() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_le_bytes();
        let folded = u32::from_le_bytes([stamp[0], stamp[4], stamp[8], stamp[12]]);
        let name = format!("/mmt-{:08x}{folded:08x}", std::process::id());
        let bytes = [1, 2, 3, 4, 5, 6];
        create(&name, &bytes).unwrap();

        let fd = shm::open(name.as_str(), shm::OFlags::RDONLY, fs::Mode::empty()).unwrap();
        let file = File::from(fd);
        // SAFETY: the object was created above with exactly `bytes.len()` initialized bytes and
        // remains linked for the lifetime of this read-only mapping.
        let mapping = unsafe { MmapOptions::new().len(bytes.len()).map(&file).unwrap() };
        assert_eq!(&mapping[..], bytes);

        unlink(&name).unwrap();
        assert!(shm::open(name.as_str(), shm::OFlags::RDONLY, fs::Mode::empty(),).is_err());
    }
}
