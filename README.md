A slightly refactored version of [ippper.rs](https://github.com/ArcticLampyrid/ippper.rs).

## TODO:

1. Linux client support: CUPS “Generic PDF Printer” refuses to send raw PDF jobs, but instead sends HP/JL (PJL) files. It's trivial to convert them to PDF, but it needs a bit of time.

2. (maybe?) Log all jobs.

3. Command-line interface to choose settings (e.g. virtual printer name, target printer, etc).

4. Test against CUPS Class.

## How to run (Windows, Linux)

1. Install CUPS and libcups(?) if you're using Linux.

2. Ensure there is a printer connected to your PC.

3. Then run `cargo run`.

4. Add IPP printer `http://<IP>:1337`. Ensure to specify `http` scheme if you're on Windows.
