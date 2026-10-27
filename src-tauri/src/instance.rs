//! Hold an OS-owned lock for the full application lifetime. The file remains
//! in place so another process can never lock a newly created inode instead.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
};

pub struct InstanceGuard {
    _file: File,
}

impl InstanceGuard {
    pub fn acquire(data_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        // An exclusive Windows handle is released by the OS even after a crash.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let file = options
            .open(data_dir.join("instance.lock"))
            .map_err(|error| {
                #[cfg(windows)]
                if error.raw_os_error() == Some(32) {
                    return already_running();
                }
                error
            })?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: `file` owns a valid open descriptor throughout this call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = io::Error::last_os_error();
                return Err(if error.kind() == io::ErrorKind::WouldBlock {
                    already_running()
                } else {
                    error
                });
            }
        }
        Ok(Self { _file: file })
    }
}

fn already_running() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "IP Picker 已在运行，请使用已打开的窗口",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn probe_in_another_process(path: &Path, expected: &str) {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "instance::tests::lock_probe_child",
                "--nocapture",
            ])
            .env("IP_PICKER_TEST_INSTANCE_DIR", path)
            .env("IP_PICKER_TEST_INSTANCE_EXPECTED", expected)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn lock_probe_child() {
        let Some(path) = std::env::var_os("IP_PICKER_TEST_INSTANCE_DIR") else {
            return;
        };
        let expected = std::env::var("IP_PICKER_TEST_INSTANCE_EXPECTED").unwrap();
        match InstanceGuard::acquire(Path::new(&path)) {
            Ok(_guard) => {
                assert_eq!(expected, "available");
                // Model abrupt termination: release must not depend on Drop.
                std::process::exit(0);
            }
            Err(error) => {
                assert_eq!(expected, "blocked");
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            }
        }
    }

    #[test]
    fn another_process_cannot_open_the_store_until_the_owner_exits() {
        let dir = tempfile::tempdir().unwrap();
        let guard = InstanceGuard::acquire(dir.path()).unwrap();
        let state_path = dir.path().join("state.json");
        fs::write(&state_path, b"previous state").unwrap();
        probe_in_another_process(dir.path(), "blocked");
        fs::write(&state_path, b"newly saved favorite").unwrap();
        probe_in_another_process(dir.path(), "blocked");
        assert_eq!(fs::read(&state_path).unwrap(), b"newly saved favorite");
        drop(guard);
        probe_in_another_process(dir.path(), "available");
        // The child exited without running destructors, leaving only a stale
        // filename. A new owner must still be able to acquire the OS lock.
        assert!(InstanceGuard::acquire(dir.path()).is_ok());
    }

    #[test]
    fn unavailable_lock_path_fails_without_touching_state() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        fs::write(&state_path, b"original state").unwrap();
        fs::create_dir(dir.path().join("instance.lock")).unwrap();
        assert!(InstanceGuard::acquire(dir.path()).is_err());
        assert_eq!(fs::read(state_path).unwrap(), b"original state");
    }
}
