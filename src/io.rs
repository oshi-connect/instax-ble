//! Print I/O thread. A UI thread hands commands to this thread and never
//! blocks on Bluetooth itself. It drives any [`Printer`], real or mock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::print::{
    print_clean_frame, DiscoveredPrinter, Frame, PrintError, PrintJob, Printer, PrinterModel,
    PrinterStatus,
};

#[derive(Debug, Clone, PartialEq)]
pub enum PrintIoReply {
    /// Sent repeatedly while the image goes over the air, then once at 1.0.
    Progress(f32),
    Printed,
    Connected {
        id: String,
        name: String,
        status: PrinterStatus,
    },
    Disconnected,
    Found(Vec<DiscoveredPrinter>),
    Status(PrinterStatus),
    Failed(PrintError),
}

#[derive(Debug)]
pub enum PrintIoCmd {
    Print {
        frame: Frame,
        model: PrinterModel,
        exposure_percent: u8,
        reply: Sender<PrintIoReply>,
    },
    Scan {
        reply: Sender<PrintIoReply>,
    },
    Connect {
        id: Option<String>,
        reply: Sender<PrintIoReply>,
    },
    Disconnect {
        reply: Sender<PrintIoReply>,
    },
    RefreshStatus {
        reply: Sender<PrintIoReply>,
    },
}

/// How long a quit gives the printer to take the cancel and close the link.
///
/// Quitting uses a shorter budget than a normal Bluetooth command. The
/// wait stops here and the process goes. The budget covers the two small
/// writes the teardown makes (cancel, then disconnect) with room for one
/// retry, and stays inside the time an operator reads as an immediate quit.
pub const PRINT_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Real Bluetooth when the feature is on. A printer that always reports
/// "not connected" otherwise, so the rest of the host still runs.
pub fn spawn_default_print_io() -> PrintIo {
    #[cfg(feature = "ble")]
    if let Ok(printer) = crate::transport::InstaxBle::new() {
        return spawn_print_io(printer);
    }
    spawn_print_io(DisconnectedPrinter)
}

/// The print thread, from the UI side.
///
/// `send` never waits: it hands the command to the thread. `shutdown` is the
/// one call that waits. A quit must use it, because dropping the sender only
/// ends the loop, and a process that exits in the same moment takes the
/// thread with it before the printer hears anything. The operating system
/// then drops the link, which can leave the printer holding a download it
/// will not give up until it is power-cycled.
///
/// `Drop` shuts down as well. That covers a handle dropped while the host
/// runs, not a quit: no quit path drops the handle, so the hook is what closes
/// the link on all three of them.
#[derive(Debug)]
pub struct PrintIo {
    tx: Option<Sender<PrintIoCmd>>,
    thread: Option<thread::JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
}

impl PrintIo {
    pub fn send(&self, cmd: PrintIoCmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// Ends the print thread and returns once the link is closed, so the
    /// caller can let the process go. A transfer in flight stops at the next
    /// packet boundary and the printer gets a cancel before the disconnect.
    ///
    /// Returns `false` when `PRINT_SHUTDOWN_TIMEOUT` ran out and the thread
    /// was left behind for process exit to reclaim. A second call does
    /// nothing and returns `true`.
    pub fn shutdown(&mut self) -> bool {
        self.cancel.store(true, Ordering::Relaxed);
        drop(self.tx.take());
        match self.thread.take() {
            Some(thread) => join_within(thread, PRINT_SHUTDOWN_TIMEOUT),
            None => true,
        }
    }
}

impl Drop for PrintIo {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `JoinHandle::join` has no deadline, so a watcher thread does the join and
/// reports it over a channel the caller can wait on with one. A join that
/// runs out of time leaves both threads to the process exit.
fn join_within(thread: thread::JoinHandle<()>, limit: Duration) -> bool {
    let (done_tx, done_rx) = mpsc::channel();
    let watcher = thread::Builder::new()
        .name("instax-print-join".into())
        .spawn(move || {
            let _ = thread.join();
            let _ = done_tx.send(());
        });
    watcher.is_ok() && done_rx.recv_timeout(limit).is_ok()
}

/// I/O thread. UI never waits on BLE.
pub fn spawn_print_io(mut printer: impl Printer + Send + 'static) -> PrintIo {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let thread = thread::Builder::new()
        .name("instax-print".into())
        .spawn(move || {
            if print_loop(rx, &mut printer, &thread_cancel) {
                let _ = printer.cancel_transfer();
            }
            let _ = printer.disconnect();
        })
        .ok();
    PrintIo {
        // A thread the platform refused leaves nothing to send to. The host
        // then behaves as it does with no printer paired.
        tx: thread.is_some().then_some(tx),
        thread,
        cancel,
    }
}

/// Runs until the sender goes. Reports whether it left a download open on the
/// printer, which only a shutdown during a transfer can do.
fn print_loop(rx: Receiver<PrintIoCmd>, printer: &mut impl Printer, cancel: &AtomicBool) -> bool {
    let mut interrupted = false;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            PrintIoCmd::Print {
                frame,
                model,
                exposure_percent,
                reply,
            } => {
                let progress_reply = reply.clone();
                let mut progress = move |fraction: f32| {
                    let _ = progress_reply.send(PrintIoReply::Progress(fraction));
                };
                let result = print_clean_frame(
                    frame.as_ref(),
                    model,
                    exposure_percent,
                    printer,
                    &mut progress,
                    cancel,
                );
                interrupted = result == Err(PrintError::Cancelled);
                let _ = reply.send(match result {
                    Ok(()) => PrintIoReply::Printed,
                    Err(error) => PrintIoReply::Failed(error),
                });
            }
            PrintIoCmd::Scan { reply } => {
                let _ = reply.send(match printer.scan() {
                    Ok(found) => PrintIoReply::Found(found),
                    Err(PrintError::NoPrinter) => PrintIoReply::Found(Vec::new()),
                    Err(error) => PrintIoReply::Failed(error),
                });
            }
            PrintIoCmd::Connect { id, reply } => {
                let result = match id.as_deref() {
                    Some(id) => printer.connect_to(id),
                    None => printer.connect(),
                };
                let _ = reply.send(match result {
                    Ok(()) => {
                        let (id, name) = printer.identity().unwrap_or_default();
                        PrintIoReply::Connected {
                            id,
                            name,
                            status: printer.status().unwrap_or_default(),
                        }
                    }
                    Err(error) => PrintIoReply::Failed(error),
                });
            }
            PrintIoCmd::Disconnect { reply } => {
                let _ = reply.send(match printer.disconnect() {
                    Ok(()) => PrintIoReply::Disconnected,
                    Err(error) => PrintIoReply::Failed(error),
                });
            }
            PrintIoCmd::RefreshStatus { reply } => {
                let _ = reply.send(match printer.refresh_status() {
                    Ok(status) => PrintIoReply::Status(status),
                    Err(error) => {
                        let _ = printer.disconnect();
                        PrintIoReply::Failed(error)
                    }
                });
            }
        }
    }
    interrupted
}

#[derive(Default)]
pub struct DisconnectedPrinter;

impl Printer for DisconnectedPrinter {
    fn send(&mut self, _job: PrintJob<'_>) -> Result<(), PrintError> {
        Err(PrintError::NotConnected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_status_poll_releases_the_session_before_reconnecting() {
        struct SleepingPrinter {
            linked: bool,
        }
        impl Printer for SleepingPrinter {
            fn send(&mut self, _: PrintJob<'_>) -> Result<(), PrintError> {
                Err(PrintError::NotConnected)
            }
            fn refresh_status(&mut self) -> Result<PrinterStatus, PrintError> {
                Err(PrintError::Transfer)
            }
            fn disconnect(&mut self) -> Result<(), PrintError> {
                self.linked = false;
                Ok(())
            }
            fn connect(&mut self) -> Result<(), PrintError> {
                if self.linked {
                    return Err(PrintError::LinkFailed);
                }
                self.linked = true;
                Ok(())
            }
        }
        let io = spawn_print_io(SleepingPrinter { linked: true });
        let (reply, rx) = mpsc::channel();
        io.send(PrintIoCmd::RefreshStatus { reply });
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            PrintIoReply::Failed(PrintError::Transfer)
        ));
        let (reply, rx) = mpsc::channel();
        io.send(PrintIoCmd::Connect { id: None, reply });
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            PrintIoReply::Connected { .. }
        ));
    }

    #[test]
    fn transfer_progress_arrives_before_the_print_finishes() {
        struct PacketPrinter {
            finish: Receiver<()>,
        }
        impl Printer for PacketPrinter {
            fn send(&mut self, job: PrintJob<'_>) -> Result<(), PrintError> {
                (job.progress)(0.25);
                (job.progress)(0.75);
                self.finish.recv_timeout(Duration::from_secs(2)).unwrap();
                (job.progress)(1.0);
                Ok(())
            }
        }
        let (finish, wait) = mpsc::channel();
        let io = spawn_print_io(PacketPrinter { finish: wait });
        let (reply, rx) = mpsc::channel();
        io.send(PrintIoCmd::Print {
            frame: Frame {
                width: 1,
                height: 1,
                rgba: vec![255; 4],
            },
            model: PrinterModel::Mini,
            exposure_percent: crate::print::EXPOSURE_DEFAULT,
            reply,
        });
        for fraction in [0.25, 0.75] {
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                PrintIoReply::Progress(fraction)
            );
        }
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        finish.send(()).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            PrintIoReply::Progress(1.0)
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            PrintIoReply::Printed
        );
    }

    #[test]
    fn print_io_reports_not_connected_and_keeps_frame() {
        let io = spawn_print_io(DisconnectedPrinter);
        let frame = Frame {
            width: 2,
            height: 2,
            rgba: vec![255; 16],
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        io.send(PrintIoCmd::Print {
            frame: frame.clone(),
            model: PrinterModel::Mini,
            exposure_percent: 0,
            reply: reply_tx,
        });
        assert_eq!(
            reply_rx.recv().unwrap(),
            PrintIoReply::Failed(PrintError::NotConnected)
        );
        assert_eq!(frame.rgba.len(), 16);
    }

    struct StatusPrinter {
        found: Vec<DiscoveredPrinter>,
        status: PrinterStatus,
        connected_to: Option<String>,
    }

    impl Printer for StatusPrinter {
        fn send(&mut self, _job: PrintJob<'_>) -> Result<(), PrintError> {
            Err(PrintError::NotConnected)
        }

        fn scan(&mut self) -> Result<Vec<DiscoveredPrinter>, PrintError> {
            Ok(self.found.clone())
        }

        fn connect(&mut self) -> Result<(), PrintError> {
            self.connected_to = self.found.first().map(|p| p.id.clone());
            Ok(())
        }

        fn connect_to(&mut self, id: &str) -> Result<(), PrintError> {
            if self.found.iter().any(|p| p.id == id) {
                self.connected_to = Some(id.to_string());
                Ok(())
            } else {
                Err(PrintError::NoPrinter)
            }
        }

        fn refresh_status(&mut self) -> Result<PrinterStatus, PrintError> {
            Ok(self.status.clone())
        }

        fn status(&self) -> Option<PrinterStatus> {
            self.connected_to.as_ref().map(|_| self.status.clone())
        }

        fn identity(&self) -> Option<(String, String)> {
            let id = self.connected_to.as_ref()?;
            let name = self
                .found
                .iter()
                .find(|p| &p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            Some((id.clone(), name))
        }
    }

    fn status_printer() -> StatusPrinter {
        use crate::print::{Battery, Signal};
        StatusPrinter {
            found: vec![DiscoveredPrinter {
                id: "p1".into(),
                name: "INSTAX-1".into(),
                signal: Some(Signal::Strong),
            }],
            status: PrinterStatus {
                model: Some(PrinterModel::Mini),
                photos_left: Some(8),
                battery: Some(Battery::Ok),
                charging: false,
                signal: Some(Signal::Strong),
            },
            connected_to: None,
        }
    }

    #[test]
    fn print_io_scan_returns_the_list_the_printer_found() {
        use crate::print::Signal;
        let io = spawn_print_io(status_printer());
        let (reply_tx, reply_rx) = mpsc::channel();
        io.send(PrintIoCmd::Scan { reply: reply_tx });
        match reply_rx.recv().unwrap() {
            PrintIoReply::Found(found) => {
                assert_eq!(found.len(), 1);
                assert_eq!(found[0].id, "p1");
                assert_eq!(found[0].signal, Some(Signal::Strong));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn print_io_connect_to_a_scanned_id_returns_status() {
        let io = spawn_print_io(status_printer());
        let (reply_tx, reply_rx) = mpsc::channel();
        io.send(PrintIoCmd::Connect {
            id: Some("p1".into()),
            reply: reply_tx,
        });
        match reply_rx.recv().unwrap() {
            PrintIoReply::Connected { id, name, status } => {
                assert_eq!(id, "p1");
                assert_eq!(name, "INSTAX-1");
                assert_eq!(status.photos_left, Some(8));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn print_io_refresh_returns_remaining_shots() {
        let mut printer = status_printer();
        printer.connected_to = Some("p1".into());
        printer.status.photos_left = Some(3);
        let io = spawn_print_io(printer);
        let (reply_tx, reply_rx) = mpsc::channel();
        io.send(PrintIoCmd::RefreshStatus { reply: reply_tx });
        match reply_rx.recv().unwrap() {
            PrintIoReply::Status(status) => assert_eq!(status.photos_left, Some(3)),
            other => panic!("{other:?}"),
        }
    }
}
