/// Checks PID existence, including processes we cannot signal. This is a
/// point-in-time probe, not an identity check or a guarantee against PID reuse.
pub(crate) fn process_is_alive(pid: u32) -> std::io::Result<bool> {
    let pid = libc::pid_t::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid process id")
        })?;
    // Signal zero performs only the existence/permission check.
    if unsafe { libc::kill(pid, 0) } == 0 {
        Ok(true)
    } else {
        classify_liveness_error(std::io::Error::last_os_error())
    }
}

fn classify_liveness_error(error: std::io::Error) -> std::io::Result<bool> {
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn current_process_is_alive() {
        assert!(process_is_alive(std::process::id()).unwrap());
    }

    #[test]
    fn invalid_pids_are_rejected_before_probing() {
        for pid in [0, libc::pid_t::MAX as u32 + 1, u32::MAX] {
            assert_eq!(
                process_is_alive(pid).unwrap_err().kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn permission_denied_means_alive_and_other_errors_are_preserved() {
        assert!(classify_liveness_error(std::io::Error::from_raw_os_error(libc::EPERM)).unwrap());
        assert!(!classify_liveness_error(std::io::Error::from_raw_os_error(libc::ESRCH)).unwrap());
        assert_eq!(
            classify_liveness_error(std::io::Error::from_raw_os_error(libc::EINVAL))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EINVAL)
        );
    }

    #[test]
    fn owned_child_is_alive_until_it_exits_and_is_reaped() {
        // The child waits on its private stdin; EOF lets it exit without signals.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read -r ignored"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let alive = process_is_alive(pid);
        drop(child.stdin.take());
        child.wait().unwrap();
        let exited = process_is_alive(pid);
        assert!(alive.unwrap());
        assert!(!exited.unwrap());
    }
}
