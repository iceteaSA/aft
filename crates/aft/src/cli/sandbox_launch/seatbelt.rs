//! Minimal macOS Seatbelt binding for the sandbox launcher.
//!
//! This module contains the entire unsafe FFI surface. `apply()` constructs a
//! NUL-free profile, passes stable pointers to `sandbox_init`, and frees only
//! error buffers allocated by that API.

use aft::sandbox_profile::SandboxProfile;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;
use std::ptr;

unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

pub(super) fn apply(profile: &SandboxProfile) -> Result<(), String> {
    let source = render_profile(profile)?;
    let source = CString::new(source)
        .map_err(|_| "generated Seatbelt profile contains a NUL byte".to_string())?;
    let mut error_buffer = ptr::null_mut();

    // SAFETY: `source` is a live NUL-terminated string, flags=0 selects raw
    // profile mode, and `error_buffer` is a valid out-pointer.
    let result = unsafe { sandbox_init(source.as_ptr(), 0, &mut error_buffer) };
    if result == 0 {
        return Ok(());
    }

    let message = if error_buffer.is_null() {
        format!("sandbox_init returned error code {result}")
    } else {
        // SAFETY: sandbox_init documents a NUL-terminated error string on
        // failure, owned by the sandbox API and released below.
        let message = unsafe { CStr::from_ptr(error_buffer) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: this non-null pointer came directly from sandbox_init.
        unsafe { sandbox_free_error(error_buffer) };
        message
    };
    Err(message)
}

fn render_profile(profile: &SandboxProfile) -> Result<String, String> {
    let mut source = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");

    for root in profile.write_allow_roots() {
        source.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            escape_path(root)?
        ));
    }
    for path in &profile.write_deny_nested {
        if !path.exists() {
            continue;
        }
        source.push_str(&format!(
            "(deny file-write* ({} \"{}\"))\n",
            path_filter(path),
            escape_path(path)?
        ));
    }
    for path in &profile.read_deny {
        if !path.exists() {
            continue;
        }
        source.push_str(&format!(
            "(deny file-read* ({} \"{}\"))\n",
            path_filter(path),
            escape_path(path)?
        ));
    }
    for path in &profile.socket_deny {
        if !path.exists() {
            continue;
        }
        source.push_str(&format!(
            "(deny network-outbound (path \"{}\"))\n",
            escape_path(path)?
        ));
    }

    Ok(source)
}

fn path_filter(path: &Path) -> &'static str {
    if path.is_dir() {
        "subpath"
    } else {
        "literal"
    }
}

fn escape_path(path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or_else(|| format!("Seatbelt path is not valid UTF-8: {}", path.display()))?;
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            character if character.is_control() => {
                return Err(format!(
                    "Seatbelt path contains a control character: {path:?}"
                ));
            }
            character => escaped.push(character),
        }
    }
    Ok(escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_denies_follow_write_allows() {
        let root = tempfile::tempdir().expect("temp root");
        let project = root.path().join("project");
        let nested_deny = project.join(".git");
        let task_temp = root.path().join("task");
        std::fs::create_dir_all(&nested_deny).expect("nested deny directory");
        std::fs::create_dir_all(&task_temp).expect("task temp directory");
        let profile = SandboxProfile {
            v: 1,
            writable_roots: vec![project],
            write_deny_nested: vec![nested_deny],
            read_deny: Vec::new(),
            socket_deny: Vec::new(),
            cache_roots: Vec::new(),
            temp_dir: task_temp,
        };
        let source = render_profile(&profile).expect("render profile");
        let allow = source.find("(allow file-write*").expect("write allow");
        let deny = source
            .rfind("(deny file-write*")
            .expect("nested write deny");
        assert!(deny > allow, "nested deny must be the last matching rule");
    }
}
