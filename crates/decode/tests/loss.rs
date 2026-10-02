// A lost frame and the two ways the encoder answers it, as the viewer sees
// them, in H.264 and in HEVC: invalidation, after which the stream goes on,
// and an IDR, before which the reassembler holds frames back.

mod common;

use decode::{Codec, Decoder};
use encode::{AccessUnit, Recovery};

use common::{Reader, Stream};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const LOST: u64 = 30;
const FRAMES: u64 = 120;

fn clean(decoder: &mut Decoder, reader: &mut Reader, unit: &AccessUnit) {
    let n = unit.index;
    let decoded = decoder
        .decode(&unit.data)
        .unwrap_or_else(|e| panic!("frame {n}: {e}"))
        .unwrap_or_else(|| panic!("frame {n} gave no picture"));
    assert_eq!(
        (decoded.width, decoded.height),
        (WIDTH, HEIGHT),
        "frame {n}"
    );
    assert_eq!(
        reader.frame_number(&decoded),
        Some(n as u32),
        "frame {n} decoded to the wrong picture"
    );
}

// What a frame that predicts from the lost one decodes to, for the log.
fn damaged(decoder: &mut Decoder, reader: &mut Reader, unit: &AccessUnit) -> String {
    match decoder.decode(&unit.data) {
        Ok(Some(decoded)) => match reader.frame_number(&decoded) {
            Some(number) if u64::from(number) == unit.index => "right number".to_string(),
            Some(number) => format!("number {number}"),
            None => "number row damaged".to_string(),
        },
        Ok(None) => "no picture".to_string(),
        Err(err) => format!("error: {err}"),
    }
}

// The loss report reaches the encoder `late` frames after the lost one was
// encoded. Frames encoded before it predict from the lost frame, and the
// reassembler passes them on, since this encoder's frames survive loss.
fn invalidation(codec: Codec, late: u64) {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut decoder) = common::decoder(&gpu, codec) else {
        return;
    };
    let mut stream = Stream::new(&gpu, codec, WIDTH, HEIGHT);
    let mut reader = Reader::new(&gpu);
    let reported = LOST + late;
    let mut between = Vec::new();

    for n in 0..FRAMES {
        let unit = stream.next(false);
        assert!(!unit.idr || n == 0, "frame {n} is an IDR");
        if n == LOST {
            // Dropped on the way.
        } else if n > LOST && n <= reported {
            between.push(format!(
                "{n}: {}",
                damaged(&mut decoder, &mut reader, &unit)
            ));
        } else {
            clean(&mut decoder, &mut reader, &unit);
        }
        if n == reported {
            assert_eq!(
                stream.encoder.recover(LOST),
                Recovery::Invalidated,
                "NVENC still holds frame {} to predict from",
                LOST - 1
            );
        }
    }
    println!(
        "{codec}: frame {LOST} lost, reported after {late} more frames: every frame from {} on decoded whole with its own number{}",
        reported + 1,
        if between.is_empty() {
            String::new()
        } else {
            format!("; the ones in between: {}", between.join(", "))
        }
    );
}

#[test]
fn invalidation_at_once() {
    invalidation(Codec::H264, 0);
}

#[test]
fn invalidation_late() {
    invalidation(Codec::H264, 3);
}

#[test]
fn hevc_invalidation_at_once() {
    invalidation(Codec::Hevc, 0);
}

#[test]
fn hevc_invalidation_late() {
    invalidation(Codec::Hevc, 3);
}

// The loss is reported only once NVENC no longer holds a frame from before
// it, so it answers with an IDR.
fn idr_recovery(codec: Codec, feed_the_broken_frames: bool) {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut decoder) = common::decoder(&gpu, codec) else {
        return;
    };
    let mut stream = Stream::new(&gpu, codec, WIDTH, HEIGHT);
    let mut reader = Reader::new(&gpu);
    let mut reported = None;
    let mut between = Vec::new();

    for n in 0..FRAMES {
        let unit = stream.next(false);
        assert_eq!(
            unit.idr,
            n == 0 || reported.is_some_and(|at| n == at + 1),
            "frame {n}"
        );
        if n == LOST {
            // Dropped on the way.
        } else if n > LOST && reported.is_none() {
            // The reassembler holds these until the IDR.
            if feed_the_broken_frames {
                between.push(damaged(&mut decoder, &mut reader, &unit));
            }
        } else {
            clean(&mut decoder, &mut reader, &unit);
        }
        // How many references NVENC keeps depends on the codec and the level
        // it picked, so the report waits for the first frame after which an
        // invalidation no longer reaches back.
        if n > LOST && reported.is_none() && stream.encoder.needs_idr(LOST) {
            assert_eq!(stream.encoder.recover(LOST), Recovery::Idr);
            reported = Some(n);
        }
    }
    let reported = reported.expect("NVENC let go of the frames before the lost one");
    // The invalidation tests recover this loss reported 3 frames late, so
    // needs_idr saying yes by then would be wrong, and this test would not
    // be about frames NVENC has let go of.
    assert!(
        reported > LOST + 3,
        "{codec}: needs_idr said yes {} frames after the loss",
        reported - LOST
    );
    let whole = between.iter().filter(|b| *b == "right number").count();
    println!(
        "{codec}: frame {LOST} lost, needed an IDR once reported after {} more frames, IDR at frame {}: the IDR and every frame after it decoded whole; {}",
        reported - LOST,
        reported + 1,
        if feed_the_broken_frames {
            format!(
                "the {} frames in between went to the decoder too and {whole} of them still showed their own number: {}",
                between.len(),
                between.join(", ")
            )
        } else {
            "the frames in between were held back".to_string()
        }
    );
}

#[test]
fn idr_after_held_frames() {
    idr_recovery(Codec::H264, false);
}

#[test]
fn idr_after_broken_frames() {
    idr_recovery(Codec::H264, true);
}

#[test]
fn hevc_idr_after_held_frames() {
    idr_recovery(Codec::Hevc, false);
}

#[test]
fn hevc_idr_after_broken_frames() {
    idr_recovery(Codec::Hevc, true);
}
