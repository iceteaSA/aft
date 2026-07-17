# Spike B: brush as AFT's Windows bash shell

**Date:** 2026-07-17
**Machine:** Asus ROG Ally, `ufuka@192.168.1.42`
**AFT clone:** `%USERPROFILE%\aft`, current shell lane observed from the
existing resolver
**Brush:** upstream `reubeno/brush`, `brush-shell-v0.4.0` (commit
`96a26d0c66cbc018a1517e9562944418fef5b272`)
**Disposition:** report-only; no AFT product code changed.

## Executive result

Brush is a credible replacement for the *command-language* part of the
Windows fallback ladder: it passed the Bash-shaped syntax and exit-status
cases that failed under the Ally's selected Windows PowerShell 5.1. It is not
drop-in for AFT's current background-task contract. In particular, the
background wrapper needs an atomic marker write, and brush v0.4.0 did not have
an `mv` command on this Windows build. Process-tree ownership, job-object
breakaway, watchdog identity, and kill policy would remain AFT responsibilities.

**Recommendation: do not adopt as the next fallback change.** Keep brush as a
promising sidecar experiment. A production trial needs a Windows release
artifact, an explicit wrapper/marker strategy (preferably an embedder callback
or native helper rather than shelling out to `mv`), and an AFT-owned process
identity/tree-control layer.

## Provisioning story

The latest GitHub release checked was `brush-shell-v0.4.0`. Its assets included
Apple and Linux archives only; there was no `windows`, `pc-windows`, or
`*-windows-*` asset. The release's `cargo-binstall` metadata uses a target
template, but the corresponding Windows object is absent, so `cargo binstall`
cannot provision this release on Windows.

Fallback provisioning succeeded from a shallow clone on the Ally:

* Rust: `rustc 1.96.1 (31fca3adb 2026-06-26)`.
* Command: `cargo build --release --locked -p brush-shell`.
* Build time: about 2m37s on the warm/partially cached Ally run.
* Result: `brush.exe`, 6,217,216 bytes, copied to `%USERPROFILE%`.

The binary was removed after the measurements. A sidecar is technically small
enough for the existing platform-package approach (about 6.2 MiB for this
build), but release CI would need to publish at least the Windows MSVC target,
checksum/sign it, map it in the downloader/cache, and define update policy.
This spike did not establish a Windows ARM64 artifact or build requirement.

## Ally battery

The current-ladder run used the same command strings as the brush run. On this
box `$SHELL` was `c:\windows\system32\cmd.exe`; AFT correctly ignores that as a
non-POSIX `$SHELL` value. The observed candidate order was:

`powershell.exe` (Windows PowerShell 5.1) → Git Bash → `cmd.exe`.

`pwsh.exe` was not present. The table records exit behavior; “pass” means the
command behaved as a Bash task normally expects, not merely that a process
returned zero.

| Battery cell | brush | Current ladder (selected PowerShell 5.1) |
|---|---|---|
| Pipe: `echo alpha \| findstr alpha` | **Pass**, exit 0 | Pass, exit 0 |
| `&&` / `||` | **Pass**, `chain-ok`, exit 0 | **Fail**, parser error; exit 1 |
| `exit 37` / `$?` fidelity | **Pass**, exit 37 | Pass for explicit `exit 37` |
| Single/double quoting and `$HOME` | **Pass**; single quote stayed literal, double quote expanded | **Fail**, `printf` not found |
| Environment expansion | **Pass**, injected value returned | **Fail**, Bash `$VAR` syntax plus `printf` unsupported |
| Command substitution | **Pass**, returned `sub` | **Fail**, `printf` not found |
| File redirection | **Pass**, file created with `redir\n` | Not captured in the runner after its nested-case bookkeeping error; the same Bash `printf` command is known to fail under this shell |
| `2>&1` | **Pass**, combined `err` and `out` | **Fail**, PowerShell parse errors |
| `C:\Windows` and `C:/Windows` args | **Pass**, both preserved | Not meaningful with Bash `printf`; native PowerShell path handling is separate |
| `.exe` resolution / `where` | **Pass**, `where.exe cmd.exe` returned `C:\Windows\System32\cmd.exe`; `command -v cmd.exe` also returned a native path | `where.exe` passed; `command -v` was not a Bash-equivalent probe |
| Missing command | **Pass**, exit 127 and `command not found` | **Fail for Bash contract**, exit 1 and a PowerShell `CommandNotFoundException` |
| Syntax error | **Pass**, exit 2 with a concise colored diagnostic | **Fail for Bash contract**, exit 1 with a PowerShell parser diagnostic |
| UTF-8 | ASCII/control capture was clean; Unicode probe was inconclusive because the Windows PowerShell 5.1 harness read a UTF-8-no-BOM test script as ANSI | Not comparable; same harness encoding caveat |
| AFT-style marker body | Body exit status propagated (23), but marker failed because `mv` was not found | No Bash wrapper semantics |

The brush diagnostics included ANSI color escape sequences even with redirected
stderr. AFT should either strip these for task files or invoke brush with a
non-color setting if that is configurable.

## AFT-shaped wrappers and seam

The production AFT code has no brush override. `shell_candidates()` is cached
and its production order is `$SHELL`, `pwsh.exe`, `powershell.exe`, Git Bash,
then `cmd.exe` (`crates/aft/src/windows_shell.rs:241-264, 271-344`). The
`shell_candidates_with` closure is a test seam, not an environment or config
override.

The background path writes a per-shell wrapper and redirects stdout/stderr to
task files (`crates/aft/src/bash_background/registry.rs:4114-4184`). PowerShell
uses a temp file plus `Move-Item`; cmd uses `move /Y`; POSIX uses `mv`
(`crates/aft/src/windows_shell.rs:142-237`). A brush candidate would need a
new wrapper case and a marker implementation. The direct experiment showed
that a brush command can write the temp marker and return the intended exit
code, but the final `mv` fails with `command not found`. That is a concrete
integration blocker, not a general brush-language failure.

The PTY path has the same candidate/wrapper dependency
(`crates/aft/src/bash_background/pty_process.rs:42-87`). No end-to-end
`tests/windows-e2e` run was attempted: trying brush would require changing the
candidate list or adding a new override, which this report-only spike was
explicitly not permitted to do.

## Process-control findings

The brush process successfully spawned native Windows children: the harness
observed two descendants for a long-running `cmd.exe`/`ping` command. Running
`taskkill /PID <brush-pid> /T /F` reaped the brush root and its descendants;
the root was no longer alive afterward. The current PowerShell runner showed
the same observable result. This proves that AFT's existing tree-kill command
can reach a brush process, but it does not prove Ctrl-C or job-object behavior.

Ctrl-C was not tested through the SSH batch session because it did not provide
an interactive console. AFT's current Windows termination helper is
`taskkill /PID /T /F` (`crates/aft/src/bash_background/process.rs:48-61`),
and liveness is an `OpenProcess` plus `GetExitCodeProcess`/`STILL_ACTIVE` probe
(`process.rs:75-105`).

The oh-my-pi brush embedding is useful evidence about the remaining cost:

* `crates/pi-shell/src/windows.rs:12-74` reconstructs Git paths itself. It
  reads Git installation roots from the registry and `where git`, translates
  `/usr/bin`, `/mingw64/bin`, and `/c/...` MSYS entries to Windows paths, and
  de-duplicates the resulting PATH. Brush-core did not make that embedding
  policy disappear.
* `crates/pi-shell/src/process.rs:691-1270` owns Windows process identity and
  tree walking with Toolhelp snapshots, stable process handles, and creation
  timestamps. It explicitly avoids the `STILL_ACTIVE == 259` ambiguity with
  `WaitForSingleObject` (`process.rs:958-985`).
* The same file has no Windows process-group abstraction (`process.rs:967-969,
  1216-1220`) and implements hard tree termination by enumerating descendants
  and calling `TerminateProcess` child-first (`process.rs:897-965`). This is
  the Windows control surface an AFT brush integration would still need to
  specify; brush itself is not a watchdog.

## Adoption risks and next gate

1. **Distribution:** no released Windows artifact; source builds are too slow
   for an ordinary first-run fallback and require a Rust toolchain.
2. **Wrapper contract:** `mv` is absent in the default Windows brush binary;
   AFT's atomic exit marker cannot be copied verbatim.
3. **Process identity:** AFT's existing liveness probe is weaker than the
   stable-handle/creation-time approach used by oh-my-pi. Brush does not solve
   PID reuse, descendant enumeration, or job-object breakaway.
4. **PATH semantics:** ordinary Windows PATH and `.exe` lookup worked, but the
   MSYS path translation/deduplication work in oh-my-pi was not present in the
   AFT seam and was not fully exercised here.
5. **Encoding/diagnostics:** redirected ASCII was clean, but a UTF-8 probe was
   confounded by Windows PowerShell 5.1's no-BOM script decoding; repeat with a
   byte-level harness before shipping. Brush diagnostics may include ANSI codes
   in redirected stderr.
6. **Compatibility:** brush won decisively on Bash syntax, but that is only one
   axis. Agents may rely on Windows-native commands and PowerShell syntax,
   where the current ladder can still be useful.

If pursued, the next spike should first add a throwaway AFT adapter that uses a
native marker writer (or brush-core completion callback), then run the three
existing Windows E2E scenarios plus explicit Ctrl-C/job-object and byte-level
encoding tests. Only after that should AFT add a production candidate or a
platform-package sidecar.
