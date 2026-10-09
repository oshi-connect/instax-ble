//! Bluetooth Low Energy driver for Fujifilm Instax Link printers.
//!
//! The crate is split in layers so a host can take only what it needs:
//!
//! - [`packet`] is the pure Instax wire protocol: encode, decode, the opcode
//!   builders, image chunking, and the info-reply parsers. No radio, no image
//!   pipeline. It builds on every target.
//! - [`print`] holds the device-facing types ([`Printer`], [`PrintError`],
//!   [`PrinterModel`], [`PrinterStatus`]…) and the image pipeline that crops,
//!   darkens, and JPEG-encodes an [`Frame`] to the printer's limits.
//! - [`io`] runs the print work on its own thread so a UI thread never blocks
//!   on Bluetooth. It drives any [`Printer`], real or mock.
//! - [`transport`] is the real BLE transport ([`InstaxBle`]). It is behind the
//!   `ble` feature so a headless or test build skips the platform stack.
//!
//! The image types are owned ([`Frame`]) or borrowed ([`FrameRef`]) RGBA
//! buffers, so the crate does not depend on any particular compositor.

/// Dev trace for the wire. On in debug builds, and in release when
/// `INSTAX_BLE_TRACE` is set.
#[macro_export]
macro_rules! ble_trace {
    ($($arg:tt)*) => {
        if cfg!(debug_assertions) || std::env::var_os("INSTAX_BLE_TRACE").is_some() {
            eprintln!("[instax] {}", format_args!($($arg)*));
        }
    };
}

pub mod io;
pub mod packet;
pub mod print;

#[cfg(feature = "ble")]
pub mod transport;

pub use io::{
    spawn_default_print_io, spawn_print_io, DisconnectedPrinter, PrintIo, PrintIoCmd, PrintIoReply,
    PRINT_SHUTDOWN_TIMEOUT,
};
pub use packet::{
    apply_info, ble_transfer_packets, decode, discovered_printer, download_chunk_size,
    encode, fragments, image_to_chunks, is_instax_name, matches_reply, max_image_bytes_from_support,
    model_from_image_support, opcode_name, packet_summary, preferred_printer, response_summary,
    Response, FRAGMENT_SIZE,
};
pub use print::{
    apply_exposure_compensation, encode_jpeg, encode_print_jpeg, prepare_print_frame,
    print_clean_frame, safe_area, Battery, DiscoveredPrinter, Frame, FrameRef, PrintError,
    PrintJob, Printer, PrinterModel, PrinterStatus, Signal, DEFAULT_MAX_IMAGE_BYTES,
    EXPOSURE_DEFAULT, EXPOSURE_MAX,
};

#[cfg(feature = "ble")]
pub use transport::InstaxBle;
