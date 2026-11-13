# Neo

Send files of any size to one or many devices at once: direct, end-to-end
encrypted, and with no account. See [ARCHITECTURE.md](ARCHITECTURE.md).

## Download

Grab `neo.exe` from the [latest release](../../releases/latest) and run it.
There's nothing to install and no account. Windows may show a SmartScreen
warning because the app isn't code-signed: choose "More info", then "Run
anyway".

## Build (Windows)

You need Rust (stable, via rustup), Visual Studio Build Tools with the
"Desktop development with C++" workload, and CMake on PATH. Then:

```powershell
cargo build --release      # target/release/neo.exe and neo-cli.exe
cargo test --release
```

`build.ps1` wraps the same commands for a self-contained toolchain folder
(`NEO_TOOLROOT`).

## Run

```powershell
target/release/neo.exe                         # normal use
target/release/neo.exe --profile Laptop        # several instances on one PC
```

Received files go to `Downloads\Neo` (or `Downloads\Neo\<profile>`). You can
change this, and the device name, on the Settings page.

Contacts far away are reached over the internet automatically, with no
server, VPN or port forwarding. Set `NEO_DEBUG=1` to see what the network
layer is doing. `neo-cli --no-lan --no-private` exercises the internet path on
one machine.

Interrupted transfers resume on their own. If the connection drops, the
sender keeps retrying. If either app was closed, send the same files again
and Neo continues from where it stopped.

Headless node, for scripts and tests:

```powershell
target/release/neo-cli.exe receive --profile a --downloads D:\in
target/release/neo-cli.exe send --profile b --peers 2 D:\big.iso
```

`neo-cli pair CODE` asks the person with that code to become contacts.
`neo-cli link CODE` asks your device with that code to share your identity;
it prints the number to compare.

Testing flags for `neo.exe`:

- `--auto-accept` accepts every incoming offer without asking.
- `--demo-send FILE --demo-peers N` selects all devices once N are visible
  and sends FILE through the normal UI.
- `--open-settings` starts on the Settings page.

## License

MIT, see [LICENSE](LICENSE). `vendor/gpui` is a patched copy of GPUI from the
Zed project, under its own Apache-2.0 license (see `vendor/gpui/LICENSE-APACHE`
and `vendor/gpui/NEO-PATCH.md`).
