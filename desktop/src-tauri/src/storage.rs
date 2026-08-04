use std::{
    ffi::{OsStr, OsString},
    fs::{self, Metadata},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Map, Value};

use crate::resources::display_path;

const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_PROJECT_FILE_BYTES: u64 = 4 * 1024 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const fn attributes_have_reparse_point(attributes: u32) -> bool {
    attributes & WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PhysicalIdentity {
    device: Option<u64>,
    file: Option<u64>,
}

fn invalid_path(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn metadata_is_link(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        return attributes_have_reparse_point(metadata.file_attributes());
    }
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
#[repr(C)]
struct WindowsFileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsByHandleFileInformation {
    file_attributes: u32,
    creation_time: WindowsFileTime,
    last_access_time: WindowsFileTime,
    last_write_time: WindowsFileTime,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
fn windows_file_identity(file: &fs::File) -> io::Result<PhysicalIdentity> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle as _;

    type Handle = *mut c_void;

    #[link(name = "Kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            file: Handle,
            information: *mut WindowsByHandleFileInformation,
        ) -> i32;
    }

    let mut information = std::mem::MaybeUninit::<WindowsByHandleFileInformation>::uninit();
    let ok = unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as Handle, information.as_mut_ptr())
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let information = unsafe { information.assume_init() };
    if attributes_have_reparse_point(information.file_attributes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem object became a Windows reparse point",
        ));
    }
    Ok(PhysicalIdentity {
        device: Some(u64::from(information.volume_serial_number)),
        file: Some(
            (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low),
        ),
    })
}

fn physical_identity(file: &fs::File, metadata: &Metadata) -> io::Result<PhysicalIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let _ = file;
        Ok(PhysicalIdentity {
            device: Some(metadata.dev()),
            file: Some(metadata.ino()),
        })
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        windows_file_identity(file)
    }
    #[cfg(not(any(unix, windows)))]
    Ok(PhysicalIdentity {
        device: None,
        file: None,
    })
}

#[derive(Debug)]
struct PhysicalDirectory {
    file: fs::File,
}

impl PhysicalDirectory {
    fn from_file(file: fs::File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if metadata_is_link(&metadata) || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "filesystem component is linked, reparsed, or not a directory",
            ));
        }
        #[cfg(windows)]
        windows_file_identity(&file)?;
        Ok(Self { file })
    }

    fn try_clone_file(&self) -> io::Result<fs::File> {
        self.file.try_clone()
    }

    fn open_directory(&self, fragment: &OsStr, follow_links: bool) -> io::Result<Self> {
        platform::open_directory(&self.file, fragment, follow_links).and_then(Self::from_file)
    }

    fn create_directory(&self, fragment: &OsStr) -> io::Result<Self> {
        platform::create_directory(&self.file, fragment).and_then(Self::from_file)
    }

    fn create_new_directory(&self, fragment: &OsStr) -> io::Result<Self> {
        platform::create_new_directory(&self.file, fragment).and_then(Self::from_file)
    }

    fn open_directory_for_removal(&self, fragment: &OsStr) -> io::Result<Self> {
        platform::open_directory_for_removal(&self.file, fragment).and_then(Self::from_file)
    }

    fn open_entry(&self, fragment: &OsStr) -> io::Result<fs::File> {
        let file = platform::open_entry(&self.file, fragment)?;
        reject_linked_handle(&file)?;
        Ok(file)
    }

    fn open_regular_file(&self, fragment: &OsStr) -> io::Result<fs::File> {
        let file = platform::open_regular_file(&self.file, fragment)?;
        let metadata = reject_linked_handle(&file)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "filesystem entry is not a regular file",
            ));
        }
        Ok(file)
    }

    /// Same validation as [`Self::open_regular_file`], but the handle also
    /// carries whatever right the platform needs to delete through it. Kept
    /// separate so the read-only callers do not silently gain DELETE.
    fn open_regular_file_for_removal(&self, fragment: &OsStr) -> io::Result<fs::File> {
        let file = platform::open_regular_file_for_removal(&self.file, fragment)?;
        let metadata = reject_linked_handle(&file)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "filesystem entry is not a regular file",
            ));
        }
        Ok(file)
    }

    fn create_new_file(&self, fragment: &OsStr, mode: u32) -> io::Result<fs::File> {
        let file = platform::create_new_file(&self.file, fragment, mode)?;
        reject_linked_handle(&file)?;
        Ok(file)
    }

    fn replace_with_open_file(
        &self,
        temporary: &fs::File,
        temporary_name: &OsStr,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        platform::replace_file(&self.file, temporary, temporary_name, destination_name)
    }

    fn remove_open_file(&self, file: &fs::File, fragment: &OsStr) -> io::Result<()> {
        platform::remove_file(&self.file, file, fragment)
    }

    fn entry_names(&self, limit: usize) -> io::Result<Vec<OsString>> {
        platform::directory_entry_names(&self.file, limit)
    }

    fn remove_open_directory(
        &self,
        directory: &PhysicalDirectory,
        fragment: &OsStr,
    ) -> io::Result<()> {
        platform::remove_directory(&self.file, &directory.file, fragment)
    }

    fn identity(&self) -> io::Result<PhysicalIdentity> {
        let metadata = self.file.metadata()?;
        physical_identity(&self.file, &metadata)
    }
}

fn reject_linked_handle(file: &fs::File) -> io::Result<Metadata> {
    let metadata = file.metadata()?;
    if metadata_is_link(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem object is a symbolic link or Windows reparse point",
        ));
    }
    #[cfg(windows)]
    windows_file_identity(file)?;
    Ok(metadata)
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        os::unix::{
            ffi::{OsStrExt as _, OsStringExt as _},
            io::{AsRawFd as _, FromRawFd as _},
        },
        ptr,
    };

    fn fragment_c_string(fragment: &OsStr) -> io::Result<CString> {
        if fragment.is_empty()
            || Path::new(fragment)
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(invalid_path("filesystem fragment is not a single name"));
        }
        CString::new(fragment.as_bytes())
            .map_err(|_| invalid_path("filesystem fragment contains a NUL byte"))
    }

    fn open_at(
        parent: &fs::File,
        fragment: &OsStr,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<fs::File> {
        let fragment = fragment_c_string(fragment)?;
        // SAFETY: both descriptors and pointers are live for the duration of
        // the call, and mode is supplied for every call (openat ignores it
        // unless O_CREAT or O_TMPFILE is present).
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                fragment.as_ptr(),
                flags,
                mode as libc::c_uint,
            )
        };
        if descriptor < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: a successful openat returns one newly owned descriptor.
            Ok(unsafe { fs::File::from_raw_fd(descriptor) })
        }
    }

    pub(super) fn open_anchor(path: &Path) -> io::Result<fs::File> {
        fs::File::open(path)
    }

    pub(super) fn open_directory(
        parent: &fs::File,
        fragment: &OsStr,
        follow_links: bool,
    ) -> io::Result<fs::File> {
        let no_follow = if follow_links { 0 } else { libc::O_NOFOLLOW };
        open_at(
            parent,
            fragment,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | no_follow,
            0,
        )
    }

    pub(super) fn create_directory(parent: &fs::File, fragment: &OsStr) -> io::Result<fs::File> {
        let fragment_c = fragment_c_string(fragment)?;
        // SAFETY: parent and fragment are valid for the duration of mkdirat.
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), fragment_c.as_ptr(), 0o777) };
        if created != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        open_directory(parent, fragment, false)
    }

    pub(super) fn create_new_directory(
        parent: &fs::File,
        fragment: &OsStr,
    ) -> io::Result<fs::File> {
        let fragment_c = fragment_c_string(fragment)?;
        // SAFETY: parent and fragment are valid for the duration of mkdirat.
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), fragment_c.as_ptr(), 0o777) };
        if created != 0 {
            return Err(io::Error::last_os_error());
        }
        match open_directory(parent, fragment, false) {
            Ok(directory) => Ok(directory),
            Err(error) => {
                let _ = unsafe {
                    libc::unlinkat(parent.as_raw_fd(), fragment_c.as_ptr(), libc::AT_REMOVEDIR)
                };
                Err(error)
            }
        }
    }

    pub(super) fn open_directory_for_removal(
        parent: &fs::File,
        fragment: &OsStr,
    ) -> io::Result<fs::File> {
        open_directory(parent, fragment, false)
    }

    pub(super) fn open_entry(parent: &fs::File, fragment: &OsStr) -> io::Result<fs::File> {
        open_at(
            parent,
            fragment,
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )
    }

    pub(super) fn open_regular_file(parent: &fs::File, fragment: &OsStr) -> io::Result<fs::File> {
        open_entry(parent, fragment)
    }

    /// unlinkat operates on the directory, not the file handle, so removal
    /// needs no extra right here — this exists to match the Windows shape.
    pub(super) fn open_regular_file_for_removal(
        parent: &fs::File,
        fragment: &OsStr,
    ) -> io::Result<fs::File> {
        open_entry(parent, fragment)
    }

    pub(super) fn create_new_file(
        parent: &fs::File,
        fragment: &OsStr,
        mode: u32,
    ) -> io::Result<fs::File> {
        open_at(
            parent,
            fragment,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode as libc::mode_t,
        )
    }

    pub(super) fn replace_file(
        parent: &fs::File,
        _temporary: &fs::File,
        temporary_name: &OsStr,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        let temporary_name = fragment_c_string(temporary_name)?;
        let destination_name = fragment_c_string(destination_name)?;
        // SAFETY: both names and the directory descriptor remain live.
        let result = unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                temporary_name.as_ptr(),
                parent.as_raw_fd(),
                destination_name.as_ptr(),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn remove_file(
        parent: &fs::File,
        _file: &fs::File,
        fragment: &OsStr,
    ) -> io::Result<()> {
        let fragment = fragment_c_string(fragment)?;
        // SAFETY: parent and fragment remain live for unlinkat.
        let result = unsafe { libc::unlinkat(parent.as_raw_fd(), fragment.as_ptr(), 0) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn remove_directory(
        parent: &fs::File,
        _directory: &fs::File,
        fragment: &OsStr,
    ) -> io::Result<()> {
        let fragment = fragment_c_string(fragment)?;
        // SAFETY: parent and fragment remain live for unlinkat.
        let result =
            unsafe { libc::unlinkat(parent.as_raw_fd(), fragment.as_ptr(), libc::AT_REMOVEDIR) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    struct DirectoryStream(*mut libc::DIR);

    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            // SAFETY: fdopendir returned one owned DIR pointer.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe fn errno_pointer() -> *mut libc::c_int {
        // SAFETY: forwarded to the platform C runtime.
        unsafe { libc::__errno_location() }
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    unsafe fn errno_pointer() -> *mut libc::c_int {
        // SAFETY: forwarded to the platform C runtime.
        unsafe { libc::__error() }
    }

    pub(super) fn directory_entry_names(
        parent: &fs::File,
        limit: usize,
    ) -> io::Result<Vec<OsString>> {
        let dot = c".";
        // Open a fresh file description rather than dup(2): duplicated
        // directory descriptors share a stream offset, which would make a
        // second enumeration begin at the prior call's end.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: descriptor is an owned descriptor for a directory.
        let stream = unsafe { libc::fdopendir(descriptor) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: fdopendir did not consume descriptor on failure.
            unsafe {
                libc::close(descriptor);
            }
            return Err(error);
        }
        let stream = DirectoryStream(stream);
        let mut names = Vec::new();
        loop {
            // POSIX signals end-of-stream with null and unchanged errno.
            unsafe {
                *errno_pointer() = 0;
            }
            // SAFETY: the DIR pointer remains live and access is serialized by
            // this function.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let error_code = unsafe { *errno_pointer() };
                if error_code != 0 {
                    return Err(io::Error::from_raw_os_error(error_code));
                }
                break;
            }
            // SAFETY: d_name is NUL-terminated for a successful readdir.
            let bytes = unsafe { CStr::from_ptr(ptr::addr_of!((*entry).d_name).cast()) }.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            names.push(OsString::from_vec(bytes.to_vec()));
            if names.len() > limit {
                break;
            }
        }
        Ok(names)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ffi::c_void,
        os::windows::{
            ffi::{OsStrExt as _, OsStringExt as _},
            fs::OpenOptionsExt as _,
            io::{AsRawHandle as _, FromRawHandle as _},
        },
        ptr,
    };

    type Handle = *mut c_void;
    type NtStatus = i32;

    const OBJ_CASE_INSENSITIVE: u32 = 0x40;
    const FILE_READ_DATA: u32 = 0x0001;
    const FILE_LIST_DIRECTORY: u32 = 0x0001;
    const FILE_WRITE_DATA: u32 = 0x0002;
    const FILE_APPEND_DATA: u32 = 0x0004;
    const FILE_READ_EA: u32 = 0x0008;
    const FILE_WRITE_EA: u32 = 0x0010;
    const FILE_EXECUTE: u32 = 0x0020;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    const DELETE: u32 = 0x0001_0000;
    const READ_CONTROL: u32 = 0x0002_0000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_GENERIC_READ: u32 =
        READ_CONTROL | FILE_READ_DATA | FILE_READ_ATTRIBUTES | FILE_READ_EA | SYNCHRONIZE;
    const FILE_GENERIC_WRITE: u32 = READ_CONTROL
        | FILE_WRITE_DATA
        | FILE_WRITE_ATTRIBUTES
        | FILE_WRITE_EA
        | FILE_APPEND_DATA
        | SYNCHRONIZE;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const FILE_OPEN: u32 = 1;
    const FILE_CREATE: u32 = 2;
    const FILE_OPEN_IF: u32 = 3;
    const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    /// NT FileInformationClass for FILE_RENAME_INFORMATION. Note this is not
    /// the Win32 FILE_RENAME_INFO value (3) — the two enumerations differ, and
    /// the rename below goes through the NT entry point.
    const FILE_RENAME_INFORMATION_CLASS: u32 = 10;
    const FILE_DISPOSITION_INFO_CLASS: u32 = 4;
    const FILE_NAMES_INFORMATION_CLASS: u32 = 12;
    const STATUS_NO_MORE_FILES: NtStatus = 0x8000_0006_u32 as NtStatus;
    const STATUS_BUFFER_OVERFLOW: NtStatus = 0x8000_0005_u32 as NtStatus;
    const MAX_WINDOWS_COMPONENT_UNITS: usize = 255;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: Handle,
        object_name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }

    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: isize,
        information: usize,
    }

    #[repr(C)]
    pub(super) struct FileRenameInfoBuffer {
        pub(super) replace_if_exists: i32,
        pub(super) root_directory: Handle,
        pub(super) file_name_length: u32,
        pub(super) file_name: [u16; MAX_WINDOWS_COMPONENT_UNITS],
    }

    #[repr(C)]
    struct FileDispositionInfo {
        delete_file: u8,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtCreateFile(
            file_handle: *mut Handle,
            desired_access: u32,
            object_attributes: *mut ObjectAttributes,
            io_status_block: *mut IoStatusBlock,
            allocation_size: *mut i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *mut c_void,
            ea_length: u32,
        ) -> NtStatus;
        fn RtlNtStatusToDosError(status: NtStatus) -> u32;
        fn NtSetInformationFile(
            file_handle: Handle,
            io_status_block: *mut IoStatusBlock,
            file_information: *mut c_void,
            length: u32,
            file_information_class: u32,
        ) -> NtStatus;
        fn NtQueryDirectoryFile(
            file_handle: Handle,
            event: Handle,
            apc_routine: *mut c_void,
            apc_context: *mut c_void,
            io_status_block: *mut IoStatusBlock,
            file_information: *mut c_void,
            length: u32,
            file_information_class: u32,
            return_single_entry: u8,
            file_name: *mut UnicodeString,
            restart_scan: u8,
        ) -> NtStatus;
    }

    #[link(name = "Kernel32")]
    extern "system" {
        fn SetFileInformationByHandle(
            file: Handle,
            information_class: u32,
            information: *mut c_void,
            buffer_size: u32,
        ) -> i32;
    }

    fn fragment_wide(fragment: &OsStr) -> io::Result<Vec<u16>> {
        if fragment.is_empty()
            || Path::new(fragment)
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(invalid_path("filesystem fragment is not a single name"));
        }
        let wide = fragment.encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(invalid_path("filesystem fragment contains a NUL"));
        }
        if wide.len() > MAX_WINDOWS_COMPONENT_UNITS {
            return Err(invalid_path("filesystem fragment is too long"));
        }
        Ok(wide)
    }

    fn nt_error(status: NtStatus) -> io::Error {
        // SAFETY: RtlNtStatusToDosError accepts every NTSTATUS.
        let code = unsafe { RtlNtStatusToDosError(status) };
        io::Error::from_raw_os_error(code as i32)
    }

    fn open_relative(
        parent: &fs::File,
        fragment: &OsStr,
        desired_access: u32,
        file_attributes: u32,
        disposition: u32,
        options: u32,
    ) -> io::Result<fs::File> {
        let mut wide = fragment_wide(fragment)?;
        let byte_length = wide
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|length| u16::try_from(length).ok())
            .ok_or_else(|| invalid_path("filesystem fragment is too long"))?;
        let mut name = UnicodeString {
            length: byte_length,
            maximum_length: byte_length,
            buffer: wide.as_mut_ptr(),
        };
        let mut attributes = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: parent.as_raw_handle() as Handle,
            object_name: &mut name,
            attributes: OBJ_CASE_INSENSITIVE,
            security_descriptor: ptr::null_mut(),
            security_quality_of_service: ptr::null_mut(),
        };
        let mut io_status = IoStatusBlock {
            status_or_pointer: 0,
            information: 0,
        };
        let mut handle = ptr::null_mut();
        // SAFETY: all ABI structures are repr(C), pointers remain live for the
        // call, and NtCreateFile initializes one owned handle on success.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                desired_access,
                &mut attributes,
                &mut io_status,
                ptr::null_mut(),
                file_attributes,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                disposition,
                options,
                ptr::null_mut(),
                0,
            )
        };
        if status < 0 {
            return Err(nt_error(status));
        }
        if handle.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "NtCreateFile returned an empty handle",
            ));
        }
        // SAFETY: a successful NtCreateFile returned one newly owned handle.
        Ok(unsafe { fs::File::from_raw_handle(handle) })
    }

    pub(super) fn open_anchor(path: &Path) -> io::Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .access_mode(FILE_LIST_DIRECTORY | FILE_EXECUTE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        options.open(path)
    }

    pub(super) fn open_directory(
        parent: &fs::File,
        fragment: &OsStr,
        follow_links: bool,
    ) -> io::Result<fs::File> {
        let reparse = if follow_links {
            0
        } else {
            FILE_OPEN_REPARSE_POINT
        };
        open_relative(
            parent,
            fragment,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | FILE_EXECUTE | SYNCHRONIZE,
            FILE_ATTRIBUTE_DIRECTORY,
            FILE_OPEN,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | reparse,
        )
    }

    pub(super) fn create_directory(parent: &fs::File, fragment: &OsStr) -> io::Result<fs::File> {
        open_relative(
            parent,
            fragment,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | FILE_EXECUTE | SYNCHRONIZE,
            FILE_ATTRIBUTE_DIRECTORY,
            FILE_OPEN_IF,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
        )
    }

    pub(super) fn create_new_directory(
        parent: &fs::File,
        fragment: &OsStr,
    ) -> io::Result<fs::File> {
        open_relative(
            parent,
            fragment,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | FILE_EXECUTE | SYNCHRONIZE,
            FILE_ATTRIBUTE_DIRECTORY,
            FILE_CREATE,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
        )
    }

    pub(super) fn open_directory_for_removal(
        parent: &fs::File,
        fragment: &OsStr,
    ) -> io::Result<fs::File> {
        open_relative(
            parent,
            fragment,
            DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_ATTRIBUTE_DIRECTORY,
            FILE_OPEN,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
        )
    }

    pub(super) fn open_entry(parent: &fs::File, fragment: &OsStr) -> io::Result<fs::File> {
        open_relative(
            parent,
            fragment,
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_ATTRIBUTE_NORMAL,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
        )
    }

    pub(super) fn open_regular_file(parent: &fs::File, fragment: &OsStr) -> io::Result<fs::File> {
        open_relative(
            parent,
            fragment,
            FILE_GENERIC_READ,
            FILE_ATTRIBUTE_NORMAL,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        )
    }

    /// Deletion here marks the open handle via FILE_DISPOSITION_INFO rather
    /// than unlinking a path, and that requires DELETE on the handle itself.
    /// Opening with FILE_GENERIC_READ alone fails the disposition call with
    /// ERROR_ACCESS_DENIED, so removal gets its own opener instead of widening
    /// the read path.
    pub(super) fn open_regular_file_for_removal(
        parent: &fs::File,
        fragment: &OsStr,
    ) -> io::Result<fs::File> {
        open_relative(
            parent,
            fragment,
            FILE_GENERIC_READ | DELETE,
            FILE_ATTRIBUTE_NORMAL,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        )
    }

    pub(super) fn create_new_file(
        parent: &fs::File,
        fragment: &OsStr,
        _mode: u32,
    ) -> io::Result<fs::File> {
        open_relative(
            parent,
            fragment,
            // FILE_READ_ATTRIBUTES is required, not incidental: every caller
            // hands the new handle straight to reject_linked_handle, which
            // queries it with GetFileInformationByHandle to prove the object
            // is not a reparse point. Win32's FILE_GENERIC_WRITE does not
            // include that right (see its definition above), so without this
            // the query fails ERROR_ACCESS_DENIED and the create appears to
            // fail even though the file was made — leaving an orphaned
            // zero-byte temp behind, because the error returns before the
            // caller's cleanup path.
            FILE_GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES,
            FILE_ATTRIBUTE_NORMAL,
            FILE_CREATE,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        )
    }

    pub(super) fn replace_file(
        parent: &fs::File,
        temporary: &fs::File,
        _temporary_name: &OsStr,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        let destination = fragment_wide(destination_name)?;
        let mut information = FileRenameInfoBuffer {
            replace_if_exists: 1,
            root_directory: parent.as_raw_handle() as Handle,
            file_name_length: (destination.len() * std::mem::size_of::<u16>()) as u32,
            file_name: [0; MAX_WINDOWS_COMPONENT_UNITS],
        };
        information.file_name[..destination.len()].copy_from_slice(&destination);
        let header_size = std::mem::offset_of!(FileRenameInfoBuffer, file_name);
        let buffer_size = header_size + destination.len() * std::mem::size_of::<u16>();
        // Renaming through the NT entry point rather than Win32's
        // SetFileInformationByHandle is what makes RootDirectory mean what
        // this code needs. A directory HANDLE in RootDirectory is an NT-layer
        // concept; the Win32 wrapper does not resolve names against it, so it
        // parsed our bare component as a path and failed ERROR_INVALID_NAME
        // (123) — every atomic write died at the rename, leaving the
        // zero-byte temp behind and the real file never created.
        //
        // Keeping the rename directory-relative is deliberate: every other
        // operation here resolves against an owned parent handle so a
        // concurrently swapped path component cannot redirect the write. The
        // alternative Win32 fix — passing a fully-qualified destination —
        // would reintroduce exactly the path re-resolution this module avoids.
        let mut io_status = IoStatusBlock {
            status_or_pointer: 0,
            information: 0,
        };
        // SAFETY: the buffer has the documented FILE_RENAME_INFORMATION
        // layout (identical to FILE_RENAME_INFO) and contains buffer_size
        // initialized bytes; both handles remain live across the call.
        let status = unsafe {
            NtSetInformationFile(
                temporary.as_raw_handle() as Handle,
                &mut io_status as *mut IoStatusBlock,
                (&mut information as *mut FileRenameInfoBuffer).cast(),
                buffer_size as u32,
                FILE_RENAME_INFORMATION_CLASS,
            )
        };
        if status < 0 {
            Err(nt_error(status))
        } else {
            Ok(())
        }
    }

    pub(super) fn remove_file(
        _parent: &fs::File,
        file: &fs::File,
        _fragment: &OsStr,
    ) -> io::Result<()> {
        let mut information = FileDispositionInfo { delete_file: 1 };
        // SAFETY: information has the documented FILE_DISPOSITION_INFO layout.
        let result = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as Handle,
                FILE_DISPOSITION_INFO_CLASS,
                (&mut information as *mut FileDispositionInfo).cast(),
                std::mem::size_of::<FileDispositionInfo>() as u32,
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn remove_directory(
        _parent: &fs::File,
        directory: &fs::File,
        _fragment: &OsStr,
    ) -> io::Result<()> {
        remove_file(_parent, directory, _fragment)
    }

    pub(super) fn directory_entry_names(
        parent: &fs::File,
        limit: usize,
    ) -> io::Result<Vec<OsString>> {
        const BUFFER_BYTES: usize = 64 * 1024;
        const FILE_NAME_OFFSET: usize = 12;

        let mut buffer = vec![0_u8; BUFFER_BYTES];
        let mut restart = 1_u8;
        let mut names = Vec::new();
        loop {
            let mut io_status = IoStatusBlock {
                status_or_pointer: 0,
                information: 0,
            };
            // SAFETY: the handle and writable buffer remain live for the
            // synchronous call, and the information class uses the documented
            // FILE_NAMES_INFORMATION layout.
            let status = unsafe {
                NtQueryDirectoryFile(
                    parent.as_raw_handle() as Handle,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut io_status,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    FILE_NAMES_INFORMATION_CLASS,
                    0,
                    ptr::null_mut(),
                    restart,
                )
            };
            restart = 0;
            if status == STATUS_NO_MORE_FILES {
                break;
            }
            if status < 0 && status != STATUS_BUFFER_OVERFLOW {
                return Err(nt_error(status));
            }
            let used = io_status.information.min(buffer.len());
            if used == 0 {
                if status == STATUS_BUFFER_OVERFLOW {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory entry exceeds the query buffer",
                    ));
                }
                break;
            }

            let mut offset = 0_usize;
            loop {
                let header_end = offset.checked_add(FILE_NAME_OFFSET).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid directory entry offset")
                })?;
                if header_end > used {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated FILE_NAMES_INFORMATION entry",
                    ));
                }
                let next =
                    u32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
                let name_bytes =
                    u32::from_ne_bytes(buffer[offset + 8..offset + 12].try_into().unwrap())
                        as usize;
                if name_bytes % std::mem::size_of::<u16>() != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "odd Windows directory name byte length",
                    ));
                }
                let name_end = header_end.checked_add(name_bytes).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid directory name length")
                })?;
                if name_end > used {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated Windows directory name",
                    ));
                }
                // FILE_NAMES_INFORMATION aligns the UTF-16 name to two bytes.
                let units = buffer[header_end..name_end]
                    .chunks_exact(2)
                    .map(|bytes| u16::from_ne_bytes([bytes[0], bytes[1]]))
                    .collect::<Vec<_>>();
                let name = OsString::from_wide(&units);
                if name != OsStr::new(".") && name != OsStr::new("..") {
                    names.push(name);
                    if names.len() > limit {
                        return Ok(names);
                    }
                }
                if next == 0 {
                    break;
                }
                if next < FILE_NAME_OFFSET || offset.checked_add(next).is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid next Windows directory entry offset",
                    ));
                }
                offset += next;
                if offset >= used {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Windows directory entry offset exceeds returned bytes",
                    ));
                }
            }
        }
        Ok(names)
    }

    #[cfg(test)]
    pub(super) fn assert_native_abi_layouts() {
        use std::mem::{align_of, offset_of, size_of};

        let pointer = size_of::<Handle>();
        assert_eq!(offset_of!(UnicodeString, length), 0);
        assert_eq!(offset_of!(UnicodeString, maximum_length), 2);
        assert_eq!(
            offset_of!(UnicodeString, buffer),
            if pointer == 8 { 8 } else { 4 }
        );
        assert_eq!(
            size_of::<UnicodeString>(),
            if pointer == 8 { 16 } else { 8 }
        );

        assert_eq!(offset_of!(ObjectAttributes, length), 0);
        assert_eq!(
            offset_of!(ObjectAttributes, root_directory),
            if pointer == 8 { 8 } else { 4 }
        );
        assert_eq!(
            offset_of!(ObjectAttributes, object_name),
            if pointer == 8 { 16 } else { 8 }
        );
        assert_eq!(
            offset_of!(ObjectAttributes, attributes),
            if pointer == 8 { 24 } else { 12 }
        );
        assert_eq!(
            size_of::<ObjectAttributes>(),
            if pointer == 8 { 48 } else { 24 }
        );
        assert_eq!(align_of::<ObjectAttributes>(), pointer);

        assert_eq!(offset_of!(IoStatusBlock, status_or_pointer), 0);
        assert_eq!(offset_of!(IoStatusBlock, information), pointer);
        assert_eq!(size_of::<IoStatusBlock>(), pointer * 2);

        assert_eq!(offset_of!(FileRenameInfoBuffer, replace_if_exists), 0);
        assert_eq!(
            offset_of!(FileRenameInfoBuffer, root_directory),
            if pointer == 8 { 8 } else { 4 }
        );
        assert_eq!(
            offset_of!(FileRenameInfoBuffer, file_name_length),
            if pointer == 8 { 16 } else { 8 }
        );
        assert_eq!(
            offset_of!(FileRenameInfoBuffer, file_name),
            if pointer == 8 { 20 } else { 12 }
        );
        assert_eq!(size_of::<FileDispositionInfo>(), 1);
    }
}

fn split_absolute_path(path: &Path) -> io::Result<(PathBuf, Vec<OsString>)> {
    if !path.is_absolute() {
        return Err(invalid_path(format!(
            "{} is not an absolute path",
            display_path(path)
        )));
    }
    let mut anchor = PathBuf::new();
    let mut fragments = Vec::new();
    let mut rooted = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) => {
                if rooted || !fragments.is_empty() || !anchor.as_os_str().is_empty() {
                    return Err(invalid_path("path contains an unexpected prefix"));
                }
                anchor.push(component.as_os_str());
            }
            Component::RootDir => {
                anchor.push(component.as_os_str());
                rooted = true;
            }
            Component::Normal(fragment) => fragments.push(fragment.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid_path(format!(
                    "{} contains a parent component",
                    display_path(path)
                )))
            }
        }
    }
    if !rooted {
        return Err(invalid_path(format!(
            "{} does not have a filesystem root",
            display_path(path)
        )));
    }
    Ok((anchor, fragments))
}

fn normalized_absolute_path(path: &Path) -> io::Result<PathBuf> {
    let (mut normalized, fragments) = split_absolute_path(path)?;
    normalized.extend(fragments);
    Ok(normalized)
}

fn open_canonical_ambient_parent(
    path: &Path,
    create_missing: bool,
) -> io::Result<(PhysicalDirectory, PathBuf, OsString)> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid_path("filesystem root has no parent entry"))?
        .to_os_string();
    let mut cursor = path
        .parent()
        .ok_or_else(|| invalid_path("filesystem target has no parent"))?;
    let mut missing = Vec::new();
    let (mut current, mut canonical_parent) = loop {
        match open_absolute_directory(cursor, false) {
            Ok(original) => {
                let original_identity = original.identity()?;
                let canonical = cursor.canonicalize()?;
                let canonical = normalized_absolute_path(&canonical)?;
                let physical = open_absolute_directory(&canonical, true)?;
                if original_identity != physical.identity()? {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{} changed while its physical directory was being resolved",
                            display_path(cursor)
                        ),
                    ));
                }
                break (physical, canonical);
            }
            Err(error) if create_missing && error.kind() == io::ErrorKind::NotFound => {
                let fragment = cursor
                    .file_name()
                    .ok_or_else(|| invalid_path("could not find an existing parent directory"))?;
                missing.push(fragment.to_os_string());
                cursor = cursor
                    .parent()
                    .ok_or_else(|| invalid_path("could not find an existing parent directory"))?;
            }
            Err(error) => return Err(error),
        }
    };

    for fragment in missing.into_iter().rev() {
        canonical_parent.push(&fragment);
        current = match current.open_directory(&fragment, false) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                current.create_directory(&fragment)?
            }
            Err(error) => return Err(error),
        };
    }
    Ok((current, canonical_parent, name))
}

fn open_absolute_directory(path: &Path, strict_links: bool) -> io::Result<PhysicalDirectory> {
    let (anchor, fragments) = split_absolute_path(path)?;
    let mut current = PhysicalDirectory::from_file(platform::open_anchor(&anchor)?)?;
    for fragment in fragments {
        current = current.open_directory(&fragment, !strict_links)?;
    }
    Ok(current)
}

fn open_absolute_parent(
    path: &Path,
    create_missing: bool,
    strict_links: bool,
) -> io::Result<(PhysicalDirectory, OsString)> {
    let (anchor, mut fragments) = split_absolute_path(path)?;
    let name = fragments
        .pop()
        .ok_or_else(|| invalid_path("filesystem root has no parent entry"))?;
    let mut current = PhysicalDirectory::from_file(platform::open_anchor(&anchor)?)?;
    for fragment in fragments {
        current = match current.open_directory(&fragment, !strict_links) {
            Ok(directory) => directory,
            Err(error) if create_missing && error.kind() == io::ErrorKind::NotFound => {
                current.create_directory(&fragment)?
            }
            Err(error) => return Err(error),
        };
    }
    Ok((current, name))
}

fn open_absolute_entry(path: &Path, strict_links: bool) -> io::Result<fs::File> {
    let (anchor, fragments) = split_absolute_path(path)?;
    if fragments.is_empty() {
        return PhysicalDirectory::from_file(platform::open_anchor(&anchor)?)?.try_clone_file();
    }
    let (parent, name) = open_absolute_parent(path, false, strict_links)?;
    parent.open_entry(&name)
}

fn canonicalize_existing_physical_path(path: &Path) -> Result<(PathBuf, Metadata), String> {
    let normalized = normalized_absolute_path(path)
        .map_err(|error| format!("could not resolve {}: {error}", display_path(path)))?;
    let file = open_absolute_entry(&normalized, true)
        .map_err(|error| format!("could not resolve {}: {error}", display_path(path)))?;
    let metadata = reject_linked_handle(&file)
        .map_err(|error| format!("could not inspect {}: {error}", display_path(path)))?;
    Ok((normalized, metadata))
}

fn canonicalize_selected_directory(path: &Path) -> Result<PathBuf, String> {
    let original = open_absolute_entry(path, false)
        .map_err(|error| format!("could not inspect {}: {error}", display_path(path)))?;
    let original_metadata = reject_linked_handle(&original)
        .map_err(|error| format!("could not inspect {}: {error}", display_path(path)))?;
    if !original_metadata.is_dir() {
        return Err(format!("{} is not a physical folder", display_path(path)));
    }
    let original_identity = physical_identity(&original, &original_metadata)
        .map_err(|error| format!("could not identify {}: {error}", display_path(path)))?;

    // File pickers can return paths containing stable platform links such as
    // macOS /var -> /private/var. Resolve that ambient spelling once, then
    // require the persisted spelling itself to have a fully physical chain.
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("could not resolve {}: {error}", display_path(path)))?;
    let (canonical, canonical_metadata) = canonicalize_existing_physical_path(&canonical)?;
    let canonical_file = open_absolute_entry(&canonical, true).map_err(|error| {
        format!(
            "could not open canonical folder {}: {error}",
            display_path(&canonical)
        )
    })?;
    let canonical_identity =
        physical_identity(&canonical_file, &canonical_metadata).map_err(|error| {
            format!(
                "could not identify canonical folder {}: {error}",
                display_path(&canonical)
            )
        })?;

    let original_after = open_absolute_entry(path, false)
        .map_err(|error| format!("could not re-open {}: {error}", display_path(path)))?;
    let original_after_metadata = reject_linked_handle(&original_after)
        .map_err(|error| format!("could not re-inspect {}: {error}", display_path(path)))?;
    let original_after_identity = physical_identity(&original_after, &original_after_metadata)
        .map_err(|error| format!("could not re-identify {}: {error}", display_path(path)))?;
    if original_identity != canonical_identity || original_identity != original_after_identity {
        return Err(format!(
            "{} changed while it was being resolved",
            display_path(path)
        ));
    }
    Ok(canonical)
}

pub(crate) fn canonicalize_physical_directory(path: &Path) -> Result<PathBuf, String> {
    let (canonical, metadata) = canonicalize_existing_physical_path(path)?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a physical folder", display_path(path)));
    }
    Ok(canonical)
}

#[derive(Debug)]
pub(crate) struct PhysicalDirectoryCapability {
    path: PathBuf,
    directory: PhysicalDirectory,
}

#[cfg(test)]
pub(crate) fn open_physical_directory(path: &Path) -> Result<PhysicalDirectoryCapability, String> {
    let path = normalized_absolute_path(path)
        .map_err(|error| format!("could not resolve {}: {error}", display_path(path)))?;
    let directory = open_absolute_directory(&path, true)
        .map_err(|error| format!("could not open {}: {error}", display_path(&path)))?;
    Ok(PhysicalDirectoryCapability { path, directory })
}

impl PhysicalDirectoryCapability {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn entry_names(&self, limit: usize) -> Result<Vec<OsString>, String> {
        let names = self.directory.entry_names(limit).map_err(|error| {
            format!(
                "could not inspect physical folder {}: {error}",
                display_path(&self.path)
            )
        })?;
        if names.len() > limit {
            return Err(format!(
                "{} contains too many direct children",
                display_path(&self.path)
            ));
        }
        Ok(names)
    }

    pub(crate) fn optional_child_directory(&self, name: &OsStr) -> Result<Option<Self>, String> {
        let path = self.path.join(name);
        match self.directory.open_directory(name, false) {
            Ok(directory) => Ok(Some(Self { path, directory })),
            Err(directory_error) => match self.directory.open_entry(name) {
                Ok(file) => {
                    let metadata = file.metadata().map_err(|error| {
                        format!(
                            "could not inspect physical child {}: {error}",
                            display_path(&path)
                        )
                    })?;
                    if metadata.is_dir() {
                        Err(format!(
                            "could not open physical folder {}: {directory_error}",
                            display_path(&path)
                        ))
                    } else {
                        Ok(None)
                    }
                }
                Err(error) => Err(format!(
                    "could not inspect physical child {}: {error}",
                    display_path(&path)
                )),
            },
        }
    }

    pub(crate) fn create_child_directory(&self, name: &OsStr) -> Result<Option<Self>, String> {
        let path = self.path.join(name);
        match self.directory.create_new_directory(name) {
            Ok(directory) => Ok(Some(Self { path, directory })),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(format!(
                "could not create physical folder {}: {error}",
                display_path(&path)
            )),
        }
    }

    pub(crate) fn read_optional_utf8(
        &self,
        name: &OsStr,
        limit: u64,
    ) -> Result<Option<String>, String> {
        let path = self.path.join(name);
        match self.directory.open_regular_file(name) {
            Ok(file) => read_utf8_from_open_file(file, &path, limit).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("could not open {}: {error}", display_path(&path))),
        }
    }

    pub(crate) fn atomic_write(&self, name: &OsStr, bytes: &[u8], mode: u32) -> Result<(), String> {
        let path = self.path.join(name);
        match self.directory.open_entry(name) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not validate destination {}: {error}",
                    display_path(&path)
                ))
            }
        }
        atomic_write_in_parent(&self.directory, name, &path, bytes, mode)
    }

    pub(crate) fn remove_child_directory_if_matches(
        &self,
        name: &OsStr,
        expected: &Self,
    ) -> Result<(), String> {
        let path = self.path.join(name);
        let current = self
            .directory
            .open_directory(name, false)
            .map_err(|error| {
                format!(
                    "could not re-open physical folder {}: {error}",
                    display_path(&path)
                )
            })?;
        let current_identity = current.identity().map_err(|error| {
            format!(
                "could not identify physical folder {}: {error}",
                display_path(&path)
            )
        })?;
        let expected_identity = expected.directory.identity().map_err(|error| {
            format!(
                "could not identify physical folder {}: {error}",
                display_path(expected.path())
            )
        })?;
        if current_identity != expected_identity {
            return Err(format!(
                "physical folder {} changed before cleanup",
                display_path(&path)
            ));
        }
        let removal = self
            .directory
            .open_directory_for_removal(name)
            .map_err(|error| {
                format!(
                    "could not open physical folder {} for cleanup: {error}",
                    display_path(&path)
                )
            })?;
        let removal_identity = removal.identity().map_err(|error| {
            format!(
                "could not identify cleanup handle for physical folder {}: {error}",
                display_path(&path)
            )
        })?;
        if removal_identity != current_identity || removal_identity != expected_identity {
            return Err(format!(
                "physical folder {} changed before cleanup",
                display_path(&path)
            ));
        }
        self.directory
            .remove_open_directory(&removal, name)
            .map_err(|error| {
                format!(
                    "could not remove physical folder {}: {error}",
                    display_path(&path)
                )
            })
    }
}

pub(crate) fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > 64 {
        return Err("key must contain between 1 and 64 characters".into());
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("key may contain only letters, numbers, dots, underscores, and hyphens".into());
    }
    Ok(())
}

pub(crate) fn state_get(path: &Path, key: &str) -> Result<Option<Value>, String> {
    validate_key(key)?;
    if key == "secrets" {
        return Err("secrets are available only through the secure secret commands".into());
    }
    Ok(read_json_object(path, MAX_STATE_BYTES)?.remove(key))
}

pub(crate) fn state_set(path: &Path, key: &str, value: Value) -> Result<(), String> {
    validate_key(key)?;
    if key == "secrets" {
        return Err("secrets are available only through the secure secret commands".into());
    }
    let mut object = read_json_object(path, MAX_STATE_BYTES)?;
    object.insert(key.to_owned(), value);
    write_json_object(path, &object, 0o600)
}

pub(crate) fn read_json_object(path: &Path, limit: u64) -> Result<Map<String, Value>, String> {
    let Some(bytes) = read_optional_file_bytes(path, limit)? else {
        return Ok(Map::new());
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} contains invalid JSON: {error}", display_path(path)))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("{} must contain a JSON object", display_path(path)))
}

pub(crate) fn write_json_object(
    path: &Path,
    object: &Map<String, Value>,
    mode: u32,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(object)
        .map_err(|error| format!("could not encode state: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("state exceeds the size limit".into());
    }
    atomic_write(path, &bytes, mode)
}

pub(crate) fn validate_project_file_path(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err("project file path must be absolute".into());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err("project file path must not contain . or .. components".into());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "project file path must have a UTF-8 file name".to_string())?;
    if !matches!(name, "ro-sync.json" | "wally.toml") {
        return Err("only ro-sync.json and wally.toml project files are allowed".into());
    }
    Ok(path)
}

pub(crate) fn authorize_project_root(store: &Path, root: &Path) -> Result<PathBuf, String> {
    let root = canonicalize_selected_directory(root)?;
    let mut roots = read_authorized_roots(store)?;
    if !roots.contains(&root) {
        if roots.len() >= 256 {
            return Err("authorized project root limit reached".into());
        }
        roots.push(root.clone());
        roots.sort();
        let mut bytes = serde_json::to_vec_pretty(
            &roots
                .iter()
                .map(|path| display_path(path))
                .collect::<Vec<_>>(),
        )
        .map_err(|error| format!("could not encode authorized project roots: {error}"))?;
        bytes.push(b'\n');
        atomic_write(store, &bytes, 0o600)?;
    }
    Ok(root)
}

pub(crate) fn ensure_authorized_path(store: &Path, path: &Path) -> Result<(), String> {
    resolve_authorized_path(store, path).map(|_| ())
}

fn safe_relative_fragments(path: &Path, relative: &Path) -> Result<Vec<OsString>, String> {
    relative
        .components()
        .map(|component| match component {
            Component::Normal(fragment) => Ok(fragment.to_os_string()),
            _ => Err(format!(
                "{} contains an unsafe path component",
                display_path(path)
            )),
        })
        .collect()
}

fn authorized_root_for_path(
    store: &Path,
    path: &Path,
) -> Result<(PhysicalDirectory, PathBuf, Vec<OsString>), String> {
    for stored_root in read_authorized_roots(store)? {
        let Ok(relative) = path.strip_prefix(&stored_root) else {
            continue;
        };
        let root = normalized_absolute_path(&stored_root).map_err(|error| {
            format!(
                "could not validate authorized project root {}: {error}",
                display_path(&stored_root)
            )
        })?;
        let directory = open_absolute_directory(&root, true).map_err(|error| {
            format!(
                "could not open authorized project root {}: {error}",
                display_path(&root)
            )
        })?;
        return Ok((directory, root, safe_relative_fragments(path, relative)?));
    }
    Err(format!(
        "{} is outside the project folders explicitly selected in Ro Sync",
        display_path(path)
    ))
}

pub(crate) fn open_authorized_directory(
    store: &Path,
    path: &Path,
) -> Result<PhysicalDirectoryCapability, String> {
    let (mut directory, mut resolved, fragments) = authorized_root_for_path(store, path)?;
    for fragment in fragments {
        resolved.push(&fragment);
        directory = directory
            .open_directory(&fragment, false)
            .map_err(|error| {
                format!(
                    "could not open authorized folder {}: {error}",
                    display_path(&resolved)
                )
            })?;
    }
    Ok(PhysicalDirectoryCapability {
        path: resolved,
        directory,
    })
}

fn authorized_parent_for_path(
    store: &Path,
    path: &Path,
    create_missing: bool,
) -> Result<(PhysicalDirectory, PathBuf, OsString), String> {
    let (mut current, root, mut fragments) = authorized_root_for_path(store, path)?;
    let name = fragments.pop().ok_or_else(|| {
        format!(
            "{} names an authorized folder rather than a file",
            display_path(path)
        )
    })?;
    let mut current_path = root;
    for fragment in fragments {
        current_path.push(&fragment);
        current = match current.open_directory(&fragment, false) {
            Ok(directory) => directory,
            Err(error) if create_missing && error.kind() == io::ErrorKind::NotFound => {
                current.create_directory(&fragment).map_err(|error| {
                    format!(
                        "could not create authorized folder {}: {error}",
                        display_path(&current_path)
                    )
                })?
            }
            Err(error) => {
                return Err(format!(
                    "could not open authorized folder {}: {error}",
                    display_path(&current_path)
                ))
            }
        };
    }
    Ok((current, current_path, name))
}

pub(crate) fn resolve_authorized_path(store: &Path, path: &Path) -> Result<PathBuf, String> {
    let (mut current, mut resolved, fragments) = authorized_root_for_path(store, path)?;
    let last = fragments.len().saturating_sub(1);
    for (index, fragment) in fragments.iter().enumerate() {
        resolved.push(fragment);
        if index == last {
            match current.open_entry(fragment) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "could not validate authorized path {}: {error}",
                        display_path(&resolved)
                    ))
                }
            }
        } else {
            match current.open_directory(fragment, false) {
                Ok(directory) => current = directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    for trailing in fragments.iter().skip(index + 1) {
                        resolved.push(trailing);
                    }
                    return Ok(resolved);
                }
                Err(error) => {
                    return Err(format!(
                        "could not validate authorized path {}: {error}",
                        display_path(&resolved)
                    ))
                }
            }
        }
    }
    Ok(resolved)
}

fn read_authorized_roots(store: &Path) -> Result<Vec<PathBuf>, String> {
    let Some(bytes) = read_optional_file_bytes(store, MAX_STATE_BYTES)? else {
        return Ok(Vec::new());
    };
    let roots: Vec<String> = serde_json::from_slice(&bytes)
        .map_err(|error| format!("authorized project root store is invalid: {error}"))?;
    Ok(roots.into_iter().map(PathBuf::from).collect())
}

fn read_utf8_from_parent(
    parent: &PhysicalDirectory,
    name: &OsStr,
    path: &Path,
    limit: u64,
) -> Result<String, String> {
    let file = parent
        .open_regular_file(name)
        .map_err(|error| format!("could not open {}: {error}", display_path(path)))?;
    read_utf8_from_open_file(file, path, limit)
}

fn read_utf8_from_open_file(file: fs::File, path: &Path, limit: u64) -> Result<String, String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", display_path(path)))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", display_path(path)));
    }
    if metadata.len() > limit {
        return Err(format!("{} exceeds the size limit", display_path(path)));
    }
    let capacity = usize::try_from(metadata.len().min(limit)).unwrap_or(usize::MAX);
    let mut text = String::with_capacity(capacity);
    file.take(limit.saturating_add(1))
        .read_to_string(&mut text)
        .map_err(|error| format!("could not read {} as UTF-8: {error}", display_path(path)))?;
    if text.len() as u64 > limit {
        return Err(format!("{} exceeds the size limit", display_path(path)));
    }
    Ok(text)
}

pub(crate) fn read_utf8_file(path: &Path, limit: u64) -> Result<String, String> {
    let (parent, _, name) = open_canonical_ambient_parent(path, false)
        .map_err(|error| format!("could not resolve {}: {error}", display_path(path)))?;
    read_utf8_from_parent(&parent, &name, path, limit)
}

pub(crate) fn read_authorized_utf8_file(
    store: &Path,
    path: &Path,
    limit: u64,
) -> Result<String, String> {
    let (parent, _, name) = authorized_parent_for_path(store, path, false)?;
    read_utf8_from_parent(&parent, &name, path, limit)
}

fn read_optional_file_bytes(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    let (parent, _, name) = match open_canonical_ambient_parent(path, false) {
        Ok(parent) => parent,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not resolve {}: {error}", display_path(path))),
    };
    let mut file = match parent.open_regular_file(&name) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not open {}: {error}", display_path(path))),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", display_path(path)))?;
    if metadata.len() > limit {
        return Err(format!("{} exceeds the size limit", display_path(path)));
    }
    let capacity = usize::try_from(metadata.len().min(limit)).unwrap_or(usize::MAX);
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {}: {error}", display_path(path)))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{} exceeds the size limit", display_path(path)));
    }
    Ok(Some(bytes))
}

fn temporary_name(name: &OsStr) -> OsString {
    let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".tmp-{}-{serial}", std::process::id()));
    temporary
}

fn atomic_write_in_parent(
    parent: &PhysicalDirectory,
    name: &OsStr,
    path: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<(), String> {
    let temporary = temporary_name(name);
    let mut file = parent.create_new_file(&temporary, mode).map_err(|error| {
        format!(
            "could not create temporary file for {}: {error}",
            display_path(path)
        )
    })?;

    let result = (|| -> Result<(), String> {
        file.write_all(bytes).map_err(|error| {
            format!(
                "could not write temporary file for {}: {error}",
                display_path(path)
            )
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(mode))
                .map_err(|error| {
                    format!(
                        "could not secure temporary file for {}: {error}",
                        display_path(path)
                    )
                })?;
        }

        file.sync_all()
            .map_err(|error| format!("could not flush {}: {error}", display_path(path)))?;

        parent
            .replace_with_open_file(&file, &temporary, name)
            .map_err(|error| {
                format!(
                    "could not replace {} with temporary file: {error}",
                    display_path(path)
                )
            })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = parent.remove_open_file(&file, &temporary);
    }
    result
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let (parent, _, name) = open_canonical_ambient_parent(path, true)
        .map_err(|error| format!("could not resolve {}: {error}", display_path(path)))?;
    atomic_write_in_parent(&parent, &name, path, bytes, mode)
}

pub(crate) fn atomic_write_authorized(
    store: &Path,
    path: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<(), String> {
    let (parent, _, name) = authorized_parent_for_path(store, path, true)?;
    match parent.open_entry(&name) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not validate authorized destination {}: {error}",
                display_path(path)
            ))
        }
    }
    atomic_write_in_parent(&parent, &name, path, bytes, mode)
}

pub(crate) fn remove_authorized_regular_file(store: &Path, path: &Path) -> Result<bool, String> {
    let (parent, _, name) = authorized_parent_for_path(store, path, false)?;
    let file = match parent.open_regular_file_for_removal(&name) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "could not open {} for deletion: {error}",
                display_path(path)
            ))
        }
    };
    parent
        .remove_open_file(&file, &name)
        .map_err(|error| format!("could not delete {}: {error}", display_path(path)))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_test_directory(directory: &tempfile::TempDir) -> PathBuf {
        directory.path().canonicalize().unwrap()
    }

    #[test]
    fn keys_are_narrowly_allowlisted() {
        assert!(validate_key("projects.v2").is_ok());
        assert!(validate_key("../../secrets").is_err());
        assert!(validate_key("").is_err());
    }

    #[test]
    fn project_files_are_allowlisted() {
        let root = if cfg!(windows) {
            "C:\\project"
        } else {
            "/project"
        };
        assert!(validate_project_file_path(&format!("{root}/ro-sync.json")).is_ok());
        assert!(validate_project_file_path(&format!("{root}/wally.toml")).is_ok());
        assert!(validate_project_file_path(&format!("{root}/notes.txt")).is_err());
        assert!(validate_project_file_path("relative/ro-sync.json").is_err());
    }

    #[test]
    fn state_round_trips_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        state_set(&path, "projects", serde_json::json!([{"id": 1}])).unwrap();
        state_set(&path, "projects", serde_json::json!([{"id": 2}])).unwrap();
        assert_eq!(
            state_get(&path, "projects").unwrap(),
            Some(serde_json::json!([{"id": 2}]))
        );
        assert!(state_set(&path, "secrets", serde_json::json!({})).is_err());
    }

    #[test]
    fn authorization_restricts_paths_to_picked_roots() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let store = directory.path().join("roots.json");
        let project = authorize_project_root(&store, &project).unwrap();
        assert!(ensure_authorized_path(&store, &project.join("ro-sync.json")).is_ok());
        assert!(
            ensure_authorized_path(&store, &directory.path().join("other/wally.toml")).is_err()
        );
    }

    #[test]
    fn authorized_regular_file_removal_is_non_recursive_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let store = directory.path().join("roots.json");
        let project = authorize_project_root(&store, &project).unwrap();
        let marker = project.join("init.luau");
        fs::write(&marker, "return true").unwrap();

        assert!(remove_authorized_regular_file(&store, &marker).unwrap());
        assert!(!marker.exists());
        assert!(!remove_authorized_regular_file(&store, &marker).unwrap());
        assert!(remove_authorized_regular_file(&store, &project).is_err());
    }

    #[test]
    fn matching_child_cleanup_removes_the_expected_empty_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root_path = canonical_test_directory(&directory);
        let root = open_physical_directory(&root_path).unwrap();
        let child = root
            .create_child_directory(OsStr::new("remove-me"))
            .unwrap()
            .unwrap();
        let child_path = child.path().to_path_buf();
        let sibling = root
            .create_child_directory(OsStr::new("keep-me"))
            .unwrap()
            .unwrap();

        root.remove_child_directory_if_matches(OsStr::new("remove-me"), &child)
            .unwrap();
        drop(child);

        assert!(!child_path.exists());
        assert!(sibling.path().is_dir());
    }

    #[test]
    fn directory_enumeration_stops_at_limit_plus_one() {
        let directory = tempfile::tempdir().unwrap();
        let root_path = canonical_test_directory(&directory);
        for index in 0..64 {
            fs::write(root_path.join(format!("entry-{index:03}")), b"x").unwrap();
        }
        let root = open_physical_directory(&root_path).unwrap();

        assert_eq!(root.directory.entry_names(16).unwrap().len(), 17);
        let error = root.entry_names(16).unwrap_err();
        assert_eq!(
            error,
            format!(
                "{} contains too many direct children",
                display_path(&root_path)
            )
        );
        assert_eq!(root.entry_names(64).unwrap().len(), 64);
    }

    #[test]
    fn windows_reparse_attribute_is_treated_as_a_link() {
        assert!(attributes_have_reparse_point(
            WINDOWS_FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert!(!attributes_have_reparse_point(0));
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_structs_match_the_documented_abi() {
        use std::mem::{offset_of, size_of};

        platform::assert_native_abi_layouts();
        assert_eq!(size_of::<WindowsFileTime>(), 8);
        assert_eq!(
            offset_of!(WindowsByHandleFileInformation, file_attributes),
            0
        );
        assert_eq!(
            offset_of!(WindowsByHandleFileInformation, volume_serial_number),
            28
        );
        assert_eq!(
            offset_of!(WindowsByHandleFileInformation, file_index_high),
            44
        );
        assert_eq!(size_of::<WindowsByHandleFileInformation>(), 52);
    }

    #[cfg(windows)]
    #[test]
    fn windows_absolute_roots_keep_drive_unc_and_verbatim_anchors() {
        for (raw, expected_anchor, expected_tail) in [
            (r"C:\one\two", r"C:\", vec!["one", "two"]),
            (
                r"\\server\share\one\two",
                r"\\server\share\",
                vec!["one", "two"],
            ),
            (r"\\?\C:\one\two", r"\\?\C:\", vec!["one", "two"]),
            (
                r"\\?\UNC\server\share\one\two",
                r"\\?\UNC\server\share\",
                vec!["one", "two"],
            ),
        ] {
            let (anchor, fragments) = split_absolute_path(Path::new(raw)).unwrap();
            assert_eq!(anchor, PathBuf::from(expected_anchor));
            assert_eq!(
                fragments,
                expected_tail
                    .into_iter()
                    .map(OsString::from)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_link_cannot_become_an_authorized_project_root() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let linked = directory.path().join("linked");
        fs::create_dir(&physical).unwrap();
        symlink(&physical, &linked).unwrap();

        let store = directory.path().join("roots.json");
        assert!(authorize_project_root(&store, &linked).is_err());
        assert!(!store.exists());
    }

    #[cfg(unix)]
    #[test]
    fn descendant_link_cannot_jump_between_two_authorized_roots() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let store = directory.path().join("roots.json");
        let first = authorize_project_root(&store, &first).unwrap();
        let second = authorize_project_root(&store, &second).unwrap();
        symlink(&second, first.join("linked")).unwrap();

        let escaped = first.join("linked").join("ro-sync.json");
        assert!(resolve_authorized_path(&store, &escaped).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn final_link_to_the_same_inode_is_rejected_for_reads() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let base = canonical_test_directory(&directory);
        let project = base.join("project");
        fs::create_dir(&project).unwrap();
        let store = base.join("roots.json");
        let project = authorize_project_root(&store, &project).unwrap();
        let destination = project.join("ro-sync.json");
        let moved = project.join("same-file");
        fs::write(&destination, "private").unwrap();
        fs::rename(&destination, &moved).unwrap();
        symlink(&moved, &destination).unwrap();

        assert!(read_authorized_utf8_file(&store, &destination, 1024).is_err());
        assert!(read_utf8_file(&destination, 1024).is_err());
        assert_eq!(fs::read_to_string(&moved).unwrap(), "private");
    }

    #[cfg(unix)]
    #[test]
    fn authorized_read_and_write_reject_a_swapped_parent() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let base = canonical_test_directory(&directory);
        let project = base.join("project");
        let nested = project.join("nested");
        let moved = project.join("moved");
        let outside = base.join("outside");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        let store = base.join("roots.json");
        let project = authorize_project_root(&store, &project).unwrap();
        let destination = project.join("nested/ro-sync.json");
        let outside_destination = outside.join("ro-sync.json");
        fs::write(&destination, "authorized").unwrap();
        fs::write(&outside_destination, "outside sentinel").unwrap();

        // Exercise the formerly vulnerable resolve-then-operate sequence.
        assert_eq!(
            resolve_authorized_path(&store, &destination).unwrap(),
            destination
        );
        fs::rename(&nested, &moved).unwrap();
        symlink(&outside, &nested).unwrap();

        assert!(read_authorized_utf8_file(&store, &destination, 1024).is_err());
        assert!(atomic_write_authorized(&store, &destination, b"changed", 0o644).is_err());
        assert_eq!(
            fs::read_to_string(&outside_destination).unwrap(),
            "outside sentinel"
        );
        assert_eq!(
            fs::read_to_string(moved.join("ro-sync.json")).unwrap(),
            "authorized"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_parent_capability_stays_bound_after_path_swap() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let base = canonical_test_directory(&directory);
        let nested = base.join("nested");
        let moved = base.join("moved");
        let outside = base.join("outside");
        fs::create_dir(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        let destination = nested.join("ro-sync.json");
        let outside_destination = outside.join("ro-sync.json");
        fs::write(&destination, "original").unwrap();
        fs::write(&outside_destination, "outside sentinel").unwrap();

        let (parent, name) = open_absolute_parent(&destination, false, true).unwrap();
        fs::rename(&nested, &moved).unwrap();
        symlink(&outside, &nested).unwrap();

        assert_eq!(
            read_utf8_from_parent(&parent, &name, &destination, 1024).unwrap(),
            "original"
        );
        atomic_write_in_parent(&parent, &name, &destination, b"updated", 0o644).unwrap();
        assert_eq!(
            fs::read_to_string(moved.join("ro-sync.json")).unwrap(),
            "updated"
        );
        assert_eq!(
            fs::read_to_string(&outside_destination).unwrap(),
            "outside sentinel"
        );
    }

    #[test]
    fn canonical_ambient_parent_stays_bound_after_same_path_physical_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let base = canonical_test_directory(&directory);
        let parent_path = base.join("parent");
        let moved = base.join("moved");
        fs::create_dir(&parent_path).unwrap();
        let destination = parent_path.join("state.json");
        fs::write(&destination, "original").unwrap();

        let (parent, _, name) = open_canonical_ambient_parent(&destination, false).unwrap();
        fs::rename(&parent_path, &moved).unwrap();
        fs::create_dir(&parent_path).unwrap();
        let replacement_destination = parent_path.join("state.json");
        fs::write(&replacement_destination, "replacement sentinel").unwrap();

        assert_eq!(
            read_utf8_from_parent(&parent, &name, &destination, 1024).unwrap(),
            "original"
        );
        atomic_write_in_parent(&parent, &name, &destination, b"updated", 0o600).unwrap();
        assert_eq!(
            fs::read_to_string(moved.join("state.json")).unwrap(),
            "updated"
        );
        assert_eq!(
            fs::read_to_string(&replacement_destination).unwrap(),
            "replacement sentinel"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stored_root_rejects_an_ancestor_link_even_when_leaf_identity_matches() {
        use std::os::unix::fs::{symlink, MetadataExt as _};

        let directory = tempfile::tempdir().unwrap();
        let base = canonical_test_directory(&directory);
        let ancestor = base.join("ancestor");
        let relocated = base.join("relocated");
        let project = ancestor.join("project");
        fs::create_dir_all(&project).unwrap();
        let store = base.join("roots.json");
        let stored_root = authorize_project_root(&store, &project).unwrap();
        fs::write(project.join("ro-sync.json"), "sentinel").unwrap();

        fs::rename(&ancestor, &relocated).unwrap();
        symlink(&relocated, &ancestor).unwrap();
        let linked_leaf = fs::metadata(&stored_root).unwrap();
        let physical_leaf = fs::metadata(relocated.join("project")).unwrap();
        assert_eq!(linked_leaf.dev(), physical_leaf.dev());
        assert_eq!(linked_leaf.ino(), physical_leaf.ino());

        assert!(canonicalize_physical_directory(&stored_root).is_err());
        assert!(resolve_authorized_path(&store, &stored_root.join("ro-sync.json")).is_err());
        assert_eq!(
            fs::read_to_string(relocated.join("project/ro-sync.json")).unwrap(),
            "sentinel"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_reparse_cannot_jump_between_authorized_roots() {
        use std::os::windows::fs::symlink_dir;

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let store = directory.path().join("roots.json");
        let first = authorize_project_root(&store, &first).unwrap();
        let second = authorize_project_root(&store, &second).unwrap();
        let linked = first.join("linked");
        if symlink_dir(&second, &linked).is_err() {
            // Older Windows runners can lack the symlink privilege. The
            // attribute-mask unit test still covers junction classification.
            return;
        }

        assert!(resolve_authorized_path(&store, &linked.join("ro-sync.json")).is_err());
    }

    #[cfg(windows)]
    fn create_windows_directory_reparse(target: &Path, link: &Path) -> bool {
        use std::{os::windows::fs::symlink_dir, process::Command};

        if symlink_dir(target, link).is_ok() {
            return true;
        }
        Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(windows)]
    #[test]
    fn windows_authorized_read_and_write_reject_a_swapped_parent_reparse() {
        let directory = tempfile::tempdir().unwrap();
        let base = canonical_test_directory(&directory);
        let project = base.join("project");
        let nested = project.join("nested");
        let moved = project.join("moved");
        let outside = base.join("outside");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        let store = base.join("roots.json");
        let project = authorize_project_root(&store, &project).unwrap();
        let destination = project.join("nested/ro-sync.json");
        let outside_destination = outside.join("ro-sync.json");
        fs::write(&destination, "authorized").unwrap();
        fs::write(&outside_destination, "outside sentinel").unwrap();
        assert_eq!(
            resolve_authorized_path(&store, &destination).unwrap(),
            destination
        );

        fs::rename(&nested, &moved).unwrap();
        if !create_windows_directory_reparse(&outside, &nested) {
            return;
        }

        assert!(read_authorized_utf8_file(&store, &destination, 1024).is_err());
        assert!(atomic_write_authorized(&store, &destination, b"changed", 0o644).is_err());
        assert_eq!(
            fs::read_to_string(&outside_destination).unwrap(),
            "outside sentinel"
        );
        assert_eq!(
            fs::read_to_string(moved.join("ro-sync.json")).unwrap(),
            "authorized"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_final_file_reparse_is_rejected_for_reads() {
        use std::os::windows::fs::symlink_file;

        let directory = tempfile::tempdir().unwrap();
        let base = canonical_test_directory(&directory);
        let project = base.join("project");
        fs::create_dir(&project).unwrap();
        let store = base.join("roots.json");
        let project = authorize_project_root(&store, &project).unwrap();
        let destination = project.join("ro-sync.json");
        let moved = project.join("same-file");
        fs::write(&destination, "private").unwrap();
        fs::rename(&destination, &moved).unwrap();
        if symlink_file(&moved, &destination).is_err() {
            return;
        }

        assert!(read_authorized_utf8_file(&store, &destination, 1024).is_err());
        assert!(read_utf8_file(&destination, 1024).is_err());
        assert_eq!(fs::read_to_string(&moved).unwrap(), "private");
    }
}
