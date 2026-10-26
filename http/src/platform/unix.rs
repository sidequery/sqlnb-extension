//! Unix descriptor-relative notebook operations.
use crate::*;
pub(crate) use libc::{LOCK_EX, LOCK_SH};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

pub(crate) trait PlatformHandle {
    fn platform_handle(&self) -> libc::c_int;
}
impl PlatformHandle for File {
    fn platform_handle(&self) -> libc::c_int {
        self.as_raw_fd()
    }
}

pub(crate) fn open_lock_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("failed to open UI cache lock {}: {error}", path.display()))
}

pub(crate) fn lock_file(file: &File, operation: libc::c_int, label: &str) -> Result<(), String> {
    loop {
        if unsafe { libc::flock(file.platform_handle(), operation) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("failed to lock {label}: {error}"));
        }
    }
}

pub(crate) fn try_lock_file(
    file: &File,
    operation: libc::c_int,
    label: &str,
) -> Result<bool, String> {
    loop {
        if unsafe { libc::flock(file.platform_handle(), operation | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
        {
            return Ok(false);
        }
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("failed to lock {label}: {error}"));
        }
    }
}

pub(crate) fn unlock_file(file: &File) -> Result<(), String> {
    lock_file(file, libc::LOCK_UN, "UI cache file")
}

pub(crate) fn cstring_component(value: &std::ffi::OsStr) -> Result<CString, ApiError> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(value.as_bytes()).map_err(|_| invalid_path())
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

pub(crate) fn list_directory_fd(
    directory: libc::c_int,
    relative: &Path,
    files: &mut Vec<FileEntry>,
    directories: &mut Vec<String>,
) -> Result<(), ApiError> {
    let dot = c".";
    // openat(".") creates an independent open file description. dup would
    // share the directory offset and make a second listing appear empty.
    let reopened = open_directory_at(directory, dot).map_err(file_io)?;
    let reopened = reopened.into_raw_fd();
    let stream = unsafe { libc::fdopendir(reopened) };
    if stream.is_null() {
        unsafe { libc::close(reopened) };
        return Err(file_io(std::io::Error::last_os_error()));
    }
    let stream = DirectoryStream(stream);
    loop {
        clear_errno();
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = current_errno();
            if error != 0 {
                return Err(file_io(std::io::Error::from_raw_os_error(error)));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"."
            || name.to_bytes() == b".."
            || name.to_bytes().eq_ignore_ascii_case(b".git")
        {
            continue;
        }
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                directory,
                name.as_ptr(),
                &mut metadata,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(file_io(std::io::Error::last_os_error()));
        }
        let kind = metadata.st_mode & libc::S_IFMT;
        if kind == libc::S_IFLNK {
            continue;
        }
        let file_name = std::ffi::OsString::from_vec(name.to_bytes().to_vec());
        let child_relative = relative.join(file_name);
        if kind == libc::S_IFDIR {
            let child_name = CString::new(name.to_bytes()).map_err(|_| invalid_path())?;
            let child = open_directory_at(directory, &child_name).map_err(file_io)?;
            if let Ok(path) = slash_path(&child_relative) {
                directories.push(path);
            }
            list_directory_fd(child.as_raw_fd(), &child_relative, files, directories)?;
        } else if kind == libc::S_IFREG && allowed_extension(&child_relative) {
            if let Ok(path) = slash_path(&child_relative) {
                files.push(FileEntry { path });
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn clear_errno() {
    unsafe { *libc::__error() = 0 }
}

#[cfg(target_os = "macos")]
pub(crate) fn current_errno() -> i32 {
    unsafe { *libc::__error() }
}

#[cfg(target_os = "linux")]
pub(crate) fn clear_errno() {
    unsafe { *libc::__errno_location() = 0 }
}

#[cfg(target_os = "linux")]
pub(crate) fn current_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

pub(crate) fn open_directory(path: &Path) -> std::io::Result<File> {
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub(crate) fn open_directory_at(parent: libc::c_int, name: &CStr) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub(crate) fn mkdir_at(parent: libc::c_int, name: &CStr) -> std::io::Result<()> {
    let result = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o755) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn open_file_at(parent: libc::c_int, name: &CStr) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub(crate) fn metadata_at(parent: libc::c_int, name: &CStr) -> std::io::Result<FileIdentity> {
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            &mut metadata,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(FileIdentity {
        volume: metadata.st_dev as u64,
        index: metadata.st_ino as u64,
        regular: metadata.st_mode & libc::S_IFMT == libc::S_IFREG,
    })
}
pub(crate) fn file_identity(file: &File) -> std::io::Result<FileIdentity> {
    let meta = file.metadata()?;
    Ok(FileIdentity {
        volume: meta.dev(),
        index: meta.ino(),
        regular: meta.is_file(),
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn rename_no_replace(
    source_parent: libc::c_int,
    source_name: &CStr,
    destination_parent: libc::c_int,
    destination_name: &CStr,
) -> std::io::Result<()> {
    let result = unsafe {
        libc::renameatx_np(
            source_parent,
            source_name.as_ptr(),
            destination_parent,
            destination_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn rename_no_replace(
    source_parent: libc::c_int,
    source_name: &CStr,
    destination_parent: libc::c_int,
    destination_name: &CStr,
) -> std::io::Result<()> {
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent,
            source_name.as_ptr(),
            destination_parent,
            destination_name.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn atomic_write_at(
    parent: libc::c_int,
    name: &CStr,
    source: &[u8],
    expected_revision: &str,
) -> std::io::Result<()> {
    let temporary = CString::new(format!(".sidequery-{}.tmp", uuid::Uuid::new_v4()))
        .expect("UUID contains no NUL");
    let fd = unsafe {
        libc::openat(
            parent,
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        file.write_all(source)?;
        file.sync_all()?;
        if expected_revision.is_empty() {
            let linked =
                unsafe { libc::linkat(parent, temporary.as_ptr(), parent, name.as_ptr(), 0) };
            if linked != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _ = unsafe { libc::unlinkat(parent, temporary.as_ptr(), 0) };
        } else {
            // Recheck after the temporary file is durable, immediately before
            // replacement. Portable filesystems do not expose a cross-process
            // compare-and-swap rename, so a non-cooperating writer can still
            // race the final rename after this check.
            let current = open_file_at(parent, name)?;
            let metadata = current.metadata()?;
            if !metadata.is_file() || metadata.len() > MAX_NOTEBOOK_BYTES as u64 {
                return Err(std::io::Error::from_raw_os_error(libc::EBUSY));
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            current
                .take((MAX_NOTEBOOK_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_NOTEBOOK_BYTES || revision(&bytes) != expected_revision {
                return Err(std::io::Error::from_raw_os_error(libc::EBUSY));
            }
            let renamed =
                unsafe { libc::renameat(parent, temporary.as_ptr(), parent, name.as_ptr()) };
            if renamed != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        let synced = unsafe { libc::fsync(parent) };
        if synced != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = unsafe { libc::unlinkat(parent, temporary.as_ptr(), 0) };
    }
    result
}

pub(crate) fn create_directory_at(parent: libc::c_int, name: &CStr) -> std::io::Result<()> {
    if unsafe { libc::mkdirat(parent, name.as_ptr(), 0o755) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
pub(crate) fn remove_file_at(parent: libc::c_int, name: &CStr) -> std::io::Result<()> {
    if unsafe { libc::unlinkat(parent, name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
pub(crate) fn sync_directory(parent: libc::c_int) -> std::io::Result<()> {
    if unsafe { libc::fsync(parent) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
pub(crate) fn private_new_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
pub(crate) fn open_lease_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}
pub(crate) fn is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}
