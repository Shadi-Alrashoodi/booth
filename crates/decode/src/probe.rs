// Whether the GPU decodes a codec at a size, asked of Direct3D alone, so the
// room can answer it the moment someone presses Watch: no FFmpeg, no
// decoder made, no video memory.

use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_SINGLETHREADED, D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
    D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_VIDEO_DECODER_DESC, ID3D11Device, ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::core::{GUID, Interface};

use crate::decoder::{Codec, adapter_name, adapter_vendor};
use crate::error::DecodeError;
use crate::ffi::booth_too_large;

const NEWER_DRIVER: &str = "A newer graphics driver may add it";

const INTEL: u32 = 0x8086;

// An Intel Iris Xe failed two frames in a row of an NVENC HEVC share a few
// seconds into every watch, where NVIDIA's decoder and FFmpeg's own decode
// the same kind of stream whole, and nothing in the stream breaks the
// standard. Why the Iris Xe failed is not known, so until HEVC is tested on
// Intel graphics, an Intel viewer takes no HEVC and the share goes in H.264.
pub(crate) fn hevc_untested(vendor_id: u32) -> bool {
    vendor_id == INTEL
}

/// Whether the GPU `device` is on decodes `codec` at `width` x `height`
/// into NV12, as [`crate::Decoder`] needs, or why not. The answer is the
/// driver's: whether it lists a decoder for the codec (for HEVC, Main
/// profile), whether that decoder writes NV12, and whether it has a
/// configuration at that size. A size past what the decoder takes from any
/// stream is [`DecodeError::Oversized`] whatever the driver says; both ask
/// the same function in fields.c. HEVC on Intel graphics is
/// [`DecodeError::HevcUntested`] whatever the driver says.
///
/// FFmpeg need not be loaded, and nothing is made on the device. Only the
/// device is called, never its immediate context, so a viewer may draw on
/// the same device from another thread meanwhile. That holds only because a
/// device made single-threaded is refused here, as [`crate::Decoder::new`]
/// refuses it: the calls of such a device are not safe from two threads.
/// On my RTX 4070 Ti SUPER the first answer on a device
/// takes about 0.5 ms and later ones a few microseconds (tests/probe.rs).
pub fn probe(
    device: &ID3D11Device,
    codec: Codec,
    width: u32,
    height: u32,
) -> Result<(), DecodeError> {
    if booth_too_large(codec.id(), width.into(), height.into()) != 0 {
        return Err(DecodeError::Oversized { width, height });
    }
    // SAFETY: a getter on a live device.
    let flags = unsafe { device.GetCreationFlags() };
    if flags & D3D11_CREATE_DEVICE_SINGLETHREADED.0 != 0 {
        return Err(DecodeError::NotShareable {
            gpu: adapter_name(device),
            why: "it was made single-threaded",
        });
    }
    if codec == Codec::Hevc && hevc_untested(adapter_vendor(device)) {
        return Err(DecodeError::HevcUntested {
            gpu: adapter_name(device),
        });
    }
    let not = |why, next| DecodeError::NotDecodable {
        gpu: adapter_name(device),
        codec,
        width,
        height,
        why,
        next,
    };
    let Ok(video) = device.cast::<ID3D11VideoDevice>() else {
        return Err(not("Direct3D offers no video decoding on it", NEWER_DRIVER));
    };
    let profile = match codec {
        Codec::H264 => D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
        Codec::Hevc => D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
    };
    if !lists(device, &video, &profile)? {
        return Err(not("its driver lists no decoder for it", NEWER_DRIVER));
    }
    // SAFETY: a live interface and a GUID that outlives the call.
    let nv12 = unsafe { video.CheckVideoDecoderFormat(&profile, DXGI_FORMAT_NV12) }
        .map_err(|source| failed(device, source))?;
    if !nv12.as_bool() {
        return Err(not(
            "its decoder for it does not write NV12, the one format the viewer draws",
            NEWER_DRIVER,
        ));
    }
    // FFmpeg sets the decoder up at the coded size, so this asks at the
    // largest one an encoder can send for the picture: whole macroblocks for
    // H.264, whole 64x64 coding tree blocks for HEVC (NVENC codes 720 lines
    // as 736).
    let block = match codec {
        Codec::H264 => 16,
        Codec::Hevc => 64,
    };
    let desc = D3D11_VIDEO_DECODER_DESC {
        Guid: profile,
        SampleWidth: width.next_multiple_of(block),
        SampleHeight: height.next_multiple_of(block),
        OutputFormat: DXGI_FORMAT_NV12,
    };
    // SAFETY: a live interface and a full description. Drivers answer a
    // size they do not take with an error or with no configurations.
    let configs = unsafe { video.GetVideoDecoderConfigCount(&desc) }.unwrap_or(0);
    if configs > 0 {
        return Ok(());
    }
    Err(lost(device).unwrap_or_else(|| {
        not(
            "its decoder does not take that size",
            "The sharer can share at a smaller size",
        )
    }))
}

fn lists(
    device: &ID3D11Device,
    video: &ID3D11VideoDevice,
    profile: &GUID,
) -> Result<bool, DecodeError> {
    // SAFETY: a getter on a live interface.
    let count = unsafe { video.GetVideoDecoderProfileCount() };
    for index in 0..count {
        // SAFETY: an index below the count just read.
        let listed = unsafe { video.GetVideoDecoderProfile(index) }
            .map_err(|source| failed(device, source))?;
        if listed == *profile {
            return Ok(true);
        }
    }
    Ok(false)
}

fn lost(device: &ID3D11Device) -> Option<DecodeError> {
    // SAFETY: a getter on a live device.
    let removed = unsafe { device.GetDeviceRemovedReason() };
    removed.err().map(|source| DecodeError::DeviceLost {
        gpu: adapter_name(device),
        source,
    })
}

fn failed(device: &ID3D11Device, source: windows::core::Error) -> DecodeError {
    lost(device).unwrap_or(DecodeError::Direct3D {
        action: "ask the graphics card's driver which video it decodes",
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

    // WARP draws on the CPU and decodes no video, so no GPU is touched.
    fn warp(flags: D3D11_CREATE_DEVICE_FLAG) -> ID3D11Device {
        let mut device = None;
        // SAFETY: a live out parameter; no adapter, context or level wanted.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .expect("a WARP device");
        device.expect("a WARP device")
    }

    #[test]
    fn no_video_decoder() {
        let device = warp(D3D11_CREATE_DEVICE_FLAG(0));
        for codec in [Codec::H264, Codec::Hevc] {
            let err = probe(&device, codec, 4096, 2304).expect_err("WARP decodes no video");
            let text = err.to_string();
            println!("{text}");
            assert!(matches!(err, DecodeError::NotDecodable { .. }), "{err:?}");
            assert!(
                text.contains(&format!("does not decode {codec} at 4096x2304 because")),
                "{text}"
            );
            assert!(
                text.ends_with(". A newer graphics driver may add it"),
                "{text}"
            );
        }
    }

    #[test]
    fn size_past_the_limit() {
        let device = warp(D3D11_CREATE_DEVICE_FLAG(0));
        for codec in [Codec::H264, Codec::Hevc] {
            let err = probe(&device, codec, 4096, 2320).unwrap_err();
            assert!(
                matches!(
                    err,
                    DecodeError::Oversized {
                        width: 4096,
                        height: 2320
                    }
                ),
                "{err:?}"
            );
        }
    }

    // FFmpeg rounds an HEVC surface pool up to 128 each way, and the limit
    // counts blocks of that, so shapes H.264 takes can be past it in HEVC.
    // The sizes Booth shares pass in both, and reach the driver.
    #[test]
    fn hevc_pool_size_limit() {
        let device = warp(D3D11_CREATE_DEVICE_FLAG(0));
        let oversized = |codec, (width, height)| {
            matches!(
                probe(&device, codec, width, height),
                Err(DecodeError::Oversized { .. })
            )
        };
        for size in [
            (1280, 720),
            (1920, 1080),
            (2560, 1440),
            (3440, 1440),
            (3840, 2160),
            (5120, 1440),
            (4096, 2304),
        ] {
            for codec in [Codec::H264, Codec::Hevc] {
                assert!(!oversized(codec, size), "{codec} {size:?}");
            }
        }
        // 36859 and 36792 blocks of 16, but 43392 and 38912 once rounded
        // to 128.
        for size in [(648, 14384), (4032, 2336)] {
            assert!(!oversized(Codec::H264, size), "H.264 {size:?}");
            assert!(oversized(Codec::Hevc, size), "HEVC {size:?}");
        }
    }

    #[test]
    fn hevc_off_on_intel() {
        assert!(hevc_untested(0x8086), "Intel");
        for vendor in [0x10de, 0x1002, 0x1414, 0] {
            assert!(!hevc_untested(vendor), "{vendor:#06x}");
        }
        let text = DecodeError::HevcUntested {
            gpu: "Intel(R) Iris(R) Xe Graphics".to_string(),
        }
        .to_string();
        assert_eq!(
            text,
            "HEVC stays off on Intel graphics until it is tested there, after an Iris Xe failed on it a few seconds into every watch, so the Intel(R) Iris(R) Xe Graphics gets H.264"
        );
        // WARP is Microsoft's, so the driver is asked as usual.
        let err = probe(&warp(D3D11_CREATE_DEVICE_FLAG(0)), Codec::Hevc, 1280, 720).unwrap_err();
        assert!(matches!(err, DecodeError::NotDecodable { .. }), "{err:?}");
    }

    #[test]
    fn single_threaded_device_refused() {
        let device = warp(D3D11_CREATE_DEVICE_SINGLETHREADED);
        let err = probe(&device, Codec::Hevc, 1280, 720).unwrap_err();
        let text = err.to_string();
        println!("{text}");
        assert!(matches!(err, DecodeError::NotShareable { .. }), "{err:?}");
        assert!(
            text.ends_with("because it was made single-threaded"),
            "{text}"
        );
    }
}
