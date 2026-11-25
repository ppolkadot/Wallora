//! Atomic no-clobber file publication for Windows using `MoveFileExW`.

use std::path::Path;

use wallora_core::services::{FilePublisher, PublishError};

/// Publishes a staged file to its final destination without ever overwriting an existing file.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsFilePublisher;

#[cfg(target_os = "windows")]
fn encode_wide_null_terminated(path: &Path) -> Result<Vec<u16>, PublishError> {
    use std::os::windows::ffi::OsStrExt;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(PublishError::Io(
            "path contains an interior NUL character".to_string(),
        ));
    }
    wide.push(0);
    Ok(wide)
}

impl FilePublisher for WindowsFilePublisher {
    fn publish_no_clobber(&self, src: &Path, dst: &Path) -> Result<(), PublishError> {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
            use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
            use windows::core::{HRESULT, PCWSTR};

            let src_wide = encode_wide_null_terminated(src)?;
            let dst_wide = encode_wide_null_terminated(dst)?;

            // SAFETY: Both buffers are valid, null-terminated UTF-16 slices alive for the call.
            // Passing `MOVE_FILE_FLAGS(0)` deliberately omits `MOVEFILE_REPLACE_EXISTING`,
            // so Windows fails atomically when `dst` exists rather than clobbering it.
            let res = unsafe {
                MoveFileExW(
                    PCWSTR(src_wide.as_ptr()),
                    PCWSTR(dst_wide.as_ptr()),
                    MOVE_FILE_FLAGS(0),
                )
            };

            match res {
                Ok(()) => Ok(()),
                Err(err)
                    if err.code() == HRESULT::from_win32(ERROR_ALREADY_EXISTS.0)
                        || err.code() == HRESULT::from_win32(ERROR_FILE_EXISTS.0) =>
                {
                    Err(PublishError::AlreadyExists)
                }
                Err(err) => Err(PublishError::Io(err.message())),
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            wallora_core::services::HardLinkFilePublisher.publish_no_clobber(src, dst)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("wallora-win-publish-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn move_file_ex_publishes_when_destination_is_absent() {
        let dir = TempDir::new();
        let src = dir.0.join(".wallhaven-abc123.png.part");
        let dst = dir.0.join("wallhaven-abc123.png");
        let payload = b"exact-original-png-bytes";
        std::fs::write(&src, payload).expect("write part");

        WindowsFilePublisher
            .publish_no_clobber(&src, &dst)
            .expect("absent destination publishes cleanly");

        assert!(!src.exists(), "source .part should be moved away");
        assert_eq!(std::fs::read(&dst).expect("read published"), payload);
    }

    #[test]
    fn move_file_ex_returns_already_exists_without_overwriting_destination() {
        let dir = TempDir::new();
        let src = dir.0.join(".wallhaven-abc123.png.part");
        let dst = dir.0.join("wallhaven-abc123.png");
        let existing = b"existing-user-file-do-not-overwrite";
        let staged = b"new-downloaded-payload";
        std::fs::write(&dst, existing).expect("write existing");
        std::fs::write(&src, staged).expect("write part");

        let err = WindowsFilePublisher
            .publish_no_clobber(&src, &dst)
            .expect_err("existing destination must return AlreadyExists");
        assert_eq!(err, PublishError::AlreadyExists);
        assert_eq!(
            std::fs::read(&dst).expect("read existing"),
            existing,
            "existing destination bytes must remain untouched"
        );
        assert_eq!(
            std::fs::read(&src).expect("read staged"),
            staged,
            "staged source must remain intact for numbered collision retry"
        );
    }

    #[test]
    fn move_file_ex_concurrent_race_never_clobbers() {
        let dir = TempDir::new();
        let src_a = dir.0.join(".a.part");
        let src_b = dir.0.join(".b.part");
        let dst = dir.0.join("target.png");
        let bytes_a = b"winner-or-loser-payload-A";
        let bytes_b = b"winner-or-loser-payload-B";
        std::fs::write(&src_a, bytes_a).expect("write a");
        std::fs::write(&src_b, bytes_b).expect("write b");

        let barrier = Arc::new(Barrier::new(2));
        let b1 = Arc::clone(&barrier);
        let dst1 = dst.clone();
        let s1 = src_a.clone();
        let t1 = std::thread::spawn(move || {
            b1.wait();
            WindowsFilePublisher.publish_no_clobber(&s1, &dst1)
        });

        let b2 = Arc::clone(&barrier);
        let dst2 = dst.clone();
        let s2 = src_b.clone();
        let t2 = std::thread::spawn(move || {
            b2.wait();
            WindowsFilePublisher.publish_no_clobber(&s2, &dst2)
        });

        let r1 = t1.join().expect("thread 1");
        let r2 = t2.join().expect("thread 2");

        match (&r1, &r2) {
            (Ok(()), Err(PublishError::AlreadyExists)) => {
                assert_eq!(std::fs::read(&dst).unwrap(), bytes_a);
                assert!(!src_a.exists());
                assert_eq!(std::fs::read(&src_b).unwrap(), bytes_b);
            }
            (Err(PublishError::AlreadyExists), Ok(())) => {
                assert_eq!(std::fs::read(&dst).unwrap(), bytes_b);
                assert!(!src_b.exists());
                assert_eq!(std::fs::read(&src_a).unwrap(), bytes_a);
            }
            other => panic!("unexpected concurrent race outcome: {other:?}"),
        }
    }
}
