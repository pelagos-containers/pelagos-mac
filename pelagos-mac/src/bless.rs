//! SMJobBless integration — automatic privileged helper installation.
//!
//! On first `pelagos vm start`, if `/var/run/pelagos-pfctl.sock` is absent,
//! `ensure_pfctl_blessed()` calls `SMJobBless` to install `com.pelagos.pfctl`.
//! macOS shows a one-time admin credential dialog; after that the helper runs
//! permanently as a LaunchDaemon and restarts automatically on reboot.
//!
//! # Signing requirements
//!
//! SMJobBless validates a cross-signed trust chain at install time:
//! - The calling binary (`pelagos`) must have `SMPrivilegedExecutables` in its
//!   embedded `__TEXT,__info_plist` section listing the helper's designated
//!   requirement string.
//! - The helper binary (`com.pelagos.pfctl`) must have `SMAuthorizedClients` in
//!   its embedded `__TEXT,__launchd_plist` section listing the caller's
//!   designated requirement string.
//! - Both binaries must be signed with the same certificate identity.
//!
//! Both plists are generated at build time by their respective `build.rs` with
//! `@CALLER_DR@` / `@HELPER_DR@` substituted from env vars. For development,
//! create a local "pelagos-mac Dev" code-signing certificate in Keychain Access
//! and sign both binaries with it (see `scripts/sign.sh`).

use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::time::{Duration, Instant};

/// Socket path for the pfctl helper daemon.
const PFCTL_SOCK: &str = "/var/run/pelagos-pfctl.sock";

/// Bundle identifier / LaunchDaemon label for the privileged helper.
const HELPER_LABEL: &str = "com.pelagos.pfctl";

/// Ensure the pelagos-pfctl privileged helper is installed and running.
///
/// Fast path: if the socket already exists, return `Ok(())` immediately.
///
/// Install path: SMJobBless copies the helper binary to
/// `/Library/PrivilegedHelperTools/com.pelagos.pfctl`, installs its embedded
/// plist as a LaunchDaemon, and loads it. macOS prompts for admin credentials
/// exactly once; subsequent calls always hit the fast path.
pub fn ensure_pfctl_blessed() -> io::Result<()> {
    if pfctl_socket_present() {
        return Ok(());
    }
    bless_helper()
}

fn pfctl_socket_present() -> bool {
    std::fs::metadata(PFCTL_SOCK)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

fn bless_helper() -> io::Result<()> {
    // Locate the helper binary in the same directory as this executable, named
    // exactly by its bundle identifier.  This is the conventional location that
    // SMJobBless uses for non-app-bundle CLIs.
    //
    // Homebrew layout: /opt/homebrew/bin/pelagos → binary
    //                  /opt/homebrew/bin/com.pelagos.pfctl → symlink to pkgshare/pelagos-pfctl
    //
    // Dev layout:      target/aarch64-apple-darwin/release/pelagos
    //                  target/aarch64-apple-darwin/release/com.pelagos.pfctl
    //                  (sign.sh creates the latter as a signed copy)
    let exe = std::env::current_exe()?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot determine exe directory"))?;
    let helper_path = exe_dir.join(HELPER_LABEL);

    if !helper_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "privileged helper not found at {path}\n\
                 If using Homebrew, reinstall: brew reinstall pelagos-containers/tap/pelagos-mac\n\
                 If developing locally, run: bash scripts/sign.sh",
                path = helper_path.display()
            ),
        ));
    }

    log::info!(
        "bless: installing privileged helper — macOS will prompt for admin credentials"
    );

    // Safety: all raw pointer operations are confined to do_smjobbless().
    unsafe { do_smjobbless() }.map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;

    // SMJobBless loads the LaunchDaemon asynchronously; poll until the socket appears.
    wait_for_socket(Duration::from_secs(10))
}

fn wait_for_socket(timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if pfctl_socket_present() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "helper installed but {PFCTL_SOCK} did not appear within {}s\n\
                     Check /var/log/pelagos-pfctl.log for errors.",
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---------------------------------------------------------------------------
// SMJobBless FFI
// ---------------------------------------------------------------------------

#[allow(non_camel_case_types, non_upper_case_globals)]
mod ffi {
    use std::ffi::c_void;

    pub type OSStatus = i32;
    pub type Boolean = u8;
    pub type CFTypeRef = *const c_void;
    pub type CFStringRef = *const c_void;
    pub type CFErrorRef = *mut c_void;
    pub type CFAllocatorRef = *const c_void;

    pub const kCFAllocatorDefault: CFAllocatorRef = std::ptr::null();
    pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

    // AuthorizationRef is opaque.
    pub type AuthorizationRef = *mut c_void;

    pub const errAuthorizationSuccess: OSStatus = 0;

    // Authorization flags
    pub const kAuthorizationFlagDefaults: u32 = 0;
    pub const kAuthorizationFlagInteractionAllowed: u32 = 1 << 0;
    pub const kAuthorizationFlagExtendRights: u32 = 1 << 1;
    pub const kAuthorizationFlagPreAuthorize: u32 = 1 << 2;

    // The right name that allows calling SMJobBless.
    // Value from Security/Authorization.h: kSMRightBlessPrivilegedHelper
    pub const SM_BLESS_RIGHT: &std::ffi::CStr =
        c"com.apple.ServiceManagement.blesshelper";

    #[repr(C)]
    pub struct AuthorizationItem {
        pub name: *const std::ffi::c_char,
        pub value_length: usize,
        pub value: *mut c_void,
        pub flags: u32,
    }

    #[repr(C)]
    pub struct AuthorizationRights {
        pub count: u32,
        pub items: *mut AuthorizationItem,
    }

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        pub fn AuthorizationCreate(
            rights: *const AuthorizationRights,
            environment: *const c_void,
            flags: u32,
            authorization: *mut AuthorizationRef,
        ) -> OSStatus;
        pub fn AuthorizationCopyRights(
            authorization: AuthorizationRef,
            rights: *const AuthorizationRights,
            environment: *const c_void,
            flags: u32,
            authorized_rights: *mut *mut AuthorizationRights,
        ) -> OSStatus;
        pub fn AuthorizationFree(authorization: AuthorizationRef, flags: u32) -> OSStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFStringCreateWithCString(
            alloc: CFAllocatorRef,
            c_str: *const std::ffi::c_char,
            encoding: u32,
        ) -> CFStringRef;
        pub fn CFStringGetCStringPtr(
            the_string: CFStringRef,
            encoding: u32,
        ) -> *const std::ffi::c_char;
        pub fn CFErrorCopyDescription(err: CFErrorRef) -> CFStringRef;
        pub fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "ServiceManagement", kind = "framework")]
    extern "C" {
        // The domain constant for system (root) LaunchDaemons.
        pub static kSMDomainSystemLaunchd: CFStringRef;

        pub fn SMJobBless(
            domain: CFStringRef,
            job_label: CFStringRef,
            auth: AuthorizationRef,
            out_error: *mut CFErrorRef,
        ) -> Boolean;
    }
}

/// Call SMJobBless to install the privileged helper.
///
/// # Safety
/// Raw pointer operations on opaque OS handles. All pointers obtained from OS
/// APIs; lifetimes managed by explicit `CFRelease` / `AuthorizationFree` calls.
unsafe fn do_smjobbless() -> Result<(), String> {
    use ffi::*;

    // 1. Create an empty AuthorizationRef.
    let mut auth: AuthorizationRef = std::ptr::null_mut();
    let status = AuthorizationCreate(
        std::ptr::null(),
        std::ptr::null(),
        kAuthorizationFlagDefaults,
        &mut auth,
    );
    if status != errAuthorizationSuccess {
        return Err(format!("AuthorizationCreate failed: OSStatus {status}"));
    }

    // 2. Request kSMRightBlessPrivilegedHelper — this triggers the macOS admin
    //    credential dialog.
    let right_name = SM_BLESS_RIGHT.as_ptr();
    let mut right_item = AuthorizationItem {
        name: right_name,
        value_length: 0,
        value: std::ptr::null_mut(),
        flags: 0,
    };
    let rights = AuthorizationRights {
        count: 1,
        items: &mut right_item,
    };
    let status = AuthorizationCopyRights(
        auth,
        &rights,
        std::ptr::null(),
        kAuthorizationFlagInteractionAllowed
            | kAuthorizationFlagExtendRights
            | kAuthorizationFlagPreAuthorize,
        std::ptr::null_mut(), // we don't need the granted rights back
    );
    if status != errAuthorizationSuccess {
        AuthorizationFree(auth, kAuthorizationFlagDefaults);
        return Err(format!(
            "Authorization failed (user may have cancelled): OSStatus {status}"
        ));
    }

    // 3. Call SMJobBless — copies helper to /Library/PrivilegedHelperTools/,
    //    installs the embedded plist as a LaunchDaemon, and loads it.
    let label_cstr = CString::new(HELPER_LABEL).unwrap();
    let label_cf = CFStringCreateWithCString(
        kCFAllocatorDefault,
        label_cstr.as_ptr(),
        kCFStringEncodingUTF8,
    );

    let mut error: CFErrorRef = std::ptr::null_mut();
    let ok = SMJobBless(kSMDomainSystemLaunchd, label_cf, auth, &mut error);

    CFRelease(label_cf);
    AuthorizationFree(auth, kAuthorizationFlagDefaults);

    if ok == 0 {
        let description = if !error.is_null() {
            let desc_cf = CFErrorCopyDescription(error);
            let s = cfstring_to_string(desc_cf);
            if !desc_cf.is_null() {
                CFRelease(desc_cf);
            }
            CFRelease(error as _);
            s
        } else {
            "unknown error".to_string()
        };
        Err(format!(
            "SMJobBless failed: {description}\n\
             \n\
             If developing locally: ensure both 'pelagos' and 'com.pelagos.pfctl' are\n\
             signed with the same certificate and that the designated requirement strings\n\
             in pelagos-mac/assets/Info.plist.in (HELPER_DR) and\n\
             pelagos-pfctl/assets/com.pelagos.pfctl.embedded.plist.in (CALLER_DR)\n\
             match the actual signatures.\n\
             \n\
             Inspect signatures: codesign -dv --verbose=4 $(which pelagos)\n\
             Create a dev cert: Keychain Access → Certificate Assistant →\n\
             Create a Certificate → Name: \"pelagos-mac Dev\", Type: Code Signing"
        ))
    } else {
        log::info!("bless: com.pelagos.pfctl installed successfully");
        Ok(())
    }
}

unsafe fn cfstring_to_string(cf: ffi::CFStringRef) -> String {
    if cf.is_null() {
        return String::new();
    }
    let ptr = ffi::CFStringGetCStringPtr(cf, ffi::kCFStringEncodingUTF8);
    if ptr.is_null() {
        return "(non-UTF8 CFString)".to_string();
    }
    CStr::from_ptr(ptr).to_string_lossy().into_owned()
}
