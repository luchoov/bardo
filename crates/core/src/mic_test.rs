//! The microphone test in the settings, outside voice: the level meter for setting the voice
//! activity threshold, hearing yourself live, and recording a few seconds to hear them back the way
//! the others do, after Opus.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use bardo_audio::{Capture, FRAME_LEN, Playback, STEREO_FRAME_LEN, TransmitControl, Transmitter, VoiceCodec};
use rtrb::RingBuffer;
use tracing::warn;

/// How much "record and play back" records: 5 s.
const RECORD_FRAMES: u32 = 250;
/// How often the recorder looks for new frames.
const RECORDER_POLL: Duration = Duration::from_millis(10);
/// Left for the device to play out the end of the recording.
const DRAIN: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MicTestKind {
    /// Only the level meter.
    #[default]
    Meter,
    /// The processed microphone, live on the output.
    Listen,
    /// Record a few seconds of the processed microphone, then play them back through Opus.
    Record,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicTestPhase {
    Metering,
    Listening,
    Recording,
    Playing,
    /// The recording was played back, or could not be.
    Finished,
}

impl MicTestPhase {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Metering,
            1 => Self::Listening,
            2 => Self::Recording,
            3 => Self::Playing,
            _ => Self::Finished,
        }
    }
}

/// Shared between the test, its capture thread and the recorder.
#[derive(Default)]
struct State {
    phase: AtomicU8,
    /// Frames recorded or played back so far.
    frames: AtomicU32,
    /// A device went away, or the default one changed.
    lost: AtomicBool,
    stop: AtomicBool,
}

impl State {
    fn set_phase(&self, phase: MicTestPhase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }
}

/// Captures from the microphone through the transmit pipeline, updating
/// [`TransmitControl::level_db`], until dropped. Nothing is sent anywhere.
pub struct MicTest {
    _capture: Capture,
    _playback: Option<Playback>,
    state: Arc<State>,
}

impl MicTest {
    /// Starts a test of `kind` on the input with `input` (or the default one); listening and
    /// playing back use the output with `output`.
    pub fn start(
        kind: MicTestKind,
        input: Option<&str>,
        output: Option<&str>,
        control: Arc<TransmitControl>,
    ) -> Result<Self, String> {
        let state = Arc::new(State::default());
        let mut transmitter = Transmitter::new(control);
        let lost = {
            let state = state.clone();
            move || state.lost.store(true, Ordering::Relaxed)
        };
        let (capture, playback) = match kind {
            MicTestKind::Meter => {
                let on_frame = move |frame: &mut [f32]| {
                    transmitter.process(frame);
                };
                (Capture::start(input, on_frame, lost).map_err(|e| e.to_string())?, None)
            }
            MicTestKind::Listen => {
                let (playback, mut sink) = Playback::start(output, lost.clone()).map_err(|e| e.to_string())?;
                let mut stereo = vec![0.0; STEREO_FRAME_LEN];
                let on_frame = move |frame: &mut [f32]| {
                    // What the others would hear: nothing while the gate is closed.
                    let open = transmitter.process(frame);
                    for (pair, &sample) in stereo.as_chunks_mut::<2>().0.iter_mut().zip(frame.iter()) {
                        pair.fill(if open { sample } else { 0.0 });
                    }
                    sink.push(&stereo);
                };
                state.set_phase(MicTestPhase::Listening);
                (Capture::start(input, on_frame, lost).map_err(|e| e.to_string())?, Some(playback))
            }
            MicTestKind::Record => {
                let codec = VoiceCodec::new()?;
                let (mut producer, consumer) = RingBuffer::new(FRAME_LEN * (RECORD_FRAMES as usize + 1));
                let recording = state.clone();
                let mut recorded = 0;
                let on_frame = move |frame: &mut [f32]| {
                    let open = transmitter.process(frame);
                    if recorded >= RECORD_FRAMES {
                        return;
                    }
                    if !open {
                        frame.fill(0.0);
                    }
                    if producer.push_entire_slice(frame).is_ok() {
                        recorded += 1;
                        recording.frames.store(recorded, Ordering::Relaxed);
                    }
                };
                state.set_phase(MicTestPhase::Recording);
                let capture = Capture::start(input, on_frame, lost).map_err(|e| e.to_string())?;
                spawn_recorder(consumer, codec, output.map(str::to_owned), state.clone());
                (capture, None)
            }
        };
        Ok(Self { _capture: capture, _playback: playback, state })
    }

    pub fn phase(&self) -> MicTestPhase {
        MicTestPhase::from_u8(self.state.phase.load(Ordering::Relaxed))
    }

    /// How far recording or playing back has got, from 0 to 1.
    pub fn progress(&self) -> f32 {
        self.state.frames.load(Ordering::Relaxed) as f32 / RECORD_FRAMES as f32
    }

    /// A device went away, or the default one changed: start a new test to keep going.
    pub fn is_lost(&self) -> bool {
        self.state.lost.load(Ordering::Relaxed)
    }
}

impl Drop for MicTest {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
    }
}

/// Encodes and decodes the recorded frames as they come, then plays them back.
fn spawn_recorder(
    mut consumer: rtrb::Consumer<f32>,
    mut codec: VoiceCodec,
    output: Option<String>,
    state: Arc<State>,
) {
    let recorder = state.clone();
    let spawned = thread::Builder::new().name("bardo-mic-record".into()).spawn(move || {
        let state = recorder;
        let mut decoded = vec![0.0; STEREO_FRAME_LEN * RECORD_FRAMES as usize];
        let mut frames = 0;
        while frames < RECORD_FRAMES as usize {
            if state.stop.load(Ordering::Relaxed) {
                return;
            }
            match consumer.read_chunk(FRAME_LEN) {
                Ok(chunk) => {
                    let (first, second) = chunk.as_slices();
                    let frame: Vec<f32> = first.iter().chain(second).copied().collect();
                    chunk.commit_all();
                    let out = &mut decoded[frames * STEREO_FRAME_LEN..(frames + 1) * STEREO_FRAME_LEN];
                    if let Err(e) = codec.round_trip(&frame, out) {
                        warn!("the microphone test could not encode the recording: {e}");
                        return state.set_phase(MicTestPhase::Finished);
                    }
                    frames += 1;
                }
                Err(_) => thread::sleep(RECORDER_POLL),
            }
        }

        state.frames.store(0, Ordering::Relaxed);
        state.set_phase(MicTestPhase::Playing);
        let lost = {
            let state = state.clone();
            move || state.lost.store(true, Ordering::Relaxed)
        };
        match Playback::start(output.as_deref(), lost) {
            Ok((_playback, mut sink)) => {
                let finished = bardo_audio::play_paced(&mut sink, &decoded, Instant::now(), || {
                    state.frames.fetch_add(1, Ordering::Relaxed);
                    !state.stop.load(Ordering::Relaxed)
                });
                if finished {
                    thread::sleep(DRAIN);
                }
            }
            Err(e) => warn!("the microphone test could not open the output: {e}"),
        }
        state.set_phase(MicTestPhase::Finished);
    });
    if let Err(e) = spawned {
        warn!("failed to start the microphone recorder: {e}");
        state.set_phase(MicTestPhase::Finished);
    }
}

