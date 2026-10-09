//! Bluetooth transport for Instax printers.
//!
//! [`crate::packet`] owns the packet bytes and stays pure. This module only
//! moves those bytes over the air. It runs on the print I/O thread, so every
//! call here may block for seconds. The UI thread must never call it.

use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::time::Duration;

use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures_util::{Stream, StreamExt};
use tokio::runtime::Runtime;
use tokio::time::{sleep, timeout, Instant};
use uuid::{uuid, Uuid};

use crate::packet::{
    apply_info, decode, discovered_printer, download_cancel_packet, download_chunk_size,
    download_data_packet, download_end_packet, download_start_packet, fragments, image_to_chunks,
    info_packet, is_instax_name, matches_reply, max_image_bytes_from_support, opcode_name,
    packet_summary, print_image_packet, response_summary, Response, INFO_TYPE_BATTERY,
    INFO_TYPE_IMAGE_SUPPORT, INFO_TYPE_PRINTER_FUNCTION,
};
use crate::print::{
    DiscoveredPrinter, PrintError, PrintJob, Printer, PrinterModel, PrinterStatus, Signal,
};

const WRITE_CHAR: Uuid = uuid!("70954783-2d83-473d-9e5f-81e1d02d5273");
const NOTIFY_CHAR: Uuid = uuid!("70954784-2d83-473d-9e5f-81e1d02d5273");

const SCAN_WINDOW: Duration = Duration::from_secs(3);
const SCAN_POLL: Duration = Duration::from_millis(250);
const CONNECT_ATTEMPTS: u32 = 4;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(1_500);
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// The quit path waits far less than a print does. It has a whole-shutdown
/// budget to stay inside, and a printer that does not answer the cancel is
/// already past helping.
const SHUTDOWN_ACK_TIMEOUT: Duration = Duration::from_millis(400);
const SHUTDOWN_DISCONNECT_TIMEOUT: Duration = Duration::from_millis(400);

type Notifications = Pin<Box<dyn Stream<Item = ValueNotification> + Send>>;

async fn transfer_with_deadline<T>(
    operation: impl std::future::Future<Output = Result<T, PrintError>>,
    limit: Duration,
) -> Result<T, PrintError> {
    timeout(limit, operation).await.map_err(|_| {
        PrintError::transfer(
            "Bluetooth",
            format!("command timed out after {:.1}s", limit.as_secs_f32()),
        )
    })?
}

pub struct InstaxBle {
    runtime: Runtime,
    session: Option<Session>,
}

impl InstaxBle {
    /// Upload and validate an image without issuing PRINT_IMAGE, then request
    /// cancellation. Some firmware refuses cancellation after DOWNLOAD_END;
    /// callers must disconnect after this diagnostic.
    /// Uses the same transfer and acknowledgements as printing.
    pub fn send_only(&mut self, job: PrintJob<'_>) -> Result<(), PrintError> {
        let Self { runtime, session } = self;
        let session = session.as_mut().ok_or(PrintError::NotConnected)?;
        runtime.block_on(transfer_image(
            job,
            TransferMode::SendOnly,
            async |packet: &[u8]| session.command(packet).await,
        ))
    }

    pub fn new() -> Result<Self, PrintError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| PrintError::NotConnected)?;
        Ok(Self {
            runtime,
            session: None,
        })
    }
}

impl Printer for InstaxBle {
    fn connect(&mut self) -> Result<(), PrintError> {
        self.open(None)
    }

    fn connect_to(&mut self, id: &str) -> Result<(), PrintError> {
        self.open(Some(id))
    }

    fn disconnect(&mut self) -> Result<(), PrintError> {
        let Self { runtime, session } = self;
        if let Some(session) = session.take() {
            ble_trace!("disconnect {} ({})", session.name, session.id);
            runtime.block_on(async {
                let _ = timeout(SHUTDOWN_DISCONNECT_TIMEOUT, session.peripheral.disconnect()).await;
            });
        }
        Ok(())
    }

    fn cancel_transfer(&mut self) -> Result<(), PrintError> {
        let Self { runtime, session } = self;
        let session = session.as_mut().ok_or(PrintError::NotConnected)?;
        runtime.block_on(session.cancel_download())
    }

    fn send(&mut self, job: PrintJob<'_>) -> Result<(), PrintError> {
        let Self { runtime, session } = self;
        let session = session.as_mut().ok_or(PrintError::NotConnected)?;
        runtime.block_on(transfer_image(
            job,
            TransferMode::Print,
            async |packet: &[u8]| session.command(packet).await,
        ))
    }

    fn negotiated_model(&self) -> Option<PrinterModel> {
        self.session.as_ref().map(|session| session.model)
    }

    fn max_image_bytes(&self) -> Option<usize> {
        self.session
            .as_ref()
            .and_then(|session| session.max_image_bytes)
    }

    fn scan(&mut self) -> Result<Vec<DiscoveredPrinter>, PrintError> {
        self.runtime.block_on(async {
            let central = adapter().await?;
            let hits = scan_instax(&central, None, true).await?;
            Ok(hits
                .into_iter()
                .map(|hit| discovered_printer(hit.id, hit.name, hit.rssi))
                .collect())
        })
    }

    fn refresh_status(&mut self) -> Result<PrinterStatus, PrintError> {
        let Self { runtime, session } = self;
        let session = session.as_mut().ok_or(PrintError::NotConnected)?;
        runtime.block_on(session.refresh_status())
    }

    fn status(&self) -> Option<PrinterStatus> {
        self.session.as_ref().map(|session| session.status.clone())
    }

    fn identity(&self) -> Option<(String, String)> {
        self.session
            .as_ref()
            .map(|session| (session.id.clone(), session.name.clone()))
    }
}

impl InstaxBle {
    fn open(&mut self, id: Option<&str>) -> Result<(), PrintError> {
        self.disconnect()?;
        let Self { runtime, session } = self;
        *session = Some(runtime.block_on(open_session(id))?);
        Ok(())
    }
}

struct Session {
    peripheral: Peripheral,
    write: Characteristic,
    notifications: Notifications,
    model: PrinterModel,
    max_image_bytes: Option<usize>,
    id: String,
    name: String,
    status: PrinterStatus,
}

#[derive(Clone, Copy)]
enum TransferMode {
    Print,
    SendOnly,
}

async fn transfer_image(
    job: PrintJob<'_>,
    mode: TransferMode,
    mut command: impl AsyncFnMut(&[u8]) -> Result<Response, PrintError>,
) -> Result<(), PrintError> {
    let began = Instant::now();
    let result = async {
        let start = command(&download_start_packet(job.jpeg.len() as u32)).await?;
        let chunk_len = download_chunk_size(&start, job.model)?;
        let chunks = image_to_chunks(job.jpeg, chunk_len);
        ble_trace!(
            "transfer {:?}: {}B, {} chunks of {}B",
            job.model,
            job.jpeg.len(),
            chunks.len(),
            chunk_len
        );
        let total = chunks.len().max(1) as f32;
        for (index, chunk) in chunks.iter().enumerate() {
            if job.cancel.load(Ordering::Relaxed) {
                return Err(PrintError::Cancelled);
            }
            command(&download_data_packet(chunk))
                .await
                .map_err(|error| {
                    PrintError::transfer(
                        format!("Packet {}/{}", index + 1, chunks.len()),
                        error.to_string(),
                    )
                })?;
            (job.progress)((index + 1) as f32 / total);
        }
        if job.cancel.load(Ordering::Relaxed) {
            return Err(PrintError::Cancelled);
        }
        command(&download_end_packet()).await?;
        if job.cancel.load(Ordering::Relaxed) {
            return Err(PrintError::Cancelled);
        }
        match mode {
            TransferMode::Print => {
                command(&print_image_packet()).await?;
            }
            TransferMode::SendOnly => match command(&download_cancel_packet()).await {
                Ok(_) => {}
                Err(PrintError::PrinterRejected {
                    opcode: 0x1003,
                    status: 0x82,
                }) => {
                    ble_trace!("CANCEL refused after accepted END; disconnect without PRINT_IMAGE");
                }
                Err(error) => return Err(error),
            },
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        ble_trace!("print failed: {error}");
        // A failed START/END can leave an open download too. Never retry
        // PRINT_IMAGE automatically: a lost reply may still have printed.
        if error != PrintError::Cancelled {
            let _ = command(&download_cancel_packet()).await;
        }
        return Err(error);
    }

    ble_trace!("transfer completed in {}ms", began.elapsed().as_millis());
    Ok(())
}

impl Session {
    /// Gives the open download back to the printer on the way out. The wait
    /// is short: the packet matters, the reply only confirms it.
    async fn cancel_download(&mut self) -> Result<(), PrintError> {
        for fragment in fragments(&download_cancel_packet()) {
            self.peripheral
                .write(&self.write, fragment, WriteType::WithoutResponse)
                .await
                .map_err(|error| PrintError::transfer("Bluetooth write", error.to_string()))?;
        }
        await_ack(
            &mut self.notifications,
            &download_cancel_packet(),
            SHUTDOWN_ACK_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    async fn handshake(&mut self) -> Result<(), PrintError> {
        let support = self.command(&info_packet(INFO_TYPE_IMAGE_SUPPORT)).await?;
        self.max_image_bytes = max_image_bytes_from_support(&support.payload);
        ble_trace!(
            "image support: {:02x?}; limit {:?} bytes",
            support.payload,
            self.max_image_bytes
        );
        apply_info(&mut self.status, &support);
        if let Some(model) = self.status.model {
            self.model = model;
        }
        let battery = self.command(&info_packet(INFO_TYPE_BATTERY)).await?;
        apply_info(&mut self.status, &battery);
        let function = self
            .command(&info_packet(INFO_TYPE_PRINTER_FUNCTION))
            .await?;
        apply_info(&mut self.status, &function);
        self.refresh_signal().await;
        Ok(())
    }

    async fn refresh_status(&mut self) -> Result<PrinterStatus, PrintError> {
        let battery = self.command(&info_packet(INFO_TYPE_BATTERY)).await?;
        apply_info(&mut self.status, &battery);
        let function = self
            .command(&info_packet(INFO_TYPE_PRINTER_FUNCTION))
            .await?;
        apply_info(&mut self.status, &function);
        self.refresh_signal().await;
        Ok(self.status.clone())
    }

    async fn refresh_signal(&mut self) {
        if let Ok(Some(props)) = self.peripheral.properties().await {
            self.status.signal = props.rssi.map(Signal::from_rssi);
        }
    }

    /// Write one packet, then wait for the printer to ack it. Instax acks the
    /// packet, not each fragment, so only the last write is followed by a read.
    async fn command(&mut self, packet: &[u8]) -> Result<Response, PrintError> {
        let began = Instant::now();
        ble_trace!(">> {}", packet_summary(packet));
        // CoreBluetooth may never finish a write after the printer powers off.
        // The deadline must cover writes as well as the reply.
        let acked = transfer_with_deadline(
            async {
                for fragment in fragments(packet) {
                    self.peripheral
                        .write(&self.write, fragment, WriteType::WithoutResponse)
                        .await
                        .map_err(|error| {
                            PrintError::transfer("Bluetooth write", error.to_string())
                        })?;
                }
                await_ack(&mut self.notifications, packet, ACK_TIMEOUT).await
            },
            ACK_TIMEOUT,
        )
        .await
        .map_err(|error| match error {
            PrintError::PrinterRejected { .. } => error,
            _ => PrintError::transfer(
                opcode_name(u16::from_be_bytes([packet[4], packet[5]])),
                error.to_string(),
            ),
        });
        match &acked {
            Ok(response) => ble_trace!(
                "<< {} {}ms",
                response_summary(response),
                began.elapsed().as_millis()
            ),
            Err(error) => ble_trace!("!! {error} after {}ms", began.elapsed().as_millis()),
        }
        acked
    }
}

async fn await_ack(
    notifications: &mut Notifications,
    packet: &[u8],
    limit: Duration,
) -> Result<Response, PrintError> {
    let deadline = Instant::now() + limit;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, notifications.next()).await {
            Ok(Some(notification)) if notification.uuid == NOTIFY_CHAR => {
                match decode(&notification.value) {
                    Ok(response) if matches_reply(&response, packet) => {
                        if response.status != 0 {
                            return Err(PrintError::PrinterRejected {
                                opcode: response.opcode,
                                status: response.status,
                            });
                        }
                        return Ok(response);
                    }
                    Ok(response) => {
                        ble_trace!("ignored unrelated reply: {}", response_summary(&response))
                    }
                    Err(error) => ble_trace!("invalid printer reply: {error}"),
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => return Err(PrintError::NotConnected),
            Err(_) => {
                return Err(PrintError::transfer(
                    "Bluetooth",
                    format!(
                        "no matching printer acknowledgement within {:.1}s",
                        limit.as_secs_f32()
                    ),
                ))
            }
        }
        if Instant::now() >= deadline {
            return Err(PrintError::transfer(
                "Bluetooth",
                "timed out waiting for matching printer acknowledgement",
            ));
        }
    }
}

async fn open_session(want: Option<&str>) -> Result<Session, PrintError> {
    let central = adapter().await?;
    let mut last = PrintError::NoPrinter;
    for attempt in 0..CONNECT_ATTEMPTS {
        if attempt > 0 {
            sleep(CONNECT_RETRY_DELAY).await;
        }
        match pick_printer(&central, want).await {
            Ok(peripheral) => match attach(peripheral).await {
                Ok(session) => return Ok(session),
                Err(error) => last = error,
            },
            Err(error) => last = error,
        }
    }
    Err(last)
}

async fn adapter() -> Result<Adapter, PrintError> {
    ble_trace!("opening Bluetooth adapter");
    Manager::new()
        .await
        .map_err(|error| {
            ble_trace!("Bluetooth manager failed: {error}");
            PrintError::NoBluetooth
        })?
        .adapters()
        .await
        .map_err(|error| {
            ble_trace!("listing Bluetooth adapters failed: {error}");
            PrintError::NoBluetooth
        })?
        .into_iter()
        .next()
        .ok_or(PrintError::NoBluetooth)
}

struct ScanHit {
    peripheral: Peripheral,
    id: String,
    name: String,
    rssi: Option<i16>,
}

async fn pick_printer(central: &Adapter, want: Option<&str>) -> Result<Peripheral, PrintError> {
    let hits = scan_instax(central, want, false).await?;
    let hit = match want {
        Some(id) => hits.into_iter().find(|hit| hit.id == id),
        None => hits.into_iter().next(),
    };
    hit.map(|hit| hit.peripheral).ok_or(PrintError::NoPrinter)
}

async fn scan_instax(
    central: &Adapter,
    want: Option<&str>,
    linger: bool,
) -> Result<Vec<ScanHit>, PrintError> {
    ble_trace!(
        "scan start: target={want:?}, state={:?}",
        central.adapter_state().await
    );
    central
        .start_scan(ScanFilter::default())
        .await
        .map_err(|error| {
            ble_trace!("starting Bluetooth scan failed: {error}");
            PrintError::NoBluetooth
        })?;
    let deadline = Instant::now() + SCAN_WINDOW;
    let mut found = collect_instax(central).await;
    while Instant::now() < deadline {
        if !linger && has_target(&found, want) {
            break;
        }
        sleep(SCAN_POLL).await;
        found = collect_instax(central).await;
    }
    let _ = central.stop_scan().await;
    ble_trace!("scan finished: {} Instax printers", found.len());
    for hit in &found {
        ble_trace!("scan found {} ({}) RSSI={:?}", hit.name, hit.id, hit.rssi);
    }
    if found.is_empty() {
        Err(PrintError::NoPrinter)
    } else {
        Ok(found)
    }
}

fn has_target(found: &[ScanHit], want: Option<&str>) -> bool {
    match want {
        Some(id) => found.iter().any(|hit| hit.id == id),
        None => !found.is_empty(),
    }
}

async fn collect_instax(central: &Adapter) -> Vec<ScanHit> {
    let Ok(peripherals) = central.peripherals().await else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for peripheral in peripherals {
        let Some(props) = peripheral.properties().await.ok().flatten() else {
            continue;
        };
        let Some(name) = props.local_name else {
            continue;
        };
        if !is_instax_name(&name) {
            continue;
        }
        found.push(ScanHit {
            id: peripheral.id().to_string(),
            name,
            rssi: props.rssi,
            peripheral,
        });
    }
    found.sort_by_key(|hit| std::cmp::Reverse(hit.rssi.unwrap_or(i16::MIN)));
    found
}

async fn attach(peripheral: Peripheral) -> Result<Session, PrintError> {
    if !peripheral.is_connected().await.unwrap_or(false) {
        peripheral
            .connect()
            .await
            .map_err(|_| PrintError::LinkFailed)?;
    }
    peripheral
        .discover_services()
        .await
        .map_err(|_| PrintError::LinkFailed)?;

    let write = characteristic(&peripheral, WRITE_CHAR, CharPropFlags::empty())?;
    let notify = characteristic(&peripheral, NOTIFY_CHAR, CharPropFlags::NOTIFY)?;

    // A stale subscription survives an app crash and silently swallows acks.
    let _ = peripheral.unsubscribe(&notify).await;
    peripheral
        .subscribe(&notify)
        .await
        .map_err(|_| PrintError::LinkFailed)?;
    let notifications = peripheral
        .notifications()
        .await
        .map_err(|_| PrintError::LinkFailed)?;

    let id = peripheral.id().to_string();
    let name = peripheral
        .properties()
        .await
        .ok()
        .flatten()
        .and_then(|props| props.local_name)
        .unwrap_or_default();

    let mut session = Session {
        peripheral,
        write,
        notifications,
        model: PrinterModel::Mini,
        max_image_bytes: None,
        id,
        name,
        status: PrinterStatus::default(),
    };
    session.handshake().await?;
    Ok(session)
}

fn characteristic(
    peripheral: &Peripheral,
    uuid: Uuid,
    needs: CharPropFlags,
) -> Result<Characteristic, PrintError> {
    peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == uuid && c.properties.contains(needs))
        .ok_or(PrintError::LinkFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_only_transfers_negotiated_chunks_and_cancels_without_printing() {
        let printer = InstaxBle::new().unwrap();
        let mut packets = Vec::new();
        let jpeg = vec![42; 1000];
        printer
            .runtime
            .block_on(transfer_image(
                PrintJob {
                    jpeg: &jpeg,
                    model: PrinterModel::Square,
                    progress: &mut |_| {},
                    cancel: &std::sync::atomic::AtomicBool::new(false),
                },
                TransferMode::SendOnly,
                async |packet: &[u8]| {
                    packets.push(packet.to_vec());
                    Ok(Response {
                        opcode: u16::from_be_bytes([packet[4], packet[5]]),
                        status: 0,
                        command: 0,
                        payload: vec![0, 3, 0x84],
                    })
                },
            ))
            .unwrap();
        let opcodes: Vec<_> = packets
            .iter()
            .map(|p| u16::from_be_bytes([p[4], p[5]]))
            .collect();
        assert_eq!(opcodes, [0x1000, 0x1001, 0x1001, 0x1002, 0x1003]);
        assert_eq!(&packets[0][10..14], &1000u32.to_be_bytes());
        assert_eq!(packets[1].len(), 911);
        assert_eq!(&packets[2][6..10], &[0, 0, 0, 1]);
        assert_eq!(&packets[2][10..110], &[42; 100]);
        assert!(packets[2][110..910].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn cancellation_after_last_chunk_never_reaches_print() {
        let printer = InstaxBle::new().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut opcodes = Vec::new();
        let result = printer.runtime.block_on(transfer_image(
            PrintJob {
                jpeg: &[42],
                model: PrinterModel::Mini,
                progress: &mut |_| cancel.store(true, Ordering::Relaxed),
                cancel: &cancel,
            },
            TransferMode::Print,
            async |packet: &[u8]| {
                let opcode = u16::from_be_bytes([packet[4], packet[5]]);
                opcodes.push(opcode);
                Ok(Response {
                    opcode,
                    status: 0,
                    command: 0,
                    payload: vec![],
                })
            },
        ));
        assert_eq!(result, Err(PrintError::Cancelled));
        assert!(!opcodes.contains(&0x1080));
    }

    #[test]
    fn failed_transfer_is_cancelled_without_retrying_print() {
        let printer = InstaxBle::new().unwrap();
        for failed_opcode in [0x1000, 0x1001, 0x1002, 0x1080] {
            let mut opcodes = Vec::new();
            let result = printer.runtime.block_on(transfer_image(
                PrintJob {
                    jpeg: &[42],
                    model: PrinterModel::Mini,
                    progress: &mut |_| {},
                    cancel: &std::sync::atomic::AtomicBool::new(false),
                },
                TransferMode::Print,
                async |packet: &[u8]| {
                    let opcode = u16::from_be_bytes([packet[4], packet[5]]);
                    opcodes.push(opcode);
                    if opcode == failed_opcode {
                        return Err(PrintError::Transfer);
                    }
                    Ok(Response {
                        opcode,
                        status: 0,
                        command: 0,
                        payload: vec![],
                    })
                },
            ));
            let expected = if failed_opcode == 0x1001 {
                PrintError::transfer("Packet 1/1", "print failed")
            } else {
                PrintError::Transfer
            };
            assert_eq!(result, Err(expected));
            assert_eq!(opcodes.last(), Some(&0x1003));
            assert_eq!(
                opcodes.iter().filter(|&&op| op == 0x1080).count(),
                usize::from(failed_opcode == 0x1080)
            );
        }
    }

    #[test]
    fn print_occurs_once_after_successful_download() {
        let printer = InstaxBle::new().unwrap();
        let mut opcodes = Vec::new();
        printer
            .runtime
            .block_on(transfer_image(
                PrintJob {
                    jpeg: &[42],
                    model: PrinterModel::Mini,
                    progress: &mut |_| {},
                    cancel: &std::sync::atomic::AtomicBool::new(false),
                },
                TransferMode::Print,
                async |packet: &[u8]| {
                    let opcode = u16::from_be_bytes([packet[4], packet[5]]);
                    opcodes.push(opcode);
                    Ok(Response {
                        opcode,
                        status: 0,
                        command: 0,
                        payload: vec![],
                    })
                },
            ))
            .unwrap();
        assert_eq!(opcodes, [0x1000, 0x1001, 0x1002, 0x1080]);
    }

    #[test]
    fn send_only_accepts_link_plus_cancel_refusal_after_end_without_printing() {
        let printer = InstaxBle::new().unwrap();
        let mut opcodes = Vec::new();
        printer
            .runtime
            .block_on(transfer_image(
                PrintJob {
                    jpeg: &[42],
                    model: PrinterModel::Mini,
                    progress: &mut |_| {},
                    cancel: &std::sync::atomic::AtomicBool::new(false),
                },
                TransferMode::SendOnly,
                async |packet: &[u8]| {
                    let opcode = u16::from_be_bytes([packet[4], packet[5]]);
                    opcodes.push(opcode);
                    if opcode == 0x1003 {
                        return Err(PrintError::PrinterRejected {
                            opcode,
                            status: 0x82,
                        });
                    }
                    Ok(Response {
                        opcode,
                        status: 0,
                        command: 0,
                        payload: vec![],
                    })
                },
            ))
            .unwrap();
        assert_eq!(opcodes, [0x1000, 0x1001, 0x1002, 0x1003]);
    }

    fn reply(opcode: u16, status: u8, command: u8) -> ValueNotification {
        ValueNotification {
            uuid: NOTIFY_CHAR,
            value: crate::packet::encode(opcode, &[status, command]),
        }
    }

    #[test]
    fn image_ack_skips_unsolicited_status_and_wrong_characteristic() {
        let printer = InstaxBle::new().unwrap();
        let opcode = crate::packet::OPCODE_PRINT_IMAGE_DOWNLOAD_DATA;
        let mut wrong_pipe = reply(opcode, 0, 0);
        wrong_pipe.uuid = WRITE_CHAR;
        let mut notifications: Notifications = Box::pin(futures_util::stream::iter([
            reply(
                crate::packet::OPCODE_SUPPORT_FUNCTION_INFO,
                0,
                INFO_TYPE_BATTERY,
            ),
            wrong_pipe,
            reply(opcode, 0, 0),
        ]));
        let response = printer
            .runtime
            .block_on(await_ack(
                &mut notifications,
                &download_data_packet(&[1]),
                ACK_TIMEOUT,
            ))
            .unwrap();
        assert_eq!(response.opcode, opcode);
    }

    #[test]
    fn status_ack_matches_the_requested_info_type() {
        let printer = InstaxBle::new().unwrap();
        let opcode = crate::packet::OPCODE_SUPPORT_FUNCTION_INFO;
        let mut notifications: Notifications = Box::pin(futures_util::stream::iter([
            reply(opcode, 0, INFO_TYPE_BATTERY),
            reply(opcode, 0, INFO_TYPE_PRINTER_FUNCTION),
        ]));
        let response = printer
            .runtime
            .block_on(await_ack(
                &mut notifications,
                &info_packet(INFO_TYPE_PRINTER_FUNCTION),
                ACK_TIMEOUT,
            ))
            .unwrap();
        assert_eq!(response.command, INFO_TYPE_PRINTER_FUNCTION);
    }

    #[test]
    fn rejection_is_not_success_for_any_print_phase() {
        let printer = InstaxBle::new().unwrap();
        for packet in [
            download_start_packet(10),
            download_data_packet(&[1]),
            download_end_packet(),
            print_image_packet(),
        ] {
            let opcode = u16::from_be_bytes([packet[4], packet[5]]);
            let mut notifications: Notifications =
                Box::pin(futures_util::stream::iter([reply(opcode, 3, 0)]));
            let error = printer
                .runtime
                .block_on(await_ack(&mut notifications, &packet, ACK_TIMEOUT))
                .unwrap_err();
            assert_eq!(
                error,
                PrintError::PrinterRejected {
                    opcode,
                    status: 0x03
                }
            );
        }
    }

    #[test]
    fn a_silent_printer_returns_a_diagnostic_timeout() {
        let printer = InstaxBle::new().unwrap();
        let mut notifications: Notifications = Box::pin(futures_util::stream::pending());
        let error = printer
            .runtime
            .block_on(await_ack(
                &mut notifications,
                &download_start_packet(10),
                Duration::from_millis(5),
            ))
            .unwrap_err();
        assert!(matches!(error, PrintError::TransferDetail { .. }));
        assert!(error.to_string().contains("acknowledgement"));
    }

    #[test]
    fn size_hint_is_specific_to_download_start_rejection() {
        let printer = InstaxBle::new().unwrap();
        for packet in [download_start_packet(400_000), print_image_packet()] {
            let opcode = u16::from_be_bytes([packet[4], packet[5]]);
            let mut notifications: Notifications =
                Box::pin(futures_util::stream::iter([reply(opcode, 0x81, 0)]));
            let error = printer
                .runtime
                .block_on(await_ack(&mut notifications, &packet, ACK_TIMEOUT))
                .unwrap_err()
                .to_string();
            assert!(error.contains("status 0x81"));
            assert_eq!(
                error.contains("byte limit"),
                opcode == crate::packet::OPCODE_PRINT_IMAGE_DOWNLOAD_START
            );
        }
    }

    #[test]
    fn a_stalled_bluetooth_write_releases_the_worker() {
        let printer = InstaxBle::new().unwrap();
        let result = printer.runtime.block_on(async {
            timeout(
                Duration::from_millis(100),
                transfer_with_deadline(
                    std::future::pending::<Result<(), PrintError>>(),
                    Duration::from_millis(5),
                ),
            )
            .await
        });
        assert!(matches!(result, Ok(Err(PrintError::TransferDetail { .. }))));
    }

    #[test]
    fn a_fresh_transport_is_not_connected() {
        let printer = InstaxBle::new().unwrap();
        assert_eq!(printer.negotiated_model(), None);
    }

    #[test]
    fn sending_without_a_session_fails_before_any_radio_work() {
        let mut printer = InstaxBle::new().unwrap();
        let error = printer
            .send(PrintJob {
                jpeg: &[1, 2, 3],
                model: PrinterModel::Mini,
                progress: &mut |_| {},
                cancel: &std::sync::atomic::AtomicBool::new(false),
            })
            .unwrap_err();
        assert_eq!(error, PrintError::NotConnected);
    }

    #[test]
    fn disconnecting_an_idle_transport_is_not_an_error() {
        assert_eq!(InstaxBle::new().unwrap().disconnect(), Ok(()));
    }
}
