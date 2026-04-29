use std::{io, path::Path, thread, time::Duration};

const REPLACE_RETRY_ATTEMPTS: usize = 20;
const REPLACE_RETRY_DELAY_MS: u64 = 50;

pub(crate) fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    for attempt in 0..REPLACE_RETRY_ATTEMPTS {
        match replace_file_once(source, destination) {
            Ok(()) => return Ok(()),
            Err(error) if should_retry_replace(&error) && attempt + 1 < REPLACE_RETRY_ATTEMPTS => {
                thread::sleep(Duration::from_millis(REPLACE_RETRY_DELAY_MS));
            }
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

#[cfg(not(windows))]
fn replace_file_once(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file_once(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(
            lpExistingFileName: *const u16,
            lpNewFileName: *const u16,
            dwFlags: u32,
        ) -> i32;
    }

    fn encode_wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let source_wide = encode_wide(source);
    let destination_wide = encode_wide(destination);
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn should_retry_replace(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::AlreadyExists
    ) || matches!(error.raw_os_error(), Some(5 | 32))
}

#[cfg(test)]
mod tests {
    use super::replace_file;
    use std::fs;

    #[test]
    fn replace_file_overwrites_existing_destination() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        let source = temp.path().join("source.tmp");
        let destination = temp.path().join("destination.txt");
        fs::write(&source, "next").expect("source write should succeed");
        fs::write(&destination, "previous").expect("destination write should succeed");

        replace_file(&source, &destination).expect("replace should succeed");

        assert_eq!(
            fs::read_to_string(&destination).expect("destination read should succeed"),
            "next"
        );
        assert!(!source.exists(), "source should be moved away");
    }
}
