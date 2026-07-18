# S1b first-party sandbox launcher

Date: 2026-07-18

## Result

`aft sandbox-launch --profile-fd <fd> -- <command> ...` now applies a first-party platform sandbox before AFT initializes logging, PATH discovery, or application threads, then replaces itself with the target using `exec`.

- macOS uses a small raw `sandbox_init` FFI module and a generated Seatbelt profile.
- Linux uses exactly pinned `landlock = 0.4.5` in `CompatLevel::BestEffort` mode.
- Other platforms return `sandbox_unavailable` with exit code 78.
- Any Seatbelt or Landlock application failure returns `sandbox_unavailable` with exit code 78 and does not execute the target.
- The inherited JSON descriptor is checked with `F_GETFD` before `File::from_raw_fd`, read, and closed before sandbox application. The closed-descriptor integration test exits normally instead of hitting Rust's IO-safety abort.

The profile wire shape is:

```json
{
  "v": 1,
  "writable_roots": [],
  "write_deny_nested": [],
  "read_deny": [],
  "socket_deny": [],
  "cache_roots": [],
  "temp_dir": ""
}
```

`SandboxProfile::build` canonicalizes existing paths before serialization. The launcher validates and canonicalizes again. Writable roots, cache roots, and `temp_dir` must be existing directories. Missing deny-list targets are accepted as normalized absolute paths and skipped by the platform applier if still absent at launch.

## Asymmetric guarantee surface

### macOS / Seatbelt

The profile starts from broad reads and ordinary process/network access, denies all filesystem writes, then grants writes only beneath `writable_roots`, `cache_roots`, and `temp_dir`. Final rules deny writes beneath `write_deny_nested`, reads beneath `read_deny`, and outbound connects to pathname sockets in `socket_deny`.

The deny rules follow the allow rules so Seatbelt's observed last-match behavior protects nested `.git` and `.cortexkit` directories. The native probes verify this ordering on macOS 26.5.2.

### Linux / Landlock

Landlock handles only filesystem mutation rights. The ruleset grants those rights beneath `writable_roots`, `cache_roots`, and `temp_dir`; reads and networking remain unrestricted.

Landlock is additive, so Linux intentionally does **not** claim enforcement for:

- `write_deny_nested`
- `read_deny`
- pathname `socket_deny`

Every Linux launcher invocation writes exactly one machine-readable warning line. For the probe profile it is:

```text
sandbox-launch: unenforced=[nested_write_deny,read_deny,socket_deny]
```

The pinned crate's best known ABI is V7. The launcher requests every filesystem mutation right through ABI V3 (including cross-directory refer and truncate) while leaving device ioctls outside the pathname-write contract. On a supported kernel below V3, BestEffort still applies the available rights and extends the same warning line with `landlock_abi=... landlock_required=V3`. A wholly unenforced or unavailable ruleset fails closed.

## Probe verdicts

Every probe drives the real launcher chain. The macOS run used the debug launcher for the full battery; P10 was repeated with the release launcher. The Linux runner builds both profiles and repeats P10 in release mode.

| Probe | Contract | macOS 26.5.2 / Seatbelt | Linux 7.0.11 OrbStack / Landlock V7 |
|---|---|---|---|
| P1 write in project | ALLOWED | **ALLOWED — pass** | **ALLOWED — pass** |
| P2 `rm -rf` outside allowlist | DENIED | **DENIED — pass**; sentinel survived | **DENIED — pass**; sentinel survived |
| P3 nested shell write outside | DENIED | **DENIED — pass** | **DENIED — pass**; restriction inherited |
| P4 write under `.git` / `.cortexkit` | macOS DENIED; Linux ALLOWED + warning | **DENIED — pass** for both | **ALLOWED — pass**; `nested_write_deny` warning asserted |
| P5 secret read / ordinary read | macOS DENIED / ALLOWED; Linux ALLOWED / ALLOWED + warning | **DENIED / ALLOWED — pass** | **ALLOWED / ALLOWED — pass**; `read_deny` warning asserted |
| P6 project symlink to outside write | DENIED | **DENIED — pass** | **DENIED — pass** |
| P7 pre-existing hardlink alias | record | **ALLOWED**; outside inode changed | **ALLOWED**; outside inode changed |
| P8 fake Docker and agent socket connects | macOS DENIED; Linux ALLOWED + warning | **DENIED — pass** for both sockets | **ALLOWED — pass**; `socket_deny` warning asserted |
| P9 PTY child and nested child | DENIED outside write | **DENIED — pass** | **DENIED — pass** |
| P10 20-spawn debug overhead | measure | bare 1.825 ms; launcher 17.759 ms; **+15.934 ms** | bare 0.552 ms; launcher 2.681 ms; **+2.129 ms** |
| P10 20-spawn release overhead | measure | bare 2.132 ms; launcher 14.538 ms; **+12.406 ms** | bare 0.627 ms; launcher 1.825 ms; **+1.198 ms** |

P7 remains a documented path-sandbox limitation: a pre-existing hardlink beneath an allowed root can modify the same inode through its allowed alias even when another pathname lies outside the allowlist.

`portable_pty` closes non-stdio descriptors, so P9 uses the validated shell trampoline: the shell opens the serialized profile on fd 9 and immediately execs the real launcher. Product PTY wiring still needs an explicit descriptor-preservation strategy; this slice does not change product PTY wiring.

## ABI, size, and dependency measurements

Linux support output:

```json
{"platform":"linux","supported":true,"backend":"landlock","landlock_abi":"V7","target_abi":"V7","required_write_abi":"V3","partially_enforced":false}
```

The Linux container kernel was:

```text
Linux 7.0.11-orbstack-00360-gc9bc4d96ac70 aarch64
```

Release binary size on the macOS host, measured with `stat -f '%z' target/release/aft`:

| Build | Bytes |
|---|---:|
| Base `387a22cb` | 71,434,480 |
| First-party launcher | 71,500,720 |
| Delta | **+66,240 (+0.093%)** |

Cargo lockfile package count:

| Build | Packages |
|---|---:|
| Base | 485 |
| First-party launcher | 488 |
| Delta | **+3** |

The three added lockfile packages are `landlock`, `enumflags2`, and `enumflags2_derive`. `libc` moves from 0.2.183 to 0.2.186 because landlock 0.4.5 requires it, but that does not add another package entry. The rejected nono spike raised this repository's lockfile count from 485 to 567 (+82 package entries, with an 88-package Sigstore-heavy dependency graph before overlap), so the first-party implementation removes the dependency-size blocker.

## Verification commands

```text
cargo build -p agent-file-tools --bin aft
cargo build --release -p agent-file-tools --bin aft
cargo test -p agent-file-tools --test sandbox_launch_probe -- --test-threads=1 --nocapture
cargo test --release -p agent-file-tools --test sandbox_launch_probe p10_launcher_latency_delta_is_measured_over_twenty_iterations -- --exact --nocapture
spikes/sandbox-s1/run-linux.sh
cargo clippy -p agent-file-tools --all-targets -- -D warnings
cargo fmt --all -- --check
```

Native macOS battery: 13 passed. Linux battery: 13 passed and the three macOS-only expected-deny assertions were ignored; the Linux companion assertions for P4, P5, and P8 passed with the structured warning.

Seatbelt remains a private macOS API despite its long-lived `sandbox_init` interface. The FFI is intentionally isolated in one module so this risk and its unsafe boundary stay auditable.
