//! Device-facing types and the image pipeline.
//!
//! A host hands the driver an RGBA [`Frame`] (or a borrowed [`FrameRef`]). The
//! pipeline crops it to the printer aspect, darkens it for Instax film, and
//! JPEG-encodes it to the printer's byte limit before the transport sends it.

use std::sync::atomic::AtomicBool;

/// Owned RGBA image, 8 bits per channel, row-major, no stride padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        self.as_ref().pixel(x, y)
    }

    pub fn as_ref(&self) -> FrameRef<'_> {
        FrameRef {
            width: self.width,
            height: self.height,
            rgba: &self.rgba,
        }
    }
}

/// Borrowed RGBA image. The pipeline reads through this so a host does not
/// clone its own frame type to call the driver.
#[derive(Debug, Clone, Copy)]
pub struct FrameRef<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
}

impl<'a> FrameRef<'a> {
    pub fn new(width: u32, height: u32, rgba: &'a [u8]) -> Self {
        Self {
            width,
            height,
            rgba,
        }
    }

    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = ((y * self.width + x) * 4) as usize;
        self.rgba.get(i..i + 4)?.try_into().ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrinterModel {
    Mini,
    Square,
    Wide,
}

/// BLE RSSI mapped to a band. The host never sees a dBm number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Weak,
    Fair,
    Strong,
}

impl Signal {
    pub fn from_rssi(rssi: i16) -> Self {
        if rssi >= -65 {
            Self::Strong
        } else if rssi >= -80 {
            Self::Fair
        } else {
            Self::Weak
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Weak => "Weak",
            Self::Fair => "Fair",
            Self::Strong => "Strong",
        }
    }

    /// How many of the three meter bars this band fills.
    pub fn bars(self) -> u8 {
        match self {
            Self::Weak => 1,
            Self::Fair => 2,
            Self::Strong => 3,
        }
    }
}

/// Battery percent mapped to a band. The host never sees the raw reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Battery {
    Low,
    Ok,
    Full,
}

impl Battery {
    pub fn from_percent(level: u8) -> Self {
        if level <= 20 {
            Self::Low
        } else if level <= 60 {
            Self::Ok
        } else {
            Self::Full
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Ok => "Ok",
            Self::Full => "Full",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrinterStatus {
    pub model: Option<PrinterModel>,
    pub photos_left: Option<u8>,
    pub battery: Option<Battery>,
    pub charging: bool,
    pub signal: Option<Signal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPrinter {
    pub id: String,
    pub name: String,
    pub signal: Option<Signal>,
}

impl PrinterModel {
    pub const ALL: [Self; 3] = [Self::Mini, Self::Square, Self::Wide];

    pub fn size(self) -> (u32, u32) {
        match self {
            Self::Mini => (600, 800),
            Self::Square => (800, 800),
            Self::Wide => (1260, 840),
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "mini" => Some(Self::Mini),
            "square" => Some(Self::Square),
            "wide" => Some(Self::Wide),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mini => "Mini",
            Self::Square => "Square",
            Self::Wide => "Wide",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrintError {
    NotConnected,
    /// The machine has no usable Bluetooth radio, or it is off.
    NoBluetooth,
    /// The radio works, but no printer answered the scan.
    NoPrinter,
    /// The printer answered the scan but refused the link or the handshake.
    LinkFailed,
    Encode,
    ImageTooLarge {
        max_bytes: usize,
    },
    /// An explicit firmware rejection, distinct from a lost acknowledgement.
    PrinterRejected {
        opcode: u16,
        status: u8,
    },
    Transfer,
    TransferDetail {
        stage: String,
        detail: String,
    },
    /// The host is quitting and left the download open. Only the shutdown
    /// path makes this, and it means a START reached the printer.
    Cancelled,
}

impl std::fmt::Display for PrintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => write!(f, "printer not connected"),
            Self::NoBluetooth => write!(f, "Bluetooth unavailable"),
            Self::NoPrinter => write!(f, "no Instax printer found"),
            Self::LinkFailed => write!(f, "printer refused the link"),
            Self::Encode => write!(f, "print encode failed"),
            Self::ImageTooLarge { max_bytes } => write!(
                f,
                "cannot fit JPEG within the printer's {max_bytes}-byte limit"
            ),
            Self::Transfer => write!(f, "print failed"),
            Self::PrinterRejected { opcode, status } => {
                write!(
                    f,
                    "Printer rejected command 0x{opcode:04x}: status 0x{status:02x}"
                )?;
                if *opcode == 0x1000 && *status == 0x81 {
                    write!(f, "; the JPEG may exceed the usable byte limit")?;
                }
                Ok(())
            }
            Self::TransferDetail { stage, detail } => write!(f, "{stage}: {detail}"),
            Self::Cancelled => write!(f, "print cancelled"),
        }
    }
}

impl std::error::Error for PrintError {}

impl PrintError {
    pub fn transfer(stage: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::TransferDetail {
            stage: stage.into(),
            detail: detail.into(),
        }
    }
}

pub struct PrintJob<'a> {
    pub jpeg: &'a [u8],
    pub model: PrinterModel,
    /// Fraction of the image already sent, from 0.0 to 1.0.
    pub progress: &'a mut dyn FnMut(f32),
    /// Set when the host is quitting. A transfer reads it at each packet
    /// boundary and gives up with `Cancelled`, so a quit does not wait for a
    /// whole image. It is never set while the host runs.
    pub cancel: &'a AtomicBool,
}

pub trait Printer {
    fn send(&mut self, job: PrintJob<'_>) -> Result<(), PrintError>;

    fn connect(&mut self) -> Result<(), PrintError> {
        Err(PrintError::NotConnected)
    }

    fn disconnect(&mut self) -> Result<(), PrintError> {
        Ok(())
    }

    /// Tells the printer to drop the download it holds. A printer that still
    /// believes an image is on the way refuses the next START, which an
    /// operator sees as a printer that stopped working. Shutdown only.
    fn cancel_transfer(&mut self) -> Result<(), PrintError> {
        Ok(())
    }

    /// Paper size the connected printer reports. The printer wins over the
    /// operator setting, because it holds the film.
    fn negotiated_model(&self) -> Option<PrinterModel> {
        None
    }

    /// Maximum JPEG byte length reported by the connected printer, if available.
    fn max_image_bytes(&self) -> Option<usize> {
        None
    }

    fn scan(&mut self) -> Result<Vec<DiscoveredPrinter>, PrintError> {
        Ok(Vec::new())
    }

    fn connect_to(&mut self, _id: &str) -> Result<(), PrintError> {
        self.connect()
    }

    fn refresh_status(&mut self) -> Result<PrinterStatus, PrintError> {
        self.status().ok_or(PrintError::NotConnected)
    }

    fn status(&self) -> Option<PrinterStatus> {
        None
    }

    fn identity(&self) -> Option<(String, String)> {
        None
    }
}

/// Largest centered printer-aspect rect inside the source (object-fit contain).
pub fn safe_area(src_w: u32, src_h: u32, paper_w: u32, paper_h: u32) -> (u32, u32, u32, u32) {
    if src_w == 0 || src_h == 0 || paper_w == 0 || paper_h == 0 {
        return (0, 0, src_w, src_h);
    }
    let src_a = src_w as f32 / src_h as f32;
    let paper_a = paper_w as f32 / paper_h as f32;
    if src_a > paper_a {
        let w = ((src_h as f32) * paper_a).round() as u32;
        let w = w.min(src_w);
        ((src_w - w) / 2, 0, w, src_h)
    } else {
        let h = ((src_w as f32) / paper_a).round() as u32;
        let h = h.min(src_h);
        (0, (src_h - h) / 2, src_w, h)
    }
}

pub fn prepare_print_frame(frame: FrameRef<'_>, model: PrinterModel) -> Frame {
    let (dst_w, dst_h) = model.size();
    if frame.width == dst_w && frame.height == dst_h {
        return Frame {
            width: frame.width,
            height: frame.height,
            rgba: frame.rgba.to_vec(),
        };
    }
    let (sx, sy, sw, sh) = safe_area(frame.width, frame.height, dst_w, dst_h);
    let mut rgba = vec![0u8; (dst_w * dst_h * 4) as usize];
    for y in 0..dst_h {
        let src_y = sy + y * sh / dst_h.max(1);
        for x in 0..dst_w {
            let src_x = sx + x * sw / dst_w.max(1);
            if let Some(px) = frame.pixel(src_x, src_y) {
                let i = ((y * dst_w + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&px);
            }
        }
    }
    Frame {
        width: dst_w,
        height: dst_h,
        rgba,
    }
}

/// Darken RGB for Instax (multiply). Live preview and backups stay undarkened.
/// `percent` is clamped to 0..=100. Zero leaves the frame alone.
pub fn apply_exposure_compensation(frame: &mut Frame, percent: u8) {
    let percent = percent.min(100);
    if percent == 0 {
        return;
    }
    let keep = 100u16.saturating_sub(u16::from(percent));
    for pixel in frame.rgba.chunks_exact_mut(4) {
        pixel[0] = ((u16::from(pixel[0]) * keep) / 100) as u8;
        pixel[1] = ((u16::from(pixel[1]) * keep) / 100) as u8;
        pixel[2] = ((u16::from(pixel[2]) * keep) / 100) as u8;
    }
}

pub const EXPOSURE_DEFAULT: u8 = 30;
pub const EXPOSURE_MAX: u8 = 50;
/// Conservative compatibility limit when older firmware omits image support.
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 105 * 1024;

pub fn encode_jpeg(frame: FrameRef<'_>) -> Result<Vec<u8>, PrintError> {
    encode_rgb_jpeg(&frame_rgb(frame), frame.width, frame.height, 92)
}

fn frame_rgb(frame: FrameRef<'_>) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(frame.rgba.len() / 4 * 3);
    for pixel in frame.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&pixel[..3]);
    }
    rgb
}

fn encode_rgb_jpeg(
    rgb: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, PrintError> {
    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .encode(rgb, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|_| PrintError::Encode)?;
    Ok(out)
}

/// Keep printer dimensions and exposure; adjust only JPEG quality. Each trial
/// owns a fresh buffer so a smaller encode cannot retain trailing image bytes.
pub fn encode_print_jpeg(frame: FrameRef<'_>, max_bytes: usize) -> Result<Vec<u8>, PrintError> {
    let rgb = frame_rgb(frame);
    let encode = |quality| encode_rgb_jpeg(&rgb, frame.width, frame.height, quality);
    let original = encode(92)?;
    if original.len() <= max_bytes {
        return Ok(original);
    }
    let mut best = None;
    let (mut low, mut high) = (1u8, 91u8);
    while low <= high {
        let quality = low + (high - low) / 2;
        let candidate = encode(quality)?;
        if candidate.len() <= max_bytes {
            best = Some(candidate);
            low = quality + 1;
        } else {
            high = quality - 1;
        }
    }
    best.ok_or(PrintError::ImageTooLarge { max_bytes })
}

/// Crop, downscale, darken for the printer, then send. The caller keeps the
/// source frame on both success and failure.
pub fn print_clean_frame(
    frame: FrameRef<'_>,
    model: PrinterModel,
    exposure_percent: u8,
    printer: &mut impl Printer,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<(), PrintError> {
    let model = printer.negotiated_model().unwrap_or(model);
    let mut prepared = prepare_print_frame(frame, model);
    apply_exposure_compensation(&mut prepared, exposure_percent);
    let max_bytes = printer
        .max_image_bytes()
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_MAX_IMAGE_BYTES);
    let jpeg = encode_print_jpeg(prepared.as_ref(), max_bytes)?;
    crate::ble_trace!(
        "JPEG prepared: {} bytes (printer limit {max_bytes})",
        jpeg.len()
    );
    let result = printer.send(PrintJob {
        jpeg: &jpeg,
        model,
        progress,
        cancel,
    });
    // Link+ can reject START below its advertised 300 KiB limit. Retry only
    // this explicit pre-transfer refusal, never a timeout or PRINT rejection.
    if matches!(
        result,
        Err(PrintError::PrinterRejected {
            opcode: 0x1000,
            status: 0x81
        })
    ) && jpeg.len() > DEFAULT_MAX_IMAGE_BYTES
        && !cancel.load(std::sync::atomic::Ordering::Relaxed)
    {
        let jpeg = encode_print_jpeg(prepared.as_ref(), DEFAULT_MAX_IMAGE_BYTES)?;
        crate::ble_trace!(
            "START refused advertised budget; retrying with {} bytes",
            jpeg.len()
        );
        return printer.send(PrintJob {
            jpeg: &jpeg,
            model,
            progress,
            cancel,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Frame {
        let mut frame = Frame {
            width,
            height,
            rgba: vec![0; (width * height * 4) as usize],
        };
        for pixel in frame.rgba.chunks_exact_mut(4) {
            pixel.copy_from_slice(&rgba);
        }
        frame
    }

    fn noisy(width: u32, height: u32) -> Frame {
        let mut frame = solid(width, height, [0, 0, 0, 255]);
        let mut noise = 17u32;
        for pixel in frame.rgba.chunks_exact_mut(4) {
            for channel in &mut pixel[..3] {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                *channel = noise as u8;
            }
        }
        frame
    }

    #[derive(Default)]
    struct OkPrinter {
        sent_model: Option<PrinterModel>,
        negotiated: Option<PrinterModel>,
    }
    impl Printer for OkPrinter {
        fn send(&mut self, job: PrintJob<'_>) -> Result<(), PrintError> {
            assert!(!job.jpeg.is_empty());
            self.sent_model = Some(job.model);
            Ok(())
        }

        fn negotiated_model(&self) -> Option<PrinterModel> {
            self.negotiated
        }
    }

    struct FailPrinter;
    impl Printer for FailPrinter {
        fn send(&mut self, _job: PrintJob<'_>) -> Result<(), PrintError> {
            Err(PrintError::Transfer)
        }
    }

    #[test]
    fn detailed_photo_is_compressed_to_fit_before_transfer() {
        struct SizeLimitedPrinter;
        impl Printer for SizeLimitedPrinter {
            fn send(&mut self, job: PrintJob<'_>) -> Result<(), PrintError> {
                assert!(
                    job.jpeg.len() <= 105 * 1024,
                    "JPEG was {} bytes",
                    job.jpeg.len()
                );
                let decoded = image::load_from_memory(job.jpeg).unwrap();
                assert_eq!((decoded.width(), decoded.height()), (600, 800));
                Ok(())
            }
        }
        let frame = noisy(600, 800);
        assert!(encode_jpeg(frame.as_ref()).unwrap().len() > 105 * 1024);
        print_clean_frame(
            frame.as_ref(),
            PrinterModel::Mini,
            0,
            &mut SizeLimitedPrinter,
            &mut |_| {},
            &running(),
        )
        .unwrap();
    }

    #[test]
    fn fitting_jpeg_keeps_original_quality_and_accepts_exact_limit() {
        let frame = solid(600, 800, [80, 140, 200, 255]);
        let original = encode_jpeg(frame.as_ref()).unwrap();
        assert_eq!(
            encode_print_jpeg(frame.as_ref(), original.len()).unwrap(),
            original
        );
    }

    #[test]
    fn negotiated_limits_and_legacy_fallback_preserve_printer_dimensions() {
        struct CapturePrinter {
            model: PrinterModel,
            limit: Option<usize>,
            sent: Vec<u8>,
        }
        impl Printer for CapturePrinter {
            fn negotiated_model(&self) -> Option<PrinterModel> {
                Some(self.model)
            }
            fn max_image_bytes(&self) -> Option<usize> {
                self.limit
            }
            fn send(&mut self, job: PrintJob<'_>) -> Result<(), PrintError> {
                self.sent = job.jpeg.to_vec();
                Ok(())
            }
        }
        let frame = noisy(600, 800);
        for (model, limit, budget, dimensions) in [
            (PrinterModel::Mini, Some(65_536), 65_536, (600, 800)),
            (PrinterModel::Mini, Some(215_040), 215_040, (600, 800)),
            (PrinterModel::Mini, Some(0), 107_520, (600, 800)),
            (PrinterModel::Mini, None, 107_520, (600, 800)),
            (PrinterModel::Square, None, 107_520, (800, 800)),
            (PrinterModel::Wide, Some(337_920), 337_920, (1260, 840)),
        ] {
            let mut printer = CapturePrinter {
                model,
                limit,
                sent: vec![],
            };
            print_clean_frame(
                frame.as_ref(),
                PrinterModel::Wide,
                0,
                &mut printer,
                &mut |_| {},
                &running(),
            )
            .unwrap();
            assert!(printer.sent.len() <= budget, "{model:?} exceeded {budget}");
            assert!(
                printer.sent.ends_with(&[0xff, 0xd9]),
                "trailing bytes after JPEG"
            );
            let decoded = image::load_from_memory(&printer.sent).unwrap();
            assert_eq!((decoded.width(), decoded.height()), dimensions);
        }
    }

    #[test]
    fn negotiated_image_limit_is_checked_before_transfer() {
        struct TinyLimitPrinter;
        impl Printer for TinyLimitPrinter {
            fn max_image_bytes(&self) -> Option<usize> {
                Some(1)
            }
            fn send(&mut self, _: PrintJob<'_>) -> Result<(), PrintError> {
                panic!("oversized JPEG must never reach Bluetooth");
            }
        }
        let result = print_clean_frame(
            solid(600, 800, [80, 140, 200, 255]).as_ref(),
            PrinterModel::Mini,
            30,
            &mut TinyLimitPrinter,
            &mut |_| {},
            &running(),
        );
        assert_eq!(result, Err(PrintError::ImageTooLarge { max_bytes: 1 }));
    }

    #[test]
    fn explicit_start_size_rejection_retries_once_at_compatibility_limit() {
        struct RejectLarge {
            sizes: Vec<usize>,
        }
        impl Printer for RejectLarge {
            fn max_image_bytes(&self) -> Option<usize> {
                Some(307_200)
            }
            fn send(&mut self, job: PrintJob<'_>) -> Result<(), PrintError> {
                self.sizes.push(job.jpeg.len());
                if job.jpeg.len() > 107_520 {
                    Err(PrintError::PrinterRejected {
                        opcode: 0x1000,
                        status: 0x81,
                    })
                } else {
                    Ok(())
                }
            }
        }
        let frame = noisy(600, 800);
        let mut printer = RejectLarge { sizes: vec![] };
        print_clean_frame(
            frame.as_ref(),
            PrinterModel::Mini,
            0,
            &mut printer,
            &mut |_| {},
            &running(),
        )
        .unwrap();
        assert_eq!(printer.sizes.len(), 2);
        assert!(printer.sizes[0] > 107_520);
        assert!(printer.sizes[1] <= 107_520);
    }

    #[test]
    fn size_fallback_never_retries_ambiguous_or_late_errors() {
        struct Reject {
            error: PrintError,
            calls: usize,
        }
        impl Printer for Reject {
            fn max_image_bytes(&self) -> Option<usize> {
                Some(307_200)
            }
            fn send(&mut self, _: PrintJob<'_>) -> Result<(), PrintError> {
                self.calls += 1;
                Err(self.error.clone())
            }
        }
        let frame = noisy(600, 800);
        for (error, calls) in [
            (PrintError::transfer("DOWNLOAD_START", "timeout"), 1),
            (
                PrintError::PrinterRejected {
                    opcode: 0x1001,
                    status: 0x81,
                },
                1,
            ),
            (
                PrintError::PrinterRejected {
                    opcode: 0x1002,
                    status: 0x81,
                },
                1,
            ),
            (
                PrintError::PrinterRejected {
                    opcode: 0x1080,
                    status: 0x81,
                },
                1,
            ),
            (
                PrintError::PrinterRejected {
                    opcode: 0x1000,
                    status: 0x82,
                },
                1,
            ),
            (
                PrintError::PrinterRejected {
                    opcode: 0x1000,
                    status: 0x81,
                },
                2,
            ),
        ] {
            let mut printer = Reject {
                error: error.clone(),
                calls: 0,
            };
            let result = print_clean_frame(
                frame.as_ref(),
                PrinterModel::Mini,
                0,
                &mut printer,
                &mut |_| {},
                &running(),
            );
            assert_eq!(result, Err(error));
            assert_eq!(printer.calls, calls);
        }
    }

    #[test]
    fn mini_safe_area_is_centered_portrait_slice() {
        let (x, y, w, h) = safe_area(16, 9, 3, 4);
        assert_eq!(y, 0);
        assert_eq!(h, 9);
        assert!(w < 16);
        assert_eq!(x, (16 - w) / 2);
    }

    #[test]
    fn prepare_print_frame_is_printer_size() {
        let src = solid(16, 9, [10, 20, 30, 255]);
        let out = prepare_print_frame(src.as_ref(), PrinterModel::Mini);
        assert_eq!((out.width, out.height), (600, 800));
    }

    #[test]
    fn exposure_zero_leaves_pixels() {
        let mut frame = solid(2, 2, [200, 100, 50, 255]);
        apply_exposure_compensation(&mut frame, 0);
        assert_eq!(&frame.rgba[..4], &[200, 100, 50, 255]);
    }

    #[test]
    fn exposure_fifty_halves_rgb() {
        let mut frame = solid(1, 1, [200, 100, 50, 255]);
        apply_exposure_compensation(&mut frame, 50);
        assert_eq!(&frame.rgba[..4], &[100, 50, 25, 255]);
    }

    #[test]
    fn print_darkens_while_backup_prepare_does_not() {
        let src = solid(8, 8, [200, 100, 50, 255]);
        let backup = prepare_print_frame(src.as_ref(), PrinterModel::Square);
        let mut for_print = prepare_print_frame(src.as_ref(), PrinterModel::Square);
        apply_exposure_compensation(&mut for_print, 10);
        // Backup path is prepare only. Print path darkens a copy of that prepare.
        assert_ne!(&backup.rgba[..4], &for_print.rgba[..4]);
        assert!(for_print.rgba[0] < backup.rgba[0]);
        assert!(for_print.rgba[1] < backup.rgba[1]);
        assert!(for_print.rgba[2] < backup.rgba[2]);
        assert_eq!(for_print.rgba[3], backup.rgba[3]);
    }

    #[test]
    fn print_failure_leaves_caller_frame() {
        let frame = solid(8, 8, [1, 2, 3, 255]);
        let err = print_clean_frame(
            frame.as_ref(),
            PrinterModel::Square,
            0,
            &mut FailPrinter,
            &mut |_| {},
            &running(),
        )
        .unwrap_err();
        assert_eq!(err, PrintError::Transfer);
        assert_eq!(frame.width, 8);
        assert_eq!(frame.rgba.len(), 8 * 8 * 4);
    }

    #[test]
    fn print_success_sends_jpeg() {
        let frame = solid(8, 8, [200, 10, 10, 255]);
        let mut printer = OkPrinter::default();
        print_clean_frame(
            frame.as_ref(),
            PrinterModel::Square,
            10,
            &mut printer,
            &mut |_| {},
            &running(),
        )
        .unwrap();
        assert_eq!(printer.sent_model, Some(PrinterModel::Square));
    }

    #[test]
    fn connected_printer_overrides_the_operator_paper_size() {
        let frame = solid(8, 8, [200, 10, 10, 255]);
        let mut printer = OkPrinter {
            negotiated: Some(PrinterModel::Wide),
            ..OkPrinter::default()
        };
        print_clean_frame(
            frame.as_ref(),
            PrinterModel::Mini,
            0,
            &mut printer,
            &mut |_| {},
            &running(),
        )
        .unwrap();
        assert_eq!(printer.sent_model, Some(PrinterModel::Wide));
    }

    #[test]
    fn a_signal_band_is_one_to_three_bars() {
        assert_eq!(Signal::Weak.bars(), 1);
        assert_eq!(Signal::Fair.bars(), 2);
        assert_eq!(Signal::Strong.bars(), 3);
    }
}
