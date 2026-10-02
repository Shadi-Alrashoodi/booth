// Which H.264 encoder open_codec() picks, forcing one with
// open_kind_codec(), and what the caller is told when a faster one is passed
// over or the share is too big for the software encoder. WARP, Windows'
// software rasterizer, stands in for a GPU with no hardware encoder.

mod common;

use encode::{Codec, EncodeError, Fit, Frame, Kind, Preset, Settings};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

use common::Frames;

fn settings() -> Settings {
    Settings {
        bitrate: 8_000_000,
        preset: Preset::P1,
    }
}

#[test]
fn nvenc_first_each_kind_forced() {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let encoder = encode::open_codec(Codec::H264, &gpu.device, 1920, 1080, 60, &settings())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(encoder.name().starts_with("NVENC"), "{}", encoder.name());
    assert_eq!(encoder.notes(), "", "nothing was passed over");
    drop(encoder);

    let mut frames = Frames::new(&gpu, 1920, 1080);
    for (i, kind) in Kind::ALL.into_iter().enumerate() {
        let mut encoder =
            encode::open_kind_codec(kind, Codec::H264, &gpu.device, 1920, 1080, 60, &settings())
                .unwrap_or_else(|e| panic!("{kind}: {e}"));
        println!("{kind}: {}", encoder.name());
        let expected = match kind {
            Kind::Nvenc => "NVENC H.264",
            Kind::MfHardware => "Media Foundation hardware H.264",
            Kind::MfSoftware => "Media Foundation H.264, software, 1080p60",
        };
        assert!(encoder.name().starts_with(expected), "{}", encoder.name());
        let texture = frames.frame(i as u64);
        let unit = encoder
            .encode(&Frame {
                texture: &texture,
                index: 0,
                force_idr: false,
            })
            .unwrap_or_else(|e| panic!("{kind}: {e}"));
        assert!(unit.idr);
    }
}

#[test]
fn software_on_warp_with_reason() {
    let _turn = common::turn();
    let gpu = common::warp();
    let mut encoder = encode::open_codec(Codec::H264, &gpu.device, 1920, 1080, 60, &settings())
        .unwrap_or_else(|e| panic!("{e}"));
    println!("{}", encoder.name());
    println!("{}", encoder.notes());
    assert_eq!(encoder.name(), "Media Foundation H.264, software, 1080p60");
    assert!(
        encoder.notes().starts_with(
            "the Media Foundation hardware encoder could not open: Windows lists no hardware H.264 encoder for the "
        ),
        "{}",
        encoder.notes()
    );
    assert!(
        encoder
            .notes()
            .contains("; using the Media Foundation software encoder."),
        "{}",
        encoder.notes()
    );

    // Blank frames: WARP maps an NV12 texture differently from a GPU, and
    // this only needs to show the encoder runs there.
    let texture = common::texture(&gpu, 1920, 1080, DXGI_FORMAT_NV12);
    for i in 0..3 {
        let unit = encoder
            .encode(&Frame {
                texture: &texture,
                index: i,
                force_idr: false,
            })
            .unwrap_or_else(|e| panic!("frame {i}: {e}"));
        assert_eq!(unit.idr, i == 0);
    }
}

#[test]
fn too_big_for_software() {
    let _turn = common::turn();
    let gpu = common::warp();
    let Err(error) = encode::open_codec(Codec::H264, &gpu.device, 2560, 1440, 120, &settings())
    else {
        panic!("a 2560x1440 share at 120 fps opened on WARP");
    };
    println!("{error}");
    let fit = Fit {
        width: 1920,
        height: 1080,
        fps: 60,
    };
    let EncodeError::NoEncoderForGpu {
        fit: Some(given), ..
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(*given, fit);
    assert!(
        error
            .to_string()
            .ends_with("Share at 1920x1080 and 60 fps to use the software encoder"),
        "{error}"
    );
    assert_eq!(encode::software_fit(2560, 1440, 120), Some(fit));

    let Err(error) = encode::open_kind_codec(
        Kind::MfSoftware,
        Codec::H264,
        &gpu.device,
        2560,
        1440,
        120,
        &settings(),
    ) else {
        panic!("the software encoder opened at 2560x1440 and 120 fps");
    };
    println!("{error}");
    assert!(
        matches!(error, EncodeError::SoftwareLimit { fit: f, .. } if f == fit),
        "{error}"
    );

    // What open_codec() says to do: open again at that size, so the faster
    // encoders get another try there. WARP has none, so it is the software
    // one.
    let encoder = encode::open_codec(
        Codec::H264,
        &gpu.device,
        fit.width,
        fit.height,
        fit.fps,
        &settings(),
    )
    .unwrap_or_else(|e| panic!("at the size it gave: {e}"));
    assert_eq!(encoder.name(), "Media Foundation H.264, software, 1080p60");
}

#[test]
fn software_fit_any_shape() {
    let _turn = common::turn();
    let gpu = common::warp();
    // 16:9, 21:9, 32:9 and 16:10, then 16:9 and 32:9 turned on their side.
    for (width, height) in [
        (2560, 1440),
        (3440, 1440),
        (5120, 1440),
        (2560, 1600),
        (1440, 2560),
        (1440, 5120),
    ] {
        let fit = encode::software_fit(width, height, 120).expect("larger than 1080p60");
        let mut encoder = encode::open_kind_codec(
            Kind::MfSoftware,
            Codec::H264,
            &gpu.device,
            fit.width,
            fit.height,
            fit.fps,
            &settings(),
        )
        .unwrap_or_else(|e| panic!("{width}x{height} at {}x{}: {e}", fit.width, fit.height));
        println!("{width}x{height} is shared at {}x{}", fit.width, fit.height);
        // Blank frames, as in the test above.
        let texture = common::texture(&gpu, fit.width, fit.height, DXGI_FORMAT_NV12);
        for i in 0..2 {
            let unit = encoder
                .encode(&Frame {
                    texture: &texture,
                    index: i,
                    force_idr: false,
                })
                .unwrap_or_else(|e| panic!("{width}x{height}, frame {i}: {e}"));
            assert_eq!(unit.idr, i == 0);
        }
    }
}

#[test]
fn nvenc_forced_on_warp() {
    let gpu = common::warp();
    let Err(error) = encode::open_kind_codec(
        Kind::Nvenc,
        Codec::H264,
        &gpu.device,
        1920,
        1080,
        60,
        &settings(),
    ) else {
        panic!("NVENC opened on WARP");
    };
    println!("{error}");
    assert!(
        error
            .to_string()
            .ends_with("NVENC runs on NVIDIA GPUs only"),
        "{error}"
    );
}

#[test]
fn kind_words_round_trip() {
    for kind in Kind::ALL {
        assert_eq!(kind.word().parse::<Kind>(), Ok(kind));
    }
    assert_eq!("Software".parse::<Kind>(), Ok(Kind::MfSoftware));
    let Err(error) = "amf".parse::<Kind>() else {
        panic!("amf is not an encoder Booth has");
    };
    assert_eq!(
        error,
        "there is no encoder called \"amf\": the choices are nvenc, hardware and software"
    );
}
