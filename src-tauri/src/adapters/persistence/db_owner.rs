//! Private file-actor ownership. Lock contents are never authority.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OwnerError {
    #[error("database ownership is unavailable")]
    Unavailable,
    #[error("database ownership is unavailable")]
    AccessDenied,
    #[error("database identity is unsafe")]
    UnsafeIdentity,
    #[error("database already has an owner")]
    Busy,
    #[error("database does not exist")]
    Missing,
    #[error("database destination already exists")]
    DestinationExists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerDetail {
    SharingOrLockContended,
    PathUnavailable,
    UnsafePath,
    AccessDenied,
    OtherUnavailable,
}
impl OwnerError {
    pub(super) fn detail(self) -> OwnerDetail {
        match self {
            Self::Busy => OwnerDetail::SharingOrLockContended,
            Self::Missing => OwnerDetail::PathUnavailable,
            Self::UnsafeIdentity => OwnerDetail::UnsafePath,
            Self::AccessDenied => OwnerDetail::AccessDenied,
            Self::Unavailable | Self::DestinationExists => OwnerDetail::OtherUnavailable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerStep {
    CanonicalParentResolution,
    AncestorPin,
    LockFileAcquire,
    MainFilePin,
    IdentityValidation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockFallbackObservation {
    ExistingLockAbsent,
    ExistingLockOpenDenied,
    ExistingLockSharingConflict,
    ExistingLockOther,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryAccessRequestObservation {
    OpenedSameParent,
    OpenAccessDenied,
    OtherUnavailable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ParentAccessRequests {
    pub(crate) list_directory: DirectoryAccessRequestObservation,
    pub(crate) add_file: DirectoryAccessRequestObservation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{kind}")]
pub struct OwnerFailure {
    pub(crate) kind: OwnerError,
    pub(crate) step: OwnerStep,
    pub(super) fallback_detail: Option<LockFallbackObservation>,
    pub(super) parent_access_requests: Option<ParentAccessRequests>,
}
impl OwnerError {
    pub(super) fn at(self, step: OwnerStep) -> OwnerFailure {
        OwnerFailure {
            kind: self,
            step,
            fallback_detail: None,
            parent_access_requests: None,
        }
    }
}
impl OwnerFailure {
    pub(super) fn diagnostic_parent_access(self) -> Option<ParentAccessRequests> {
        (self.diagnostic_fallback() == Some(LockFallbackObservation::ExistingLockAbsent))
            .then_some(self.parent_access_requests)
            .flatten()
    }
    pub(super) fn diagnostic_fallback(self) -> Option<LockFallbackObservation> {
        if self.kind == OwnerError::AccessDenied && self.step == OwnerStep::LockFileAcquire {
            self.fallback_detail
        } else {
            None
        }
    }
    pub(super) fn detail(self) -> OwnerDetail {
        self.kind.detail()
    }
    pub(super) fn diagnostic_step(self) -> Option<OwnerStep> {
        matches!(
            self.detail(),
            OwnerDetail::AccessDenied
                | OwnerDetail::OtherUnavailable
                | OwnerDetail::PathUnavailable
        )
        .then_some(self.step)
    }
}

// No Clone/Debug/serialization, no public capability/identity getters.
pub(super) struct DatabaseOwner {
    path: PathBuf,
    #[cfg(windows)]
    _files: Vec<std::fs::File>,
}
impl DatabaseOwner {
    pub(super) fn acquire(path: &Path) -> Result<Self, OwnerFailure> {
        Self::acquire_mode(path, OpenMode::Actor)
    }
    pub(super) fn acquire_vacant(path: &Path) -> Result<Self, OwnerFailure> {
        Self::acquire_mode(path, OpenMode::Vacant)
    }
    pub(super) fn acquire_existing_read_only(path: &Path) -> Result<Self, OwnerFailure> {
        Self::acquire_mode(path, OpenMode::ExistingReadOnly)
    }
    pub(super) fn acquire_existing_read_only_for_configuration(
        path: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, OwnerFailure> {
        #[cfg(windows)]
        {
            native::acquire_inner(path, OpenMode::ExistingReadOnly, Some(cancelled))
        }
        #[cfg(not(windows))]
        {
            let _ = cancelled;
            Self::acquire_existing_read_only(path)
        }
    }
    fn acquire_mode(path: &Path, mode: OpenMode) -> Result<Self, OwnerFailure> {
        if path == Path::new(":memory:") {
            if mode != OpenMode::Actor {
                return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
            }
            return Ok(Self {
                path: path.to_owned(),
                #[cfg(windows)]
                _files: Vec::new(),
            });
        }
        #[cfg(windows)]
        {
            native::acquire(path, mode)
        }
        #[cfg(not(windows))]
        {
            let _ = mode;
            Err(OwnerError::Unavailable.at(OwnerStep::IdentityValidation))
        }
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    Actor,
    Vacant,
    ExistingReadOnly,
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        fs::{File, OpenOptions},
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        path::Component,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ADD_FILE, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    };

    pub(super) fn error(error: std::io::Error) -> OwnerError {
        match error.raw_os_error() {
            Some(32 | 33) => OwnerError::Busy,
            Some(80 | 183) => OwnerError::DestinationExists,
            Some(2 | 3) => OwnerError::Missing,
            Some(5) => OwnerError::AccessDenied,
            _ => OwnerError::Unavailable,
        }
    }
    pub(super) fn failure(error: std::io::Error, step: OwnerStep) -> OwnerFailure {
        self::error(error).at(step)
    }
    pub(super) fn fallback_open_failure(error: std::io::Error) -> OwnerFailure {
        let mut failure = failure(error, OwnerStep::LockFileAcquire);
        failure.fallback_detail = Some(match failure.kind {
            OwnerError::Missing => LockFallbackObservation::ExistingLockAbsent,
            OwnerError::AccessDenied => LockFallbackObservation::ExistingLockOpenDenied,
            OwnerError::Busy => LockFallbackObservation::ExistingLockSharingConflict,
            _ => LockFallbackObservation::ExistingLockOther,
        });
        failure
    }
    pub(super) fn fallback_identity_failure(mut failure: OwnerFailure) -> OwnerFailure {
        failure.fallback_detail = (failure.kind != OwnerError::UnsafeIdentity)
            .then_some(LockFallbackObservation::ExistingLockOther);
        failure
    }
    pub(super) fn access_denied_fallback<T>(
        primary: impl FnOnce() -> Result<T, OwnerFailure>,
        fallback: impl FnOnce() -> Result<T, OwnerFailure>,
    ) -> Result<T, OwnerFailure> {
        match primary() {
            Err(
                primary @ OwnerFailure {
                    kind: OwnerError::AccessDenied,
                    step: OwnerStep::LockFileAcquire,
                    ..
                },
            ) => match fallback() {
                // Production Ok is a final exclusive handle with validated pre/open/post identities.
                // Uncertain fallback attempts retain the primary diagnosis; unsafe proof wins.
                Ok(fully_validated_owner_handle) => Ok(fully_validated_owner_handle),
                Err(
                    unsafe_identity @ OwnerFailure {
                        kind: OwnerError::UnsafeIdentity,
                        step: OwnerStep::IdentityValidation,
                        ..
                    },
                ) => Err(fallback_identity_failure(unsafe_identity)),
                Err(fallback) => Err(OwnerFailure {
                    fallback_detail: Some(
                        fallback
                            .fallback_detail
                            .unwrap_or(LockFallbackObservation::ExistingLockOther),
                    ),
                    ..primary
                }),
            },
            result => result,
        }
    }
    fn lock_identity(file: &File) -> Result<(u32, u32, u32), OwnerFailure> {
        let info = information(file).map_err(|e| e.at(OwnerStep::IdentityValidation))?;
        if info.nNumberOfLinks != 1
            || !file
                .metadata()
                .map_err(|e| failure(e, OwnerStep::IdentityValidation))?
                .is_file()
        {
            return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
        }
        Ok((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
    pub(super) fn existing_lock_identity(path: &Path) -> Result<(u32, u32, u32), OwnerFailure> {
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(fallback_open_failure)?;
        lock_identity(&file).map_err(fallback_identity_failure)
    }
    pub(super) fn readonly_existing_lock(path: &Path) -> Result<File, OwnerFailure> {
        // The caller already pins the canonical parent and exact sibling name.
        // Identity snapshots are not ownership: only the final share-zero handle is.
        let before = existing_lock_identity(path)?;
        readonly_lock_after_snapshot(path, before)
    }
    pub(super) fn readonly_lock_after_snapshot(
        path: &Path,
        before: (u32, u32, u32),
    ) -> Result<File, OwnerFailure> {
        let file = OpenOptions::new()
            .access_mode(FILE_READ_DATA | FILE_READ_ATTRIBUTES)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(fallback_open_failure)?;
        let opened = lock_identity(&file).map_err(fallback_identity_failure)?;
        let after = existing_lock_identity(path)?;
        if before != opened || opened != after {
            return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
        }
        Ok(file)
    }
    fn information(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION, OwnerError> {
        let mut info = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
            return Err(error(std::io::Error::last_os_error()));
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(OwnerError::UnsafeIdentity);
        }
        Ok(info)
    }
    fn simple_filename(path: &Path) -> Result<&std::ffi::OsStr, OwnerError> {
        let name = path.file_name().ok_or(OwnerError::UnsafeIdentity)?;
        let text = name.to_str().ok_or(OwnerError::UnsafeIdentity)?;
        if text.is_empty()
            || text.len() > 240
            || text.ends_with(['.', ' '])
            || text.contains([':', '/', '\\', '\0'])
        {
            return Err(OwnerError::UnsafeIdentity);
        }
        Ok(name)
    }
    fn directory_identity(file: &File) -> Result<(u32, u32, u32), OwnerError> {
        let info = information(file)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(OwnerError::UnsafeIdentity);
        }
        Ok((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
    // Borrowed while every ancestor pin remains live; never capability or wire evidence.
    struct PinnedParent<'a> {
        path: &'a Path,
        pin: &'a File,
        identity: Result<(u32, u32, u32), OwnerError>,
    }
    impl PinnedParent<'_> {
        fn inspect(&self, file: &File) -> Result<(), OwnerError> {
            let expected = self.identity?;
            if directory_identity(file)? != expected {
                return Err(OwnerError::UnsafeIdentity);
            }
            Ok(())
        }
        fn open(&self, mask: u32) -> Result<File, std::io::Error> {
            // OPEN_EXISTING: no read/write/create options, no entry/content APIs.
            OpenOptions::new()
                .access_mode(mask)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(self.path)
        }
    }
    pub(super) fn parent_access_failure_with<T>(
        primary: OwnerFailure,
        cancelled: Option<&dyn Fn() -> bool>,
        mut bind: impl FnMut() -> Result<(), OwnerError>,
        mut open: impl FnMut(u32) -> Result<T, std::io::Error>,
        mut inspect: impl FnMut(&T) -> Result<(), OwnerError>,
    ) -> OwnerFailure {
        let Some(cancelled) = cancelled else {
            return primary;
        };
        if primary.diagnostic_fallback() != Some(LockFallbackObservation::ExistingLockAbsent)
            || cancelled()
        {
            return primary;
        }
        let unsafe_failure = || OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation);
        match bind() {
            Ok(()) => {}
            Err(OwnerError::UnsafeIdentity) => return unsafe_failure(),
            Err(_) => return primary,
        }
        let mut handles = Vec::new();
        let mut observations = Vec::new();
        for mask in [
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_ADD_FILE | FILE_READ_ATTRIBUTES,
        ] {
            if cancelled() {
                return primary;
            }
            let observation = match open(mask) {
                Ok(handle) => {
                    let result = inspect(&handle);
                    handles.push(handle); // Keep successful opens through final binding.
                    match result {
                        Ok(()) => DirectoryAccessRequestObservation::OpenedSameParent,
                        Err(OwnerError::UnsafeIdentity) => return unsafe_failure(),
                        Err(_) => DirectoryAccessRequestObservation::OtherUnavailable,
                    }
                }
                Err(e) => {
                    if error(e) == OwnerError::AccessDenied {
                        DirectoryAccessRequestObservation::OpenAccessDenied
                    } else {
                        DirectoryAccessRequestObservation::OtherUnavailable
                    }
                }
            };
            observations.push(observation);
        }
        if cancelled() {
            return primary;
        }
        match bind() {
            Ok(()) => {}
            Err(OwnerError::UnsafeIdentity) => return unsafe_failure(),
            Err(_) => return primary,
        }
        if cancelled() {
            return primary;
        }
        OwnerFailure {
            parent_access_requests: Some(ParentAccessRequests {
                list_directory: observations[0],
                add_file: observations[1],
            }),
            ..primary
        }
    }
    pub(super) fn acquire(path: &Path, mode: OpenMode) -> Result<DatabaseOwner, OwnerFailure> {
        acquire_inner(path, mode, None)
    }
    pub(super) fn acquire_inner(
        path: &Path,
        mode: OpenMode,
        configuration_cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<DatabaseOwner, OwnerFailure> {
        let configuration_cancelled = if mode == OpenMode::ExistingReadOnly {
            configuration_cancelled
        } else {
            None
        };
        // Reject traversal/device/UNC aliases. Canonicalization below is still
        // required, but cannot erase an untrusted reparse ancestor first.
        if !path.is_absolute() && matches!(path.components().next(), Some(Component::Prefix(_))) {
            return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
        }
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map_err(|e| failure(e, OwnerStep::CanonicalParentResolution))?
                .join(path)
        };
        if absolute
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir)
                || matches!(part, Component::Normal(name) if name.to_str().is_none_or(|text| text.contains([':', '\0']) || text.ends_with(['.', ' ']))))
        {
            return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
        }
        let prefix = absolute
            .components()
            .next()
            .ok_or_else(|| OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation))?;
        if !matches!(prefix, Component::Prefix(p) if matches!(p.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)))
        {
            return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
        }
        let filename = simple_filename(&absolute)
            .map_err(|e| e.at(OwnerStep::IdentityValidation))?
            .to_owned();
        let parent = absolute
            .parent()
            .ok_or_else(|| OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation))?;
        let mut directories = parent.ancestors().collect::<Vec<_>>();
        directories.reverse();
        let mut files = Vec::new();
        for directory in directories {
            if directory.as_os_str().is_empty() {
                continue;
            }
            // Retain each ancestor identity pin without DELETE sharing. Attribute-only
            // directory access does not itself guarantee rename exclusion.
            let file = OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(directory)
                .map_err(|e| failure(e, OwnerStep::AncestorPin))?;
            information(&file).map_err(|e| e.at(OwnerStep::IdentityValidation))?;
            if !file
                .metadata()
                .map_err(|e| failure(e, OwnerStep::AncestorPin))?
                .is_dir()
            {
                return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
            }
            files.push(file);
        }
        let canonical_parent = std::fs::canonicalize(parent)
            .map_err(|e| failure(e, OwnerStep::CanonicalParentResolution))?;
        // OPEN_REPARSE_POINT means inspecting and rejecting a link itself.
        // It must precede following/canonicalizing an existing database name.
        match std::fs::symlink_metadata(&absolute) {
            Ok(meta) => {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(failure(e, OwnerStep::IdentityValidation)),
        }
        let requested = canonical_parent.join(filename);
        // Existing names are canonicalized only after rejecting the link itself.
        let path = if requested
            .try_exists()
            .map_err(|e| failure(e, OwnerStep::IdentityValidation))?
        {
            std::fs::canonicalize(&requested)
                .map_err(|e| failure(e, OwnerStep::IdentityValidation))?
        } else {
            requested
        };
        if path.parent() != Some(canonical_parent.as_path()) {
            return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
        }
        let name = simple_filename(&path).map_err(|e| e.at(OwnerStep::IdentityValidation))?;
        let mut lock_name = name.to_owned();
        lock_name.push(".owner-lock");
        let lock_path = canonical_parent.join(lock_name);
        let parent_context = configuration_cancelled.and_then(|_| {
            files.last().map(|pin| PinnedParent {
                path: &canonical_parent,
                pin,
                identity: directory_identity(pin),
            })
        });
        let lock = access_denied_fallback(
            || {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .share_mode(0)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(&lock_path)
                    .map_err(|e| failure(e, OwnerStep::LockFileAcquire))
            },
            || readonly_existing_lock(&lock_path),
        );
        let lock = match lock {
            Ok(lock) => lock,
            Err(primary) => {
                return Err(match parent_context {
                    Some(context) => parent_access_failure_with(
                        primary,
                        configuration_cancelled,
                        || context.inspect(context.pin),
                        |mask| context.open(mask),
                        |file| context.inspect(file),
                    ),
                    None => primary,
                });
            }
        };
        let info = information(&lock).map_err(|e| e.at(OwnerStep::IdentityValidation))?;
        if info.nNumberOfLinks != 1
            || lock
                .metadata()
                .map_err(|e| failure(e, OwnerStep::IdentityValidation))?
                .is_dir()
        {
            return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
        }
        files.push(lock);
        // Pin/create the main identity under the exclusive sibling owner.
        // Actor/backup share read/write for SQLite. Read-only selection shares
        // only read, rejecting even unguarded existing/new native writers.
        // Both modes deny rename/delete/replacement for the guard lifetime.
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(mode != OpenMode::ExistingReadOnly)
            .share_mode(if mode == OpenMode::ExistingReadOnly {
                FILE_SHARE_READ
            } else {
                FILE_SHARE_READ | FILE_SHARE_WRITE
            })
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        if mode == OpenMode::Vacant {
            options.create_new(true);
        } else if mode == OpenMode::Actor {
            options.create(true);
        }
        let main = options
            .open(&path)
            .map_err(|e| failure(e, OwnerStep::MainFilePin))?;
        let info = information(&main).map_err(|e| e.at(OwnerStep::IdentityValidation))?;
        if info.nNumberOfLinks != 1
            || main
                .metadata()
                .map_err(|e| failure(e, OwnerStep::IdentityValidation))?
                .is_dir()
        {
            return Err(OwnerError::UnsafeIdentity.at(OwnerStep::IdentityValidation));
        }
        files.push(main);
        Ok(DatabaseOwner {
            path,
            _files: files,
        })
    }
}
