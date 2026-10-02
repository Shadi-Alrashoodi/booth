mod ffi;
#[cfg(test)]
mod layout_test;
mod references;
mod session;
pub(crate) mod status;

use windows::Win32::Graphics::Direct3D11::ID3D11Device;

use crate::gpu;
use crate::level::{self, Choice, HevcChoice};
use crate::{
    AccessUnit, Codec, EncodeError, Encoder, Frame, Kind, Preset, Recovery, Request, Settings,
};
use ffi::*;
use references::{Plan, References};
use session::{Params, Picture, Session, codec_guid};

pub(crate) const API_MAJOR: u32 = NVENCAPI_MAJOR_VERSION;
pub(crate) const API_MINOR: u32 = NVENCAPI_MINOR_VERSION;

// Invalidation needs the frame before the lost one still in the encoder's
// memory when the loss report arrives, which is about a round trip plus the
// frame interval the viewer gives an unfinished frame, after the frame was
// encoded. 100 ms of frames covers a 50 ms round trip with room to spare:
// 12 frames at 120 fps, as many as H.264's level 5.2 allows at 2560x1440
// (HEVC's level 6).
const RECOVERY_WINDOW_MS: u32 = 100;

// The top of the upload setting. The level is chosen for it so that raising
// the bitrate mid-stream never outgrows the level.
const MAX_BITRATE: u32 = 80_000_000;

struct Caps {
    intra_refresh: bool,
    single_slice_intra_refresh: bool,
    invalidation: bool,
    bitrate_change: bool,
    // In the codec's own level numbers; 0 when the driver does not say.
    level_max: u32,
}

/// The level Booth declares and the references it keeps.
#[derive(Debug, Clone, Copy)]
enum Level {
    H264(Choice),
    Hevc(HevcChoice),
}

impl Level {
    fn choose(request: &Request, bitrate: u32) -> Level {
        let Request {
            codec,
            width,
            height,
            fps,
        } = *request;
        let references = (fps * RECOVERY_WINDOW_MS).div_ceil(1000).clamp(4, 16);
        match codec {
            Codec::H264 => Level::H264(level::choose(width, height, fps, bitrate, references)),
            Codec::Hevc => Level::Hevc(level::choose_hevc(width, height, fps, bitrate, references)),
        }
    }

    fn references(self) -> u32 {
        match self {
            Level::H264(choice) => choice.references,
            Level::Hevc(choice) => choice.references,
        }
    }

    fn idc(self) -> u32 {
        match self {
            Level::H264(choice) => choice.level,
            Level::Hevc(choice) => choice.level,
        }
    }
}

/// A level as people write it: H.264 counts in tenths (52 is 5.2), HEVC in
/// thirtieths (153 is 5.1).
fn level_name(codec: Codec, idc: u32) -> String {
    let tenths = match codec {
        Codec::H264 => idc,
        Codec::Hevc => idc / 3,
    };
    match tenths % 10 {
        0 => format!("{}", tenths / 10),
        rest => format!("{}.{rest}", tenths / 10),
    }
}

/// What the GPU's encoder can do for this request, or the reason it cannot
/// take it at all.
fn caps(session: &Session, request: &Request) -> Result<Caps, EncodeError> {
    let Request {
        codec,
        width,
        height,
        ..
    } = *request;
    if !session.offers_codec(codec)? {
        return Err(EncodeError::NvencCodecMissing { codec });
    }
    let cap = |cap| session.cap(codec, cap);
    let min = (cap(NV_ENC_CAPS_WIDTH_MIN)?, cap(NV_ENC_CAPS_HEIGHT_MIN)?);
    let max = (cap(NV_ENC_CAPS_WIDTH_MAX)?, cap(NV_ENC_CAPS_HEIGHT_MAX)?);
    let (w, h) = (width as i32, height as i32);
    if w < min.0 || h < min.1 || w > max.0 || h > max.1 {
        return Err(EncodeError::NvencSizeUnsupported {
            codec,
            width,
            height,
            min: (min.0 as u32, min.1 as u32),
            max: (max.0 as u32, max.1 as u32),
        });
    }
    if cap(NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE)? == 0 {
        return Err(EncodeError::NvencMissingFeature {
            feature: "custom VBV buffer size",
        });
    }
    Ok(Caps {
        intra_refresh: cap(NV_ENC_CAPS_SUPPORT_INTRA_REFRESH)? != 0,
        single_slice_intra_refresh: cap(NV_ENC_CAPS_SINGLE_SLICE_INTRA_REFRESH)? != 0,
        invalidation: cap(NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION)? != 0,
        bitrate_change: cap(NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE)? != 0,
        level_max: cap(NV_ENC_CAPS_LEVEL_MAX)?.max(0) as u32,
    })
}

/// Whether NVENC takes the request, asked through a session that encodes
/// nothing; the reason as a sentence when it does not. The caller has
/// checked that some level of the codec holds the size and rate.
pub(crate) fn offers(device: &ID3D11Device, request: &Request) -> Result<(), String> {
    let _opening = gpu::opening();
    let session = Session::open(device).map_err(|e| e.to_string())?;
    let caps = caps(&session, request).map_err(|e| e.to_string())?;
    match past_level_max(request, caps.level_max) {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// Why NVENC cannot take the request when the level Booth would declare is
/// past the top one the driver reports, `level_max` in the codec's own
/// numbers (0 when the driver does not say).
fn past_level_max(request: &Request, level_max: u32) -> Option<String> {
    let Request {
        codec,
        width,
        height,
        fps,
    } = *request;
    let level = Level::choose(request, MAX_BITRATE);
    (level_max != 0 && level.idc() > level_max).then(|| {
        format!(
            "this GPU's NVENC goes up to {codec} level {} and {width}x{height} at {fps} fps with {} references needs {}",
            level_name(codec, level_max),
            level.references(),
            level_name(codec, level.idc())
        )
    })
}

pub(crate) struct Nvenc {
    session: Session,
    params: Box<Params>,
    name: String,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    caps: Caps,
    references: References,
    idr_next: bool,
    last_index: Option<u64>,
    // Frames still to give back before the driver's release of the output
    // is reported as failed: crate::fault's, None outside a test.
    #[cfg(feature = "fault")]
    fail_in: Option<u64>,
}

impl Nvenc {
    pub(crate) fn open(
        device: &ID3D11Device,
        request: &Request,
        settings: &Settings,
    ) -> Result<Nvenc, EncodeError> {
        let Request {
            codec,
            width,
            height,
            fps,
        } = *request;
        let _opening = gpu::opening();
        let mut session = Session::open(device)?;
        let caps = caps(&session, request)?;
        let level = Level::choose(request, settings.bitrate.max(MAX_BITRATE));

        let preset = match settings.preset {
            Preset::P1 => NV_ENC_PRESET_P1_GUID,
            Preset::P2 => NV_ENC_PRESET_P2_GUID,
            Preset::P3 => NV_ENC_PRESET_P3_GUID,
            Preset::P4 => NV_ENC_PRESET_P4_GUID,
        };
        let mut params = Box::new(Params {
            init: zeroed(),
            config: session.preset_config(codec, preset)?,
        });
        configure(&mut params.config, fps, settings.bitrate, level, &caps);

        let init = &mut params.init;
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = codec_guid(codec);
        init.presetGUID = preset;
        init.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        init.encodeWidth = width;
        init.encodeHeight = height;
        init.darWidth = width;
        init.darHeight = height;
        init.maxEncodeWidth = width;
        init.maxEncodeHeight = height;
        init.frameRateNum = fps;
        init.frameRateDen = 1;
        // Synchronous, one frame in flight: locking the bitstream waits for
        // the frame, and nothing is ever queued behind it.
        init.enableEncodeAsync = 0;
        init.enablePTD = 1;
        session.initialize(&mut params)?;

        Ok(Nvenc {
            session,
            params,
            name: format!("NVENC {codec} {:?}", settings.preset),
            codec,
            width,
            height,
            fps,
            references: References::new(level.references() as usize),
            caps,
            idr_next: true,
            last_index: None,
            #[cfg(feature = "fault")]
            fail_in: crate::fault::take_nvenc_failure(),
        })
    }
}

fn configure(config: &mut NV_ENC_CONFIG, fps: u32, bitrate: u32, level: Level, caps: &Caps) {
    config.version = NV_ENC_CONFIG_VER;
    // IDRs come only at the start, on request, for a new size and for
    // recovery, never on a schedule.
    config.gopLength = NVENC_INFINITE_GOPLENGTH;
    // No B frames: a B frame waits for the frame after it, which is pure
    // delay.
    config.frameIntervalP = 1;

    let rc = &mut config.rcParams;
    // CBR so the encoder never bursts.
    rc.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    set_rate(rc, bitrate, fps);
    // Asks for every IDR to be no bigger than any other frame: a big one sits
    // in the router's queue, and the voice and input packets behind it wait
    // as long. NVENC cannot hold it on a detailed picture: the test pattern's
    // IDRs come out at 5.6 to 6 frames' worth at 15 Mbit/s and 2560x1440, at
    // QP 50.
    rc.lowDelayKeyFrameScale = 1;
    // Declares that no frame is ever reordered, so a decoder shows each frame
    // the moment it is decoded instead of holding a few back.
    rc.set_zero_reorder_delay(1);
    // Lookahead holds frames back to plan their bits.
    rc.set_enable_lookahead(0);
    // Every frame stays a reference, which the recovery model in
    // references.rs counts on.
    rc.set_enable_non_ref_p(0);
    // A first pass at quarter resolution, which both P1 and P4 pick on their
    // own; said here because the one-frame VBV depends on it. Measured at
    // 2560x1440 and 15 Mbit/s: with a single pass the rate ran 39 percent
    // over with frames up to 2.7 frames' worth; with this no frame passed 1.1,
    // for 0.7 ms more encode time (1.9 against 2.6 ms on P1).
    rc.multiPass = NV_ENC_TWO_PASS_QUARTER_RESOLUTION;

    // Rolling intra refresh heals damage without an IDR, but NVENC refreshes
    // in bursts: at 2560x1440, 15 Mbit/s and P1 the test pattern came out
    // with a frame of 2 to 2.4 frames' worth every few frames for half of
    // every second, where without it no frame passed 1.1 frames' worth.
    // Reference invalidation recovers a reported loss without that cost, so
    // the refresh is only for a GPU that cannot invalidate. There a sweep
    // starts every second, spread over all but one of its frames (the header
    // wants the count below the period): 119 frames at 120 fps, of which
    // NVENC was seen to use about 63. Placeholder numbers to measure on such
    // a GPU. Single slice, where the GPU can, since otherwise NVENC splits
    // refreshing frames into several slices.
    let refresh = (caps.intra_refresh && !caps.invalidation).then_some(fps.max(2));
    let single_slice_refresh = u32::from(caps.single_slice_intra_refresh);

    match level {
        Level::H264(choice) => {
            // CABAC and 8x8 transforms save bits over Main, and every GPU
            // decoder takes High.
            config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
            let h264 = config.h264();
            h264.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            h264.level = choice.level;
            h264.maxNumRefFrames = choice.references;
            // With every IDR, so a viewer can start from any IDR alone.
            h264.set_repeat_sps_pps(1);
            // Delimiters and filler are bytes on the wire that say nothing.
            h264.set_output_aud(0);
            h264.set_enable_filler_data_insertion(0);
            // No long-term references: the recovery model is a plain sliding
            // window.
            h264.set_enable_ltr(0);
            // One slice per frame. The viewer decodes whole frames only (the
            // parity covers a whole frame), so more slices would cost header
            // bits and prediction across slice edges and buy nothing.
            h264.sliceMode = 0;
            h264.sliceModeData = 0;
            if let Some(period) = refresh {
                h264.set_enable_intra_refresh(1);
                h264.intraRefreshPeriod = period;
                h264.intraRefreshCnt = period - 1;
                h264.set_single_slice_intra_refresh(single_slice_refresh);
            }
            let vui = &mut h264.h264VUIParameters;
            bt709_limited(vui);
            // Writes max_num_reorder_frames = 0 into the stream. Without it a
            // decoder may hold frames back in case some come out of order.
            vui.bitstreamRestrictionFlag = 1;
        }
        Level::Hevc(choice) => {
            // 8-bit 4:2:0, what capture makes and every HEVC decoder takes.
            config.profileGUID = NV_ENC_HEVC_PROFILE_MAIN_GUID;
            let hevc = config.hevc();
            hevc.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            hevc.level = choice.level;
            hevc.tier = if choice.high_tier {
                NV_ENC_TIER_HEVC_HIGH
            } else {
                NV_ENC_TIER_HEVC_MAIN
            };
            hevc.maxNumRefFramesInDPB = choice.references;
            hevc.set_chroma_format_idc(1);
            hevc.inputBitDepth = NV_ENC_BIT_DEPTH_8;
            hevc.outputBitDepth = NV_ENC_BIT_DEPTH_8;
            // The same as for H.264 above: VPS, SPS and PPS with every IDR,
            // no delimiters or filler, no long-term references, one slice.
            hevc.set_repeat_sps_pps(1);
            hevc.set_output_aud(0);
            hevc.set_enable_filler_data_insertion(0);
            hevc.set_enable_ltr(0);
            hevc.sliceMode = 0;
            hevc.sliceModeData = 0;
            if let Some(period) = refresh {
                hevc.set_enable_intra_refresh(1);
                hevc.intraRefreshPeriod = period;
                hevc.intraRefreshCnt = period - 1;
                hevc.set_single_slice_intra_refresh(single_slice_refresh);
            }
            // HEVC says there is no reordering in the SPS itself
            // (sps_max_num_reorder_pics, 0 in every test), so its VUI needs
            // no bitstream restriction for it.
            bt709_limited(&mut hevc.hevcVUIParameters);
        }
    }
}

/// What the capture shader produces: BT.709 primaries, transfer and matrix,
/// limited range.
fn bt709_limited(vui: &mut NV_ENC_CONFIG_H264_VUI_PARAMETERS) {
    vui.videoSignalTypePresentFlag = 1;
    vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui.videoFullRangeFlag = 0;
    vui.colourDescriptionPresentFlag = 1;
    vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    vui.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
}

fn set_rate(rc: &mut NV_ENC_RC_PARAMS, bitrate: u32, fps: u32) {
    rc.averageBitRate = bitrate;
    rc.maxBitRate = bitrate;
    // A VBV of one frame's worth: the encoder may never put more than one
    // frame interval of bits on the wire, so no frame bursts past the next.
    // The price is that every frame's share works as a ceiling rather than an
    // average, so the rate lands under the setting: on the test pattern 15
    // to 20 percent under at 15 Mbit/s and about 24 under at 5. NVENC takes
    // its frame budget from the VBV in this mode; raising averageBitRate by
    // 40 percent, or switching to VBR, changed not one byte of output.
    rc.vbvBufferSize = bitrate / fps;
    rc.vbvInitialDelay = bitrate / fps;
}

impl Encoder for Nvenc {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> Kind {
        Kind::Nvenc
    }

    fn codec(&self) -> Codec {
        self.codec
    }

    fn encode(&mut self, frame: &Frame<'_>) -> Result<AccessUnit, EncodeError> {
        if let Some(previous) = self.last_index
            && frame.index <= previous
        {
            return Err(EncodeError::FrameOutOfOrder {
                index: frame.index,
                previous,
            });
        }

        let picture = Picture {
            width: self.width,
            height: self.height,
            timestamp: frame.index,
            force_idr: self.idr_next || frame.force_idr,
        };
        #[cfg(feature = "fault")]
        match self.fail_in {
            Some(0) => {
                self.fail_in = None;
                self.session.fail_next_unlock = true;
            }
            Some(frames) => self.fail_in = Some(frames - 1),
            None => {}
        }
        let out = match self.session.encode(frame.texture, &picture) {
            Ok(out) => out,
            Err(failed) => {
                // The driver may keep this frame as a reference though the
                // viewer never gets it. A driver failure is rare, so this
                // takes the sure way out, an IDR, rather than invalidating a
                // frame nobody knows made it into the driver's memory.
                if failed.submitted {
                    self.idr_next = true;
                }
                return Err(failed.error);
            }
        };
        let idr = out.picture_type == NV_ENC_PIC_TYPE_IDR;
        self.last_index = Some(frame.index);
        self.references.encoded(frame.index, idr);
        if idr {
            self.idr_next = false;
        }

        Ok(AccessUnit {
            data: out.data,
            index: frame.index,
            idr,
            submitted: out.submitted,
            ready: out.ready,
        })
    }

    fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), EncodeError> {
        if bits_per_second < self.fps {
            return Err(EncodeError::BadRate {
                fps: self.fps,
                bitrate: bits_per_second,
            });
        }
        if !self.caps.bitrate_change {
            return Err(EncodeError::BitrateChangeUnsupported);
        }
        let before = self.params.config.rcParams;
        set_rate(&mut self.params.config.rcParams, bits_per_second, self.fps);
        let result = self.session.reconfigure(&mut self.params);
        if result.is_err() {
            self.params.config.rcParams = before;
        }
        result
    }

    fn invalidates(&self) -> bool {
        self.caps.invalidation
    }

    fn needs_idr(&self, lost_frame_index: u64) -> bool {
        self.idr_next
            || !self.caps.invalidation
            || self.references.plan(lost_frame_index) == Plan::Idr
    }

    fn recover(&mut self, lost_frame_index: u64) -> Recovery {
        if self.needs_idr(lost_frame_index) {
            self.idr_next = true;
            return Recovery::Idr;
        }
        if let Plan::Invalidate(frames) = self.references.plan(lost_frame_index) {
            for &frame in &frames {
                if self.session.invalidate(frame).is_err() {
                    self.idr_next = true;
                    return Recovery::Idr;
                }
            }
            self.references.invalidated(&frames);
        }
        Recovery::Invalidated
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice,
        ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};
    use windows::core::Interface;

    use super::*;

    fn nvidia_device() -> Option<ID3D11Device> {
        // SAFETY: plain DXGI and Direct3D calls; EnumAdapters1 fails past the
        // last adapter and every out pointer is valid for its call.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let adapter = (0..)
                .map_while(|i| factory.EnumAdapters1(i).ok())
                .find(|a| a.GetDesc1().is_ok_and(|d| d.VendorId == gpu::NVIDIA))?;
            let adapter: IDXGIAdapter = adapter.cast().ok()?;
            let mut device = None;
            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .ok()?;
            device
        }
    }

    // Blank, which is all this test needs: it looks at frame types only.
    fn nv12(device: &ID3D11Device, width: u32, height: u32) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: a valid description and out pointer.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.expect("NV12 texture");
        texture.expect("NV12 texture")
    }

    fn frame(texture: &ID3D11Texture2D, index: u64) -> Frame<'_> {
        Frame {
            texture,
            index,
            force_idr: false,
        }
    }

    #[test]
    fn driver_loss_forces_idr() {
        let _turn = gpu::test_turn();
        let Some(device) = nvidia_device() else {
            println!("skipped: no NVIDIA GPU on this PC, so there is no NVENC to test");
            return;
        };
        for codec in Codec::ALL {
            lost_inside_the_driver(&device, codec);
        }
    }

    fn lost_inside_the_driver(device: &ID3D11Device, codec: Codec) {
        let texture = nv12(device, 1280, 720);
        let wrong_size = nv12(device, 640, 360);
        let request = Request {
            codec,
            width: 1280,
            height: 720,
            fps: 60,
        };
        let mut nvenc = Nvenc::open(device, &request, &Settings::default())
            .unwrap_or_else(|e| panic!("{codec}: {e}"));
        for i in 0..5 {
            let unit = nvenc
                .encode(&frame(&texture, i))
                .unwrap_or_else(|e| panic!("frame {i}: {e}"));
            assert_eq!(unit.idr, i == 0, "frame {i}");
        }

        // Refused before the driver saw it, so there is nothing to cut off.
        assert!(nvenc.encode(&frame(&wrong_size, 5)).is_err());
        let unit = nvenc
            .encode(&frame(&texture, 5))
            .unwrap_or_else(|e| panic!("frame 5: {e}"));
        assert!(!unit.idr, "an IDR after a frame the driver never saw");

        // Encoded, kept as a reference, then lost on the way out.
        nvenc.session.fail_next_unlock = true;
        let Err(error) = nvenc.encode(&frame(&texture, 6)) else {
            panic!("the failed unlock went unnoticed");
        };
        println!("{error}");
        assert!(nvenc.needs_idr(5), "an IDR is due, whatever was lost");
        let unit = nvenc
            .encode(&frame(&texture, 7))
            .unwrap_or_else(|e| panic!("frame 7: {e}"));
        assert!(
            unit.idr,
            "{codec}: frame 7 is not an IDR, so it may predict from frame 6, which the viewer never got"
        );
    }

    // NVENC's level numbers, from nvEncodeAPI.h.
    const HEVC_LEVELS: [u32; 13] = [
        NV_ENC_LEVEL_HEVC_1,
        NV_ENC_LEVEL_HEVC_2,
        NV_ENC_LEVEL_HEVC_21,
        NV_ENC_LEVEL_HEVC_3,
        NV_ENC_LEVEL_HEVC_31,
        NV_ENC_LEVEL_HEVC_4,
        NV_ENC_LEVEL_HEVC_41,
        NV_ENC_LEVEL_HEVC_5,
        NV_ENC_LEVEL_HEVC_51,
        NV_ENC_LEVEL_HEVC_52,
        NV_ENC_LEVEL_HEVC_6,
        NV_ENC_LEVEL_HEVC_61,
        NV_ENC_LEVEL_HEVC_62,
    ];
    const H264_LEVELS: [u32; 20] = [
        9, 10, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52, 60, 61, 62,
    ];

    #[test]
    fn hevc_levels_are_nvencs() {
        for idc in level::hevc_level_idcs() {
            assert!(
                HEVC_LEVELS.contains(&idc),
                "level_idc {idc} is not one of NVENC's"
            );
        }
        assert_eq!(level_name(Codec::Hevc, NV_ENC_LEVEL_HEVC_6), "6");
        assert_eq!(level_name(Codec::Hevc, NV_ENC_LEVEL_HEVC_51), "5.1");
        assert_eq!(level_name(Codec::H264, 52), "5.2");
        assert_eq!(level_name(Codec::H264, 40), "4");
    }

    fn request(codec: Codec, width: u32, height: u32, fps: u32) -> Request {
        Request {
            codec,
            width,
            height,
            fps,
        }
    }

    #[test]
    fn level_max_too_low() {
        // 1440p120 keeps 12 references, which takes HEVC level 6 and H.264
        // level 5.2.
        let hevc = request(Codec::Hevc, 2560, 1440, 120);
        assert_eq!(
            past_level_max(&hevc, NV_ENC_LEVEL_HEVC_51).as_deref(),
            Some(
                "this GPU's NVENC goes up to HEVC level 5.1 and 2560x1440 at 120 fps with 12 references needs 6"
            )
        );
        assert_eq!(past_level_max(&hevc, NV_ENC_LEVEL_HEVC_6), None);
        assert_eq!(past_level_max(&hevc, 0), None);
        let h264 = request(Codec::H264, 2560, 1440, 120);
        assert!(past_level_max(&h264, 51).is_some());
        assert_eq!(past_level_max(&h264, 52), None);
    }

    #[test]
    fn level_max_in_codec_numbers() {
        let _turn = gpu::test_turn();
        let Some(device) = nvidia_device() else {
            println!("skipped: no NVIDIA GPU on this PC, so there is no NVENC to ask");
            return;
        };
        let session = Session::open(&device).unwrap_or_else(|e| panic!("{e}"));
        for codec in Codec::ALL {
            let caps = caps(&session, &request(codec, 1280, 720, 60))
                .unwrap_or_else(|e| panic!("{codec}: {e}"));
            println!("{codec}: LEVEL_MAX {}", caps.level_max);
            // H.264 counts in tenths and HEVC in thirtieths, so a driver
            // that answered HEVC in H.264's numbers, or the other way round,
            // would make offer() refuse or pass the wrong requests.
            let own: &[u32] = match codec {
                Codec::H264 => &H264_LEVELS,
                Codec::Hevc => &HEVC_LEVELS,
            };
            assert!(
                caps.level_max == 0 || own.contains(&caps.level_max),
                "{codec}: NVENC says its top level is {}, which is not one of {codec}'s",
                caps.level_max
            );
        }
    }
}
