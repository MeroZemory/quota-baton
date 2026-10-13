use super::StoreError;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::mpsc,
    thread::{self, JoinHandle},
};
use windows::Win32::{
    Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
};

pub(super) struct Lease {
    release: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Lease {
    pub fn acquire(dir: &Path) -> Result<Self, StoreError> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(".lock"))
            .map_err(|_| StoreError::Io)?;
        // 재귀 mutex 획득과 별개로 같은 홈의 중복 소유를 막는다.
        FileExt::try_lock_exclusive(&lock).map_err(|_| StoreError::LeaseBusy)?;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::channel();
        let mutex_name = mutex_name(dir);
        // Windows mutex는 획득한 스레드에서만 해제할 수 있다.
        let thread = thread::Builder::new()
            .name("credential-lease".into())
            .spawn(move || {
                let result = unsafe {
                    CreateMutexW(None, false, windows::core::PCWSTR(mutex_name.as_ptr()))
                };
                let handle = match result {
                    Ok(handle) => handle,
                    Err(_) => {
                        let _ = ready_tx.send(Err(StoreError::LeaseUnavailable));
                        return;
                    }
                };
                let status = unsafe { WaitForSingleObject(handle, 0) };
                if status == WAIT_OBJECT_0 || status == WAIT_ABANDONED {
                    if ready_tx.send(Ok(())).is_ok() {
                        let _ = release_rx.recv();
                    }
                    unsafe {
                        let _ = ReleaseMutex(handle);
                    }
                } else {
                    let _ = ready_tx.send(Err(if status == WAIT_TIMEOUT {
                        StoreError::LeaseBusy
                    } else {
                        StoreError::LeaseUnavailable
                    }));
                }
                unsafe {
                    let _ = CloseHandle(handle);
                }
                drop(lock);
            })
            .map_err(|_| StoreError::LeaseUnavailable)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                release,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(StoreError::LeaseUnavailable)
            }
        }
    }
}

fn mutex_name(accounts_dir: &Path) -> Vec<u16> {
    let home = accounts_dir
        .parent()
        .unwrap_or(accounts_dir)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(accounts_dir));
    let normalized = home.to_string_lossy().to_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    let suffix: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("Local\\quota-baton-credentials-{suffix}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
