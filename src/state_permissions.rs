use std::{io, path::Path};

/// Makes a state directory accessible only to the current operating-system user.
#[cfg(unix)]
pub(crate) fn restrict_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    let mode = std::fs::symlink_metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(io::Error::other(format!(
            "private directory mode verification failed: {mode:o}"
        )));
    }
    Ok(())
}

/// Makes a state file accessible only to the current operating-system user.
#[cfg(unix)]
pub(crate) fn restrict_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let mode = std::fs::symlink_metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(io::Error::other(format!(
            "private file mode verification failed: {mode:o}"
        )));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn restrict_directory(path: &Path) -> io::Result<()> {
    windows::replace_with_current_user_dacl(path, true)
}

#[cfg(windows)]
pub(crate) fn restrict_file(path: &Path) -> io::Result<()> {
    windows::replace_with_current_user_dacl(path, false)
}

#[cfg(not(any(unix, windows)))]
compile_error!("lspc state privacy is implemented only for its supported Unix and Windows hosts");

#[cfg(windows)]
mod windows {
    use std::{ffi::c_void, io, mem, os::windows::ffi::OsStrExt, path::Path, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{
                EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT,
                SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
                TRUSTEE_IS_USER, TRUSTEE_W,
            },
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetTokenInformation,
            PROTECTED_DACL_SECURITY_INFORMATION, PSID, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::{
            SystemServices::ACCESS_ALLOWED_ACE_TYPE,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    pub(super) fn replace_with_current_user_dacl(
        path: &Path,
        inherit_to_children: bool,
    ) -> io::Result<()> {
        let token = current_process_token()?;
        let token_user = token_user(&token)?;
        let user_sid = unsafe { (*(token_user.as_ptr() as *const TOKEN_USER)).User.Sid };
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: if inherit_to_children {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                0
            },
            Trustee: TRUSTEE_W {
                pMultipleTrustee: ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: user_sid.cast(),
            },
        };
        let mut acl: *mut ACL = ptr::null_mut();
        let status = unsafe { SetEntriesInAclW(1, &access, ptr::null(), &mut acl) };
        if status != 0 {
            return Err(os_error(status));
        }
        let acl_allocation = LocalAllocation(acl.cast());
        let mut wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            )
        };
        if status != 0 {
            return Err(os_error(status));
        }
        verify_current_user_dacl(path, user_sid, inherit_to_children)?;
        drop(acl_allocation);
        Ok(())
    }

    fn current_process_token() -> io::Result<Handle> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Handle(token))
    }

    fn token_user(token: &Handle) -> io::Result<Vec<usize>> {
        let mut required = 0u32;
        unsafe {
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
        }
        if required == 0
            || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        {
            return Err(io::Error::last_os_error());
        }
        let words = (required as usize).div_ceil(mem::size_of::<usize>());
        let mut buffer = vec![0usize; words];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(buffer)
    }

    fn verify_current_user_dacl(
        path: &Path,
        user_sid: PSID,
        inherit_to_children: bool,
    ) -> io::Result<()> {
        let mut wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut acl: *mut ACL = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut acl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(os_error(status));
        }
        let _descriptor = LocalAllocation(descriptor);
        if acl.is_null() {
            return Err(io::Error::other(
                "private DACL verification found a null ACL",
            ));
        }
        let mut information = ACL_SIZE_INFORMATION::default();
        if unsafe {
            GetAclInformation(
                acl,
                ptr::addr_of_mut!(information).cast(),
                mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if information.AceCount != 1 {
            return Err(io::Error::other(format!(
                "private DACL verification found {} entries",
                information.AceCount
            )));
        }
        let mut raw_ace = ptr::null_mut();
        if unsafe { GetAce(acl, 0, &mut raw_ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let ace = raw_ace.cast::<ACCESS_ALLOWED_ACE>();
        let required_flags = if inherit_to_children {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            0
        };
        let sid = unsafe { ptr::addr_of_mut!((*ace).SidStart).cast() };
        let valid = unsafe {
            u32::from((*ace).Header.AceType) == ACCESS_ALLOWED_ACE_TYPE
                && (*ace).Mask == FILE_ALL_ACCESS
                && u32::from((*ace).Header.AceFlags) & SUB_CONTAINERS_AND_OBJECTS_INHERIT
                    == required_flags
                && EqualSid(user_sid, sid) != 0
        };
        if !valid {
            return Err(io::Error::other(
                "private DACL verification found unexpected access",
            ));
        }
        Ok(())
    }

    fn os_error(status: u32) -> io::Error {
        io::Error::from_raw_os_error(status as i32)
    }
}
