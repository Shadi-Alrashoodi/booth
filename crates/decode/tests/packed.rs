// Access units no encoder writes and a friend's broken or infected PC could:
// one small NAL unit over and over, on each copy of which FFmpeg spends time or
// memory inside one call nothing can interrupt. Each is refused before FFmpeg
// sees it, the most that is not refused stays cheap, and the stream's next IDR
// decodes whole. Every encoder this PC has must pass untouched. The cases with
// no picture in them need no encoder, and FFmpeg sets nothing up on the GPU for
// them.

mod common;

use std::time::{Duration, Instant};

use annexb::hevc;
use decode::{Codec, DecodeError, Decoder, MAX_ACCESS_UNIT};
use encode::{Frame, Kind, Settings};

use common::{Reader, Stream};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
// A decode that takes longer is as good as a hang (tests/garbage.rs).
const PATIENCE: Duration = Duration::from_secs(1);

fn private_bytes() -> u64 {
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: the pseudo handle of this process and a live structure of the
    // size given, which the EX form extends.
    unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            counters.cb,
        )
    }
    .ok()
    .expect("this process's memory counters");
    counters.PrivateUsage as u64
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

// Bits of an RBSP, most significant first.
struct Writer {
    bits: Vec<bool>,
}

impl Writer {
    fn new() -> Writer {
        Writer { bits: Vec::new() }
    }

    fn u(&mut self, n: u32, value: u32) -> &mut Writer {
        for i in (0..n).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
        self
    }

    fn ue(&mut self, value: u32) -> &mut Writer {
        let coded = value + 1;
        let len = 32 - coded.leading_zeros();
        self.u(len - 1, 0).u(len, coded)
    }

    // Start code, header, then the RBSP with its stop bit and emulation
    // prevention.
    fn nal(&mut self, header: &[u8]) -> Vec<u8> {
        self.bits.push(true);
        while !self.bits.len().is_multiple_of(8) {
            self.bits.push(false);
        }
        let mut out = vec![0, 0, 1];
        out.extend_from_slice(header);
        let mut zeros = 0;
        for byte in self.bits.chunks(8) {
            let byte = byte.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b));
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            out.push(byte);
        }
        out
    }
}

// profile_tier_level for Main at level 6, one sub-layer.
fn profile(w: &mut Writer) {
    w.u(2, 0).u(1, 0).u(5, 1).u(32, 0x6000_0000);
    w.u(4, 0b1001).u(32, 0).u(12, 0).u(8, 180);
}

fn hevc_vps() -> Vec<u8> {
    let mut w = Writer::new();
    w.u(4, 0)
        .u(1, 1)
        .u(1, 1)
        .u(6, 0)
        .u(3, 0)
        .u(1, 1)
        .u(16, 0xffff);
    profile(&mut w);
    w.u(1, 1).ue(4).ue(0).ue(0);
    w.u(6, 0).ue(0).u(1, 0).u(1, 0);
    w.nal(&[hevc::VPS << 1, 1])
}

// The longest picture Booth takes at its full area in the smallest coding tree
// blocks HEVC has, 512 by 72 of them.
fn hevc_sps() -> Vec<u8> {
    let mut w = Writer::new();
    w.u(4, 0).u(3, 0).u(1, 1);
    profile(&mut w);
    w.ue(0).ue(1).ue(8192).ue(1152).u(1, 0);
    w.ue(0).ue(0).ue(4).u(1, 1).ue(4).ue(0).ue(0);
    w.ue(0).ue(1).ue(0).ue(2).ue(0).ue(0);
    w.u(1, 0).u(1, 0).u(1, 0).u(1, 0).ue(0).u(1, 0);
    w.u(1, 0).u(1, 0).u(1, 0).u(1, 0);
    w.nal(&[hevc::SPS << 1, 1])
}

// A PPS with a tile for every block, which is what FFmpeg takes longest to work
// out. Two with different `qp` take turns past its check for one it has
// already.
fn hevc_pps(qp: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.ue(0).ue(0).u(1, 0).u(1, 0).u(3, 0).u(1, 0).u(1, 0);
    w.ue(0).ue(0).ue(qp).u(1, 0).u(1, 0).u(1, 0);
    w.ue(0).ue(0).u(1, 0).u(1, 0).u(1, 0).u(1, 0);
    w.u(1, 1).u(1, 0).ue(511).ue(71).u(1, 1).u(1, 1);
    w.u(1, 1).u(1, 0).u(1, 0).u(1, 0).ue(0).u(1, 0).u(1, 0);
    w.nal(&[hevc::PPS << 1, 1])
}

// A slice FFmpeg gives up on at its header, so nothing is set up on the GPU, in
// the place of the picture HEVC's wait for an IDR asks for.
const NO_PICTURE: [u8; 6] = [0, 0, 1, hevc::IDR_W_RADL << 1, 1, 0xff];

fn h264_sps() -> Vec<u8> {
    let mut w = Writer::new();
    w.u(8, 100).u(8, 0).u(8, 52).ue(0);
    w.ue(1).ue(0).ue(0).u(1, 0).u(1, 0);
    w.ue(4).ue(0).ue(2).ue(4).u(1, 0);
    w.ue(79).ue(44).u(1, 1).u(1, 1).u(1, 0).u(1, 0);
    w.nal(&[0x67])
}

fn h264_pps(qp: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.ue(0).ue(0).u(1, 0).u(1, 0).ue(0);
    w.ue(0).ue(0).u(1, 0).u(2, 0);
    w.ue(qp).ue(0).ue(0).u(1, 0).u(1, 0).u(1, 0);
    w.nal(&[0x68])
}

fn sei_flood(header: &[u8], bytes: usize) -> Vec<u8> {
    // Type 5, the one FFmpeg keeps, in the smallest message it takes.
    let message = [&[5u8, 16][..], &[0x11; 16]].concat();
    let mut unit = vec![0, 0, 1];
    unit.extend_from_slice(header);
    while unit.len() + message.len() < bytes {
        unit.extend_from_slice(&message);
    }
    unit.push(0x80);
    unit
}

fn without_a_picture(codec: Codec) -> Option<(common::Gpu, Decoder)> {
    let gpu = common::nvidia()?;
    let decoder = common::decoder(&gpu, codec)?;
    Some((gpu, decoder))
}

fn refused_as(result: Result<Option<decode::Decoded>, DecodeError>, what: &str) {
    match result {
        Err(DecodeError::Packed { what: said, .. }) if said == what => {}
        other => panic!("for {what}: {other:?}"),
    }
}

// FFmpeg kept about 4 KB for each of these for the rest of the watch: about
// 2.9 GB for the 2.75 MB a frame can carry.
#[test]
fn nal_unit_floods_keep_nothing() {
    let _turn = common::turn();
    for (codec, piece) in [
        (Codec::H264, vec![0, 0, 1, 9]),
        (Codec::Hevc, vec![0, 0, 1, 0x46, 1]),
    ] {
        let Some((_gpu, mut decoder)) = without_a_picture(codec) else {
            return;
        };
        let mut unit = piece.repeat((1 << 20) / piece.len());
        unit.extend_from_slice(&NO_PICTURE);
        let before = private_bytes();
        let result = decoder.decode(&unit);
        let rise = private_bytes().saturating_sub(before);
        println!("{codec}: private bytes up {} KB", rise >> 10);
        refused_as(result, "NAL units");
        assert!(rise < 64 << 20, "{codec}: private bytes up {rise}");
    }
}

// Each start code in front of a zero byte reads as padding, which the search
// for the first picture walks past one block at a time: about 10 ms for 3 MiB
// when it came first. The count stops at the first too many. In a debug build,
// which counts about 140 times slower, the walk took over 1 s.
#[test]
fn padding_costs_only_the_count() {
    let _turn = common::turn();
    let Some((_gpu, mut decoder)) = without_a_picture(Codec::Hevc) else {
        return;
    };
    let unit = [0u8, 0, 1].repeat(MAX_ACCESS_UNIT / 3);
    let longest = if cfg!(debug_assertions) {
        Duration::from_millis(100)
    } else {
        Duration::from_micros(250)
    };
    let mut fastest = Duration::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        let result = decoder.decode(&unit);
        fastest = fastest.min(started.elapsed());
        refused_as(result, "NAL units");
    }
    println!("refused in {:.3} ms", ms(fastest));
    assert!(fastest < longest, "{fastest:?}");
}

// Without a limit 196,000 of these in one frame would keep FFmpeg busy for
// minutes.
#[test]
fn hevc_parameter_sets() {
    let _turn = common::turn();
    let Some((_gpu, mut decoder)) = without_a_picture(Codec::Hevc) else {
        return;
    };
    let (one, other) = (hevc_pps(0), hevc_pps(1));
    let turns = |count: usize| -> Vec<u8> {
        let mut unit = Vec::new();
        for n in 0..count {
            unit.extend_from_slice(if n % 2 == 0 { &other } else { &one });
        }
        unit.extend_from_slice(&NO_PICTURE);
        unit
    };
    let first = [hevc_vps(), hevc_sps(), one.clone(), NO_PICTURE.to_vec()].concat();
    let answer = decoder.decode(&first);
    assert!(matches!(answer, Ok(None)), "{answer:?}");

    // The most it can be made to work out in one call, with `one` the PPS it
    // has last each time.
    let mut most = Duration::ZERO;
    for _ in 0..5 {
        let unit = turns(8);
        let started = Instant::now();
        let answer = decoder.decode(&unit);
        most = most.max(started.elapsed());
        assert!(matches!(answer, Ok(None)), "{answer:?}");
    }
    println!("8 PPSs with a tile for every block took {:.1} ms", ms(most));
    assert!(most < PATIENCE, "{most:?}");

    let started = Instant::now();
    refused_as(decoder.decode(&turns(9)), "parameter sets");
    refused_as(decoder.decode(&turns(1000)), "parameter sets");
    let took = started.elapsed();
    println!("refused in {:.3} ms", ms(took));
    assert!(took < PATIENCE, "{took:?}");
}

#[test]
fn h264_parameter_sets() {
    let _turn = common::turn();
    let Some((_gpu, mut decoder)) = without_a_picture(Codec::H264) else {
        return;
    };
    let turns = |count: usize| -> Vec<u8> {
        let mut unit = h264_sps();
        for n in 0..count {
            unit.extend_from_slice(&h264_pps(n as u32 % 2));
        }
        unit
    };
    let started = Instant::now();
    let answer = decoder.decode(&turns(7));
    println!(
        "an SPS and 7 PPSs took {:.3} ms: {answer:?}",
        ms(started.elapsed())
    );
    assert!(
        matches!(answer, Err(DecodeError::Damaged { .. })),
        "{answer:?}"
    );
    refused_as(decoder.decode(&turns(8)), "parameter sets");
    refused_as(decoder.decode(&turns(1000)), "parameter sets");
}

// One unit full of the smallest messages took 1.2 s, in either codec.
#[test]
fn sei_floods() {
    let _turn = common::turn();
    for (codec, header) in [
        (Codec::H264, vec![6]),
        (Codec::Hevc, vec![hevc::PREFIX_SEI << 1, 1]),
    ] {
        let Some((_gpu, mut decoder)) = without_a_picture(codec) else {
            return;
        };
        let flood = sei_flood(&header, 2_752_000);
        let started = Instant::now();
        refused_as(decoder.decode(&flood), "bytes of SEI");
        let refused = started.elapsed();
        let most = [&sei_flood(&header, 64 << 10)[..], &NO_PICTURE].concat();
        let started = Instant::now();
        let answer = decoder.decode(&most);
        let took = started.elapsed();
        println!(
            "{codec}: refused in {:.3} ms, 64 KB of messages took {:.3} ms: {answer:?}",
            ms(refused),
            ms(took)
        );
        assert!(
            !matches!(answer, Err(DecodeError::Packed { .. })),
            "{answer:?}"
        );
        assert!(
            refused < PATIENCE && took < PATIENCE,
            "{refused:?}, {took:?}"
        );
    }
}

struct Stream5 {
    decoder: Decoder,
    reader: Reader,
    // Frame 0 an IDR, 1 and 2 P frames, 3 an IDR, 4 a P frame.
    units: Vec<Vec<u8>>,
}

fn stream5(codec: Codec) -> Option<Stream5> {
    let gpu = common::nvidia()?;
    let decoder = common::decoder(&gpu, codec)?;
    let mut stream = Stream::new(&gpu, codec, WIDTH, HEIGHT);
    let units: Vec<Vec<u8>> = (0..5).map(|n| stream.next(n == 3).data).collect();
    drop(stream);
    Some(Stream5 {
        reader: Reader::new(&gpu),
        decoder,
        units,
    })
}

impl Stream5 {
    fn whole(&mut self, n: usize) {
        let decoded = self
            .decoder
            .decode(&self.units[n])
            .unwrap_or_else(|e| panic!("frame {n}: {e}"))
            .unwrap_or_else(|| panic!("frame {n} gave no picture"));
        assert_eq!(
            self.reader.frame_number(&decoded),
            Some(n as u32),
            "frame {n}"
        );
    }

    fn slice(&self, n: usize, codec: Codec) -> Vec<u8> {
        let slice = annexb::nal_units(&self.units[n])
            .find(|nal| match codec {
                Codec::H264 => nal.is_slice(),
                Codec::Hevc => hevc::is_slice(nal),
            })
            .expect("a slice");
        [&[0u8, 0, 1][..], slice.data].concat()
    }
}

// The H.264 decoder ended each picture and sent it to the GPU, with one shown,
// and made its surfaces and a decoder again for slices that took turns naming
// two SPSs.
#[test]
fn more_than_one_picture() {
    let _turn = common::turn();
    for codec in common::CODECS {
        let Some(mut s) = stream5(codec) else { return };
        s.whole(0);
        // Cut short: a picture's start is all a slice is read for here, and a
        // whole IDR slice 1000 times is past the size a unit may have.
        let slice = s.slice(0, codec);
        let unit = [&s.units[0][..], &slice[..64].repeat(1000)].concat();
        let answer = s.decoder.decode(&unit);
        assert!(
            matches!(answer, Err(DecodeError::ManyPictures)),
            "{codec}: {answer:?}"
        );
        let unit = [&s.units[0][..], &slice].concat();
        let answer = s.decoder.decode(&unit);
        assert!(
            matches!(answer, Err(DecodeError::ManyPictures)),
            "{codec}: {answer:?}"
        );
        // A P frame after a refused unit predicts from what is not there, and
        // FFmpeg's HEVC decoder would make it a picture all the same.
        if codec == Codec::Hevc {
            let after = s.decoder.decode(&s.units[1]);
            assert!(matches!(after, Ok(None)), "{after:?}");
        }
        s.whole(3);
        s.whole(4);
    }
}

// Whatever each encoder on this PC writes at the sizes that make it cut a
// picture up the most, never refused: every frame comes back.
fn all_pass(kind: Kind, codec: Codec, width: u32, height: u32, fps: u32) {
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut decoder) = common::decoder(&gpu, codec) else {
        return;
    };
    let mut pattern =
        capture::Pattern::new(&gpu.device, width, height, 0).unwrap_or_else(|e| panic!("{e}"));
    let share_codec = match codec {
        Codec::H264 => encode::Codec::H264,
        Codec::Hevc => encode::Codec::Hevc,
    };
    let opened = encode::open_kind_codec(
        kind,
        share_codec,
        &gpu.device,
        width,
        height,
        fps,
        &Settings::default(),
    );
    let mut encoder = match opened {
        Ok(encoder) => encoder,
        Err(err) => {
            println!("skipped {kind} {codec} {width}x{height}: {err}");
            return;
        }
    };
    let (mut most_units, mut most_sei) = (0, 0);
    for n in 0..60 {
        let frame = pattern.next().unwrap_or_else(|e| panic!("{e}"));
        let unit = encoder
            .encode(&Frame {
                texture: &frame.texture,
                index: frame.number,
                force_idr: n == 30,
            })
            .unwrap_or_else(|e| panic!("frame {n}: {e}"));
        let (mut units, mut sei) = (0, 0);
        for data in annexb::after_start_codes(&unit.data) {
            units += 1;
            let nal = annexb::Nal { data };
            sei += match codec {
                Codec::H264 if nal.kind() == annexb::NAL_SEI => data.len(),
                Codec::Hevc if matches!(hevc::kind(&nal), hevc::PREFIX_SEI | hevc::SUFFIX_SEI) => {
                    data.len()
                }
                _ => 0,
            };
        }
        most_units = most_units.max(units);
        most_sei = most_sei.max(sei);
        let decoded = decoder
            .decode(&unit.data)
            .unwrap_or_else(|e| panic!("{kind} {codec} {width}x{height}, frame {n}: {e}"));
        assert!(
            decoded.is_some(),
            "{kind} {codec} {width}x{height}, frame {n} gave no picture"
        );
    }
    println!(
        "{} at {width}x{height}: at most {most_units} NAL units, {most_sei} bytes of SEI",
        encoder.name()
    );
}

#[test]
fn every_encoder_here_passes() {
    let _turn = common::turn();
    for (width, height) in [(1280, 720), (2560, 1440), (3840, 2160)] {
        all_pass(Kind::Nvenc, Codec::H264, width, height.min(1440), 120);
        all_pass(Kind::Nvenc, Codec::Hevc, width, height, 120);
        all_pass(Kind::MfHardware, Codec::H264, width, height.min(1440), 60);
        all_pass(Kind::MfHardware, Codec::Hevc, width, height, 60);
    }
    for (width, height) in [(1920, 1080), (1080, 1920), (766, 2720), (2716, 764)] {
        all_pass(Kind::MfSoftware, Codec::H264, width, height, 60);
    }
}
