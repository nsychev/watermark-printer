Virtual IPP Printer that adds watermarks and forwards job to another printer.

A slightly refactored version of [ippper.rs](https://github.com/ArcticLampyrid/ippper.rs).

## Build

Latest stable Rust will be fine. `cargo build -r` will do the job.

## How to run

1. You need IPP support in your target printer. For example, if you have CUPS, it will work out of the box.

   Fetch printer URL. E.g. for CUPS printer it will be `ipp://localhost:631/printers/name` or `ipp://localhost:631/classes/name`.

2. Run binary. The only required option is `--next-ipp <url>` that sets your printer URL.
   
   Also you can use `-n / --name <name>`, `-p / --port <port>` (default: 631), `-s / --storage <path>` (default: `/tmp/nercprint`).

3. Add IPP printer `http://<IP>:<PORT>`. Ensure to specify `http` scheme if you're on Windows.

## How to customize

We now print the THIRD octet of user IP address on the printout with leading zeroes.

It can be changed in [`handler.rs`](src/handler.rs#L76).

Sorry, but now it requires rebuilding.
