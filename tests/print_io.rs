//! Integration · print I/O thread.
//! Reason: print leaves the UI thread and talks to a printer (mock here; a
//! linked Instax is the hardware path).

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use instax_ble::{
    print_clean_frame, spawn_print_io, Frame, PrintError, PrintIoCmd, PrintIoReply, PrintJob,
    Printer, PrinterModel, PRINT_SHUTDOWN_TIMEOUT,
};

struct RecordingPrinter {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Printer for RecordingPrinter {
    fn send(&mut self, job: PrintJob<'_>) -> Result<(), PrintError> {
        (job.progress)(0.5);
        (job.progress)(1.0);
        self.sent.lock().expect("lock").push(job.jpeg.to_vec());
        Ok(())
    }
}

/// What the print thread asked the printer to do, in order. `Dropped` is the
/// last entry a thread that ran to the end can leave, so seeing it proves the
/// thread is gone and not merely told to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    TransferStarted,
    Cancelled,
    Disconnected,
    Dropped,
}

type Log = Arc<Mutex<Vec<Step>>>;

fn record(log: &Log, step: Step) {
    log.lock().expect("lock").push(step);
}

fn steps(log: &Log) -> Vec<Step> {
    log.lock().expect("lock").clone()
}

/// A printer whose transfer runs until the host quits, the way a real one
/// does part way through an image.
struct TransferringPrinter {
    log: Log,
    started: mpsc::Sender<()>,
}

impl Printer for TransferringPrinter {
    fn send(&mut self, job: PrintJob<'_>) -> Result<(), PrintError> {
        record(&self.log, Step::TransferStarted);
        let _ = self.started.send(());
        while !job.cancel.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(5));
        }
        Err(PrintError::Cancelled)
    }

    fn cancel_transfer(&mut self) -> Result<(), PrintError> {
        record(&self.log, Step::Cancelled);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), PrintError> {
        record(&self.log, Step::Disconnected);
        Ok(())
    }
}

impl Drop for TransferringPrinter {
    fn drop(&mut self) {
        record(&self.log, Step::Dropped);
    }
}

/// A printer that answers nothing: out of range, or wedged mid-write.
struct WedgedPrinter;

impl Printer for WedgedPrinter {
    fn send(&mut self, _job: PrintJob<'_>) -> Result<(), PrintError> {
        thread::sleep(Duration::from_secs(30));
        Err(PrintError::Transfer)
    }
}

fn frame(size: u32) -> Frame {
    Frame {
        width: size,
        height: size,
        rgba: vec![200; (size * size * 4) as usize],
    }
}

#[test]
fn print_io_thread_sends_jpeg_and_keeps_the_caller_frame() {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let io = spawn_print_io(RecordingPrinter {
        sent: Arc::clone(&sent),
    });
    let frame = frame(8);
    let (reply_tx, reply_rx) = mpsc::channel();
    io.send(PrintIoCmd::Print {
        frame: frame.clone(),
        model: PrinterModel::Mini,
        exposure_percent: 0,
        reply: reply_tx,
    });

    let mut replies = Vec::new();
    loop {
        let reply = reply_rx.recv().unwrap();
        let done = reply == PrintIoReply::Printed;
        replies.push(reply);
        if done {
            break;
        }
    }
    assert_eq!(
        replies,
        vec![
            PrintIoReply::Progress(0.5),
            PrintIoReply::Progress(1.0),
            PrintIoReply::Printed
        ]
    );

    assert_eq!(frame.rgba.len(), 8 * 8 * 4);
    let jpegs = sent.lock().expect("lock");
    assert_eq!(jpegs.len(), 1);
    assert!(jpegs[0].starts_with(&[0xFF, 0xD8]));
}

#[test]
fn mock_print_failure_leaves_clean_frame() {
    struct Fail;
    impl Printer for Fail {
        fn send(&mut self, _job: PrintJob<'_>) -> Result<(), PrintError> {
            Err(PrintError::Transfer)
        }
    }
    let frame = Frame {
        width: 4,
        height: 4,
        rgba: vec![1; 64],
    };
    let err = print_clean_frame(
        frame.as_ref(),
        PrinterModel::Square,
        0,
        &mut Fail,
        &mut |_| {},
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(err, PrintError::Transfer);
    assert_eq!(frame.width, 4);
}

#[test]
fn quitting_during_a_transfer_cancels_the_download_before_the_link_closes() {
    let log: Log = Arc::default();
    let (started_tx, started_rx) = mpsc::channel();
    let mut io = spawn_print_io(TransferringPrinter {
        log: Arc::clone(&log),
        started: started_tx,
    });
    let (reply_tx, _reply_rx) = mpsc::channel();
    io.send(PrintIoCmd::Print {
        frame: frame(8),
        model: PrinterModel::Mini,
        exposure_percent: 0,
        reply: reply_tx,
    });
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("transfer started");

    assert!(io.shutdown());
    assert_eq!(
        steps(&log),
        vec![
            Step::TransferStarted,
            Step::Cancelled,
            Step::Disconnected,
            Step::Dropped
        ]
    );
}

#[test]
fn quitting_with_no_transfer_in_flight_sends_no_cancel() {
    let log: Log = Arc::default();
    let (started_tx, _started_rx) = mpsc::channel();
    let mut io = spawn_print_io(TransferringPrinter {
        log: Arc::clone(&log),
        started: started_tx,
    });

    assert!(io.shutdown());
    assert_eq!(steps(&log), vec![Step::Disconnected, Step::Dropped]);
}

#[test]
fn shutdown_joins_the_print_thread_rather_than_only_signalling_it() {
    let log: Log = Arc::default();
    let (started_tx, _started_rx) = mpsc::channel();
    let mut io = spawn_print_io(TransferringPrinter {
        log: Arc::clone(&log),
        started: started_tx,
    });

    io.shutdown();
    // The printer belongs to the print thread, so its `Drop` can only be on
    // the log once that thread has run to the end.
    assert!(steps(&log).contains(&Step::Dropped));
}

#[test]
fn dropping_the_handle_closes_the_link_as_well_as_the_quit_hook_does() {
    let log: Log = Arc::default();
    let (started_tx, _started_rx) = mpsc::channel();
    drop(spawn_print_io(TransferringPrinter {
        log: Arc::clone(&log),
        started: started_tx,
    }));

    assert_eq!(steps(&log), vec![Step::Disconnected, Step::Dropped]);
}

#[test]
fn a_printer_that_never_answers_does_not_hold_the_quit() {
    let mut io = spawn_print_io(WedgedPrinter);
    let (reply_tx, _reply_rx) = mpsc::channel();
    io.send(PrintIoCmd::Print {
        frame: frame(8),
        model: PrinterModel::Mini,
        exposure_percent: 0,
        reply: reply_tx,
    });

    let start = Instant::now();
    let joined = io.shutdown();
    let waited = start.elapsed();

    assert!(!joined, "a wedged printer cannot be joined");
    assert!(
        waited < PRINT_SHUTDOWN_TIMEOUT * 2,
        "shutdown waited {waited:?}"
    );
}
