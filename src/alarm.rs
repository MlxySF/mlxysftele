//! Repeated unread alert. Audio runs independently of egui's repaint cycle, so
//! minimizing the window does not stop it. No audio device is opened at idle.
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use rodio::{OutputStreamBuilder, Sink, Source};

const REPEAT: Duration = Duration::from_secs(5);

pub struct Alarm {
    tx: Sender<bool>,
    active: bool,
}

impl Alarm {
    pub fn new(report_error: impl Fn(String) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("unread-alarm".into())
            .spawn(move || run(rx, report_error))
            .expect("failed to start alarm thread");
        Self { tx, active: false }
    }

    pub fn set_active(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            let _ = self.tx.send(active);
        }
    }
}

fn run(rx: Receiver<bool>, report_error: impl Fn(String)) {
    let mut reported = false;
    while let Ok(active) = rx.recv() {
        if !active { continue; }
        'active: loop {
            let mut stream = match OutputStreamBuilder::open_default_stream() {
                Ok(stream) => { reported = false; Some(stream) }
                Err(e) => {
                    if !reported {
                        report_error(format!("Unread alarm: cannot open audio output: {e}"));
                        reported = true;
                    }
                    None
                }
            };
            if let Some(ref mut stream) = stream { stream.log_on_drop(false); }
            let sink = stream.as_ref().map(|stream| Sink::connect_new(stream.mixer()));
            loop {
                if let Some(ref sink) = sink {
                    if sink.empty() {
                        sink.append(rodio::source::SineWave::new(880.0)
                            .take_duration(Duration::from_millis(300)).amplify(0.18));
                    }
                }
                match rx.recv_timeout(REPEAT) {
                    Ok(false) | Err(RecvTimeoutError::Disconnected) => {
                        if let Some(sink) = sink { sink.stop(); }
                        break 'active;
                    }
                    Ok(true) => {}
                    Err(RecvTimeoutError::Timeout) if stream.is_none() => break, // retry device
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
            // No audio device: retry after REPEAT, while still listening for stop.
        }
    }
}
