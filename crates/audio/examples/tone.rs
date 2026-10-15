//! Plays a quiet one-second 440 Hz tone on the default output device, fed in 20 ms frames the
//! same way received voice is. `cargo run -p bardo-audio --example tone`

use std::{
    f32::consts::TAU,
    thread,
    time::{Duration, Instant},
};

use bardo_audio::{FRAME_LEN, Playback, SAMPLE_RATE, output_devices};

fn main() -> Result<(), bardo_audio::Error> {
    for device in output_devices() {
        println!("output device: {}", device.name);
    }

    let (_playback, mut sink) = Playback::start(None)?;
    let mut frame = vec![0.0; FRAME_LEN * 2];
    let mut phase = 0.0f32;
    let start = Instant::now();
    for tick in 0..50u32 {
        for pair in frame.as_chunks_mut::<2>().0 {
            let sample = phase.sin() * 0.05;
            pair.fill(sample);
            phase = (phase + TAU * 440.0 / SAMPLE_RATE as f32) % TAU;
        }
        sink.push(&frame);
        // Pace like the voice driver: one frame every 20 ms.
        let next = start + Duration::from_millis(20 * u64::from(tick + 1));
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    thread::sleep(Duration::from_millis(200));
    println!("played {} ms", start.elapsed().as_millis());
    Ok(())
}
