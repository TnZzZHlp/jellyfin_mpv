#[cfg(windows)]
mod platform {
    use std::{io, ptr::null};

    use anyhow::{Context, Result};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE},
        System::Threading::CreateMutexW,
    };

    const MUTEX_NAME: &str = "Local\\JellyfinMpvCast.SingleInstance";

    pub struct SingleInstanceGuard(HANDLE);

    impl Drop for SingleInstanceGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn acquire() -> Result<Option<SingleInstanceGuard>> {
        acquire_named(MUTEX_NAME)
    }

    fn acquire_named(name: &str) -> Result<Option<SingleInstanceGuard>> {
        let name = name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error()).context("创建单实例互斥锁失败");
        }

        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already_running {
            unsafe {
                CloseHandle(handle);
            }
            Ok(None)
        } else {
            Ok(Some(SingleInstanceGuard(handle)))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rejects_a_second_instance_and_releases_the_mutex_on_drop() {
            let name = format!(
                "Local\\JellyfinMpvCast.SingleInstance.Test.{}",
                std::process::id()
            );
            let first = acquire_named(&name).unwrap().expect("第一个实例应取得锁");
            assert!(acquire_named(&name).unwrap().is_none());
            drop(first);
            assert!(acquire_named(&name).unwrap().is_some());
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::Result;

    pub struct SingleInstanceGuard;

    pub fn acquire() -> Result<Option<SingleInstanceGuard>> {
        Ok(Some(SingleInstanceGuard))
    }
}

pub use platform::acquire;
