# Watermark Printer

Virtual IPP printer that acts as a proxy and adds watermarks to incoming jobs.

Tested on Windows 10, 11 and Linux (CUPS) clients.

## Build

You need latest stable [Rust](https://www.rust-lang.org/). Run `cargo build -r` and fetch the binary at `target/release/watermark-printer`. It's ready!

## Run

```
$ watermark-printer --help
Usage: watermark-printer [OPTIONS] --next-ipp <NEXT_IPP>

Options:
  -n, --name <NAME>                      Printer name [default: PDF-Printer]
  -p, --port <PORT>                      Port to listen on [default: 631]
  -s, --storage <STORAGE>                Path to store all PDFs [default: /tmp/printouts]
  -t, --team-id-script <TEAM_ID_SCRIPT>  Path to Lua script with custom `get_team_id' function
  -I, --next-ipp <NEXT_IPP>              Next printer IPP URL
```

## I don't know anything about printers, how to set everything up?

1. You need the print server — any machine that is available to your clients and capable of running this utility.

2. You need either:

   a. a printer that can print via IPP — in that case you need to turn on IPP in the printer settings and get URL.

      > It's recommended to set static IP to such printer to avoid issues when IP address changes.
  
   b. Linux and CUPS set up on your print server, and printer connected to your computer. In that case, your URL will be `http://localhost:631/printers/<printer-name>`.

3. If you use CUPS, **ensure that you have turned firewall on and it is blocking outside connections to CUPS**.

   Firstly, there were vulnerabilities in CUPS that even allowed remote code execution on your server; and secondly, anyone will be able to print without watermarks.

   ```bash
   # If you use iptables
   sudo iptables -A INPUT -p tcp --dport 631 -j REJECT
   sudo iptables -A INPUT -p udp --dport 631 -j REJECT
   sudo iptables-save | sudo tee /etc/iptables/rules.v4 # you need iptables-persistent package on your machine to make rules survive reboots

   # If you use UFW
   sudo ufw deny 631
   sudo ufw enable
   ```

   If you print directly to printer via IPP, ensure that your clients don't have direct network access to that printer.

4. Run our tool:

   ```bash
   ./watermark-printer -p 6631 -I <your-url>
   ```

5. Add new printer to your clients. Use URL `ipp://<your-ip>:6631/` on Linux, and `http://<your-ip>:631/` on Windows.

   On Linux you should choose **Select printer from database** → **Generic** → **PDF** → **Generic PDF Printer [en]** when asked for printer model.

6. Print test page and see it with the watermark!

### Bonus: Round Robin

When using CUPS, you can easily distribute your jobs via several printers to increase throughput.

Open `http://localhost:631` on your print server. Go to **Administration** and click **Add Class**.

Authenticate with your current UNIX username and password.

Set some name (for example, `round-robin-class`) and choose a few printers.

Then use `http://localhost:631/classes/<class-name>` as `-I` argument. Jobs will be distributed equally between included printers.

## Watermark customization

By default, the tool supports only IPv4 addresses and takes the third octet for watermark, filling it up to 3 digits with zeroes. For example, job that came from 10.21.32.43 will have watermark “032”.

To customize it, create a Lua script containing just a single function `get_team_id`. It should accept source IP address as a string and return either string — the watermark content — or `nil`. Be aware that the address might be an IPv6 and process that correctly.

Pass path to this script as `--team-id-script`.

You can find the built-in script described earlier at [`handler.rs:36`](src/handler.rs#L36).

If the script returns `nil`, the job will not be printed at all.

## Known issues

1. By some unknown reason, Linux and Windows treat the printer differently — one of them sends “mirrored” PDF files so that watermarks become mirrored.

   If you get this weird behaviour, the only way to fix it now is to change [`src/drawer.rs`](src/drawer.rs#L32) to comment or uncomment `Projection::scale(1.0, -1.0)` part and recompile the binary.

   We hope you have identical machines so you get either one or another behaviour on all of them.

2. Watermark text is colored `#808080C0`, so if contestant prints something gray, digits may be indistinguishable.

3. Initially I implemented native printing (using `libcups` on Linux / `winspool` on Windows), but when we tried to build it at NEF, it failed due to [bug in the the third-party Rust printing library](https://github.com/talesluna/rust-printers/issues/28), so I replaced it with printing via IPP.

  The bug seems to be fixed since, so maybe it's time to turn back. If your printer doesn't speak IPP and you can't use CUPS, create an issue.
