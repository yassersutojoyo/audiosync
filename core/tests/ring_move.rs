//! A move of the network speakers' timeline announced on the ring (S281):
//! each renderer fades out on the move's frame, mid-callback, and plays on
//! at its hold-back plus the move, as a delay change does (S187).
//!
//! The ring holds 0.5 on the left and the frame's own index on the right,
//! so a played frame tells which ring frame it was whatever its gain.

use std::sync::Arc;

use audiosync_core::reader::Reader;
use audiosync_core::render::{Renderer, RendererControl};
use audiosync_core::ring::{Ring, RingMove, RingWriter};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
const CALLBACK: usize = 512;
const HOLD_BACK: i64 = 4_800;
const NAMES: u64 = 65_536;

struct Output {
    writer: RingWriter,
    control: RendererControl,
    renderer: Renderer,
    /// Every frame played: its left sample and the ring frame it was.
    played: Vec<(f32, Option<u64>)>,
}

fn output() -> Output {
    let ring = Ring::for_rate(RATE).unwrap();
    let writer = ring.writer().unwrap();
    let reader = Reader::new(ring, RATE);
    reader.control().set_delay_frames(HOLD_BACK);
    let control = RendererControl::new(&reader);
    let renderer = Renderer::new(
        reader,
        &control,
        Arc::new(SharedGain::new(1.0)),
        RATE,
        RATE,
        2,
    )
    .unwrap();
    Output {
        writer,
        control,
        renderer,
        played: Vec::new(),
    }
}

impl Output {
    /// The capture writes a callback's worth, then the device asks for one.
    fn tick(&mut self) {
        let first = self.writer.ring().frames_written();
        let block: Vec<f32> = (first..first + CALLBACK as u64)
            .flat_map(|frame| [0.5, 0.5 * (frame % NAMES) as f32 / NAMES as f32])
            .collect();
        self.writer.write(&block, CALLBACK);
        let mut out = vec![0.0f32; CALLBACK * 2];
        self.renderer.render(&mut out);
        self.played.extend(out.chunks(2).map(|frame| {
            let named = (frame[0] > 0.0)
                .then(|| (f64::from(frame[1] / frame[0]) * NAMES as f64).round() as u64);
            (frame[0], named)
        }));
    }

    /// The ring frame the last frame played was, while it plays.
    fn reading(&self) -> u64 {
        self.played.last().and_then(|(_, named)| *named).unwrap()
    }

    /// The first frame played from `from` on that is below full, and the
    /// ring frame it was.
    fn first_fade(&self, from: usize) -> (usize, u64) {
        let at = from
            + self.played[from..]
                .iter()
                .position(|(left, _)| *left < 0.5)
                .expect("a fade-out");
        (at, self.played[at].1.unwrap())
    }
}

// covers: S281, S187
#[test]
fn a_move_fades_each_output_out_on_its_frame_and_adds_it_to_the_hold_back() {
    let mut out = output();
    for _ in 0..40 {
        out.tick();
    }
    assert!(out.played.last().unwrap().0 == 0.5, "plays at full");
    // Ahead of the frame being read, off any callback's edge.
    let frame = out.reading() + 3 * CALLBACK as u64 + 100;
    out.writer.announce_move(RingMove {
        frame,
        frames: 4_800,
    });
    let from = out.played.len();
    for _ in 0..40 {
        out.tick();
    }
    let (_, named) = out.first_fade(from);
    assert_eq!(
        named % NAMES,
        frame % NAMES,
        "fades out on the move's frame"
    );
    assert_eq!(out.control.delay_frames(), HOLD_BACK + 4_800);
    let silent = from
        + out.played[from..]
            .iter()
            .position(|(left, _)| *left == 0.0)
            .unwrap();
    let back = silent
        + out.played[silent..]
            .iter()
            .position(|(left, _)| *left == 0.5)
            .expect("plays again");
    assert!(
        out.played[back..].iter().all(|(left, _)| *left == 0.5),
        "one fade"
    );
}

// covers: S281
#[test]
fn a_delay_the_host_sets_before_the_moves_frame_replaces_the_move() {
    let mut out = output();
    for _ in 0..40 {
        out.tick();
    }
    let frame = out.reading() + 20 * CALLBACK as u64;
    out.writer.announce_move(RingMove {
        frame,
        frames: 4_800,
    });
    out.tick();
    out.control.set_delay_frames(HOLD_BACK + 2_400);
    for _ in 0..60 {
        out.tick();
    }
    assert_eq!(out.control.delay_frames(), HOLD_BACK + 2_400);
    assert!(out.played.last().unwrap().0 == 0.5, "plays again");
}

// covers: S281
#[test]
fn an_output_made_after_a_move_was_announced_does_not_follow_it() {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    writer.announce_move(RingMove {
        frame: 0,
        frames: 4_800,
    });
    let reader = Reader::new(ring, RATE);
    reader.control().set_delay_frames(HOLD_BACK);
    let control = RendererControl::new(&reader);
    let mut renderer = Renderer::new(
        reader,
        &control,
        Arc::new(SharedGain::new(1.0)),
        RATE,
        RATE,
        2,
    )
    .unwrap();
    let mut out = vec![0.0f32; CALLBACK * 2];
    for _ in 0..20 {
        writer.write(&[0.5; CALLBACK * 2], CALLBACK);
        renderer.render(&mut out);
    }
    assert_eq!(control.delay_frames(), HOLD_BACK);
}
