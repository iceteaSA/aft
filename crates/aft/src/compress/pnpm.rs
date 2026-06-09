use crate::compress::generic::GenericCompressor;
use crate::compress::{CompressionResult, Compressor, Specificity};

pub struct PnpmCompressor;

impl Compressor for PnpmCompressor {
    fn specificity(&self) -> Specificity {
        Specificity::PackageManager
    }

    fn matches(&self, command: &str) -> bool {
        command
            .split_whitespace()
            .next()
            .is_some_and(|head| head == "pnpm")
    }

    fn compress_with_exit_code(
        &self,
        command: &str,
        output: &str,
        _exit_code: Option<i32>,
    ) -> CompressionResult {
        match pnpm_subcommand(command).as_deref() {
            Some("install" | "i" | "add" | "remove") => compress_package(output).into(),
            Some("run" | "test" | "build") => GenericCompressor::compress_output(output).into(),
            _ => GenericCompressor::compress_output(output).into(),
        }
    }
}

/// Known pnpm subcommands. Same rationale as bun.rs::BUN_SUBCOMMANDS —
/// using a whitelist instead of "first non-flag" avoids returning flag
/// values like `--filter <pattern>` as the subcommand for command lines
/// such as `pnpm --filter ./packages/foo test`.
const PNPM_SUBCOMMANDS: &[&str] = &[
    "install",
    "i",
    "add",
    "remove",
    "rm",
    "uninstall",
    "un",
    "update",
    "up",
    "upgrade",
    "outdated",
    "audit",
    "outdated-of",
    "publish",
    "pack",
    "run",
    "test",
    "t",
    "exec",
    "x",
    "dlx",
    "create",
    "init",
    "build",
    "start",
    "link",
    "unlink",
    "view",
    "info",
    "show",
    "config",
    "help",
    "version",
    "ls",
    "list",
    "list-modules",
    "list-bin",
    "ping",
    "whoami",
    "login",
    "logout",
    "deploy",
    "dedupe",
    "fetch",
    "import",
    "patch",
    "patch-commit",
    "patch-remove",
    "prune",
    "rebuild",
    "recursive",
    "root",
    "store",
    "why",
    "doctor",
    "env",
    "server",
    "setup",
];

fn pnpm_subcommand(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .skip_while(|token| *token != "pnpm")
        .skip(1)
        .find(|token| PNPM_SUBCOMMANDS.contains(token))
        .map(ToString::to_string)
}

fn compress_package(output: &str) -> String {
    let mut result = Vec::new();
    let mut progress_seen = 0usize;
    let mut up_to_date_seen = false;

    for line in output.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Progress: resolved ") {
            progress_seen += 1;
            if progress_seen > 2 {
                continue;
            }
        }
        if trimmed == "Already up-to-date" {
            if up_to_date_seen {
                continue;
            }
            up_to_date_seen = true;
        }
        if trimmed.contains("WARN GET_NO_AUTH")
            || trimmed.starts_with("ERR_PNPM_")
            || trimmed.starts_with("Progress: resolved ")
            || trimmed == "Already up-to-date"
            || trimmed.starts_with("dependencies:")
            || trimmed.starts_with("devDependencies:")
            || trimmed.starts_with("Done in ")
        {
            result.push(line.to_string());
        }
    }

    trim_trailing_lines(&result.join("\n"))
}

fn trim_trailing_lines(input: &str) -> String {
    input
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}
