//! Root-relative Windows file operations. No child path is resolved through a reparse point.
use crate::*;
use std::io::{Error, ErrorKind, Result};
use std::os::windows::{
    ffi::OsStringExt,
    fs::{MetadataExt, OpenOptionsExt},
    io::{AsRawHandle, FromRawHandle},
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FileRenameInformation, NtCreateFile, NtSetInformationFile, FILE_CREATE,
            FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
            FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
        },
    },
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::IO::{IO_STATUS_BLOCK, OVERLAPPED},
    },
};

pub(crate) const LOCK_EX: i32 = 2;
pub(crate) const LOCK_SH: i32 = 1;
pub(crate) trait PlatformHandle {
    fn platform_handle(&self) -> HANDLE;
}
impl PlatformHandle for File {
    fn platform_handle(&self) -> HANDLE {
        self.as_raw_handle()
    }
}

fn reparse_error() -> Error {
    Error::new(
        ErrorKind::PermissionDenied,
        "reparse points are not allowed",
    )
}
pub(crate) fn is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
fn checked(file: File) -> Result<File> {
    if is_reparse(&file.metadata()?) {
        Err(reparse_error())
    } else {
        Ok(file)
    }
}

fn checked_component(file: File) -> Result<File> {
    let file = checked(file)?;
    let mut buffer = vec![0u16; 512];
    loop {
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.platform_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_NT,
            )
        } as usize;
        if length == 0 {
            return Err(Error::last_os_error());
        }
        if length >= buffer.len() {
            if length > 32768 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "resolved filename too long",
                ));
            }
            buffer.resize(length + 1, 0);
            continue;
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
        // NTFS short aliases are not reparse points. Check the actual component
        // on the opened handle as well as the caller's spelling (e.g. GIT~1).
        if path.file_name().is_none_or(|name| !valid_component(name)) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "reserved notebook component",
            ));
        }
        return Ok(file);
    }
}

pub(crate) fn validate_existing_component(path: &Path) -> Result<()> {
    checked_component(
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?,
    )
    .map(drop)
}
pub(crate) fn cstring_component(value: &std::ffi::OsStr) -> std::result::Result<CString, ApiError> {
    CString::new(value.to_str().ok_or_else(invalid_path)?).map_err(|_| invalid_path())
}
fn wide_component(name: &CStr) -> Result<Vec<u16>> {
    let text = name
        .to_str()
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid UTF-8 filename"))?;
    if text.is_empty() || text.contains(['/', '\\', ':']) || text == ".." {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "invalid filename component",
        ));
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    if wide.len() > (u16::MAX as usize / 2) {
        return Err(Error::new(ErrorKind::InvalidInput, "filename too long"));
    }
    Ok(wide)
}

// NtCreateFile is the Windows openat equivalent: ObjectAttributes.RootDirectory
// binds one component to a retained directory handle. OPEN_REPARSE_POINT opens
// the point itself, and checking that handle rejects junctions and every other tag.
fn open_at(parent: HANDLE, name: &CStr, access: u32, disposition: u32, kind: u32) -> Result<File> {
    let mut wide = wide_component(name)?;
    open_relative(parent, &mut wide, access, disposition, kind)
}

fn open_relative(
    parent: HANDLE,
    wide: &mut [u16],
    access: u32,
    disposition: u32,
    kind: u32,
) -> Result<File> {
    let mut unicode = UNICODE_STRING {
        Length: (wide.len() * 2) as u16,
        MaximumLength: (wide.len() * 2) as u16,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: &mut unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null_mut(),
        SecurityQualityOfService: ptr::null_mut(),
    };
    let mut status = IO_STATUS_BLOCK::default();
    let mut handle = ptr::null_mut();
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            disposition,
            kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    checked_component(unsafe { File::from_raw_handle(handle) })
}
pub(crate) fn open_directory(path: &Path) -> Result<File> {
    checked(
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?,
    )
}
pub(crate) fn open_directory_at(parent: HANDLE, name: &CStr) -> Result<File> {
    open_at(
        parent,
        name,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
    )
}

fn reopen_directory(directory: HANDLE) -> Result<File> {
    // NT paths do not interpret "." as Win32 paths do. An empty relative name
    // reopens this exact directory with an independent enumeration cursor.
    // Keep empty names private to this operation, not caller-supplied paths.
    open_relative(
        directory,
        &mut [],
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
    )
}
pub(crate) fn open_file_at(parent: HANDLE, name: &CStr) -> Result<File> {
    open_at(
        parent,
        name,
        FILE_READ_DATA | FILE_READ_ATTRIBUTES,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE,
    )
}
pub(crate) fn create_directory_at(parent: HANDLE, name: &CStr) -> Result<()> {
    open_at(
        parent,
        name,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_CREATE,
        FILE_DIRECTORY_FILE,
    )
    .map(drop)
}
pub(crate) fn mkdir_at(parent: HANDLE, name: &CStr) -> Result<()> {
    match create_directory_at(parent, name) {
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        result => result,
    }
}
pub(crate) fn sync_directory(_parent: HANDLE) -> Result<()> {
    // Windows has no supported directory fsync equivalent. File contents are
    // flushed before publishing; directory metadata durability is OS-managed.
    Ok(())
}
pub(crate) fn file_identity(file: &File) -> Result<FileIdentity> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.platform_handle(), &mut info) } == 0 {
        return Err(Error::last_os_error());
    }
    Ok(FileIdentity {
        volume: info.dwVolumeSerialNumber as u64,
        index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        regular: info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
            == 0,
    })
}
pub(crate) fn metadata_at(parent: HANDLE, name: &CStr) -> Result<FileIdentity> {
    file_identity(&open_file_at(parent, name)?)
}
fn rename_handle(file: &File, destination: HANDLE, name: &CStr, replace: bool) -> Result<()> {
    let wide = wide_component(name)?;
    let size = std::mem::size_of::<FILE_RENAME_INFORMATION>() + wide.len() * 2;
    // A u64 allocation supplies the structure's required alignment.
    let mut storage = vec![0u64; size.div_ceil(8)];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status = IO_STATUS_BLOCK::default();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = destination;
        (*info).FileNameLength = (wide.len() * 2) as u32;
        ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
        // Use the native directory-relative rename contract, matching NtCreateFile.
        // Resolving a pathname here would lose the pinned-parent boundary.
        let result = NtSetInformationFile(
            file.platform_handle(),
            &mut status,
            info.cast(),
            size as u32,
            FileRenameInformation,
        );
        if result < 0 {
            return Err(Error::from_raw_os_error(
                RtlNtStatusToDosError(result) as i32
            ));
        }
    }
    Ok(())
}
pub(crate) fn rename_no_replace(
    source: HANDLE,
    source_name: &CStr,
    destination: HANDLE,
    destination_name: &CStr,
) -> Result<()> {
    let file = open_at(
        source,
        source_name,
        DELETE | FILE_READ_ATTRIBUTES,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE,
    )?;
    rename_handle(&file, destination, destination_name, false)
}
fn delete_handle(file: &File) -> Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe {
        SetFileInformationByHandle(
            file.platform_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(Error::last_os_error());
    }
    Ok(())
}
pub(crate) fn remove_file_at(parent: HANDLE, name: &CStr) -> Result<()> {
    let file = open_at(
        parent,
        name,
        DELETE | FILE_READ_ATTRIBUTES,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE,
    )?;
    delete_handle(&file)
}
pub(crate) fn atomic_write_at(
    parent: HANDLE,
    name: &CStr,
    source: &[u8],
    expected: &str,
) -> Result<()> {
    let temporary = CString::new(format!(".sidequery-{}.tmp", uuid::Uuid::new_v4())).unwrap();
    let mut file = open_at(
        parent,
        &temporary,
        FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | DELETE,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE,
    )?;
    let result = (|| {
        file.write_all(source)?;
        file.sync_all()?;
        if !expected.is_empty() {
            let current = open_file_at(parent, name)?;
            let metadata = current.metadata()?;
            if !metadata.is_file() || metadata.len() > MAX_NOTEBOOK_BYTES as u64 {
                return Err(Error::new(ErrorKind::ResourceBusy, "notebook changed"));
            }
            let mut bytes = Vec::new();
            current
                .take((MAX_NOTEBOOK_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_NOTEBOOK_BYTES || revision(&bytes) != expected {
                return Err(Error::new(ErrorKind::ResourceBusy, "notebook changed"));
            }
        }
        // As on Unix, a non-cooperating editor can race the last revision check.
        // The destination is nevertheless resolved relative to the pinned parent.
        rename_handle(&file, parent, name, !expected.is_empty())
    })();
    if result.is_err() {
        let _ = delete_handle(&file);
    }
    result
}
pub(crate) fn list_directory_fd(
    directory: HANDLE,
    relative: &Path,
    files: &mut Vec<FileEntry>,
    directories: &mut Vec<String>,
) -> std::result::Result<(), ApiError> {
    let directory = reopen_directory(directory).map_err(file_io)?;
    let mut storage = vec![0u64; 8192];
    let mut first = true;
    loop {
        let class = if first {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        first = false;
        if unsafe {
            GetFileInformationByHandleEx(
                directory.platform_handle(),
                class,
                storage.as_mut_ptr().cast(),
                (storage.len() * 8) as u32,
            )
        } == 0
        {
            let error = Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Ok(());
            }
            return Err(file_io(error));
        }
        let mut offset = 0usize;
        loop {
            let capacity = storage.len() * 8;
            let name_offset = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            if offset % std::mem::align_of::<FILE_ID_BOTH_DIR_INFO>() != 0
                || offset + std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() > capacity
            {
                return Err(file_io(Error::new(
                    ErrorKind::InvalidData,
                    "invalid directory entry",
                )));
            }
            let entry = unsafe {
                &*storage
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_BOTH_DIR_INFO>()
            };
            if entry.FileNameLength % 2 != 0
                || offset + name_offset + entry.FileNameLength as usize > capacity
            {
                return Err(file_io(Error::new(
                    ErrorKind::InvalidData,
                    "invalid directory filename",
                )));
            }
            let name = unsafe {
                std::slice::from_raw_parts(
                    entry.FileName.as_ptr(),
                    entry.FileNameLength as usize / 2,
                )
            };
            if let Ok(name) = String::from_utf16(name) {
                if name != "."
                    && name != ".."
                    && valid_component(std::ffi::OsStr::new(&name))
                    && entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                {
                    let child_relative = relative.join(&name);
                    let child_name = CString::new(name).map_err(|_| invalid_path())?;
                    if entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                        let child = open_directory_at(directory.platform_handle(), &child_name)
                            .map_err(file_io)?;
                        directories.push(slash_path(&child_relative)?);
                        list_directory_fd(
                            child.platform_handle(),
                            &child_relative,
                            files,
                            directories,
                        )?;
                    } else if allowed_extension(&child_relative) {
                        // Reopen through the secured boundary: enumeration metadata can race.
                        if open_file_at(directory.platform_handle(), &child_name).is_ok() {
                            files.push(FileEntry {
                                path: slash_path(&child_relative)?,
                            });
                        }
                    }
                }
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            if (entry.NextEntryOffset as usize) < name_offset + entry.FileNameLength as usize {
                return Err(file_io(Error::new(
                    ErrorKind::InvalidData,
                    "invalid directory offset",
                )));
            }
            offset += entry.NextEntryOffset as usize;
        }
    }
}
pub(crate) fn private_new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}
pub(crate) fn open_lease_file(path: &Path) -> Result<File> {
    checked(
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?,
    )
}
pub(crate) fn open_lock_file(path: &Path) -> std::result::Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .and_then(checked)
        .map_err(|error| format!("failed to open lock: {error}"))
}
fn lock(file: &File, exclusive: bool, immediate: bool) -> Result<bool> {
    let mut overlapped = OVERLAPPED::default();
    let flags = if exclusive {
        LOCKFILE_EXCLUSIVE_LOCK
    } else {
        0
    } | if immediate {
        LOCKFILE_FAIL_IMMEDIATELY
    } else {
        0
    };
    if unsafe { LockFileEx(file.platform_handle(), flags, 0, 1, 0, &mut overlapped) } != 0 {
        return Ok(true);
    }
    let error = Error::last_os_error();
    if immediate && error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        return Ok(false);
    }
    Err(error)
}
pub(crate) fn lock_file(
    file: &File,
    operation: i32,
    label: &str,
) -> std::result::Result<(), String> {
    lock(file, operation == LOCK_EX, false)
        .map(|_| ())
        .map_err(|error| format!("failed to lock {label}: {error}"))
}
pub(crate) fn try_lock_file(
    file: &File,
    operation: i32,
    label: &str,
) -> std::result::Result<bool, String> {
    lock(file, operation == LOCK_EX, true)
        .map_err(|error| format!("failed to lock {label}: {error}"))
}
pub(crate) fn unlock_file(file: &File) -> std::result::Result<(), String> {
    let mut overlapped = OVERLAPPED::default();
    if unsafe { UnlockFileEx(file.platform_handle(), 0, 1, 0, &mut overlapped) } == 0 {
        return Err(format!("failed to unlock file: {}", Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn junction(target: &Path, link: &Path) {
        let output = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn junctions_are_rejected_and_pinned_parent_cannot_be_redirected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(root.join("folder")).unwrap();
        let root_handle = open_directory(&root).unwrap();
        let parent = open_directory_at(root_handle.platform_handle(), c"folder").unwrap();
        fs::rename(root.join("folder"), root.join("original")).unwrap();
        junction(&outside, &root.join("folder"));
        assert!(open_directory_at(root_handle.platform_handle(), c"folder").is_err());
        atomic_write_at(parent.platform_handle(), c"test.sql", b"select 1", "").unwrap();
        assert_eq!(
            fs::read(root.join("original/test.sql")).unwrap(),
            b"select 1"
        );
        assert!(!outside.join("test.sql").exists());

        let revision = revision(b"select 1");
        atomic_write_at(
            parent.platform_handle(),
            c"test.sql",
            b"select 2",
            &revision,
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join("original/test.sql")).unwrap(),
            b"select 2"
        );
        let mut files = Vec::new();
        let mut directories = Vec::new();
        list_directory_fd(
            parent.platform_handle(),
            Path::new(""),
            &mut files,
            &mut directories,
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "test.sql");
        assert!(!outside.join("test.sql").exists());
    }

    #[test]
    fn native_write_flush_and_relative_rename_preserve_collision_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let root = open_directory(temp.path()).unwrap();
        let mut file = open_at(
            root.platform_handle(),
            c"temporary",
            FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | DELETE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
        )
        .expect("create a file relative to the root handle");
        file.write_all(b"select 42;").expect("write file contents");
        file.sync_all().expect("flush file contents");
        rename_handle(&file, root.platform_handle(), c"π.sql", false)
            .expect("rename relative to the root handle");
        assert_eq!(fs::read(temp.path().join("π.sql")).unwrap(), b"select 42;");
        drop(file);

        assert_eq!(
            atomic_write_at(root.platform_handle(), c"π.sql", b"overwrite", "")
                .unwrap_err()
                .kind(),
            ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(temp.path().join("π.sql")).unwrap(), b"select 42;");
    }

    #[test]
    fn windows_aliases_and_streams_are_invalid_notebook_paths() {
        for path in [
            "name.sql:stream",
            "CON.sql",
            "aux.sql",
            "LPT1/query.sql",
            "COM¹.sql",
            "git~1/config.sql",
            "GIT~2/config.sql",
            "folder./test.sql",
            "folder /test.sql",
            "C:/outside.sql",
            "\\\\server\\share\\a.sql",
        ] {
            assert!(validate_relative_path(path).is_err(), "accepted {path}");
        }
        assert!(validate_relative_path("Unicode/π.sql").is_ok());
    }

    #[test]
    fn handle_boundary_rejects_git_even_without_lexical_validation() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let root = open_directory(temp.path()).unwrap();
        assert!(open_directory_at(root.platform_handle(), c".git").is_err());
        assert!(validate_existing_component(&temp.path().join(".git")).is_err());
    }

    #[test]
    fn shared_leases_exclude_writers_until_last_handle_closes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lock");
        let first = open_lock_file(&path).unwrap();
        let second = open_lock_file(&path).unwrap();
        let writer = open_lock_file(&path).unwrap();
        lock_file(&first, LOCK_SH, "first").unwrap();
        lock_file(&second, LOCK_SH, "second").unwrap();
        assert!(!try_lock_file(&writer, LOCK_EX, "writer").unwrap());
        drop(first);
        assert!(!try_lock_file(&writer, LOCK_EX, "writer").unwrap());
        drop(second);
        assert!(try_lock_file(&writer, LOCK_EX, "writer").unwrap());
    }
}
