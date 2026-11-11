# Watermark Printer

A virtual IPP 2.0 printer that marks every job with the watermark based on
source IP address and forwards it to a real printer.

## Build

You need a recent stable [Rust](https://www.rust-lang.org/). Run
`cargo build --release`; the binary is `target/release/watermark-printer`.

## Usage

```
$ watermark-printer --help
A virtual printer that watermarks and forwards print jobs

Options:
  -n, --name <NAME>              Printer name [default: Watermark Printer]
  -p, --bind <BIND>              Address or port to bind [default: 631]
  -I, --destination <DEST>       Where to send marked jobs: an ipp:// URI, or
                                 auto, lp, lpr, winspooler, sumatra[:path],
                                 or command:<template> [default: auto]
      --printer <PRINTER>        Printer queue name, for local destinations
  -s, --storage <STORAGE>        Keep a copy of every marked document here
  -t, --team-id-script <SCRIPT>  Lua script defining get_team_id(ip_address)
      --max-pages <MAX_PAGES>    Refuse a job with more pages [default: none]
      --max-queued-per-ip <N>    Refuse a client with N queued [default: 16]
      --max-document-mb <MB>     Refuse a larger PDF or JPEG [default: 5]
      --max-raster-mb <MB>       Refuse a larger PWG Raster job [default: 50]
      --paper-size <PAPER_SIZE>  The only paper size this printer offers
                                 [default: iso_a4_210x297mm]
      --color                    Advertise the printer as colour
      --watermark-weight <W>     Thickness of the watermark outline, as a
                                 fraction of its font size [default: 0.03]
```

## Quick Start

1. You need a separate computer that will act as a print server. It should be
   capable of running this binary and your client should be able to reach it
   over the network.

2. You need to connect a printer to it. Your options:

   - On Linux, it prints via [CUPS]. Just add a default printer in the settings.

     > [!NOTE]
     > 
     > Do not expose CUPS to the network:
     >
     > ```bash
     > sudo cupsctl --no-remote-admin --no-remote-any
     >
     > # If you use iptables
     > sudo iptables -A INPUT ! -s 127.0.0.1 -p tcp --dport 631 -j REJECT
     > sudo iptables -A INPUT ! -s 127.0.0.1 -p udp --dport 631 -j REJECT
     > sudo iptables-save | sudo tee /etc/iptables/rules.v4
     > 
     > # If you use UFW
     > sudo ufw deny 631
     > sudo ufw enable
     > ```

   - On Windows, you need [SumatraPDF]. Put `SumatraPDF.exe` in the `PATH` or
     near `watermark-printer` binary. Also you can try to use native Windows
     Print Spooler, but printers may not understand the format.

   - If it's a network printer, it probably speaks over [IPP]. Enable IPP in
     the printer settings and grab its URL.

     > [!NOTE]
     >
     > Give the printer a static IP address, so the URL won't change.
     >
     > Configure your network so that clients won't have direct access to the printer.

[IPP]: https://en.wikipedia.org/wiki/Internet_Printing_Protocol
[SumatraPDF]: https://www.sumatrapdfreader.org/download-free-pdf-viewer

3. Run the tool:

   ```bash
   watermark-printer -p 6631
   ```

   For the IPP network printer, pass `-I ipp://<url>/`.
   For the native printing using non-default printer, pass `--printer <name>`.

4. Add the printer on each client using URI `ipp://<server>:6631`.

   * **Linux (CUPS).** Use CUPS admin panel at http://localhost:631/ or your OS settings.
     Also you can run `lpadmin -p teamprint -E -v ipp://<server>:6631/ipp/print -m everywhere`.
   * **Windows.** Settings → Add printer → *The printer that I want isn't
     listed* → *Select a shared printer by name*, and enter
     `http://<server>:6631/ipp/print`.
   * **macOS.** Add printer → IP, protocol *Internet Printing Protocol (IPP)*,
     address `<server>:6631`, queue `ipp/print`.

6. Print a test page.

> Discovery over DNS-SD (Bonjour/Avahi) is off.
> See [`ippserver`](https://github.com/nsychev/ippserver-rs) documentation
> for instructions if you need it.

## Round Robin

With CUPS you can spread jobs over several printers. Open
`http://localhost:631`, go to **Administration** → **Add Class**, authenticate
with your UNIX account, name the class and pick the printers. Then pass
`http://localhost:631/classes/<class-name>` to `-I` or `<class-name>` to `--printer`.

## Watermark configuration

By default the tool reads the third octet of the client's IPv4 address and
pads it to three digits, so a job from 10.21.32.43 is marked “032”.

To change that, write a Lua script with a single function `get_team_id`. It
takes the address as a string and returns either the mark or `nil`; mind that
the address may be IPv6. Pass the path with `--team-id-script`. The built-in
script is [`default_team_id_script.lua`](crates/watermark-printer/src/default_team_id_script.lua).

If a scrpt returns `nil`, the jobs will be refused.

## Media configuration

By default, printer advertises A4 monochrome support only. `--paper-size`
allows you to choose paper format; and `--color` enables color printing.

## Limits

* `--max-pages` — maximum pages per job
* `--max-queued-per-ip` — maximum jobs per IP in the queue
* `--max-document-mb` — limit for PDF and JPEG jobs, defaults to 5 MB
* `--max-raster-mb` — limit for rasterized PWG jobs, defaults to 50 MB

## Conformance

Checked against the CUPS IPP Everywhere test suite:

```bash
ipptool -V 2.0 -tf document-a4.pdf ipp://localhost:6631/ipp/print ipp-everywhere.test
```

which covers RFC 8011, IPP/2.0 (PWG 5100.12) and IPP Everywhere (PWG 5100.14),
and prints the PWG Raster sample corpus at every raster type and resolution the
printer advertises.
