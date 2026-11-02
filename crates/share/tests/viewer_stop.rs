// What a viewer takes from its inbox at one wake can be thousands of one-packet
// frames from a friend's PC, and a hostile one can make FFmpeg spend tens of ms
// on each. Leave joins the viewer's thread on the panel's own, and a closed
// window leaves a watch running with nothing to show it in, so an end has to be
// heard between two of them, not only when the batch is done.

use std::sync::Arc;

use channels::Packetizer;
use channels::video::FrameFacts;
use share::{Inbox, Line, LinkNumbers, PAYLOAD_LAN, Report, Screen, Show, Watch};

const FRAMES: u32 = 200;

fn open(inbox: &Arc<Inbox>) -> Option<Screen> {
    let watch = Watch {
        title: "Booth viewer stop test".to_string(),
        width: 1280,
        height: 720,
        fps: 60,
        vsync: false,
        show: Show::Hidden,
        hevc: false,
        clock: None,
        link: LinkNumbers::default(),
        keep_times: false,
    };
    match Screen::open(&watch, Arc::clone(inbox), &mut |_| {}) {
        Ok(screen) => Some(screen),
        Err(why) => {
            println!("skipped: {why}");
            None
        }
    }
}

// Each frame an IDR of `unit`, one packet each, in the inbox.
fn fill(inbox: &Inbox, frames: u32, unit: &[u8]) {
    let mut packetizer = Packetizer::new(PAYLOAD_LAN).unwrap();
    for number in 0..frames {
        let facts = FrameFacts {
            number,
            idr: true,
            ..FrameFacts::default()
        };
        let packets = packetizer.packetize(&facts, unit, 20).unwrap();
        inbox.packet(packets.get(0).unwrap());
    }
}

// `end` runs at the first frame's failure, which is said at once.
fn ended_in_the_middle_of_a_batch(end: fn(&Inbox)) {
    let inbox = Arc::new(Inbox::new().unwrap());
    let Some(screen) = open(&inbox) else { return };
    // An access unit delimiter and no picture, which FFmpeg answers with an
    // error.
    fill(&inbox, FRAMES, &[0, 0, 1, 9, 0x10]);
    let numbers = screen
        .run(&mut |report| {
            if let Report::Line(_) = report {
                end(&inbox);
            }
        })
        .unwrap();
    assert_eq!(numbers.decode_failed, 1, "frames decoded after the end");
}

#[test]
fn stop_in_the_middle_of_a_batch() {
    ended_in_the_middle_of_a_batch(Inbox::stop);
}

// Inbox::close ends the loop as the person closing the window does.
#[test]
fn close_in_the_middle_of_a_batch() {
    ended_in_the_middle_of_a_batch(Inbox::close);
}

// The third refusal in a row ends the watch and its text reaches the person, in
// plain words. The log keeps what was too many.
#[test]
fn packed_frame_is_plain_for_the_person_and_exact_in_the_log() {
    let inbox = Arc::new(Inbox::new().unwrap());
    let Some(screen) = open(&inbox) else { return };
    fill(&inbox, 3, &[0, 0, 1, 9, 0x10].repeat(65));
    let mut lines = Vec::new();
    let why = screen
        .run(&mut |report| {
            if let Report::Line(line) = report {
                lines.push(line);
            }
        })
        .expect_err("three refusals in a row end the watch");
    println!("{why}");
    println!("{lines:?}");
    assert!(why.contains("packed with far more"), "{why}");
    assert!(!why.contains("NAL"), "{why}");
    assert!(
        lines.contains(&Line::Log(
            "frame 0 carries more than 64 NAL units".to_string()
        )),
        "{lines:?}"
    );
}
