//! Explicit local control and private-key security descriptors.
use std::{
    ffi::c_void,
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
};
use windows_sys::Win32::{
    Foundation::*, Security::Authorization::*, Security::*, Storage::FileSystem::*,
    System::Threading::*,
};

pub(crate) struct Descriptor(pub(crate) *mut c_void);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
impl Descriptor {
    pub(crate) fn parse(sddl: &str) -> io::Result<Self> {
        let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
    pub(crate) fn private() -> io::Result<Self> {
        let owner = current_user_sid()?;
        Self::parse(&format!(
            "O:{owner}D:P(A;;FA;;;{owner})(A;;FA;;;SY)(A;;FA;;;BA)"
        ))
    }
    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _allocation = Descriptor(text.cast());
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, len)
    }))
}

pub fn current_user_sid() -> io::Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut bytes,
        );
    }
    if bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    // TOKEN_USER includes pointers, so the backing allocation must be aligned.
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    unsafe { sid_string(user.User.Sid) }
}

fn create_with_descriptor(path: &Path, descriptor: &Descriptor) -> io::Result<File> {
    let mut path: Vec<u16> = path.as_os_str().encode_wide().collect();
    if path.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    path.push(0);
    let attributes = descriptor.attributes();
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(handle) }))
}

pub fn create_private_key(path: &Path) -> io::Result<File> {
    let file = create_with_descriptor(path, &Descriptor::private()?)?;
    validate_private_key(&file)?;
    Ok(file)
}

pub fn open_private_key(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(GENERIC_READ | READ_CONTROL)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    validate_private_key(&file)?;
    Ok(file)
}

pub fn validate_private_key(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private key must be a regular file, not a device, directory or reparse point",
        ));
    }
    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // Inspect the opened object, not a second pathname lookup. Owner/DACL
    // pointers remain valid while the returned LocalAlloc descriptor is owned.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let _descriptor = Descriptor(descriptor);
    let expected = current_user_sid()?;
    if owner.is_null() || unsafe { sid_string(owner)? } != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private key owner must be the daemon's executing SID",
        ));
    }
    if dacl.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private key must not have a NULL DACL",
        ));
    }
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut ace = std::ptr::null_mut();
        if unsafe { GetAce(dacl, index as u32, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        // Deny ACEs cannot grant access. Unknown/object/callback grants are
        // refused rather than incompletely evaluating their additional rules.
        if header.AceType == 1 {
            continue;
        }
        if header.AceType != 0
            || (header.AceSize as usize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private key contains an unsupported access-granting ACE",
            ));
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast();
        let trustee = unsafe { sid_string(sid)? };
        if trustee != expected && trustee != "S-1-5-18" && trustee != "S-1-5-32-544" {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private key grants access outside its executing SID, SYSTEM and Administrators",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    #[test]
    fn private_key_round_trip_and_exclusive_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("鍵.pem");
        let mut file = create_private_key(&path).unwrap();
        file.write_all(b"synthetic-private-key").unwrap();
        drop(file);
        assert!(create_private_key(&path).is_err());
        let mut bytes = Vec::new();
        open_private_key(&path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"synthetic-private-key");
    }
    #[test]
    fn world_readable_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        let owner = current_user_sid().unwrap();
        let descriptor =
            Descriptor::parse(&format!("O:{owner}D:P(A;;FA;;;{owner})(A;;FR;;;WD)")).unwrap();
        let file = create_with_descriptor(&path, &descriptor).unwrap();
        drop(file);
        assert_eq!(
            open_private_key(&path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
    #[test]
    fn device_is_rejected_before_reading() {
        assert!(open_private_key(Path::new("NUL")).is_err());
    }
}
