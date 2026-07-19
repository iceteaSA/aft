#[cfg(any(target_os = "macos", target_os = "linux"))]
use aft::sandbox_profile::SandboxProfile;
use std::ffi::OsString;
use std::fmt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::fs::{File, OpenOptions};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command;

#[cfg(target_os = "linux")]
mod landlock_backend;
#[cfg(target_os = "macos")]
mod seatbelt;

pub const SANDBOX_UNAVAILABLE_EXIT_CODE: i32 = 78;

#[derive(Debug)]
pub struct SandboxLaunchError {
    message: String,
    exit_code: i32,
}

impl SandboxLaunchError {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 2,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: format!("sandbox_unavailable: {}", message.into()),
            exit_code: SANDBOX_UNAVAILABLE_EXIT_CODE,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for SandboxLaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SandboxLaunchError {}

pub fn run(args: Vec<OsString>) -> Result<(), SandboxLaunchError> {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = args;
        return Err(SandboxLaunchError::unavailable(
            "sandbox-launch is supported only on macOS and Linux",
        ));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let args = LaunchArgs::parse(args)?;
        if args.help {
            print_usage();
            return Ok(());
        }
        if args.support {
            return print_support();
        }

        if args.command.is_empty() {
            return Err(SandboxLaunchError::usage("missing target command after --"));
        }

        let profile = match (args.profile_fd, args.profile_file.as_deref()) {
            (Some(fd), None) => read_profile(fd)?,
            (None, Some(path)) => read_profile_file(path)?,
            (None, None) => {
                return Err(SandboxLaunchError::usage(
                    "missing required --profile-fd <fd> or --profile-file <path>",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(SandboxLaunchError::usage(
                    "--profile-fd and --profile-file are mutually exclusive",
                ));
            }
        }
        .canonicalize_for_launch()
        .map_err(|error| SandboxLaunchError::usage(error.to_string()))?;
        if let Err(error) = apply_sandbox(&profile) {
            if let Some(path) = args.failure_marker.as_deref() {
                let _ = write_failure_marker(path);
            }
            return Err(error);
        }
        exec_target(&args.command)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Default)]
struct LaunchArgs {
    profile_fd: Option<i32>,
    profile_file: Option<PathBuf>,
    failure_marker: Option<PathBuf>,
    command: Vec<OsString>,
    help: bool,
    support: bool,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl LaunchArgs {
    fn parse(args: Vec<OsString>) -> Result<Self, SandboxLaunchError> {
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--" {
                parsed.command.extend(args);
                break;
            }
            if arg == "--help" || arg == "-h" {
                parsed.help = true;
                continue;
            }
            if arg == "--support" {
                parsed.support = true;
                continue;
            }
            if arg == "--profile-fd" {
                let value = args.next().ok_or_else(|| {
                    SandboxLaunchError::usage("--profile-fd requires an integer value")
                })?;
                parsed.profile_fd = Some(parse_fd(&value)?);
                continue;
            }
            if let Some(value) = arg
                .to_str()
                .and_then(|value| value.strip_prefix("--profile-fd="))
            {
                parsed.profile_fd = Some(parse_fd(&OsString::from(value))?);
                continue;
            }
            if arg == "--profile-file" {
                let value = args.next().ok_or_else(|| {
                    SandboxLaunchError::usage("--profile-file requires a path value")
                })?;
                parsed.profile_file = Some(PathBuf::from(value));
                continue;
            }
            if let Some(value) = arg
                .to_str()
                .and_then(|value| value.strip_prefix("--profile-file="))
            {
                parsed.profile_file = Some(PathBuf::from(value));
                continue;
            }
            if arg == "--failure-marker" {
                let value = args.next().ok_or_else(|| {
                    SandboxLaunchError::usage("--failure-marker requires a path value")
                })?;
                parsed.failure_marker = Some(PathBuf::from(value));
                continue;
            }
            if let Some(value) = arg
                .to_str()
                .and_then(|value| value.strip_prefix("--failure-marker="))
            {
                parsed.failure_marker = Some(PathBuf::from(value));
                continue;
            }
            return Err(SandboxLaunchError::usage(format!(
                "unknown sandbox-launch argument: {}",
                arg.to_string_lossy()
            )));
        }
        Ok(parsed)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_fd(value: &OsString) -> Result<i32, SandboxLaunchError> {
    let value = value
        .to_str()
        .ok_or_else(|| SandboxLaunchError::usage("--profile-fd must be valid UTF-8"))?;
    let fd = value
        .parse::<i32>()
        .map_err(|_| SandboxLaunchError::usage("--profile-fd must be a non-negative integer"))?;
    if fd < 0 {
        return Err(SandboxLaunchError::usage(
            "--profile-fd must be a non-negative integer",
        ));
    }
    Ok(fd)
}

#[cfg(unix)]
fn read_profile(fd: RawFd) -> Result<SandboxProfile, SandboxLaunchError> {
    // Validate first because constructing `File` ownership from a closed raw
    // descriptor triggers Rust's IO-safety abort instead of a recoverable error.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(SandboxLaunchError::runtime(format!(
            "sandbox profile fd {fd} is not open: {}",
            std::io::Error::last_os_error()
        )));
    }

    // The launcher owns this inherited descriptor. Dropping the File before
    // parsing guarantees the target cannot inherit profile contents.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        SandboxLaunchError::runtime(format!("failed to read sandbox profile fd {fd}: {error}"))
    })?;
    drop(file);
    serde_json::from_slice(&bytes).map_err(|error| {
        SandboxLaunchError::usage(format!("invalid sandbox profile JSON: {error}"))
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_profile_file(path: &Path) -> Result<SandboxProfile, SandboxLaunchError> {
    let mut file = File::open(path).map_err(|error| {
        SandboxLaunchError::runtime(format!(
            "failed to open sandbox profile file {}: {error}",
            path.display()
        ))
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        SandboxLaunchError::runtime(format!(
            "failed to read sandbox profile file {}: {error}",
            path.display()
        ))
    })?;
    drop(file);
    let _ = std::fs::remove_file(path);
    serde_json::from_slice(&bytes).map_err(|error| {
        SandboxLaunchError::usage(format!("invalid sandbox profile JSON: {error}"))
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_failure_marker(path: &Path) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(b"sandbox_unavailable")?;
    file.sync_all()
}

#[cfg(target_os = "macos")]
fn apply_sandbox(profile: &SandboxProfile) -> Result<(), SandboxLaunchError> {
    seatbelt::apply(profile).map_err(|error| {
        SandboxLaunchError::unavailable(format!("failed to apply Seatbelt profile: {error}"))
    })
}

#[cfg(target_os = "linux")]
fn apply_sandbox(profile: &SandboxProfile) -> Result<(), SandboxLaunchError> {
    let applied = landlock_backend::apply(profile).map_err(|error| {
        SandboxLaunchError::unavailable(format!("failed to apply Landlock ruleset: {error}"))
    })?;
    emit_linux_warning(profile, applied);
    Ok(())
}

#[cfg(target_os = "linux")]
fn emit_linux_warning(profile: &SandboxProfile, applied: landlock_backend::AppliedLandlock) {
    let mut unenforced = Vec::new();
    if !profile.write_deny_nested.is_empty() {
        unenforced.push("nested_write_deny");
    }
    if !profile.read_deny.is_empty() {
        unenforced.push("read_deny");
    }
    if !profile.socket_deny.is_empty() {
        unenforced.push("socket_deny");
    }

    eprint!("sandbox-launch: unenforced=[{}]", unenforced.join(","));
    if applied.effective_abi < landlock_backend::REQUIRED_WRITE_ABI || applied.partially_enforced {
        eprint!(
            " landlock_abi={} landlock_required={}",
            landlock_backend::abi_label(applied.effective_abi),
            landlock_backend::abi_label(landlock_backend::REQUIRED_WRITE_ABI)
        );
    }
    eprintln!();
}

#[cfg(unix)]
fn exec_target(command: &[OsString]) -> Result<(), SandboxLaunchError> {
    let error = Command::new(&command[0]).args(&command[1..]).exec();
    Err(SandboxLaunchError::runtime(format!(
        "failed to exec target {}: {error}",
        command[0].to_string_lossy()
    )))
}

#[cfg(target_os = "macos")]
fn print_support() -> Result<(), SandboxLaunchError> {
    print_json(serde_json::json!({
        "platform": "macos",
        "supported": true,
        "backend": "seatbelt"
    }))
}

#[cfg(target_os = "linux")]
fn print_support() -> Result<(), SandboxLaunchError> {
    let applied = landlock_backend::probe().map_err(|error| {
        SandboxLaunchError::unavailable(format!("failed to probe Landlock: {error}"))
    })?;
    print_json(serde_json::json!({
        "platform": "linux",
        "supported": true,
        "backend": "landlock",
        "landlock_abi": landlock_backend::abi_label(applied.effective_abi),
        "target_abi": landlock_backend::abi_label(landlock_backend::TARGET_ABI),
        "required_write_abi": landlock_backend::abi_label(landlock_backend::REQUIRED_WRITE_ABI),
        "partially_enforced": applied.partially_enforced
    }))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn print_json(value: serde_json::Value) -> Result<(), SandboxLaunchError> {
    println!(
        "{}",
        serde_json::to_string(&value).map_err(|error| {
            SandboxLaunchError::runtime(format!("failed to serialize support information: {error}"))
        })?
    );
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn print_usage() {
    println!("Usage: aft sandbox-launch --profile-fd <fd> -- <command> [args...]");
    println!("       aft sandbox-launch --profile-file <path> -- <command> [args...]");
    println!("       aft sandbox-launch --support");
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    fn os_args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_profile_fd_and_command() {
        let args = LaunchArgs::parse(os_args(&[
            "--profile-fd",
            "9",
            "--",
            "/bin/bash",
            "-c",
            "true",
        ]))
        .expect("valid launcher arguments");
        assert_eq!(args.profile_fd, Some(9));
        assert_eq!(args.profile_file, None);
        assert_eq!(args.failure_marker, None);
        assert_eq!(args.command, os_args(&["/bin/bash", "-c", "true"]));
    }

    #[test]
    fn parses_profile_file_and_command() {
        let args = LaunchArgs::parse(os_args(&[
            "--profile-file",
            "/tmp/task.sandbox-profile.json",
            "--failure-marker",
            "/tmp/task.sandbox-unavailable",
            "--",
            "/bin/sh",
            "-c",
            "true",
        ]))
        .expect("valid launcher arguments");
        assert_eq!(
            args.profile_file,
            Some(PathBuf::from("/tmp/task.sandbox-profile.json"))
        );
        assert_eq!(args.profile_fd, None);
        assert_eq!(
            args.failure_marker,
            Some(PathBuf::from("/tmp/task.sandbox-unavailable"))
        );
        assert_eq!(args.command, os_args(&["/bin/sh", "-c", "true"]));
    }

    #[test]
    fn rejects_negative_profile_fd() {
        let error = LaunchArgs::parse(os_args(&["--profile-fd", "-1", "--", "true"]))
            .expect_err("negative descriptor must fail");
        assert_eq!(error.exit_code(), 2);
    }
}
