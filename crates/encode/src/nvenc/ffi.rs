// The parts of nvEncodeAPI.h that Booth uses, copied by hand from
// nv-codec-headers n12.2.72.0 (NVENC API 12.2). That header carries this
// notice, which applies to the copied declarations:
//
// Copyright (c) 2010-2024 NVIDIA Corporation
//
// Permission is hereby granted, free of charge, to any person
// obtaining a copy of this software and associated documentation
// files (the "Software"), to deal in the Software without
// restriction, including without limitation the rights to use,
// copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the
// software is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES
// OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT
// HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
// OTHER DEALINGS IN THE SOFTWARE.
//
// Names are the header's, so every line can be held against it. Every
// struct keeps all of its fields in C order, used or not, because the
// driver reads the whole thing. Unions carry the members Booth never uses
// as opaque blocks of the right size, so the union has its true size.
// layout.c exports the header's own view of all of this and layout_test.rs
// compares it field by field.

#![allow(
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::upper_case_acronyms
)]

use std::ffi::{c_char, c_void};

pub(crate) use windows::core::GUID;

// C enums are 32-bit ints on MSVC. They stay plain integers here: a driver
// newer than this header may hand back a value the header does not name,
// and a Rust enum holding that would be undefined behaviour.
pub(crate) type NVENCSTATUS = u32;
pub(crate) type NV_ENC_CAPS = u32;
pub(crate) type NV_ENC_TUNING_INFO = u32;
pub(crate) type NV_ENC_INPUT_PTR = *mut c_void;
pub(crate) type NV_ENC_OUTPUT_PTR = *mut c_void;
pub(crate) type NV_ENC_REGISTERED_PTR = *mut c_void;

const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

// Structs that the header marks with the top bit in their version.
const fn struct_version_high(ver: u32) -> u32 {
    struct_version(ver) | (1 << 31)
}

macro_rules! consts {
    ($($name:ident = $value:expr;)*) => {
        $(pub(crate) const $name: u32 = $value;)*

        #[cfg(test)]
        pub(crate) const CONSTS: &[(&str, u32)] = &[$((stringify!($name), $name),)*];
    };
}

consts! {
    NVENCAPI_MAJOR_VERSION = 12;
    NVENCAPI_MINOR_VERSION = 2;
    NVENCAPI_VERSION = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);
    NVENC_INFINITE_GOPLENGTH = 0xffff_ffff;

    NV_ENC_CAPS_PARAM_VER = struct_version(1);
    NV_ENC_CREATE_BITSTREAM_BUFFER_VER = struct_version(1);
    NV_ENC_CONFIG_VER = struct_version_high(9);
    NV_ENC_INITIALIZE_PARAMS_VER = struct_version_high(7);
    NV_ENC_RECONFIGURE_PARAMS_VER = struct_version_high(2);
    NV_ENC_PRESET_CONFIG_VER = struct_version_high(5);
    NV_ENC_PIC_PARAMS_VER = struct_version_high(7);
    NV_ENC_LOCK_BITSTREAM_VER = struct_version_high(2);
    NV_ENC_MAP_INPUT_RESOURCE_VER = struct_version(4);
    NV_ENC_REGISTER_RESOURCE_VER = struct_version(5);
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER = struct_version(1);
    NV_ENCODE_API_FUNCTION_LIST_VER = struct_version(2);

    NV_ENC_CAPS_LEVEL_MAX = 13;
    NV_ENC_CAPS_WIDTH_MAX = 16;
    NV_ENC_CAPS_HEIGHT_MAX = 17;
    NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE = 20;
    NV_ENC_CAPS_SUPPORT_INTRA_REFRESH = 25;
    NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE = 26;
    NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION = 28;
    NV_ENC_CAPS_WIDTH_MIN = 45;
    NV_ENC_CAPS_HEIGHT_MIN = 46;
    NV_ENC_CAPS_SINGLE_SLICE_INTRA_REFRESH = 50;

    NV_ENC_PARAMS_RC_CBR = 2;
    NV_ENC_TWO_PASS_QUARTER_RESOLUTION = 1;
    NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY = 3;

    NV_ENC_PIC_FLAG_FORCEIDR = 0x2;
    NV_ENC_PIC_FLAG_OUTPUT_SPSPPS = 0x4;
    NV_ENC_PIC_FLAG_EOS = 0x8;
    NV_ENC_PIC_STRUCT_FRAME = 1;
    NV_ENC_PIC_TYPE_IDR = 3;

    NV_ENC_BUFFER_FORMAT_NV12 = 1;
    NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX = 0;
    NV_ENC_INPUT_IMAGE = 0;
    NV_ENC_DEVICE_TYPE_DIRECTX = 0;

    // NVENC's HEVC levels are general_level_idc itself, 30 times the level,
    // which level.rs writes straight in.
    NV_ENC_LEVEL_HEVC_1 = 30;
    NV_ENC_LEVEL_HEVC_2 = 60;
    NV_ENC_LEVEL_HEVC_21 = 63;
    NV_ENC_LEVEL_HEVC_3 = 90;
    NV_ENC_LEVEL_HEVC_31 = 93;
    NV_ENC_LEVEL_HEVC_4 = 120;
    NV_ENC_LEVEL_HEVC_41 = 123;
    NV_ENC_LEVEL_HEVC_5 = 150;
    NV_ENC_LEVEL_HEVC_51 = 153;
    NV_ENC_LEVEL_HEVC_52 = 156;
    NV_ENC_LEVEL_HEVC_6 = 180;
    NV_ENC_LEVEL_HEVC_61 = 183;
    NV_ENC_LEVEL_HEVC_62 = 186;
    NV_ENC_TIER_HEVC_MAIN = 0;
    NV_ENC_TIER_HEVC_HIGH = 1;
    NV_ENC_BIT_DEPTH_8 = 8;

    NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED = 5;
    NV_ENC_VUI_COLOR_PRIMARIES_BT709 = 1;
    NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709 = 1;
    NV_ENC_VUI_MATRIX_COEFFS_BT709 = 1;

    NV_ENC_SUCCESS = 0;
    NV_ENC_ERR_NO_ENCODE_DEVICE = 1;
    NV_ENC_ERR_UNSUPPORTED_DEVICE = 2;
    NV_ENC_ERR_INVALID_ENCODERDEVICE = 3;
    NV_ENC_ERR_INVALID_DEVICE = 4;
    NV_ENC_ERR_DEVICE_NOT_EXIST = 5;
    NV_ENC_ERR_INVALID_PTR = 6;
    NV_ENC_ERR_INVALID_EVENT = 7;
    NV_ENC_ERR_INVALID_PARAM = 8;
    NV_ENC_ERR_INVALID_CALL = 9;
    NV_ENC_ERR_OUT_OF_MEMORY = 10;
    NV_ENC_ERR_ENCODER_NOT_INITIALIZED = 11;
    NV_ENC_ERR_UNSUPPORTED_PARAM = 12;
    NV_ENC_ERR_LOCK_BUSY = 13;
    NV_ENC_ERR_NOT_ENOUGH_BUFFER = 14;
    NV_ENC_ERR_INVALID_VERSION = 15;
    NV_ENC_ERR_MAP_FAILED = 16;
    NV_ENC_ERR_NEED_MORE_INPUT = 17;
    NV_ENC_ERR_ENCODER_BUSY = 18;
    NV_ENC_ERR_EVENT_NOT_REGISTERD = 19;
    NV_ENC_ERR_GENERIC = 20;
    NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY = 21;
    NV_ENC_ERR_UNIMPLEMENTED = 22;
    NV_ENC_ERR_RESOURCE_REGISTER_FAILED = 23;
    NV_ENC_ERR_RESOURCE_NOT_REGISTERED = 24;
    NV_ENC_ERR_RESOURCE_NOT_MAPPED = 25;
    NV_ENC_ERR_NEED_MORE_OUTPUT = 26;
}

macro_rules! guids {
    ($($name:ident = $value:literal;)*) => {
        $(pub(crate) const $name: GUID = GUID::from_u128($value);)*

        #[cfg(test)]
        pub(crate) const GUIDS: &[(&str, GUID)] = &[$((stringify!($name), $name),)*];
    };
}

guids! {
    NV_ENC_CODEC_H264_GUID = 0x6bc82762_4e63_4ca4_aa85_1e50f321f6bf;
    NV_ENC_CODEC_HEVC_GUID = 0x790cdc88_4522_4d7b_9425_bda9975f7603;
    NV_ENC_H264_PROFILE_HIGH_GUID = 0xe7cbc309_4f7a_4b89_af2a_d537c92be310;
    NV_ENC_HEVC_PROFILE_MAIN_GUID = 0xb514c39a_b55b_40fa_878f_f1253b4dfdec;
    NV_ENC_PRESET_P1_GUID = 0xfc0a8d3e_45f8_4cf8_80c7_298871590ebf;
    NV_ENC_PRESET_P2_GUID = 0xf581cfb8_88d6_4381_93f0_df13f9c27dab;
    NV_ENC_PRESET_P3_GUID = 0x36850110_3a07_441f_94d5_3670631f91f6;
    NV_ENC_PRESET_P4_GUID = 0x90a7b826_df06_4862_b9d2_cd6d73a08681;
}

/// A C type Booth never reads or writes, standing in for a union member so
/// the union gets the size and alignment the header gives it.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub(crate) struct Opaque<const N: usize>([u8; N]);

/// Types in this file that are valid as all-zero bytes, which is how the
/// driver expects every reserved field.
///
/// # Safety
///
/// Only for types whose fields are integers, raw pointers, `Option`s of
/// function pointers, GUIDs, `Opaque`s, arrays of those, or other types from
/// this file. `ffi_types!` is the only place that implements it.
pub(crate) unsafe trait Zeroable: Sized {}

pub(crate) fn zeroed<T: Zeroable>() -> T {
    // SAFETY: Zeroable is only implemented for types for which all-zero bytes
    // are a valid value (see the trait).
    unsafe { std::mem::zeroed() }
}

#[cfg(test)]
pub(crate) struct TypeLayout {
    pub(crate) name: &'static str,
    pub(crate) size: usize,
    pub(crate) align: usize,
    pub(crate) fields: &'static [(&'static str, usize)],
}

// Declares every C struct and union in one list, so the layout test sees
// all of them: a type cannot be added here without being checked.
macro_rules! ffi_types {
    (@next [$($done:tt)*]) => {
        #[cfg(test)]
        pub(crate) const LAYOUT: &[TypeLayout] = &[$($done)*];
    };
    (@next [$($done:tt)*] struct $name:ident { $($field:ident: $ty:ty,)* } $($rest:tt)*) => {
        #[repr(C)]
        #[derive(Clone, Copy)]
        pub(crate) struct $name {
            $(pub(crate) $field: $ty,)*
        }
        // SAFETY: the fields follow the rule on Zeroable.
        unsafe impl Zeroable for $name {}
        ffi_types!(@next [$($done)* TypeLayout {
            name: stringify!($name),
            size: size_of::<$name>(),
            align: align_of::<$name>(),
            fields: &[$((stringify!($field), std::mem::offset_of!($name, $field)),)*],
        },] $($rest)*);
    };
    (@next [$($done:tt)*] union $name:ident { $($field:ident: $ty:ty,)* } $($rest:tt)*) => {
        #[repr(C)]
        #[derive(Clone, Copy)]
        pub(crate) union $name {
            $(pub(crate) $field: $ty,)*
        }
        // SAFETY: the members follow the rule on Zeroable.
        unsafe impl Zeroable for $name {}
        ffi_types!(@next [$($done)* TypeLayout {
            name: stringify!($name),
            size: size_of::<$name>(),
            align: align_of::<$name>(),
            fields: &[$((stringify!($field), std::mem::offset_of!($name, $field)),)*],
        },] $($rest)*);
    };
    (@next [$($done:tt)*] opaque $name:ident = $size:literal; $($rest:tt)*) => {
        pub(crate) type $name = Opaque<$size>;
        ffi_types!(@next [$($done)* TypeLayout {
            name: stringify!($name),
            size: size_of::<$name>(),
            align: align_of::<$name>(),
            fields: &[],
        },] $($rest)*);
    };
    ($($body:tt)*) => {
        ffi_types!(@next [] $($body)*);
    };
}

// SAFETY: Opaque is a byte array.
unsafe impl<const N: usize> Zeroable for Opaque<N> {}

// The header's PNVENC* typedefs are nullable function pointers. A Rust fn
// pointer cannot be null, so the function list holds each as an Option.
pub(crate) type PNVENCOPENENCODESESSIONEX = unsafe extern "system" fn(
    *mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
    *mut *mut c_void,
) -> NVENCSTATUS;
pub(crate) type PNVENCGETENCODEGUIDCOUNT =
    unsafe extern "system" fn(*mut c_void, *mut u32) -> NVENCSTATUS;
pub(crate) type PNVENCGETENCODEGUIDS =
    unsafe extern "system" fn(*mut c_void, *mut GUID, u32, *mut u32) -> NVENCSTATUS;
pub(crate) type PNVENCGETENCODECAPS =
    unsafe extern "system" fn(*mut c_void, GUID, *mut NV_ENC_CAPS_PARAM, *mut i32) -> NVENCSTATUS;
pub(crate) type PNVENCGETENCODEPRESETCONFIGEX = unsafe extern "system" fn(
    *mut c_void,
    GUID,
    GUID,
    NV_ENC_TUNING_INFO,
    *mut NV_ENC_PRESET_CONFIG,
) -> NVENCSTATUS;
pub(crate) type PNVENCINITIALIZEENCODER =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_INITIALIZE_PARAMS) -> NVENCSTATUS;
pub(crate) type PNVENCCREATEBITSTREAMBUFFER =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_CREATE_BITSTREAM_BUFFER) -> NVENCSTATUS;
pub(crate) type PNVENCDESTROYBITSTREAMBUFFER =
    unsafe extern "system" fn(*mut c_void, NV_ENC_OUTPUT_PTR) -> NVENCSTATUS;
pub(crate) type PNVENCENCODEPICTURE =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_PIC_PARAMS) -> NVENCSTATUS;
pub(crate) type PNVENCLOCKBITSTREAM =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_LOCK_BITSTREAM) -> NVENCSTATUS;
pub(crate) type PNVENCUNLOCKBITSTREAM =
    unsafe extern "system" fn(*mut c_void, NV_ENC_OUTPUT_PTR) -> NVENCSTATUS;
pub(crate) type PNVENCMAPINPUTRESOURCE =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_MAP_INPUT_RESOURCE) -> NVENCSTATUS;
pub(crate) type PNVENCUNMAPINPUTRESOURCE =
    unsafe extern "system" fn(*mut c_void, NV_ENC_INPUT_PTR) -> NVENCSTATUS;
pub(crate) type PNVENCDESTROYENCODER = unsafe extern "system" fn(*mut c_void) -> NVENCSTATUS;
pub(crate) type PNVENCINVALIDATEREFFRAMES =
    unsafe extern "system" fn(*mut c_void, u64) -> NVENCSTATUS;
pub(crate) type PNVENCREGISTERRESOURCE =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_REGISTER_RESOURCE) -> NVENCSTATUS;
pub(crate) type PNVENCUNREGISTERRESOURCE =
    unsafe extern "system" fn(*mut c_void, NV_ENC_REGISTERED_PTR) -> NVENCSTATUS;
pub(crate) type PNVENCRECONFIGUREENCODER =
    unsafe extern "system" fn(*mut c_void, *mut NV_ENC_RECONFIGURE_PARAMS) -> NVENCSTATUS;
pub(crate) type PNVENCGETLASTERROR = unsafe extern "system" fn(*mut c_void) -> *const c_char;

/// The two functions nvEncodeAPI64.dll exports by name.
pub(crate) type NvEncodeAPIGetMaxSupportedVersion =
    unsafe extern "system" fn(version: *mut u32) -> NVENCSTATUS;
pub(crate) type NvEncodeAPICreateInstance =
    unsafe extern "system" fn(function_list: *mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;

// Slots of the function list that Booth never calls are plain pointers:
// same size, and nothing can call them by mistake.
type Unused = *mut c_void;

// A typedef of the H.264 one in the header, so it has no layout of its own
// to check.
pub(crate) type NV_ENC_CONFIG_HEVC_VUI_PARAMETERS = NV_ENC_CONFIG_H264_VUI_PARAMETERS;

ffi_types! {
    struct NV_ENC_CAPS_PARAM {
        version: u32,
        capsToQuery: NV_ENC_CAPS,
        reserved: [u32; 62],
    }

    struct NV_ENC_CREATE_BITSTREAM_BUFFER {
        version: u32,
        size: u32,
        memoryHeap: u32,
        reserved: u32,
        bitstreamBuffer: NV_ENC_OUTPUT_PTR,
        bitstreamBufferPtr: *mut c_void,
        reserved1: [u32; 58],
        reserved2: [*mut c_void; 64],
    }

    struct NV_ENC_QP {
        qpInterP: u32,
        qpInterB: u32,
        qpIntra: u32,
    }

    struct NV_ENC_RC_PARAMS {
        version: u32,
        rateControlMode: u32,
        constQP: NV_ENC_QP,
        averageBitRate: u32,
        maxBitRate: u32,
        vbvBufferSize: u32,
        vbvInitialDelay: u32,
        bitfields: u32,
        minQP: NV_ENC_QP,
        maxQP: NV_ENC_QP,
        initialRCQP: NV_ENC_QP,
        temporallayerIdxMask: u32,
        temporalLayerQP: [u8; 8],
        targetQuality: u8,
        targetQualityLSB: u8,
        lookaheadDepth: u16,
        lowDelayKeyFrameScale: u8,
        yDcQPIndexOffset: i8,
        uDcQPIndexOffset: i8,
        vDcQPIndexOffset: i8,
        qpMapMode: u32,
        multiPass: u32,
        alphaLayerBitrateRatio: u32,
        cbQPIndexOffset: i8,
        crQPIndexOffset: i8,
        reserved2: u16,
        lookaheadLevel: u32,
        reserved: [u32; 3],
    }

    struct NV_ENC_CLOCK_TIMESTAMP_SET {
        bitfields: u32,
        timeOffset: u32,
    }

    struct NV_ENC_TIME_CODE {
        displayPicStruct: u32,
        clockTimestamp: [NV_ENC_CLOCK_TIMESTAMP_SET; 3],
        skipClockTimestampInsertion: u32,
    }

    struct NV_ENC_CONFIG_H264_VUI_PARAMETERS {
        overscanInfoPresentFlag: u32,
        overscanInfo: u32,
        videoSignalTypePresentFlag: u32,
        videoFormat: u32,
        videoFullRangeFlag: u32,
        colourDescriptionPresentFlag: u32,
        colourPrimaries: u32,
        transferCharacteristics: u32,
        colourMatrix: u32,
        chromaSampleLocationFlag: u32,
        chromaSampleLocationTop: u32,
        chromaSampleLocationBot: u32,
        bitstreamRestrictionFlag: u32,
        timingInfoPresentFlag: u32,
        numUnitInTicks: u32,
        timeScale: u32,
        reserved: [u32; 12],
    }

    struct NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE {
        bitfields: u32,
        reserved1: [u32; 3],
    }

    struct NV_ENC_CONFIG_H264 {
        bitfields: u32,
        level: u32,
        idrPeriod: u32,
        separateColourPlaneFlag: u32,
        disableDeblockingFilterIDC: u32,
        numTemporalLayers: u32,
        spsId: u32,
        ppsId: u32,
        adaptiveTransformMode: u32,
        fmoMode: u32,
        bdirectMode: u32,
        entropyCodingMode: u32,
        stereoMode: u32,
        intraRefreshPeriod: u32,
        intraRefreshCnt: u32,
        maxNumRefFrames: u32,
        sliceMode: u32,
        sliceModeData: u32,
        h264VUIParameters: NV_ENC_CONFIG_H264_VUI_PARAMETERS,
        ltrNumFrames: u32,
        ltrTrustMode: u32,
        chromaFormatIDC: u32,
        maxTemporalLayers: u32,
        useBFramesAsRef: u32,
        numRefL0: u32,
        numRefL1: u32,
        outputBitDepth: u32,
        inputBitDepth: u32,
        reserved1: [u32; 265],
        reserved2: [*mut c_void; 64],
    }

    struct NV_ENC_CONFIG_HEVC {
        level: u32,
        tier: u32,
        minCUSize: u32,
        maxCUSize: u32,
        bitfields: u32,
        idrPeriod: u32,
        intraRefreshPeriod: u32,
        intraRefreshCnt: u32,
        maxNumRefFramesInDPB: u32,
        ltrNumFrames: u32,
        vpsId: u32,
        spsId: u32,
        ppsId: u32,
        sliceMode: u32,
        sliceModeData: u32,
        maxTemporalLayersMinus1: u32,
        hevcVUIParameters: NV_ENC_CONFIG_HEVC_VUI_PARAMETERS,
        ltrTrustMode: u32,
        useBFramesAsRef: u32,
        numRefL0: u32,
        numRefL1: u32,
        tfLevel: u32,
        disableDeblockingFilterIDC: u32,
        outputBitDepth: u32,
        inputBitDepth: u32,
        reserved1: [u32; 210],
        reserved2: [*mut c_void; 64],
    }

    opaque NV_ENC_CONFIG_AV1 = 1552;
    opaque NV_ENC_CONFIG_H264_MEONLY = 1536;
    opaque NV_ENC_CONFIG_HEVC_MEONLY = 1536;

    union NV_ENC_CODEC_CONFIG {
        h264Config: NV_ENC_CONFIG_H264,
        hevcConfig: NV_ENC_CONFIG_HEVC,
        av1Config: NV_ENC_CONFIG_AV1,
        h264MeOnlyConfig: NV_ENC_CONFIG_H264_MEONLY,
        hevcMeOnlyConfig: NV_ENC_CONFIG_HEVC_MEONLY,
        reserved: [u32; 320],
    }

    struct NV_ENC_CONFIG {
        version: u32,
        profileGUID: GUID,
        gopLength: u32,
        frameIntervalP: i32,
        monoChromeEncoding: u32,
        frameFieldMode: u32,
        mvPrecision: u32,
        rcParams: NV_ENC_RC_PARAMS,
        encodeCodecConfig: NV_ENC_CODEC_CONFIG,
        reserved: [u32; 278],
        reserved2: [*mut c_void; 64],
    }

    struct NV_ENC_INITIALIZE_PARAMS {
        version: u32,
        encodeGUID: GUID,
        presetGUID: GUID,
        encodeWidth: u32,
        encodeHeight: u32,
        darWidth: u32,
        darHeight: u32,
        frameRateNum: u32,
        frameRateDen: u32,
        enableEncodeAsync: u32,
        enablePTD: u32,
        bitfields: u32,
        privDataSize: u32,
        reserved: u32,
        privData: *mut c_void,
        encodeConfig: *mut NV_ENC_CONFIG,
        maxEncodeWidth: u32,
        maxEncodeHeight: u32,
        maxMEHintCountsPerBlock: [NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE; 2],
        tuningInfo: NV_ENC_TUNING_INFO,
        bufferFormat: u32,
        numStateBuffers: u32,
        outputStatsLevel: u32,
        reserved1: [u32; 284],
        reserved2: [*mut c_void; 64],
    }

    struct NV_ENC_RECONFIGURE_PARAMS {
        version: u32,
        reserved: u32,
        reInitEncodeParams: NV_ENC_INITIALIZE_PARAMS,
        bitfields: u32,
        reserved2: u32,
    }

    struct NV_ENC_PRESET_CONFIG {
        version: u32,
        reserved: u32,
        presetCfg: NV_ENC_CONFIG,
        reserved1: [u32; 256],
        reserved2: [*mut c_void; 64],
    }

    opaque NV_ENC_PIC_PARAMS_MVC = 128;

    union NV_ENC_PIC_PARAMS_H264_EXT {
        mvcPicParams: NV_ENC_PIC_PARAMS_MVC,
        reserved1: [u32; 32],
    }

    struct NV_ENC_PIC_PARAMS_H264 {
        displayPOCSyntax: u32,
        reserved3: u32,
        refPicFlag: u32,
        colourPlaneId: u32,
        forceIntraRefreshWithFrameCnt: u32,
        bitfields: u32,
        sliceTypeData: *mut u8,
        sliceTypeArrayCnt: u32,
        seiPayloadArrayCnt: u32,
        seiPayloadArray: *mut c_void,
        sliceMode: u32,
        sliceModeData: u32,
        ltrMarkFrameIdx: u32,
        ltrUseFrameBitmap: u32,
        ltrUsageMode: u32,
        forceIntraSliceCount: u32,
        forceIntraSliceIdx: *mut u32,
        h264ExtPicParams: NV_ENC_PIC_PARAMS_H264_EXT,
        timeCode: NV_ENC_TIME_CODE,
        reserved: [u32; 202],
        reserved2: [*mut c_void; 61],
    }

    opaque NV_ENC_PIC_PARAMS_HEVC = 1536;
    opaque NV_ENC_PIC_PARAMS_AV1 = 1544;

    union NV_ENC_CODEC_PIC_PARAMS {
        h264PicParams: NV_ENC_PIC_PARAMS_H264,
        hevcPicParams: NV_ENC_PIC_PARAMS_HEVC,
        av1PicParams: NV_ENC_PIC_PARAMS_AV1,
        reserved: [u32; 256],
    }

    struct NV_ENC_PIC_PARAMS {
        version: u32,
        inputWidth: u32,
        inputHeight: u32,
        inputPitch: u32,
        encodePicFlags: u32,
        frameIdx: u32,
        inputTimeStamp: u64,
        inputDuration: u64,
        inputBuffer: NV_ENC_INPUT_PTR,
        outputBitstream: NV_ENC_OUTPUT_PTR,
        completionEvent: *mut c_void,
        bufferFmt: u32,
        pictureStruct: u32,
        pictureType: u32,
        codecPicParams: NV_ENC_CODEC_PIC_PARAMS,
        meHintCountsPerBlock: [NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE; 2],
        meExternalHints: *mut c_void,
        reserved2: [u32; 7],
        reserved5: [*mut c_void; 2],
        qpDeltaMap: *mut i8,
        qpDeltaMapSize: u32,
        reservedBitFields: u32,
        meHintRefPicDist: [u16; 2],
        reserved4: u32,
        alphaBuffer: NV_ENC_INPUT_PTR,
        meExternalSbHints: *mut c_void,
        meSbHintsCount: u32,
        stateBufferIdx: u32,
        outputReconBuffer: NV_ENC_OUTPUT_PTR,
        reserved3: [u32; 284],
        reserved6: [*mut c_void; 57],
    }

    struct NV_ENC_LOCK_BITSTREAM {
        version: u32,
        bitfields: u32,
        outputBitstream: *mut c_void,
        sliceOffsets: *mut u32,
        frameIdx: u32,
        hwEncodeStatus: u32,
        numSlices: u32,
        bitstreamSizeInBytes: u32,
        outputTimeStamp: u64,
        outputDuration: u64,
        bitstreamBufferPtr: *mut c_void,
        pictureType: u32,
        pictureStruct: u32,
        frameAvgQP: u32,
        frameSatd: u32,
        ltrFrameIdx: u32,
        ltrFrameBitmap: u32,
        temporalId: u32,
        intraMBCount: u32,
        interMBCount: u32,
        averageMVX: i32,
        averageMVY: i32,
        alphaLayerSizeInBytes: u32,
        outputStatsPtrSize: u32,
        reserved: u32,
        outputStatsPtr: *mut c_void,
        frameIdxDisplay: u32,
        reserved1: [u32; 219],
        reserved2: [*mut c_void; 63],
        reservedInternal: [u32; 8],
    }

    struct NV_ENC_MAP_INPUT_RESOURCE {
        version: u32,
        subResourceIndex: u32,
        inputResource: *mut c_void,
        registeredResource: NV_ENC_REGISTERED_PTR,
        mappedResource: NV_ENC_INPUT_PTR,
        mappedBufferFmt: u32,
        reserved1: [u32; 251],
        reserved2: [*mut c_void; 63],
    }

    struct NV_ENC_REGISTER_RESOURCE {
        version: u32,
        resourceType: u32,
        width: u32,
        height: u32,
        pitch: u32,
        subResourceIndex: u32,
        resourceToRegister: *mut c_void,
        registeredResource: NV_ENC_REGISTERED_PTR,
        bufferFormat: u32,
        bufferUsage: u32,
        pInputFencePoint: *mut c_void,
        chromaOffset: [u32; 2],
        reserved1: [u32; 246],
        reserved2: [*mut c_void; 61],
    }

    struct NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
        version: u32,
        deviceType: u32,
        device: *mut c_void,
        reserved: *mut c_void,
        apiVersion: u32,
        reserved1: [u32; 253],
        reserved2: [*mut c_void; 64],
    }

    struct NV_ENCODE_API_FUNCTION_LIST {
        version: u32,
        reserved: u32,
        nvEncOpenEncodeSession: Unused,
        nvEncGetEncodeGUIDCount: Option<PNVENCGETENCODEGUIDCOUNT>,
        nvEncGetEncodeProfileGUIDCount: Unused,
        nvEncGetEncodeProfileGUIDs: Unused,
        nvEncGetEncodeGUIDs: Option<PNVENCGETENCODEGUIDS>,
        nvEncGetInputFormatCount: Unused,
        nvEncGetInputFormats: Unused,
        nvEncGetEncodeCaps: Option<PNVENCGETENCODECAPS>,
        nvEncGetEncodePresetCount: Unused,
        nvEncGetEncodePresetGUIDs: Unused,
        nvEncGetEncodePresetConfig: Unused,
        nvEncInitializeEncoder: Option<PNVENCINITIALIZEENCODER>,
        nvEncCreateInputBuffer: Unused,
        nvEncDestroyInputBuffer: Unused,
        nvEncCreateBitstreamBuffer: Option<PNVENCCREATEBITSTREAMBUFFER>,
        nvEncDestroyBitstreamBuffer: Option<PNVENCDESTROYBITSTREAMBUFFER>,
        nvEncEncodePicture: Option<PNVENCENCODEPICTURE>,
        nvEncLockBitstream: Option<PNVENCLOCKBITSTREAM>,
        nvEncUnlockBitstream: Option<PNVENCUNLOCKBITSTREAM>,
        nvEncLockInputBuffer: Unused,
        nvEncUnlockInputBuffer: Unused,
        nvEncGetEncodeStats: Unused,
        nvEncGetSequenceParams: Unused,
        nvEncRegisterAsyncEvent: Unused,
        nvEncUnregisterAsyncEvent: Unused,
        nvEncMapInputResource: Option<PNVENCMAPINPUTRESOURCE>,
        nvEncUnmapInputResource: Option<PNVENCUNMAPINPUTRESOURCE>,
        nvEncDestroyEncoder: Option<PNVENCDESTROYENCODER>,
        nvEncInvalidateRefFrames: Option<PNVENCINVALIDATEREFFRAMES>,
        nvEncOpenEncodeSessionEx: Option<PNVENCOPENENCODESESSIONEX>,
        nvEncRegisterResource: Option<PNVENCREGISTERRESOURCE>,
        nvEncUnregisterResource: Option<PNVENCUNREGISTERRESOURCE>,
        nvEncReconfigureEncoder: Option<PNVENCRECONFIGUREENCODER>,
        reserved1: Unused,
        nvEncCreateMVBuffer: Unused,
        nvEncDestroyMVBuffer: Unused,
        nvEncRunMotionEstimationOnly: Unused,
        nvEncGetLastErrorString: Option<PNVENCGETLASTERROR>,
        nvEncSetIOCudaStreams: Unused,
        nvEncGetEncodePresetConfigEx: Option<PNVENCGETENCODEPRESETCONFIGEX>,
        nvEncGetSequenceParamEx: Unused,
        nvEncRestoreEncoderState: Unused,
        nvEncLookaheadPicture: Unused,
        reserved2: [Unused; 275],
    }
}

/// One named field inside a struct's run of C bitfields, which Rust holds as
/// the single u32 word `bitfields`.
#[cfg(test)]
pub(crate) struct Bitfield {
    pub(crate) name: &'static str,
    pub(crate) word: usize,
    pub(crate) shift: u32,
    pub(crate) width: u32,
}

const fn with_bits(word: u32, shift: u32, width: u32, value: u32) -> u32 {
    let mask = (u32::MAX >> (32 - width)) << shift;
    (word & !mask) | ((value << shift) & mask)
}

// Setters for the bitfields Booth writes. The rest stay zero, as the header
// asks of anything a client does not set.
macro_rules! bitfields {
    ($($ty:ident { $($setter:ident => $cname:ident: $shift:literal, $width:literal;)* })*) => {
        $(impl $ty {
            $(pub(crate) fn $setter(&mut self, value: u32) {
                debug_assert!(value >> $width == 0, "{value} does not fit {}", stringify!($cname));
                self.bitfields = with_bits(self.bitfields, $shift, $width, value);
            })*
        })*

        #[cfg(test)]
        pub(crate) const BITFIELDS: &[Bitfield] = &[$($(Bitfield {
            name: concat!(stringify!($ty), ".", stringify!($cname)),
            word: std::mem::offset_of!($ty, bitfields),
            shift: $shift,
            width: $width,
        },)*)*];
    };
}

bitfields! {
    NV_ENC_RC_PARAMS {
        set_enable_lookahead => enableLookahead: 5, 1;
        set_zero_reorder_delay => zeroReorderDelay: 9, 1;
        set_enable_non_ref_p => enableNonRefP: 10, 1;
    }
    NV_ENC_CONFIG_H264 {
        set_output_aud => outputAUD: 6, 1;
        set_enable_intra_refresh => enableIntraRefresh: 10, 1;
        set_repeat_sps_pps => repeatSPSPPS: 12, 1;
        set_enable_ltr => enableLTR: 14, 1;
        set_enable_filler_data_insertion => enableFillerDataInsertion: 17, 1;
        set_single_slice_intra_refresh => singleSliceIntraRefresh: 20, 1;
    }
    NV_ENC_CONFIG_HEVC {
        set_output_aud => outputAUD: 4, 1;
        set_enable_ltr => enableLTR: 5, 1;
        set_repeat_sps_pps => repeatSPSPPS: 7, 1;
        set_enable_intra_refresh => enableIntraRefresh: 8, 1;
        set_chroma_format_idc => chromaFormatIDC: 9, 2;
        set_enable_filler_data_insertion => enableFillerDataInsertion: 14, 1;
        set_single_slice_intra_refresh => singleSliceIntraRefresh: 17, 1;
    }
    NV_ENC_RECONFIGURE_PARAMS {
        set_reset_encoder => resetEncoder: 0, 1;
        set_force_idr => forceIDR: 1, 1;
    }
}

impl NV_ENC_CONFIG {
    pub(crate) fn h264(&mut self) -> &mut NV_ENC_CONFIG_H264 {
        // SAFETY: every member of the union is integers and raw pointers, for
        // which any bytes are a valid value, so the H.264 view is always sound.
        unsafe { &mut self.encodeCodecConfig.h264Config }
    }

    pub(crate) fn hevc(&mut self) -> &mut NV_ENC_CONFIG_HEVC {
        // SAFETY: as for h264(): integers and raw pointers only.
        unsafe { &mut self.encodeCodecConfig.hevcConfig }
    }
}
