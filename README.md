# Watermark Printer

Virtual IPP printer that acts as a proxy and adds watermarks to incoming jobs.

Tested on Windows 10, 11 and Linux (CUPS) clients.

## Build

You need the latest stable [Rust](https://www.rust-lang.org/). Run `cargo build -r` and fetch the binary at `target/release/watermark-printer`. It's ready!

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

## I don't know anything about printers, how do I set everything up?

1. You need the print server — any machine available to your clients capable of running this utility.

2. Either you need:

   a. a printer that can print via IPP — in that case you need to turn on IPP in the printer settings and get the URL;

      > It's recommended to set static IP to your printer to avoid issues when the IP address changes.
  
   b. or your print server must be a Linux machine running CUPS, and the printer should be connected to this server. In that case, your URL will be `http://localhost:631/printers/<printer-name>`.

3. If you use CUPS, **ensure that you have turned the firewall on and it is blocking outside connections to CUPS**.

   Firstly, there were vulnerabilities in CUPS that even allowed remote code execution on your server; and secondly, anyone will be able to print without watermarks.

   ```bash
   sudo cupsctl --no-remote-admin --no-remote-any

   # If you use iptables
   sudo iptables -A INPUT -s ! 127.0.0.1 -p tcp --dport 631 -j REJECT
   sudo iptables -A INPUT -s ! 127.0.0.1 -p udp --dport 631 -j REJECT
   sudo iptables-save | sudo tee /etc/iptables/rules.v4 # you need iptables-persistent package on your machine to make rules survive reboots

   # If you use UFW
   sudo ufw deny 631
   sudo ufw enable
   ```

   If you print directly to the printer via IPP, ensure that your clients don't have direct network access to that printer.

4. Run our tool:

   ```bash
   ./watermark-printer -p 6631 -I <your-url>
   ```

5. Add new printer to your clients. Use URL `ipp://<your-ip>:6631/` on Linux, and `http://<your-ip>:6631/` on Windows.

   On Linux, you should choose **Select printer from database** → **Generic** → **PDF** → **Generic PDF Printer [en]** when asked for the printer model.

6. Print the test page and see it with the watermark!

### Bonus: Round Robin

When using CUPS, you can easily distribute your jobs via several printers to increase throughput.

Open `http://localhost:631` on your print server. Go to **Administration** and click **Add Class**.

Authenticate with your current UNIX username and password.

Set the name (for example, `round-robin-class`) and choose a few printers.

Then use `http://localhost:631/classes/<class-name>` as `-I` argument. Jobs will be distributed equally between included printers.

## Watermark customization

By default, the tool supports only IPv4 addresses and takes the third octet for the watermark, filling it up to 3 digits with zeroes. For example, a job that came from 10.21.32.43 will have a watermark “032”.

To customize it, create a Lua script containing just a single function `get_team_id`. It should accept the source IP address as a string and return either string — the watermark content — or `nil`. Be aware that the address might be an IPv6 and process this case correctly.

Pass the path to this script as `--team-id-script`.

You can find the built-in script described earlier at [`default_team_id_script.lua`](src/default_team_id_script.lua).

If the script returns `nil`, the job will be skipped.

## Known issues

1. Watermark text is coloured `#808080C0`, so if the client prints something grey, digits may be indistinguishable.

2. Initially, I implemented native printing (using `libcups` on Linux / `winspool` on Windows), but when we tried to build it at NEF, it failed due to [a bug in the third-party Rust printing library](https://github.com/talesluna/rust-printers/issues/28), so I replaced it with printing via IPP.

   The bug seems to be fixed since, so maybe it's time to turn back. If your printer doesn't speak IPP and you can't use CUPS, create an issue.
