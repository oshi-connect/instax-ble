# instax-ble

A Bluetooth Low Energy driver for Fujifilm Instax Link printers (Mini, Square,
and Wide) in Rust.

The crate talks to a printer over the Instax packet protocol. It encodes the
packets, runs the download handshake, prepares the image to the printer's paper
size and byte limit, and runs the print work on its own thread so a UI thread
never blocks on Bluetooth. It carries no UI and no compositor, so a Tauri app, a
GPUI app, or a headless tool can all use it.

## Layers

Take only the layer you need:

| Module | Feature | What it does |
| --- | --- | --- |
| `packet` | always | Pure Instax wire protocol: encode, decode, builders, chunking, info parsers. No radio. |
| `print` | always | Device types (`Printer`, `PrintError`, `PrinterModel`, `PrinterStatus`…) and the image pipeline (crop, darken, JPEG). |
| `io` | always | The print I/O thread. Drives any `Printer`, real or mock. |
| `transport` | `ble` | The real BLE transport (`InstaxBle`) over CoreBluetooth, BlueZ, or WinRT. |

The `ble` feature is off by default, so a test or headless build skips the
platform Bluetooth stack. Turn it on for the real transport:

```toml
instax-ble = { version = "0.1", features = ["ble"] }
```

## Image input

The pipeline reads an RGBA buffer through an owned `Frame` or a borrowed
`FrameRef`, so a host does not clone its own frame type to call the driver:

```rust
use instax_ble::{FrameRef, PrinterModel, Printer};

let rgba: Vec<u8> = vec![0; 600 * 800 * 4];
let frame = FrameRef::new(600, 800, &rgba);
let jpeg = instax_ble::encode_jpeg(frame)?;
# Ok::<(), instax_ble::PrintError>(())
```

## Print on a background thread

```rust
use instax_ble::{spawn_default_print_io, Frame, PrintIoCmd, PrinterModel};
use std::sync::mpsc;

let io = spawn_default_print_io(); // real BLE with the `ble` feature, else a stub
let (reply, rx) = mpsc::channel();
io.send(PrintIoCmd::Print {
    frame: Frame { width: 600, height: 800, rgba: vec![255; 600 * 800 * 4] },
    model: PrinterModel::Mini,
    exposure_percent: 30,
    reply,
});
// `rx` reports progress, then `Printed` or `Failed`.
```

## Linux build

The BlueZ backend needs D-Bus development headers:

```bash
sudo apt install libdbus-1-dev pkg-config
```

macOS and Windows need no extra system package. On macOS, run the host from a
signed bundle so the Bluetooth permission grant sticks.

## Protocol notes

See [`docs/compatibility.md`](docs/compatibility.md) for the image-transfer
contract, the byte-limit fallback, and a captured Mini Link+ hardware check.

## Releases

A green `CI` run on `main` creates a CalVer GitHub release (`vYYYY.MM.DD`). A later release on the same UTC day uses `.2`, then `.3`. The workflow posts that release to both Discord channels: OC Industry `#release-notes` and OC Backoffice `#ci`.

The repository secret `DISCORD_BOT_TOKEN` must be set before a release can post. This token is the Kanichan bot. The bot must be a member of both servers, and it must have Send Messages and Embed Links in those channels.

## License

MIT. See [LICENSE](LICENSE).
