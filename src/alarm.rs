//! Repeated unread alert. Audio runs independently of egui's repaint cycle, so
//! minimizing the window does not stop it. No audio device is opened at idle.
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use rodio::{OutputStream, OutputStreamBuilder, Sink, Source};

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
            .spawn(move || run(rx, REPEAT, AudioOutput::open, report_error))
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

trait Beeper {
    fn beep(&mut self);
    fn stop(&mut self);
}

struct AudioOutput {
    sink: Sink,
    // The stream must stay alive until the sink has stopped.
    _stream: OutputStream,
}
impl AudioOutput {
    fn open() -> Result<Self, String> {
        let mut stream = OutputStreamBuilder::open_default_stream().map_err(|e| e.to_string())?;
        stream.log_on_drop(false);
        let sink = Sink::connect_new(stream.mixer());
        Ok(Self { sink, _stream: stream })
    }
}
impl Beeper for AudioOutput {
    fn beep(&mut self) {
        if self.sink.empty() {
            self.sink.append(rodio::source::SineWave::new(880.0)
                .take_duration(Duration::from_millis(300)).amplify(0.18));
        }
    }
    fn stop(&mut self) { self.sink.stop(); }
}

fn run<B: Beeper>(
    rx: Receiver<bool>,
    interval: Duration,
    mut open: impl FnMut() -> Result<B, String>,
    report_error: impl Fn(String),
) {
    let mut active = false;
    let mut reported = false;
    loop {
        if !active {
            match rx.recv() {
                Ok(true) => active = true,
                Ok(false) => continue,
                Err(_) => return,
            }
        }
        match open() {
            Ok(mut output) => {
                reported = false;
                while active {
                    output.beep();
                    match rx.recv_timeout(interval) {
                        Ok(false) => active = false,
                        Ok(true) | Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => { output.stop(); return; }
                    }
                }
                output.stop();
            }
            Err(error) => {
                if !reported {
                    report_error(format!("Unread alarm: cannot open audio output: {error}"));
                    reported = true;
                }
                match rx.recv_timeout(interval) {
                    Ok(false) => active = false,
                    Ok(true) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake(Sender<&'static str>);
    impl Beeper for Fake {
        fn beep(&mut self) { self.0.send("beep").unwrap(); }
        fn stop(&mut self) { self.0.send("stop").unwrap(); }
    }
    #[test]
    fn repeats_until_stopped_and_starts_again() {
        let (tx, rx) = mpsc::channel();
        let (events, observed) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            run(rx, Duration::from_millis(20), || Ok::<_, String>(Fake(events.clone())), |_| {});
        });
        tx.send(true).unwrap();
        assert_eq!(observed.recv_timeout(Duration::from_secs(1)).unwrap(), "beep");
        assert_eq!(observed.recv_timeout(Duration::from_secs(1)).unwrap(), "beep");
        tx.send(false).unwrap();
        // A beep already queued before the stop signal is harmless.
        while observed.recv_timeout(Duration::from_secs(1)).unwrap() != "stop" {}
        assert!(observed.recv_timeout(Duration::from_millis(80)).is_err());
        tx.send(true).unwrap();
        assert_eq!(observed.recv_timeout(Duration::from_secs(1)).unwrap(), "beep");
        tx.send(false).unwrap();
        assert_eq!(observed.recv_timeout(Duration::from_secs(1)).unwrap(), "stop");
        drop(tx);
        handle.join().unwrap();
    }
}
