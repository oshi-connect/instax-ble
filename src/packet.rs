//! Instax packet layer. Pure bytes, no radio. The transport puts these on the
//! air; a headless build drops that module and keeps the builders.

use crate::print::{Battery, DiscoveredPrinter, PrintError, PrinterModel, PrinterStatus, Signal};

pub const OPCODE_SUPPORT_FUNCTION_INFO: u16 = 0x0002;
pub const OPCODE_PRINT_IMAGE_DOWNLOAD_START: u16 = 0x1000;
pub const OPCODE_PRINT_IMAGE_DOWNLOAD_DATA: u16 = 0x1001;
pub const OPCODE_PRINT_IMAGE_DOWNLOAD_END: u16 = 0x1002;
pub const OPCODE_PRINT_IMAGE_DOWNLOAD_CANCEL: u16 = 0x1003;
pub const OPCODE_PRINT_IMAGE: u16 = 0x1080;

pub const INFO_TYPE_IMAGE_SUPPORT: u8 = 0;
pub const INFO_TYPE_BATTERY: u8 = 1;
pub const INFO_TYPE_PRINTER_FUNCTION: u8 = 2;

/// Instax firmware drops writes above this size, whatever the negotiated MTU.
pub const FRAGMENT_SIZE: usize = 244;

const CHUNK_SIZE_MINI: usize = 900;
const CHUNK_SIZE_SQUARE: usize = 1808;
const CHUNK_SIZE_WIDE: usize = 900;

/// Instax packet encode: header, payload, checksum.
pub fn encode(opcode: u16, payload: &[u8]) -> Vec<u8> {
    let length = payload.len() + 7;
    let mut packet = vec![
        0x41,
        0x62,
        (length >> 8) as u8,
        length as u8,
        (opcode >> 8) as u8,
        opcode as u8,
    ];
    packet.extend_from_slice(payload);
    let sum: u8 = packet.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    packet.push(sum ^ 0xff);
    packet
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub opcode: u16,
    /// Non-zero means the printer rejected the command.
    pub status: u8,
    /// Echo of the first request byte. Tells apart the three info replies.
    pub command: u8,
    pub payload: Vec<u8>,
}

pub fn decode(bytes: &[u8]) -> Result<Response, PrintError> {
    if bytes.len() < 8 {
        return Err(PrintError::Transfer);
    }
    let sum: u8 = bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    if sum != 0xff {
        return Err(PrintError::Transfer);
    }
    Ok(Response {
        opcode: u16::from_be_bytes([bytes[4], bytes[5]]),
        status: bytes[6],
        command: if bytes.len() > 8 { bytes[7] } else { 0 },
        payload: if bytes.len() > 9 {
            bytes[8..bytes.len() - 1].to_vec()
        } else {
            Vec::new()
        },
    })
}

pub fn opcode_name(opcode: u16) -> &'static str {
    match opcode {
        OPCODE_SUPPORT_FUNCTION_INFO => "INFO",
        OPCODE_PRINT_IMAGE_DOWNLOAD_START => "DOWNLOAD_START",
        OPCODE_PRINT_IMAGE_DOWNLOAD_DATA => "DOWNLOAD_DATA",
        OPCODE_PRINT_IMAGE_DOWNLOAD_END => "DOWNLOAD_END",
        OPCODE_PRINT_IMAGE_DOWNLOAD_CANCEL => "DOWNLOAD_CANCEL",
        OPCODE_PRINT_IMAGE => "PRINT_IMAGE",
        _ => "UNKNOWN",
    }
}

/// One line per outgoing packet for the dev trace: opcode, size, and the
/// number of BLE writes it takes.
pub fn packet_summary(packet: &[u8]) -> String {
    let opcode = match packet {
        [_, _, _, _, hi, lo, ..] => u16::from_be_bytes([*hi, *lo]),
        _ => 0,
    };
    let writes = packet.len().div_ceil(FRAGMENT_SIZE).max(1);
    format!(
        "{} 0x{opcode:04x} {}B/{writes}w",
        opcode_name(opcode),
        packet.len()
    )
}

pub fn response_summary(response: &Response) -> String {
    format!(
        "{} 0x{:04x} status={} cmd={} {}B",
        opcode_name(response.opcode),
        response.opcode,
        response.status,
        response.command,
        response.payload.len()
    )
}

/// Unsolicited status notifications must not acknowledge an image packet.
pub fn matches_reply(response: &Response, packet: &[u8]) -> bool {
    let Some(opcode) = packet.get(4..6) else {
        return false;
    };
    response.opcode == u16::from_be_bytes([opcode[0], opcode[1]])
        && (response.opcode != OPCODE_SUPPORT_FUNCTION_INFO
            || packet.get(6).copied() == Some(response.command))
}

/// Split one packet into writes the printer accepts. Only the last one is acked.
pub fn fragments(packet: &[u8]) -> impl Iterator<Item = &[u8]> {
    packet.chunks(FRAGMENT_SIZE)
}

/// Advertised name of every Instax printer this transport talks to.
pub fn is_instax_name(name: &str) -> bool {
    name.trim().to_ascii_uppercase().starts_with("INSTAX")
}

/// Paper size the printer reports in its image-support reply.
pub fn model_from_image_support(payload: &[u8]) -> Option<PrinterModel> {
    if payload.len() < 4 {
        return None;
    }
    match (
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
    ) {
        (600, 800) => Some(PrinterModel::Mini),
        (800, 800) => Some(PrinterModel::Square),
        (1260, 840) => Some(PrinterModel::Wide),
        _ => None,
    }
}

/// IMAGE_SUPPORT carries width/height, format/version, then a big-endian byte
/// limit. See https://github.com/javl/InstaxBLE/issues/18 for captured replies.
pub fn max_image_bytes_from_support(payload: &[u8]) -> Option<usize> {
    let bytes: [u8; 4] = payload.get(6..10)?.try_into().ok()?;
    let limit = u32::from_be_bytes(bytes) as usize;
    (limit > 0).then_some(limit)
}

pub fn image_to_chunks(img_data: &[u8], chunk_size: usize) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    let mut i = 0;
    let mut index = 0u32;
    while i < img_data.len() {
        let end = (i + chunk_size).min(img_data.len());
        let raw = &img_data[i..end];
        let mut padded = vec![0u8; chunk_size];
        padded[..raw.len()].copy_from_slice(raw);
        let mut combined = Vec::with_capacity(4 + chunk_size);
        combined.extend_from_slice(&index.to_be_bytes());
        combined.extend_from_slice(&padded);
        chunks.push(combined);
        i = end;
        index += 1;
    }
    chunks
}

pub fn chunk_size(model: PrinterModel) -> usize {
    match model {
        PrinterModel::Mini => CHUNK_SIZE_MINI,
        PrinterModel::Square => CHUNK_SIZE_SQUARE,
        PrinterModel::Wide => CHUNK_SIZE_WIDE,
    }
}

/// START returns a big-endian u32 chunk size. `decode` places its high byte
/// in `command`; only INFO responses use that byte as an echoed subcommand.
pub fn download_chunk_size(response: &Response, model: PrinterModel) -> Result<usize, PrintError> {
    if response.payload.is_empty() && response.command == 0 {
        return Ok(chunk_size(model));
    }
    let Some(&[b1, b2, b3]) = response.payload.get(..3) else {
        return Err(PrintError::transfer(
            "DOWNLOAD_START",
            "truncated chunk size",
        ));
    };
    let size = u32::from_be_bytes([response.command, b1, b2, b3]) as usize;
    // The packet length is u16 and includes seven framing and four index bytes.
    if size == 0 || size > u16::MAX as usize - 11 {
        return Err(PrintError::transfer(
            "DOWNLOAD_START",
            format!("invalid chunk size {size}"),
        ));
    }
    Ok(size)
}

pub fn download_start_packet(byte_len: u32) -> Vec<u8> {
    encode(
        OPCODE_PRINT_IMAGE_DOWNLOAD_START,
        &[
            0x02,
            0x00,
            0x00,
            0x00,
            (byte_len >> 24) as u8,
            (byte_len >> 16) as u8,
            (byte_len >> 8) as u8,
            byte_len as u8,
        ],
    )
}

pub fn download_data_packet(chunk: &[u8]) -> Vec<u8> {
    encode(OPCODE_PRINT_IMAGE_DOWNLOAD_DATA, chunk)
}

pub fn download_end_packet() -> Vec<u8> {
    encode(OPCODE_PRINT_IMAGE_DOWNLOAD_END, &[])
}

pub fn download_cancel_packet() -> Vec<u8> {
    encode(OPCODE_PRINT_IMAGE_DOWNLOAD_CANCEL, &[])
}

pub fn print_image_packet() -> Vec<u8> {
    encode(OPCODE_PRINT_IMAGE, &[])
}

pub fn info_packet(info_type: u8) -> Vec<u8> {
    encode(OPCODE_SUPPORT_FUNCTION_INFO, &[info_type])
}

/// Fold one info reply into the status the host can show. Unknown types are
/// ignored, so a handshake can feed every reply through the same path.
pub fn apply_info(status: &mut PrinterStatus, response: &Response) {
    match response.command {
        INFO_TYPE_IMAGE_SUPPORT => {
            if let Some(model) = model_from_image_support(&response.payload) {
                status.model = Some(model);
            }
        }
        INFO_TYPE_BATTERY => {
            if response.payload.len() >= 2 {
                status.charging = response.payload[0] > 5;
                status.battery = Some(Battery::from_percent(response.payload[1]));
            }
        }
        INFO_TYPE_PRINTER_FUNCTION => {
            if let Some(&byte) = response.payload.first() {
                status.photos_left = Some(byte & 0x0f);
                status.charging = (byte & 0x80) != 0;
            }
        }
        _ => {}
    }
}

pub fn discovered_printer(
    id: impl Into<String>,
    name: impl Into<String>,
    rssi: Option<i16>,
) -> DiscoveredPrinter {
    DiscoveredPrinter {
        id: id.into(),
        name: name.into(),
        signal: rssi.map(Signal::from_rssi),
    }
}

pub fn preferred_printer<'a>(
    found: &'a [DiscoveredPrinter],
    last_id: Option<&str>,
) -> Option<&'a DiscoveredPrinter> {
    last_id
        .and_then(|id| found.iter().find(|printer| printer.id == id))
        .or_else(|| found.first())
}

/// Instax download start + data packets. A host can pre-build these before the
/// transport writes them.
pub fn ble_transfer_packets(jpeg: &[u8], model: PrinterModel) -> Result<Vec<Vec<u8>>, PrintError> {
    let start = download_start_packet(jpeg.len() as u32);
    decode(&start)?;
    let mut packets = vec![start];
    for chunk in image_to_chunks(jpeg, chunk_size(model)) {
        let packet = download_data_packet(&chunk);
        decode(&packet)?;
        packets.push(packet);
    }
    Ok(packets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_uses_the_printers_chunk_size_instead_of_the_model_default() {
        // Captured DOWNLOAD_START ACK: the four bytes after status are 900.
        let response = decode(&[0x61, 0x42, 0, 12, 0x10, 0, 0, 0, 0, 3, 0x84, 0xb9]).unwrap();
        assert_eq!(
            download_chunk_size(&response, PrinterModel::Square),
            Ok(900)
        );
    }

    #[test]
    fn download_rejects_invalid_chunk_sizes() {
        for payload in [vec![0, 0, 0], vec![0, 0], vec![1, 0, 0]] {
            let response = Response {
                opcode: OPCODE_PRINT_IMAGE_DOWNLOAD_START,
                status: 0,
                command: 0,
                payload,
            };
            assert!(download_chunk_size(&response, PrinterModel::Mini).is_err());
        }
    }

    #[test]
    fn download_without_chunk_size_keeps_legacy_model_defaults() {
        let response = Response {
            opcode: OPCODE_PRINT_IMAGE_DOWNLOAD_START,
            status: 0,
            command: 0,
            payload: vec![],
        };
        assert_eq!(download_chunk_size(&response, PrinterModel::Mini), Ok(900));
        assert_eq!(
            download_chunk_size(&response, PrinterModel::Square),
            Ok(1808)
        );
    }

    #[test]
    fn encode_checksum_round_trips() {
        let packet = encode(0x0002, &[1, 2, 3]);
        assert_eq!(decode(&packet).unwrap().opcode, 0x0002);
    }

    #[test]
    fn decode_rejects_short_packet() {
        assert_eq!(decode(&[1, 2, 3]), Err(PrintError::Transfer));
    }

    #[test]
    fn packet_summary_names_the_opcode_and_counts_the_writes() {
        let chunk = vec![0u8; 4 + CHUNK_SIZE_SQUARE];
        let packet = download_data_packet(&chunk);
        assert_eq!(
            packet_summary(&packet),
            format!("DOWNLOAD_DATA 0x1001 {}B/8w", packet.len())
        );
        assert_eq!(
            packet_summary(&download_end_packet()),
            "DOWNLOAD_END 0x1002 7B/1w"
        );
        assert_eq!(packet_summary(&[]), "UNKNOWN 0x0000 0B/1w");
    }

    #[test]
    fn response_summary_shows_status_and_payload_size() {
        let response = decode(&encode(OPCODE_PRINT_IMAGE_DOWNLOAD_START, &[0, 7, 9])).unwrap();
        assert_eq!(
            response_summary(&response),
            "DOWNLOAD_START 0x1000 status=0 cmd=7 1B"
        );
    }

    #[test]
    fn transfer_packets_start_with_download_opcode() {
        let packets = ble_transfer_packets(&[1, 2, 3, 4], PrinterModel::Mini).unwrap();
        assert_eq!(
            decode(&packets[0]).unwrap().opcode,
            OPCODE_PRINT_IMAGE_DOWNLOAD_START
        );
    }

    #[test]
    fn fragments_stay_within_the_printer_write_limit() {
        let packet = download_data_packet(&[0u8; 1808]);
        let parts: Vec<_> = fragments(&packet).collect();
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|part| part.len() <= FRAGMENT_SIZE));
        assert_eq!(parts.concat(), packet);
    }

    #[test]
    fn image_support_reply_names_the_paper_size() {
        assert_eq!(
            model_from_image_support(&[0x03, 0x20, 0x03, 0x20]),
            Some(PrinterModel::Square)
        );
        assert_eq!(model_from_image_support(&[0x02, 0x58]), None);
    }

    #[test]
    fn captured_wide_support_reply_reports_330_kib_limit() {
        let response = decode(&[
            0x61, 0x42, 0x00, 0x13, 0x00, 0x02, 0x00, 0x00, 0x04, 0xec, 0x03, 0x48, 0x02, 0x7b,
            0x00, 0x05, 0x28, 0x00, 0x62,
        ])
        .unwrap();
        assert_eq!(
            max_image_bytes_from_support(&response.payload),
            Some(337_920)
        );
    }

    #[test]
    fn absent_or_zero_image_limit_uses_compatibility_fallback() {
        assert_eq!(max_image_bytes_from_support(&[0; 9]), None);
        assert_eq!(max_image_bytes_from_support(&[0; 10]), None);
    }

    #[test]
    fn captured_link_plus_support_reply_keeps_standard_mini_dimensions() {
        let payload = [
            0x02, 0x58, 0x03, 0x20, 0x02, 0x4b, 0x00, 0x04, 0xb0, 0x00, 0x01, 0, 0, 0,
        ];
        assert_eq!(model_from_image_support(&payload), Some(PrinterModel::Mini));
        assert_eq!(max_image_bytes_from_support(&payload), Some(307_200));
    }

    #[test]
    fn only_instax_advertisements_match() {
        assert!(is_instax_name("INSTAX-12345678(IOS)"));
        assert!(is_instax_name(" instax mini Link "));
        assert!(!is_instax_name("Bose QC45"));
    }

    #[test]
    fn image_chunks_include_index_prefix() {
        let chunks = image_to_chunks(&[1, 2, 3], 4);
        assert_eq!(chunks.len(), 1);
        assert_eq!(&chunks[0][..4], &0u32.to_be_bytes());
        assert_eq!(&chunks[0][4..7], &[1, 2, 3]);
    }

    #[test]
    fn rssi_maps_to_a_band_and_never_a_dbm_label() {
        assert_eq!(Signal::from_rssi(-40), Signal::Strong);
        assert_eq!(Signal::from_rssi(-65), Signal::Strong);
        assert_eq!(Signal::from_rssi(-66), Signal::Fair);
        assert_eq!(Signal::from_rssi(-80), Signal::Fair);
        assert_eq!(Signal::from_rssi(-81), Signal::Weak);
        assert_eq!(Signal::from_rssi(-100), Signal::Weak);
        for rssi in [-40, -65, -66, -80, -81, -100] {
            assert!(!Signal::from_rssi(rssi).label().contains("dBm"));
            assert!(!Signal::from_rssi(rssi).label().contains(&rssi.to_string()));
        }
    }

    #[test]
    fn battery_payload_becomes_a_band_not_a_percent() {
        let mut status = PrinterStatus::default();
        apply_info(
            &mut status,
            &decode(&info_packet(INFO_TYPE_BATTERY)).unwrap(),
        );
        // The request packet is not a reply. A live battery reply carries
        // charging in byte 0 and the percent in byte 1.
        apply_info(
            &mut status,
            &Response {
                opcode: OPCODE_SUPPORT_FUNCTION_INFO,
                status: 0,
                command: INFO_TYPE_BATTERY,
                payload: vec![0, 10],
            },
        );
        assert_eq!(status.battery, Some(Battery::Low));
        apply_info(
            &mut status,
            &Response {
                opcode: OPCODE_SUPPORT_FUNCTION_INFO,
                status: 0,
                command: INFO_TYPE_BATTERY,
                payload: vec![0, 40],
            },
        );
        assert_eq!(status.battery, Some(Battery::Ok));
        apply_info(
            &mut status,
            &Response {
                opcode: OPCODE_SUPPORT_FUNCTION_INFO,
                status: 0,
                command: INFO_TYPE_BATTERY,
                payload: vec![6, 80],
            },
        );
        assert_eq!(status.battery, Some(Battery::Full));
        assert!(status.charging);
        assert!(!Battery::Low.label().contains('%'));
    }

    #[test]
    fn printer_function_payload_is_the_remaining_shots() {
        let mut status = PrinterStatus::default();
        apply_info(
            &mut status,
            &Response {
                opcode: OPCODE_SUPPORT_FUNCTION_INFO,
                status: 0,
                command: INFO_TYPE_PRINTER_FUNCTION,
                payload: vec![0x07],
            },
        );
        assert_eq!(status.photos_left, Some(7));
        assert!(!status.charging);
        apply_info(
            &mut status,
            &Response {
                opcode: OPCODE_SUPPORT_FUNCTION_INFO,
                status: 0,
                command: INFO_TYPE_PRINTER_FUNCTION,
                payload: vec![0x87],
            },
        );
        assert_eq!(status.photos_left, Some(7));
        assert!(status.charging);
    }

    #[test]
    fn a_discovered_printer_carries_a_signal_band_not_rssi() {
        let found = discovered_printer("aa", "INSTAX-1", Some(-72));
        assert_eq!(found.signal, Some(Signal::Fair));
        assert_eq!(found.name, "INSTAX-1");
        let debug = format!("{found:?}");
        assert!(!debug.contains("-72"));
        assert!(!debug.to_lowercase().contains("dbm"));
    }

    #[test]
    fn preferred_printer_is_the_last_one_when_it_is_still_in_range() {
        let found = [
            DiscoveredPrinter {
                id: "strong".into(),
                name: "INSTAX-A".into(),
                signal: None,
            },
            DiscoveredPrinter {
                id: "last".into(),
                name: "INSTAX-B".into(),
                signal: None,
            },
        ];
        assert_eq!(
            preferred_printer(&found, Some("last")).map(|p| p.id.as_str()),
            Some("last")
        );
        assert_eq!(
            preferred_printer(&found, Some("gone")).map(|p| p.id.as_str()),
            Some("strong")
        );
        assert_eq!(
            preferred_printer(&found, None).map(|p| p.id.as_str()),
            Some("strong")
        );
    }
}
