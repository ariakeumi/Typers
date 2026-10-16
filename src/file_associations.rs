//! User-initiated Markdown file associations, isolated from editor and preference persistence.

use anyhow::{Context as _, Result, ensure};

const EXTENSIONS: [&str; 1] = [".md"];

/// Queries effective OS defaults without registering the application or changing any setting.
pub(crate) fn is_associated() -> Result<bool> {
    platform::is_associated()
}

/// Registers `.md` for the current user, never plain text or machine-wide defaults.
/// Returns a settings URI when the OS requires user confirmation instead of applying the choice.
pub(crate) fn associate() -> Result<Option<&'static str>> {
    platform::associate()
}

#[cfg(any(target_os = "windows", test))]
fn windows_shell_path(executable: &std::path::Path) -> Result<String> {
    let path = executable
        .to_str()
        .context("application path is not valid Unicode")?;
    ensure!(
        !path.contains(['"', '\0', '\r', '\n']),
        "invalid application path"
    );
    // Keep canonicalize()'s verbatim syntax out of Shell-facing commands and icon locations.
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return Ok(format!(r"\\{unc}"));
    }
    if let Some(disk) = path.strip_prefix(r"\\?\") {
        ensure!(
            disk.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && disk.as_bytes().get(1) == Some(&b':')
                && disk.as_bytes().get(2) == Some(&b'\\'),
            "application path is not supported by Windows Shell"
        );
        return Ok(disk.to_string());
    }
    Ok(path.to_string())
}

#[cfg(any(target_os = "windows", test))]
fn windows_open_command(executable: &std::path::Path) -> Result<String> {
    let path = windows_shell_path(executable)?;
    Ok(format!("\"{path}\" -- \"%1\""))
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::Path, ptr};
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, LocalFree},
        System::Registry::*,
        UI::Shell::*,
    };

    const PROG_ID: &str = "Velotype.Markdown";
    const NO_ASSOCIATION: u32 = 0x80070483; // HRESULT_FROM_WIN32(ERROR_NO_ASSOCIATION)

    /// Owns a newly opened HKCU key; predefined registry handles are never closed.
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn set_value(path: &str, name: &str, value: &str) -> Result<()> {
        let (path, name, value) = (wide(path), wide(name), wide(value));
        let mut key = ptr::null_mut();
        // All pointers refer to live, NUL-terminated UTF-16 buffers for the duration of each call.
        let status = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, path.as_ptr(), &mut key) };
        ensure!(
            status == 0,
            "cannot register file types: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        let key = Key(key);
        let status = unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        };
        ensure!(
            status == 0,
            "cannot register file types: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        Ok(())
    }

    /// Removes only our obsolete registration values; protected user defaults are untouched.
    fn remove_value(path: &str, name: &str) -> Result<()> {
        let (path, name) = (wide(path), wide(name));
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, path.as_ptr(), name.as_ptr()) };
        ensure!(
            matches!(
                status,
                ERROR_SUCCESS | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND
            ),
            "cannot update file type registrations: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        Ok(())
    }

    /// Reads effective metadata without asking Shell to resolve the handler's executable.
    fn association_string(extension: &str, kind: ASSOCSTR) -> Result<Option<String>> {
        let extension = wide(extension);
        let mut size = 0;
        let status = unsafe {
            AssocQueryStringW(
                ASSOCF_INIT_IGNOREUNKNOWN,
                kind,
                extension.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                &mut size,
            )
        };
        // Do not resolve Windows' generic OpenWith handler for an unassociated extension.
        if status as u32 == NO_ASSOCIATION {
            return Ok(None);
        }
        ensure!(
            status >= 0 && size > 0,
            "cannot query file association (0x{:08x})",
            status as u32
        );
        let mut output = vec![0u16; size as usize];
        let status = unsafe {
            AssocQueryStringW(
                ASSOCF_INIT_IGNOREUNKNOWN,
                kind,
                extension.as_ptr(),
                ptr::null(),
                output.as_mut_ptr(),
                &mut size,
            )
        };
        if status as u32 == NO_ASSOCIATION {
            return Ok(None);
        }
        ensure!(
            status == 0,
            "cannot query file association (0x{:08x})",
            status as u32
        );
        output.truncate(
            output
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(output.len()),
        );
        Ok(Some(String::from_utf16(&output)?))
    }

    pub(super) fn is_associated() -> Result<bool> {
        let current = std::env::current_exe()?;
        for extension in EXTENSIONS {
            let Some(prog_id) = association_string(extension, ASSOCSTR_PROGID)? else {
                return Ok(false);
            };
            if !is_velotype_prog_id(&prog_id) {
                return Ok(false);
            }
            let Some(command) = association_string(extension, ASSOCSTR_COMMAND)? else {
                return Ok(false);
            };
            if !command_opens_current_executable(&command, &current)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Also recognizes Windows' registration created by choosing the executable in Open With.
    fn is_velotype_prog_id(prog_id: &str) -> bool {
        prog_id.eq_ignore_ascii_case(PROG_ID)
            || prog_id.eq_ignore_ascii_case(r"Applications\velotype.exe")
    }

    /// Uses Windows' argument parser rather than splitting quoted paths or document placeholders.
    fn command_arguments(command: &str) -> Result<Vec<OsString>> {
        if command.trim().is_empty() {
            return Ok(Vec::new());
        }
        let command = wide(command);
        let mut count = 0;
        let arguments = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
        ensure!(
            !arguments.is_null(),
            "cannot parse file association command: {}",
            std::io::Error::last_os_error()
        );
        // The API returns one LocalAlloc-owned array of count NUL-terminated UTF-16 arguments.
        let result = unsafe { std::slice::from_raw_parts(arguments, count as usize) }
            .iter()
            .map(|argument| {
                let mut length = 0;
                while unsafe { *argument.add(length) } != 0 {
                    length += 1;
                }
                OsString::from_wide(unsafe { std::slice::from_raw_parts(*argument, length) })
            })
            .collect();
        unsafe { LocalFree(arguments.cast()) };
        Ok(result)
    }

    /// A matching ProgID can still point at a moved executable or an unusable verbatim path.
    fn command_opens_current_executable(command: &str, current: &Path) -> Result<bool> {
        let arguments = command_arguments(command)?;
        let Some(executable) = arguments.first() else {
            return Ok(false);
        };
        if executable.to_string_lossy().starts_with(r"\\?\")
            || !arguments.iter().skip(1).any(|argument| {
                argument == "%1" || argument.to_string_lossy().eq_ignore_ascii_case("%L")
            })
        {
            return Ok(false);
        }
        let executable = Path::new(executable);
        if !executable.is_absolute() || !executable.is_file() {
            return Ok(false);
        }
        let executable = executable.canonicalize()?;
        let current = current.canonicalize()?;
        Ok(executable
            .to_string_lossy()
            .eq_ignore_ascii_case(&current.to_string_lossy()))
    }

    pub(super) fn associate() -> Result<Option<&'static str>> {
        let executable = std::env::current_exe()?.canonicalize()?;
        let shell_path = windows_shell_path(&executable)?;
        let command = windows_open_command(&executable)?;
        let root = format!("Software\\Classes\\{PROG_ID}");
        set_value(&root, "", "Markdown Document")?;
        set_value(&format!("{root}\\shell\\open\\command"), "", &command)?;
        set_value(
            &format!("{root}\\DefaultIcon"),
            "",
            &format!("\"{shell_path}\",0"),
        )?;
        let capabilities = "Software\\Velotype\\Capabilities";
        set_value(capabilities, "ApplicationName", "Velotype")?;
        set_value(capabilities, "ApplicationDescription", "Markdown editor")?;
        for extension in EXTENSIONS {
            set_value(
                &format!("{capabilities}\\FileAssociations"),
                extension,
                PROG_ID,
            )?;
            set_value(
                &format!("Software\\Classes\\{extension}\\OpenWithProgids"),
                PROG_ID,
                "",
            )?;
        }
        // Earlier versions advertised .markdown too; keep the Settings page limited to .md.
        remove_value(&format!("{capabilities}\\FileAssociations"), ".markdown")?;
        remove_value(r"Software\Classes\.markdown\OpenWithProgids", PROG_ID)?;
        set_value("Software\\RegisteredApplications", "Velotype", capabilities)?;
        // Notify the shell about registrations, without touching protected UserChoice values.
        unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as i32,
                SHCNF_IDLIST,
                ptr::null(),
                ptr::null(),
            );
        }
        Ok(Some("ms-settings:defaultapps?registeredAppUser=Velotype"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn file_association_identity_does_not_require_other_handlers_executables() {
            assert!(is_velotype_prog_id("Velotype.Markdown"));
            assert!(is_velotype_prog_id(r"applications\VELOTYPE.EXE"));
            for other in ["Typora.md", "AppX-packaged-editor", "Unknown", ""] {
                assert!(!is_velotype_prog_id(other));
            }
        }

        #[test]
        fn missing_file_association_does_not_resolve_open_with() {
            let extension = format!(".velotype-test-{}", uuid::Uuid::new_v4());
            assert_eq!(
                association_string(&extension, ASSOCSTR_PROGID).unwrap(),
                None
            );
            // Read-only smoke check: any existing handler (including packaged apps) is valid.
            association_string(".md", ASSOCSTR_PROGID)
                .expect("the default .md identity should be queryable");
        }

        #[test]
        fn file_association_command_preserves_quoted_document_paths() {
            let executable = Path::new(r"\\?\C:\Program Files\中文 App\velotype.exe");
            let command = windows_open_command(executable).unwrap();
            assert_eq!(
                command_arguments(&command).unwrap(),
                [
                    OsString::from(r"C:\Program Files\中文 App\velotype.exe"),
                    OsString::from("--"),
                    OsString::from("%1")
                ]
            );
            let document = r"D:\My Notes\中文 & test.md";
            assert_eq!(
                command_arguments(&command.replace("%1", document)).unwrap()[2],
                document
            );
        }

        #[test]
        fn stale_or_verbatim_file_associations_remain_repairable() {
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join("中文 App With Spaces");
            std::fs::create_dir(&directory).unwrap();
            let current = directory.join("velotype.exe");
            std::fs::write(&current, b"test fixture").unwrap();
            let canonical = current.canonicalize().unwrap();
            let command = windows_open_command(&canonical).unwrap();
            assert!(command_opens_current_executable(&command, &current).unwrap());
            let other = directory.join("older-velotype.exe");
            std::fs::write(&other, b"older installation").unwrap();
            assert!(
                !command_opens_current_executable(&windows_open_command(&other).unwrap(), &current)
                    .unwrap()
            );
            assert!(
                command_opens_current_executable(&command.replace(" -- ", " "), &current).unwrap()
            );
            let verbatim = format!("\"{}\" -- \"%1\"", canonical.display());
            assert!(!command_opens_current_executable(&verbatim, &current).unwrap());
            assert!(!command_opens_current_executable("", &current).unwrap());
            assert!(
                !command_opens_current_executable(
                    &format!("\"{}\"", windows_shell_path(&canonical).unwrap()),
                    &current
                )
                .unwrap()
            );
            std::fs::remove_file(&current).unwrap();
            assert!(!command_opens_current_executable(&command, &current).unwrap());
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn desktop_entry(executable: &std::path::Path) -> Result<String> {
    let path = executable
        .to_str()
        .context("application path is not valid Unicode")?;
    ensure!(
        !path.contains(['\0', '\n', '\r']),
        "invalid application path"
    );
    let mut quoted = String::from("\"");
    for ch in path.chars() {
        match ch {
            '\\' | '"' | '`' | '$' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    // Desktop value escaping precedes Exec argument parsing, so backslashes need a second layer.
    let command = format!("Exec={} -- %F", quoted.replace('\\', "\\\\"));
    Ok(
        include_str!("../resources/linux/com.manyougz.Velotype.desktop")
            .replace("Exec=velotype %F", &command),
    )
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::{
        io::Write,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    const MIME_TYPES: [&str; 2] = ["text/markdown", "text/x-markdown"];
    const DESKTOP_ID: &str = "com.manyougz.Velotype.desktop";

    fn xdg_mime(args: &[&str]) -> Result<String> {
        let mut child = Command::new("xdg-mime")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("xdg-mime is required for Markdown file associations")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("xdg-mime timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "xdg-mime failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    pub(super) fn is_associated() -> Result<bool> {
        for mime in MIME_TYPES {
            if xdg_mime(&["query", "default", mime])? != DESKTOP_ID {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn associate() -> Result<Option<&'static str>> {
        let home =
            directories::BaseDirs::new().context("cannot locate the user's data directory")?;
        let directory = home.data_dir().join("applications");
        ensure!(
            directory.is_absolute(),
            "application data directory must be absolute"
        );
        std::fs::create_dir_all(&directory)?;
        let mut entry = tempfile::NamedTempFile::new_in(&directory)?;
        entry.write_all(desktop_entry(&std::env::current_exe()?.canonicalize()?)?.as_bytes())?;
        entry.persist(directory.join(DESKTOP_ID))?;
        xdg_mime(&["default", DESKTOP_ID, MIME_TYPES[0], MIME_TYPES[1]])?;
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use crate::app_identity::VELOTYPE_APP_ID;
    use std::{ffi::c_void, ptr};

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            value: *const i8,
            encoding: u32,
        ) -> *const c_void;
        fn CFURLCreateWithFileSystemPath(
            allocator: *const c_void,
            path: *const c_void,
            style: isize,
            directory: u8,
        ) -> *const c_void;
        fn CFEqual(left: *const c_void, right: *const c_void) -> u8;
        fn CFRelease(value: *const c_void);
    }
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn UTTypeCreatePreferredIdentifierForTag(
            class: *const c_void,
            tag: *const c_void,
            conforms: *const c_void,
        ) -> *const c_void;
        fn LSCopyDefaultRoleHandlerForContentType(
            content: *const c_void,
            roles: u32,
        ) -> *const c_void;
        fn LSSetDefaultRoleHandlerForContentType(
            content: *const c_void,
            roles: u32,
            application: *const c_void,
        ) -> i32;
        fn LSRegisterURL(url: *const c_void, update: u8) -> i32;
    }

    /// Releases Core Foundation objects returned by Create/Copy APIs.
    struct OwnedCf(*const c_void);
    impl Drop for OwnedCf {
        fn drop(&mut self) {
            unsafe {
                CFRelease(self.0);
            }
        }
    }
    impl OwnedCf {
        fn new(value: *const c_void) -> Result<Self> {
            ensure!(!value.is_null(), "Launch Services returned no result");
            Ok(Self(value))
        }
        fn string(value: &str) -> Result<Self> {
            let value = std::ffi::CString::new(value)?;
            Self::new(unsafe { CFStringCreateWithCString(ptr::null(), value.as_ptr(), 0x08000100) })
        }
    }

    fn markdown_types() -> Result<Vec<OwnedCf>> {
        let class = OwnedCf::string("public.filename-extension")?;
        EXTENSIONS
            .iter()
            .map(|extension| {
                let extension = OwnedCf::string(&extension[1..])?;
                OwnedCf::new(unsafe {
                    UTTypeCreatePreferredIdentifierForTag(class.0, extension.0, ptr::null())
                })
            })
            .collect()
    }

    pub(super) fn is_associated() -> Result<bool> {
        let application = OwnedCf::string(VELOTYPE_APP_ID)?;
        for content in markdown_types()? {
            let handler = unsafe { LSCopyDefaultRoleHandlerForContentType(content.0, u32::MAX) };
            if handler.is_null() {
                return Ok(false);
            }
            let handler = OwnedCf::new(handler)?;
            if unsafe { CFEqual(handler.0, application.0) } == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn associate() -> Result<Option<&'static str>> {
        let executable = std::env::current_exe()?.canonicalize()?;
        let bundle = executable
            .ancestors()
            .find(|path| path.extension().is_some_and(|ext| ext == "app"))
            .context("run Velotype from its .app bundle to associate Markdown files")?;
        let path = OwnedCf::string(
            bundle
                .to_str()
                .context("application path is not valid Unicode")?,
        )?;
        let url =
            OwnedCf::new(unsafe { CFURLCreateWithFileSystemPath(ptr::null(), path.0, 0, 1) })?;
        ensure!(
            unsafe { LSRegisterURL(url.0, 1) } == 0,
            "cannot register the application bundle"
        );
        let application = OwnedCf::string(VELOTYPE_APP_ID)?;
        for content in markdown_types()? {
            let status = unsafe {
                LSSetDefaultRoleHandlerForContentType(content.0, u32::MAX, application.0)
            };
            ensure!(
                status == 0,
                "cannot associate Markdown files (OSStatus {status})"
            );
        }
        Ok(None)
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;
    pub(super) fn is_associated() -> Result<bool> {
        anyhow::bail!("file associations are not supported on this platform")
    }
    pub(super) fn associate() -> Result<Option<&'static str>> {
        anyhow::bail!("file associations are not supported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_file_association_paths_use_shell_syntax() {
        use std::path::Path;
        assert_eq!(
            windows_shell_path(Path::new(r"\\?\C:\中文 App\velotype.exe")).unwrap(),
            r"C:\中文 App\velotype.exe"
        );
        assert_eq!(
            windows_shell_path(Path::new(r"\\?\UNC\server\share\中文 App\velotype.exe")).unwrap(),
            r"\\server\share\中文 App\velotype.exe"
        );
        assert_eq!(
            windows_shell_path(Path::new(r"\\server\share\velotype.exe")).unwrap(),
            r"\\server\share\velotype.exe"
        );
        assert!(windows_shell_path(Path::new(r"\\?\Volume{guid}\velotype.exe")).is_err());
    }

    #[test]
    fn association_commands_quote_paths_and_do_not_claim_plain_text() {
        let path = std::path::Path::new("C:/Program Files/中文 & test/velotype.exe");
        assert_eq!(
            windows_open_command(path).unwrap(),
            "\"C:/Program Files/中文 & test/velotype.exe\" -- \"%1\""
        );
        assert!(windows_open_command(std::path::Path::new("bad\"path")).is_err());
        let entry = desktop_entry(std::path::Path::new(
            "/home/me/中文 \"$test`100%\\/velotype",
        ))
        .unwrap();
        assert!(entry.contains(r#"Exec="/home/me/中文 \\"\\$test\\`100%%\\\\/velotype" -- %F"#));
        assert!(entry.contains("MimeType=text/markdown;text/x-markdown;"));
        assert!(!entry.contains("text/plain"));
        assert!(desktop_entry(std::path::Path::new("/bad\nExec=evil")).is_err());
        assert_eq!(EXTENSIONS, [".md"]);
    }
}
