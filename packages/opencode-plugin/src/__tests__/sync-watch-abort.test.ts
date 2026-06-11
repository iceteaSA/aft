/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import {
  __resetSyncWatchAbortForTests,
  clearSyncWatchAbort,
  isSyncWatchAborted,
  signalSyncWatchAbort,
} from "../sync-watch-abort.js";

describe("sync-watch-abort", () => {
  test("signalSyncWatchAbort sets the abort flag for a session", () => {
    __resetSyncWatchAbortForTests();
    expect(isSyncWatchAborted("session-1")).toBe(false);
    signalSyncWatchAbort("session-1");
    expect(isSyncWatchAborted("session-1")).toBe(true);
  });

  test("clearSyncWatchAbort clears the abort flag", () => {
    __resetSyncWatchAbortForTests();
    signalSyncWatchAbort("session-1");
    expect(isSyncWatchAborted("session-1")).toBe(true);
    clearSyncWatchAbort("session-1");
    expect(isSyncWatchAborted("session-1")).toBe(false);
  });

  test("abort flags are per-session", () => {
    __resetSyncWatchAbortForTests();
    signalSyncWatchAbort("session-1");
    expect(isSyncWatchAborted("session-1")).toBe(true);
    expect(isSyncWatchAborted("session-2")).toBe(false);
  });

  test("undefined sessionID uses default key", () => {
    __resetSyncWatchAbortForTests();
    signalSyncWatchAbort(undefined);
    expect(isSyncWatchAborted(undefined)).toBe(true);
    expect(isSyncWatchAborted("session-1")).toBe(false);
  });

  test("isSyncWatchAborted does not clear the flag", () => {
    __resetSyncWatchAbortForTests();
    signalSyncWatchAbort("session-1");
    expect(isSyncWatchAborted("session-1")).toBe(true);
    expect(isSyncWatchAborted("session-1")).toBe(true);
  });

  test("__resetSyncWatchAbortForTests clears all flags", () => {
    signalSyncWatchAbort("session-1");
    signalSyncWatchAbort("session-2");
    __resetSyncWatchAbortForTests();
    expect(isSyncWatchAborted("session-1")).toBe(false);
    expect(isSyncWatchAborted("session-2")).toBe(false);
  });
});
