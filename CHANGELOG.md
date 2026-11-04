# Unreleased

- `-I`/`--destination` now defaults to `auto`, so the printer starts without
  one and forwards to whatever this machine already prints with.
- Updated dependencies, notably `lopdf` to 0.47 and `mlua` to 0.12.

# 2.0.0 - 2026-11-04

- [`ippserver`][ippserver] and [`universal-print`][universal-print]
  are now separate projects.
- Printer now supports IPP 2.0 and IPP Everywhere™ for driverless
  printing.
- PDF handling is rewritten, so `-M` flag is now dropped.
- Added header to pages in addition to the watermark.
- Attempts were made to support native printing on both Linux and Windows.
- Added limits for jobs.
- Server now rejects jobs from clients if `team_id_script` returns `nil`.

[ippserver]: https://github.com/nsychev/ippserver-rs
[universal-print]: https://github.com/nsychev/universal-print-rs

# 1.2.0 - 2025-11-30

- Support automatic detection of mirrored PDF files
- Support custom bind address

# 1.1.0 - 2025-11-29

- Update vulnerable dependencies and Rust to 2024 edition
- Add `-M` flag for mirroring the watermark

# 1.0.0 - 2025-01-24

- Initial release, tested on ICPC Northern Eurasia Finals
