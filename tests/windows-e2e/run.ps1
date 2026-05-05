# =============================================================================
# Windows E2E test for AFT plugin running inside OpenCode.
#
# Mirror of `tests/docker/test-e2e.sh`, adapted for native Windows. Runs on
# both GH Actions `windows-latest` runners and locally inside a Windows VM
# (UTM/Parallels/Hyper-V) -- see `tests/windows-e2e/README.md`.
#
# What this catches that the Linux harness cannot:
#   * issue #26 -- bash transport timeout on Windows (process spawn overhead is
#     materially higher than Unix; the bridge's `max(30s, requested+5s)`
#     timeout calc may not leave enough headroom)
#   * tar.exe ZIP extraction path in onnx-runtime.ts (Windows uses tar.exe via
#     execFileSync; Unix uses `unzip`)
#   * Windows file-URI handling in lsp/client.rs (\\?\ extended paths)
#   * Lock-file recovery on Windows (no isProcessAlive -- falls back to mtime)
#   * Path-separator handling in trigram index, search, glob normalization
#
# Required env (set by tests.yml or local bootstrap EXE):
#   AFT_BINARY_PATH  -- absolute path to the locally-built aft.exe to test
#   AFT_PLUGIN_DIST  -- absolute path to packages/opencode-plugin/dist/
#
# Exit codes:
#   0  -- all checks passed
#   1  -- at least one check failed
#   2  -- environment setup failed (couldn't install OpenCode/aimock/etc.)
# =============================================================================

# Strict mode -- uncaught errors should fail the script. We intentionally do
# NOT use `Set-StrictMode -Version Latest` because some PowerShell module
# imports trigger 'variable not set' under strict mode.
$ErrorActionPreference = "Stop"

# Color helpers. PowerShell 7+ has Write-Host -ForegroundColor; we use the
# fallback escape-sequence form so the script works on Windows PowerShell 5
# too (older Windows installs).
function Write-Pass($label) { Write-Host "  PASS [$label]" -ForegroundColor Green }
function Write-Fail($label) { Write-Host "  FAIL [$label]" -ForegroundColor Red }
function Write-Skip($label) { Write-Host "  SKIP [$label]" -ForegroundColor Yellow }
function Write-Warn($label) { Write-Host "  WARN [$label]" -ForegroundColor Yellow }

$script:Pass = 0
$script:Fail = 0

# Track whether a check passed. `Check` increments the appropriate counter;
# `WarnCheck` only counts passes (used for environment-dependent assertions
# that aren't release-blocking).
#
# We wrap the condition invocation in a try/catch because `$ErrorActionPreference
# = "Stop"` (set above) escalates non-terminating errors from cmdlets like
# Select-String to terminating ones. If a check's body hits a missing path or
# parse error we want to record FAIL with the message, not crash the whole
# script. -ErrorAction SilentlyContinue inside the cmdlet is NOT enough —
# under "Stop" it's still treated as terminating. The try/catch is the only
# robust fix.
function Check {
    param([string]$Label, [scriptblock]$Condition)
    try {
        if (& $Condition) {
            Write-Pass $Label
            $script:Pass++
        } else {
            Write-Fail $Label
            $script:Fail++
        }
    } catch {
        Write-Fail "$Label  (check raised: $($_.Exception.Message))"
        $script:Fail++
    }
}

function WarnCheck {
    param([string]$Label, [scriptblock]$Condition)
    try {
        if (& $Condition) {
            Write-Pass $Label
            $script:Pass++
        } else {
            Write-Warn "$Label (non-blocking)"
        }
    } catch {
        Write-Warn "$Label (non-blocking; check raised: $($_.Exception.Message))"
    }
}

# Safe Select-String over a possibly-missing log file. Returns $false when the
# log doesn't exist, instead of throwing. Use this in Check/WarnCheck bodies
# whenever the log file might not have been created (plugin failed to load,
# bridge never started, etc.).
function LogContains {
    param([string]$Path, [string]$Pattern)
    if (-not (Test-Path $Path)) { return $false }
    return [bool] (Select-String -Path $Path -Pattern $Pattern -Quiet -ErrorAction SilentlyContinue)
}

# ---------------------------------------------------------------------------
# Environment validation
# ---------------------------------------------------------------------------

if (-not $env:AFT_BINARY_PATH -or -not (Test-Path $env:AFT_BINARY_PATH)) {
    Write-Host "AFT_BINARY_PATH not set or file missing: $env:AFT_BINARY_PATH" -ForegroundColor Red
    exit 2
}

if (-not $env:AFT_PLUGIN_DIST -or -not (Test-Path $env:AFT_PLUGIN_DIST)) {
    Write-Host "AFT_PLUGIN_DIST not set or directory missing: $env:AFT_PLUGIN_DIST" -ForegroundColor Red
    exit 2
}

Write-Host "============================================"
Write-Host "  AFT E2E Test - Windows native"
Write-Host "============================================"
Write-Host ""
Write-Host "AFT binary:   $env:AFT_BINARY_PATH"
Write-Host "Plugin dist:  $env:AFT_PLUGIN_DIST"
Write-Host ""

# ---------------------------------------------------------------------------
# Install dependencies
# ---------------------------------------------------------------------------

Write-Host "-- Installing OpenCode + aimock --"

# OpenCode via npm (the docs explicitly support this on Windows).
& npm install -g opencode-ai 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Host "Failed to install opencode-ai via npm" -ForegroundColor Red
    exit 2
}

# aimock -- the OpenAI-compatible mock LLM.
& npm install -g `@copilotkit/aimock 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Host "Failed to install @copilotkit/aimock via npm" -ForegroundColor Red
    exit 2
}

Write-Host "OpenCode version:"
& opencode --version
Write-Host ""

# ---------------------------------------------------------------------------
# Test project setup
# ---------------------------------------------------------------------------

$ProjectDir = Join-Path $env:TEMP "aft-e2e-project"
if (Test-Path $ProjectDir) { Remove-Item -Recurse -Force $ProjectDir }
New-Item -ItemType Directory -Path $ProjectDir | Out-Null

# Minimal sample project -- same shape as Linux fixtures/sample-project/.
# Git init so the trigram index has a stable cache key.
Push-Location $ProjectDir
try {
    & git init -q
    & git config user.email "test@test.com"
    & git config user.name "Test"

    Set-Content -Path "package.json" -Value '{"name":"test","version":"1.0.0"}'

    New-Item -ItemType Directory -Path "src" | Out-Null
    Set-Content -Path "src/main.py" -Value @"
def greet(name = "World"):
    print(f"Hello, {name}!")

def add(a, b):
    return a + b

if __name__ == "__main__":
    greet()
    print(add(1, 2))
"@

    Set-Content -Path "src/utils.py" -Value @"
def helper():
    return "utility"
"@

    # Pre-stage the timing script that Scenario 2 invokes. We can't
    # generate this from inside the bash tool's `command` argument
    # because PowerShell + JSON + cmd.exe quoting is a nightmare. The
    # script writes a START line with the current ISO timestamp, sleeps
    # 60 seconds via the Windows-native `timeout /t` (cmd.exe doesn't
    # have `sleep`), then writes an END line. The harness later reads
    # this file and asserts both lines are present, which is empirical
    # proof bash actually ran for the full requested duration.
    #
    # `timeout /t 60 /nobreak` cannot be interrupted by a keypress, only
    # by SIGINT. `> nul` suppresses its countdown chatter.
    Set-Content -Path "bash-timing-test.cmd" -Value @"
@echo off
powershell -NoProfile -Command "[DateTime]::UtcNow.ToString('o')" > "%TEMP%\bash-timing-marker.txt" 2>&1
echo START >> "%TEMP%\bash-timing-marker.txt"
timeout /t 60 /nobreak > nul
echo END >> "%TEMP%\bash-timing-marker.txt"
powershell -NoProfile -Command "[DateTime]::UtcNow.ToString('o')" >> "%TEMP%\bash-timing-marker.txt" 2>&1
"@

    & git add -A 2>&1 | Out-Null
    & git commit -q -m "init"
} finally {
    Pop-Location
}

# ---------------------------------------------------------------------------
# OpenCode + AFT config
# ---------------------------------------------------------------------------

# OpenCode honors $env:USERPROFILE\.config\opencode on Windows.
$ConfigDir = Join-Path $env:USERPROFILE ".config\opencode"
New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null

# Use an absolute Windows path with forward slashes for the plugin entry.
# OpenCode's plugin loader resolves paths against the config directory but
# also accepts absolute paths. Backslashes need escaping in JSON; forward
# slashes work fine on Windows in Node/Bun path APIs.
$PluginPath = ($env:AFT_PLUGIN_DIST -replace "\\", "/")

$OpencodeConfig = @"
{
  "`$schema": "https://opencode.ai/config.json",
  "plugin": ["$PluginPath"],
  "provider": {
    "mock": {
      "api": "openai",
      "name": "aimock",
      "options": { "baseURL": "http://127.0.0.1:4010/v1" },
      "models": {
        "mock-model": { "name": "Mock Model" }
      }
    }
  }
}
"@
Set-Content -Path (Join-Path $ConfigDir "opencode.json") -Value $OpencodeConfig

# AFT config -- issue #26 reproduction needs ALL bash experimentals on, plus
# search and semantic enabled, mirroring the user's reported config.
$AftConfig = @"
{
  "search_index": true,
  "semantic_search": true,
  "experimental": {
    "bash": {
      "rewrite": true,
      "compress": true,
      "background": true
    }
  }
}
"@
Set-Content -Path (Join-Path $ConfigDir "aft.jsonc") -Value $AftConfig

# Inject the locally-built binary into the cache. The plugin-version-aware
# resolver looks up `~/.cache/aft/bin/v<plugin_version>/aft.exe` first.
# On Windows, $env:USERPROFILE\.cache\aft\bin\... is the canonical layout
# we already use in resolver.ts.
$PluginPkgPath = Join-Path $env:AFT_PLUGIN_DIST "..\package.json" | Resolve-Path
$PluginVersion = (Get-Content $PluginPkgPath -Raw | ConvertFrom-Json).version
$BinDir = Join-Path $env:USERPROFILE ".cache\aft\bin\v$PluginVersion"
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
Copy-Item -Path $env:AFT_BINARY_PATH -Destination (Join-Path $BinDir "aft.exe") -Force

Write-Host "Test project: $ProjectDir"
Write-Host "AFT binary cached at: $BinDir\aft.exe"
Write-Host "Plugin version: v$PluginVersion"
Write-Host ""

# ---------------------------------------------------------------------------
# Mock server bootstrap
# ---------------------------------------------------------------------------

$MockServer = Join-Path $PSScriptRoot "mock-server.js"
if (-not (Test-Path $MockServer)) {
    Write-Host "Mock server script not found: $MockServer" -ForegroundColor Red
    exit 2
}

# Resolve the global @copilotkit/aimock install dir so `require()` in
# mock-server.js can find it without depending on cwd.
$NpmGlobalRoot = (& npm root -g).Trim()
$env:NODE_PATH = $NpmGlobalRoot

Write-Host "-- Starting aimock mock LLM --"
$MockProc = Start-Process -FilePath "node" `
    -ArgumentList @($MockServer) `
    -RedirectStandardOutput (Join-Path $env:TEMP "aimock.log") `
    -RedirectStandardError  (Join-Path $env:TEMP "aimock.err.log") `
    -PassThru -NoNewWindow

# Wait for aimock to bind port 4010.
$Ready = $false
for ($i = 0; $i -lt 15; $i++) {
    try {
        $resp = Invoke-WebRequest -Uri "http://127.0.0.1:4010/v1/models" -TimeoutSec 2 -UseBasicParsing 2>$null
        if ($resp.StatusCode -eq 200) { $Ready = $true; break }
    } catch { }
    Start-Sleep -Seconds 1
}

if (-not $Ready) {
    Write-Host "aimock did not become ready" -ForegroundColor Red
    if (Test-Path (Join-Path $env:TEMP "aimock.err.log")) {
        Write-Host "--- aimock stderr ---"
        Get-Content (Join-Path $env:TEMP "aimock.err.log")
    }
    exit 2
}

Check "aimock started" { $true }

# ---------------------------------------------------------------------------
# Helper: invoke `opencode run` in the test project with timeout
# ---------------------------------------------------------------------------

function Run-OpencodeSession {
    param(
        [string]$Prompt,
        [string]$ResultFile,
        [int]$TimeoutSec = 60
    )

    Push-Location $ProjectDir
    try {
        # OpenCode's openai adapter requires SOME api key; aimock ignores it.
        $env:OPENAI_API_KEY = "sk-mock-windows-e2e"

        # On Windows, `npm install -g opencode-ai` deposits THREE shims at
        # %APPDATA%\npm\:
        #   - opencode      (bash shim, useless on Windows)
        #   - opencode.cmd  (batch shim — what `cmd.exe /c` can run)
        #   - opencode.ps1  (PowerShell shim — what powershell.exe -File runs)
        #
        # `Get-Command opencode` resolves the FIRST match by $env:PATHEXT.
        # On Windows PowerShell 5.1, .PS1 typically wins, so we'd get back
        # opencode.ps1 — and `cmd.exe /c <opencode.ps1>` does NOT execute a
        # PowerShell script. cmd.exe sees the .ps1 extension and hangs (no
        # error, no output, just blocks). We explicitly resolve `opencode.cmd`
        # so the cmd.exe /c invocation finds a real batch shim it can run.
        #
        # Why not just use `opencode.ps1` via powershell.exe? That works too
        # but adds a 2-3s PowerShell-startup tax to every Run-OpencodeSession
        # call. The .cmd shim runs `node` directly with no wrapper interpreter.
        $opencodeCmd = (Get-Command opencode.cmd -ErrorAction SilentlyContinue).Source
        if (-not $opencodeCmd) {
            # Fall back to bare 'opencode' (might pick up .ps1 — log it so we
            # know what we got, since this can produce confusing hangs).
            $fallback = Get-Command opencode -ErrorAction SilentlyContinue
            if ($fallback) {
                Write-Host "warning: opencode.cmd not found, falling back to $($fallback.Source)" -ForegroundColor Yellow
                $opencodeCmd = $fallback.Source
            } else {
                Write-Host "opencode not found on PATH (looked for opencode.cmd and opencode)" -ForegroundColor Red
                return 127
            }
        }

        # -NoNewWindow keeps the child sharing our console; redirection works
        # cleanly with cmd.exe /c. We don't combine with -WindowStyle (mutex).
        $proc = Start-Process -FilePath "cmd.exe" `
            -ArgumentList @("/c", $opencodeCmd, "run", "--model", "mock/mock-model", $Prompt) `
            -RedirectStandardOutput $ResultFile `
            -RedirectStandardError  ($ResultFile + ".err") `
            -PassThru -NoNewWindow

        if (-not $proc.WaitForExit($TimeoutSec * 1000)) {
            Write-Host "  (opencode run timed out at ${TimeoutSec}s -- stopping process)" -ForegroundColor Yellow
            try { $proc.Kill() } catch { }
            $proc.WaitForExit(5000) | Out-Null
            return 124  # timeout exit code (matches GNU coreutils convention)
        }
        return $proc.ExitCode
    } finally {
        Pop-Location
    }
}

function Invoke-AftNdjsonScenario {
    param([string]$ProjectDir)

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $env:AFT_BINARY_PATH
    $psi.WorkingDirectory = $ProjectDir
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true

    $proc = [System.Diagnostics.Process]::new()
    $proc.StartInfo = $psi
    if (-not $proc.Start()) { throw "failed to start aft binary" }

    try {
        $configure = @{
            id = "bg-configure"
            command = "configure"
            project_root = $ProjectDir
            experimental_bash_background = $true
        } | ConvertTo-Json -Compress
        $proc.StandardInput.WriteLine($configure)
        $proc.StandardInput.Flush()
        $cfgResponse = $proc.StandardOutput.ReadLine() | ConvertFrom-Json
        if (-not $cfgResponse.success) { throw "configure failed: $($cfgResponse | ConvertTo-Json -Compress)" }

        $spawn = @{
            id = "bg-spawn"
            command = "bash"
            params = @{
                command = "cmd /c echo hello-bg"
                background = $true
            }
        } | ConvertTo-Json -Compress -Depth 5
        $proc.StandardInput.WriteLine($spawn)
        $proc.StandardInput.Flush()
        $spawnResponse = $proc.StandardOutput.ReadLine() | ConvertFrom-Json
        if (-not $spawnResponse.success) { throw "background spawn failed: $($spawnResponse | ConvertTo-Json -Compress)" }

        $taskId = [string]$spawnResponse.task_id
        if ($taskId -notmatch '^bgb-[0-9a-f]{8}$') { throw "bad task id format: $taskId" }

        Start-Sleep -Seconds 2

        $status = @{
            id = "bg-status"
            command = "bash_status"
            params = @{ task_id = $taskId }
        } | ConvertTo-Json -Compress -Depth 5
        $proc.StandardInput.WriteLine($status)
        $proc.StandardInput.Flush()

        $deadline = (Get-Date).AddSeconds(10)
        do {
            $line = $proc.StandardOutput.ReadLine()
            if (-not $line) { throw "aft stdout closed before status response" }
            $statusResponse = $line | ConvertFrom-Json
        } while ($statusResponse.id -ne "bg-status" -and (Get-Date) -lt $deadline)

        if ($statusResponse.id -ne "bg-status") { throw "timed out waiting for bg-status response" }
        if (-not $statusResponse.success) { throw "bash_status failed: $($statusResponse | ConvertTo-Json -Compress)" }
        if ($statusResponse.status -ne "completed") { throw "expected completed status, got: $($statusResponse | ConvertTo-Json -Compress)" }

        return $true
    } finally {
        try { $proc.StandardInput.Close() } catch { }
        if (-not $proc.WaitForExit(3000)) {
            try { $proc.Kill() } catch { }
            $proc.WaitForExit(3000) | Out-Null
        }
    }
}

# Plugin log path on Windows -- Node's os.tmpdir() resolves to $env:TEMP.
$PluginLog = Join-Path $env:TEMP "aft-plugin.log"
if (Test-Path $PluginLog) { Remove-Item $PluginLog -Force }

# ---------------------------------------------------------------------------
# Scenario 1: Full session -- exercises plugin load, bridge spawn, basic tools
# ---------------------------------------------------------------------------

Write-Host ""
Write-Host "-- Scenario 1: Full session --"
Write-Host ""

$Result1 = Join-Path $env:TEMP "result-scenario1.txt"
$ExitCode = Run-OpencodeSession `
    -Prompt "Outline src, read main.py, grep for def, then make a small edit and undo it." `
    -ResultFile $Result1 `
    -TimeoutSec 90

# Treat 0 (clean), 124 (our timeout), and -1/137 (killed) as completions.
# OpenCode 'run' is known to hang on session end on some platforms; we don't
# fail the test purely on exit code.
Check "session completed" { $ExitCode -eq 0 -or $ExitCode -eq 124 -or $ExitCode -eq -1 }

# CRITICAL ordering: check "plugin log exists" BEFORE any log-content checks.
# If the log was never written, the plugin never loaded — that's the signal we
# want isolated as a single FAIL with diagnostic context, not buried in a
# cascade of "no plugin crash"/"plugin loaded" misreads against a missing file.
$logExists = Test-Path $PluginLog
Check "plugin log written ($PluginLog)" { $logExists }

if (-not $logExists) {
    # The plugin never wrote a log. Possible causes, in rough order of likelihood:
    #   (a) opencode hung BEFORE its first chat turn — never sent a model request,
    #       so aimock saw nothing and tool calls never fired (the AFT plugin only
    #       starts logging when its first tool is invoked). Empty opencode
    #       stdout/stderr + empty aimock log together confirm this.
    #   (b) opencode reached aimock but tool calls never returned — aimock log
    #       has request entries but plugin log is empty. This points at the
    #       plugin/host wiring rather than at opencode itself.
    #   (c) plugin failed to load entirely — opencode stderr usually carries
    #       a Node import/syntax error in this case.
    #   (d) plugin loaded under a different temp dir than %TEMP% — checked via
    #       the alt-path probe below.
    #
    # We dump ALL three log sources (opencode stdout/stderr, aimock stdout/stderr)
    # so the failure mode is unambiguous on first read.
    Write-Host ""
    Write-Host "  -- diagnostic: plugin log missing, dumping all captured output --" -ForegroundColor Yellow

    function Show-LogTail {
        param([string]$Label, [string]$Path, [int]$Lines = 50)
        if (Test-Path $Path) {
            $size = (Get-Item $Path).Length
            Write-Host "  ${Label} (${Path}, ${size} bytes):"
            if ($size -eq 0) {
                Write-Host "    (empty)"
            } else {
                Get-Content $Path -Tail $Lines | ForEach-Object { Write-Host "    $_" }
            }
        } else {
            Write-Host "  ${Label} (no file at $Path)"
        }
    }

    Show-LogTail "opencode stdout" $Result1
    Show-LogTail "opencode stderr" ($Result1 + ".err")
    Show-LogTail "aimock stdout" (Join-Path $env:TEMP "aimock.log")
    Show-LogTail "aimock stderr" (Join-Path $env:TEMP "aimock.err.log")

    # Also probe the alternative log paths in case the plugin is using one we
    # didn't expect. Node's os.tmpdir() on Windows resolves to %TEMP% but
    # may be different under cmd.exe vs PowerShell tokens.
    $candidates = @(
        (Join-Path $env:USERPROFILE "AppData\Local\Temp\aft-plugin.log"),
        (Join-Path $env:LOCALAPPDATA "Temp\aft-plugin.log"),
        "C:\Windows\Temp\aft-plugin.log"
    )
    foreach ($p in $candidates) {
        if ($p -ne $PluginLog -and (Test-Path $p)) {
            Write-Host "  found alternate log at: $p" -ForegroundColor Yellow
        }
    }
}

# Now do log-content checks via the LogContains helper, which returns false
# (instead of throwing) when the log is missing. The "plugin log written"
# check above already converted the missing-log condition into a single FAIL,
# so these will all also fail correctly without script-killing exceptions.
# "no plugin crash" intentionally excludes "semantic index build panicked".
# That one is a Rust-side recoverable panic (caught by catch_unwind in
# crates/aft/src/commands/configure.rs) — it does NOT terminate the bridge,
# does NOT block other tools, and is reported back to the host as
# SemanticIndexEvent::Failed. The bridge keeps serving every non-semantic
# tool. Treating it as a crash makes the harness fail noisily on a feature
# that's intentionally allowed to fail soft. Real fatal crashes still match
# via "Binary crashed" (from packages/aft-bridge/src/bridge.ts) or SIGABRT.
Check "no plugin crash" {
    if (-not (Test-Path $PluginLog)) { return $true }
    $crashLines = Select-String -Path $PluginLog -Pattern "panicked|SIGABRT|Binary crashed" -ErrorAction SilentlyContinue
    if (-not $crashLines) { return $true }
    foreach ($line in $crashLines) {
        # Skip known recoverable panics
        if ($line.Line -match "semantic index build panicked") { continue }
        if ($line.Line -match "Failed to load ONNX Runtime") { continue }
        if ($line.Line -match "thread '<unnamed>' \(\d+\) panicked at.*ort-\d") { continue }
        return $false  # real crash detected
    }
    return $true
}
Check "plugin loaded" { LogContains $PluginLog "Resolved binary|Spawning binary|Copied npm binary" }
Check "bridge spawned" { LogContains $PluginLog "started, pid" }
WarnCheck "search index started" { LogContains $PluginLog "watcher started|search.*index|index.*build" }

# Empirical evidence checks: previous iterations passed without ever
# verifying that aimock's scripted turns actually fired. mock-server.js
# writes a per-request journal sidecar every 1s containing the cumulative
# request count and per-request paths/timestamps. Used here as bus-level
# proof opencode talked to the mock.
$AimockLog = Join-Path $env:TEMP "aimock.log"
$AimockJournal = Join-Path $env:TEMP "aimock-journal.txt"
Check "aimock received chat-completion requests" {
    if (-not (Test-Path $AimockJournal)) { return $false }
    $content = Get-Content $AimockJournal -Raw
    return ($content -match "(\d+) requests" -and [int]$Matches[1] -gt 0)
}
Check "opencode reached scripted turns (not fallback)" {
    -not (LogContains $Result1 "AIMOCK_FALLBACK")
}

# ---------------------------------------------------------------------------
# Scenario 2: Issue #26 reproduction -- bash with all experimentals on, varying timeouts
#
# We ask the agent to run several bash commands with different timeout values
# to measure whether the bridge transport timeout calc has enough headroom
# on Windows (where process spawn is materially slower than Unix).
# ---------------------------------------------------------------------------

Write-Host ""
Write-Host "-- Scenario 2: Bash timeout headroom (issue #26) --"
Write-Host ""

# Reset the plugin log so this scenario's assertions don't conflict with
# Scenario 1's content.
Remove-Item $PluginLog -Force -ErrorAction SilentlyContinue

# Reset the bash timing marker. If the marker exists at the end of the
# scenario, bash actually ran. If it doesn't, bash was skipped (the
# previous false-pass we caught: aimock never delivered our turn 6 to
# opencode, so bash was never invoked but the harness reported
# "PASS [no bridge timeout from bash]" because there was no bash to
# time out from).
$BashMarker = Join-Path $env:TEMP "bash-timing-marker.txt"
Remove-Item $BashMarker -Force -ErrorAction SilentlyContinue

# Single bash turn with a pre-staged 60s sleep script. Issue #26's
# boundary is at requested timeout = 65s, transport budget = 70s. The
# harness allows 240s for the whole opencode session because there's
# also model streaming overhead, plugin startup work, and S2 starts
# from a cold bridge that needs to reload ONNX + indexes.
$Result2 = Join-Path $env:TEMP "result-scenario2.txt"
$S2Start = Get-Date
$ExitCode = Run-OpencodeSession `
    -Prompt "Run the bash-timing-test.cmd script to test bash timeout handling." `
    -ResultFile $Result2 `
    -TimeoutSec 240
$S2Duration = (Get-Date) - $S2Start
Write-Host "  (S2 wall-clock: $([Math]::Round($S2Duration.TotalSeconds, 1))s)"

Check "bash session completed" { $ExitCode -eq 0 -or $ExitCode -eq 124 -or $ExitCode -eq -1 }
Check "no bridge timeout from bash" { -not (LogContains $PluginLog 'timed out after \d+ms|stdin not writable') }
Check "no plugin crash (bash)" {
    if (-not (Test-Path $PluginLog)) { return $true }
    $crashLines = Select-String -Path $PluginLog -Pattern "panicked|SIGABRT|Binary crashed" -ErrorAction SilentlyContinue
    if (-not $crashLines) { return $true }
    foreach ($line in $crashLines) {
        if ($line.Line -match "semantic index build panicked") { continue }
        if ($line.Line -match "Failed to load ONNX Runtime") { continue }
        if ($line.Line -match "thread '<unnamed>' \(\d+\) panicked at.*ort-\d") { continue }
        return $false
    }
    return $true
}

# ---- Empirical bash evidence (the actual issue #26 reproduction) ----
#
# These are the only checks that actually answer the question in issue #26.
# The marker file content + duration proves bash:
#   (1) was invoked at all
#   (2) reached its sleep without crashing early
#   (3) ran for the full 60s without being cut off by the bridge timeout
#
# If issue #26 reproduces, "bash ran full 60s duration" will FAIL with the
# marker showing only START + a sub-60s elapsed, and the bridge timeout
# entry will appear in the plugin log.
# Note: we do NOT check for AIMOCK_FALLBACK in S2's output. After the
# scripted bash turn returns, opencode issues a follow-up assistant turn
# whose user message (the bash tool result) doesn't match any specific
# fixture — so the catch-all naturally fires to end the conversation.
# That's expected. The empirical check that S2 actually exercised bash
# is the marker file below.
Check "bash actually ran (marker file written)" {
    Test-Path $BashMarker
}
Check "bash completed full duration (START + END markers)" {
    if (-not (Test-Path $BashMarker)) { return $false }
    $content = Get-Content $BashMarker -Raw
    return ($content -match "START" -and $content -match "END")
}
Check "bash duration in expected range (55-70s)" {
    if (-not (Test-Path $BashMarker)) { return $false }
    $lines = Get-Content $BashMarker
    if ($lines.Count -lt 4) { return $false }
    # Marker layout (4 lines):
    #   Line 0: ISO timestamp before sleep
    #   Line 1: "START"
    #   Line 2: "END"
    #   Line 3: ISO timestamp after sleep
    try {
        $startTs = [DateTime]::Parse($lines[0])
        $endTs   = [DateTime]::Parse($lines[3])
        $elapsed = ($endTs - $startTs).TotalSeconds
        Write-Host "  (bash sleep elapsed: $([Math]::Round($elapsed, 2))s)"
        # `timeout /t 60` sleeps 59-61s in practice; allow generous range
        # so a Windows process spawn slow-down doesn't false-fail.
        return ($elapsed -ge 55 -and $elapsed -le 70)
    } catch {
        Write-Host "  (parse error: $($_.Exception.Message))"
        return $false
    }
}
WarnCheck "aimock received >=2 requests for S2 (initial + tool result)" {
    if (-not (Test-Path $AimockJournal)) { return $false }
    $content = Get-Content $AimockJournal -Raw
    if ($content -match "(\d+) requests") { return [int]$Matches[1] -ge 2 }
    return $false
}

# ---------------------------------------------------------------------------
# Scenario 2b: Interactive-prompt deadlock (issue #26 ROOT CAUSE)
#
# This scenario reproduces the actual root cause behind issue #26: AFT bash
# inheriting stdin from the bridge protocol pipe, which would block forever
# when a child process tries to read from stdin (Read-Host, credential
# prompts, etc.).
#
# With the fix in crates/aft/src/commands/bash.rs (stdin=null + PowerShell
# -NonInteractive), Read-Host errors immediately and bash returns within
# ~1 second. Without the fix, bash would block forever, the bridge would
# hit its 30s transport timeout, and we'd see "timed out after Nms" in
# the plugin log.
# ---------------------------------------------------------------------------

Write-Host ""
Write-Host "-- Scenario 2b: Interactive-prompt deadlock (issue #26 root cause) --"
Write-Host ""

Remove-Item $PluginLog -Force -ErrorAction SilentlyContinue
$InteractiveMarker = Join-Path $env:TEMP "interactive-marker.txt"
Remove-Item $InteractiveMarker -Force -ErrorAction SilentlyContinue

$Result2b = Join-Path $env:TEMP "result-scenario2b.txt"
$S2bStart = Get-Date
$ExitCode = Run-OpencodeSession `
    -Prompt "Please run the interactive-prompt-test command via bash to validate stdin handling." `
    -ResultFile $Result2b `
    -TimeoutSec 60
$S2bDuration = (Get-Date) - $S2bStart
Write-Host "  (S2b wall-clock: $([Math]::Round($S2bDuration.TotalSeconds, 1))s)"

Check "interactive-prompt session completed" {
    $ExitCode -eq 0 -or $ExitCode -eq 124 -or $ExitCode -eq -1
}

# DEFINITIVE issue #26 fix verification:
#
# What we expect with the fix in place:
#   1. PowerShell Read-Host hangs on stdin (Windows PS 5.1 quirk: even
#      with stdin=null + -NonInteractive, Read-Host blocks for some reason
#      we don't fully understand — possibly a polling loop on the NUL
#      device).
#   2. AFT bash's per-call timeout (10s requested in this scenario) fires,
#      AFT terminates the child PowerShell process, and returns a normal
#      response to opencode with timed_out=true.
#   3. The bridge's transport timeout (max(30s, 10s+5s)=30s) is NEVER
#      hit — because bash returned before then. No "Bridge timed out"
#      error in the plugin log, opencode keeps going, conversation ends.
#
# What we'd see WITHOUT the fix:
#   1. PowerShell Read-Host inherits the bridge's stdin (the NDJSON
#      protocol pipe).
#   2. It either reads protocol bytes or blocks for input that never comes.
#   3. AFT bash's per-call timeout would still fire eventually, but in
#      between, the child PowerShell could read protocol bytes and
#      desync the bridge — triggering "Bridge timed out" for an
#      unrelated request OR "stdin not writable" when the bridge
#      tries to send the next request to a corrupted pipe.
#
# The single most important assertion: NO bridge transport timeout.
# That's what closes the loop on issue #26.
Check "no bridge timeout (issue #26 fix)" {
    -not (LogContains $PluginLog 'timed out after \d+ms|stdin not writable|Bridge timed out')
}
Check "interactive bash returned (marker written)" {
    Test-Path $InteractiveMarker
}
# Total round-trip should be well under the bridge's 30s transport budget.
# Without the fix this would be 30s+ (bridge timeout). With the fix,
# bash's own 10s timeout fires and the whole session completes in ~15s
# including opencode/aimock overhead.
Check "interactive bash returned promptly (<25s total)" {
    $S2bDuration.TotalSeconds -lt 25
}
# bash should report it timed out — that's the correct behavior when an
# interactive script hangs. The CRITICAL distinction is that this is
# bash-tool-level timeout, not bridge-transport-level timeout. bash-tool
# timeout returns cleanly to opencode; bridge timeout would kill the
# session.
WarnCheck "bash reported timed_out (expected for interactive hang)" {
    LogContains $Result2b "timed out|timeout"
}

# ---------------------------------------------------------------------------
# Scenario 2c: Background bash via the real aft binary
# ---------------------------------------------------------------------------

Write-Host ""
Write-Host "-- Scenario 2c: Background bash direct binary --"
Write-Host ""

Check "background bash direct aft binary completes" {
    Invoke-AftNdjsonScenario -ProjectDir $ProjectDir
}

# ---------------------------------------------------------------------------
# Scenario 3: ONNX runtime install (Windows tar.exe path)
# ---------------------------------------------------------------------------

Write-Host ""
Write-Host "-- Scenario 3: ONNX install via tar.exe --"
Write-Host ""

# We don't reset the plugin log here -- ONNX install kicks off during plugin
# load (Scenario 1) and finishes async. Just check that the install path used
# tar.exe (Windows) and not an extraction failure.

WarnCheck "ONNX download attempted (or already installed)" {
    LogContains $PluginLog "ONNX Runtime found at|Downloading ONNX Runtime|ONNX Runtime ready"
}
Check "no ONNX panic" {
    -not (LogContains $PluginLog "panicked.*ort|thread.*panicked")
}

# ---------------------------------------------------------------------------
# Cleanup + summary
# ---------------------------------------------------------------------------

if ($MockProc -and -not $MockProc.HasExited) {
    try { $MockProc.Kill() } catch { }
    $MockProc.WaitForExit(5000) | Out-Null
}

Write-Host ""
if (Test-Path $PluginLog) {
    Write-Host "Plugin log (last 40 lines):"
    Get-Content $PluginLog -Tail 40 | ForEach-Object { Write-Host "    $_" }
}

# Bash timing marker — empirical evidence dump for issue #26.
if (Test-Path $BashMarker) {
    Write-Host ""
    Write-Host "Bash timing marker:"
    Get-Content $BashMarker | ForEach-Object { Write-Host "    $_" }
} else {
    Write-Host ""
    Write-Host "Bash timing marker: NOT WRITTEN — bash never ran" -ForegroundColor Yellow
}

# Interactive-prompt marker — empirical evidence for issue #26 root cause.
if (Test-Path $InteractiveMarker) {
    Write-Host ""
    Write-Host "Interactive-prompt marker:"
    Get-Content $InteractiveMarker | ForEach-Object { Write-Host "    $_" }
} else {
    Write-Host ""
    Write-Host "Interactive-prompt marker: NOT WRITTEN — bash hung on stdin" -ForegroundColor Yellow
}

# aimock log — proves whether opencode actually hit the mock server.
# ($AimockLog and $AimockJournal were defined earlier during S1's checks.)
if (Test-Path $AimockLog) {
    Write-Host ""
    Write-Host "Aimock log (last 30 lines):"
    Get-Content $AimockLog -Tail 30 | ForEach-Object { Write-Host "    $_" }
}
if (Test-Path $AimockJournal) {
    Write-Host ""
    Write-Host "Aimock journal:"
    Get-Content $AimockJournal | ForEach-Object { Write-Host "    $_" }
}

Write-Host ""
Write-Host "============================================"
Write-Host "  Results: $script:Pass passed, $script:Fail failed"
Write-Host "============================================"

if ($script:Fail -gt 0) {
    Write-Host "TESTS FAILED" -ForegroundColor Red
    exit 1
}

Write-Host "ALL TESTS PASSED" -ForegroundColor Green
exit 0
