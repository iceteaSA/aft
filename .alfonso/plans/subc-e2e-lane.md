# subc e2e lane

## S4 — CI enablement

- The pinned daemon release lives in one place: `SUBC_CORE_TAG` at the top of
  `scripts/fetch-subc-core.sh`.
- To bump the pin, edit that one line and commit the change. The Linux and macOS
  unit-suite jobs re-download the tarball and its `.sha256` sidecar, verify the
  tarball checksum before extraction, and fail hard on any mismatch.
- Local runs stay offline: the subc e2e rig only reads `SUBC_CORE_BIN`, a sibling
  subconscious checkout, or the fetch-script cache. Tests never download during
  `bun test`.
