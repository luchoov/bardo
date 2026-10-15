//! Captures the default microphone for three seconds and prints its level and whether the
//! voice activity gate would transmit. Nothing is recorded.
//! `cargo run -p bardo-audio --example mic [-- --raw]`; `--raw` turns noise suppression off.

use std::{sync::Arc, thread, time::Duration};

use bardo_audio::{Capture, TransmitControl, Transmitter, input_devices};

fn main() -> Result<(), bardo_audio::Error> {
    for device in input_devices() {
        println!("input device: {}", device.name);
    }

    let control = Arc::new(TransmitControl::default());
    control.set_noise_suppression(!std::env::args().any(|arg| arg == "--raw"));
    let mut transmitter = Transmitter::new(control.clone());
    let mut frames = 0u32;
    let _capture = Capture::start(None, move |frame| {
        transmitter.process(frame);
        frames += 1;
        if frames.is_multiple_of(10) {
            println!("{:>5} ms", frames * 20);
        }
    })?;

    for _ in 0..15 {
        thread::sleep(Duration::from_millis(200));
        println!(
            "level {:>6.1} dBFS, transmitting: {}",
            control.level_db(),
            control.transmitting()
        );
    }
    Ok(())
}
