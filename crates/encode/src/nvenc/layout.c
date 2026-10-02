/*
 * The header's own view of every type, constant and GUID that ffi.rs copies
 * from nvEncodeAPI.h, for layout_test.rs to compare against the Rust side.
 * Nothing in the encoder calls this; only the test does.
 */

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include "nvEncodeAPI.h"

struct booth_entry {
    const char *name;
    uint64_t value;
};

struct booth_guid {
    const char *name;
    GUID value;
};

struct booth_bits {
    const char *name;
    uint32_t offset;
    uint32_t mask;
};

#define SIZE(t) {"size " #t, sizeof(t)}, {"align " #t, __alignof(t)}
#define FIELD(t, f) {#t "." #f, offsetof(t, f)}
#define VALUE(c) {#c, (uint64_t)(c)}

static const struct booth_entry layout[] = {
    SIZE(GUID),

    SIZE(NV_ENC_CAPS_PARAM),
    FIELD(NV_ENC_CAPS_PARAM, version),
    FIELD(NV_ENC_CAPS_PARAM, capsToQuery),
    FIELD(NV_ENC_CAPS_PARAM, reserved),

    SIZE(NV_ENC_CREATE_BITSTREAM_BUFFER),
    FIELD(NV_ENC_CREATE_BITSTREAM_BUFFER, version),
    FIELD(NV_ENC_CREATE_BITSTREAM_BUFFER, size),
    FIELD(NV_ENC_CREATE_BITSTREAM_BUFFER, memoryHeap),
    FIELD(NV_ENC_CREATE_BITSTREAM_BUFFER, reserved),
    FIELD(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBuffer),
    FIELD(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBufferPtr),
    FIELD(NV_ENC_CREATE_BITSTREAM_BUFFER, reserved1),
    FIELD(NV_ENC_CREATE_BITSTREAM_BUFFER, reserved2),

    SIZE(NV_ENC_QP),
    FIELD(NV_ENC_QP, qpInterP),
    FIELD(NV_ENC_QP, qpInterB),
    FIELD(NV_ENC_QP, qpIntra),

    SIZE(NV_ENC_RC_PARAMS),
    FIELD(NV_ENC_RC_PARAMS, version),
    FIELD(NV_ENC_RC_PARAMS, rateControlMode),
    FIELD(NV_ENC_RC_PARAMS, constQP),
    FIELD(NV_ENC_RC_PARAMS, averageBitRate),
    FIELD(NV_ENC_RC_PARAMS, maxBitRate),
    FIELD(NV_ENC_RC_PARAMS, vbvBufferSize),
    FIELD(NV_ENC_RC_PARAMS, vbvInitialDelay),
    FIELD(NV_ENC_RC_PARAMS, minQP),
    FIELD(NV_ENC_RC_PARAMS, maxQP),
    FIELD(NV_ENC_RC_PARAMS, initialRCQP),
    FIELD(NV_ENC_RC_PARAMS, temporallayerIdxMask),
    FIELD(NV_ENC_RC_PARAMS, temporalLayerQP),
    FIELD(NV_ENC_RC_PARAMS, targetQuality),
    FIELD(NV_ENC_RC_PARAMS, targetQualityLSB),
    FIELD(NV_ENC_RC_PARAMS, lookaheadDepth),
    FIELD(NV_ENC_RC_PARAMS, lowDelayKeyFrameScale),
    FIELD(NV_ENC_RC_PARAMS, yDcQPIndexOffset),
    FIELD(NV_ENC_RC_PARAMS, uDcQPIndexOffset),
    FIELD(NV_ENC_RC_PARAMS, vDcQPIndexOffset),
    FIELD(NV_ENC_RC_PARAMS, qpMapMode),
    FIELD(NV_ENC_RC_PARAMS, multiPass),
    FIELD(NV_ENC_RC_PARAMS, alphaLayerBitrateRatio),
    FIELD(NV_ENC_RC_PARAMS, cbQPIndexOffset),
    FIELD(NV_ENC_RC_PARAMS, crQPIndexOffset),
    FIELD(NV_ENC_RC_PARAMS, reserved2),
    FIELD(NV_ENC_RC_PARAMS, lookaheadLevel),
    FIELD(NV_ENC_RC_PARAMS, reserved),

    SIZE(NV_ENC_CLOCK_TIMESTAMP_SET),
    FIELD(NV_ENC_CLOCK_TIMESTAMP_SET, timeOffset),

    SIZE(NV_ENC_TIME_CODE),
    FIELD(NV_ENC_TIME_CODE, displayPicStruct),
    FIELD(NV_ENC_TIME_CODE, clockTimestamp),
    FIELD(NV_ENC_TIME_CODE, skipClockTimestampInsertion),

    SIZE(NV_ENC_CONFIG_H264_VUI_PARAMETERS),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, overscanInfoPresentFlag),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, overscanInfo),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoSignalTypePresentFlag),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFormat),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFullRangeFlag),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourDescriptionPresentFlag),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourPrimaries),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, transferCharacteristics),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourMatrix),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, chromaSampleLocationFlag),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, chromaSampleLocationTop),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, chromaSampleLocationBot),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, bitstreamRestrictionFlag),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, timingInfoPresentFlag),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, numUnitInTicks),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, timeScale),
    FIELD(NV_ENC_CONFIG_H264_VUI_PARAMETERS, reserved),

    SIZE(NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE),
    FIELD(NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE, reserved1),

    SIZE(NV_ENC_CONFIG_H264),
    FIELD(NV_ENC_CONFIG_H264, level),
    FIELD(NV_ENC_CONFIG_H264, idrPeriod),
    FIELD(NV_ENC_CONFIG_H264, separateColourPlaneFlag),
    FIELD(NV_ENC_CONFIG_H264, disableDeblockingFilterIDC),
    FIELD(NV_ENC_CONFIG_H264, numTemporalLayers),
    FIELD(NV_ENC_CONFIG_H264, spsId),
    FIELD(NV_ENC_CONFIG_H264, ppsId),
    FIELD(NV_ENC_CONFIG_H264, adaptiveTransformMode),
    FIELD(NV_ENC_CONFIG_H264, fmoMode),
    FIELD(NV_ENC_CONFIG_H264, bdirectMode),
    FIELD(NV_ENC_CONFIG_H264, entropyCodingMode),
    FIELD(NV_ENC_CONFIG_H264, stereoMode),
    FIELD(NV_ENC_CONFIG_H264, intraRefreshPeriod),
    FIELD(NV_ENC_CONFIG_H264, intraRefreshCnt),
    FIELD(NV_ENC_CONFIG_H264, maxNumRefFrames),
    FIELD(NV_ENC_CONFIG_H264, sliceMode),
    FIELD(NV_ENC_CONFIG_H264, sliceModeData),
    FIELD(NV_ENC_CONFIG_H264, h264VUIParameters),
    FIELD(NV_ENC_CONFIG_H264, ltrNumFrames),
    FIELD(NV_ENC_CONFIG_H264, ltrTrustMode),
    FIELD(NV_ENC_CONFIG_H264, chromaFormatIDC),
    FIELD(NV_ENC_CONFIG_H264, maxTemporalLayers),
    FIELD(NV_ENC_CONFIG_H264, useBFramesAsRef),
    FIELD(NV_ENC_CONFIG_H264, numRefL0),
    FIELD(NV_ENC_CONFIG_H264, numRefL1),
    FIELD(NV_ENC_CONFIG_H264, outputBitDepth),
    FIELD(NV_ENC_CONFIG_H264, inputBitDepth),
    FIELD(NV_ENC_CONFIG_H264, reserved1),
    FIELD(NV_ENC_CONFIG_H264, reserved2),

    SIZE(NV_ENC_CONFIG_HEVC),
    FIELD(NV_ENC_CONFIG_HEVC, level),
    FIELD(NV_ENC_CONFIG_HEVC, tier),
    FIELD(NV_ENC_CONFIG_HEVC, minCUSize),
    FIELD(NV_ENC_CONFIG_HEVC, maxCUSize),
    FIELD(NV_ENC_CONFIG_HEVC, idrPeriod),
    FIELD(NV_ENC_CONFIG_HEVC, intraRefreshPeriod),
    FIELD(NV_ENC_CONFIG_HEVC, intraRefreshCnt),
    FIELD(NV_ENC_CONFIG_HEVC, maxNumRefFramesInDPB),
    FIELD(NV_ENC_CONFIG_HEVC, ltrNumFrames),
    FIELD(NV_ENC_CONFIG_HEVC, vpsId),
    FIELD(NV_ENC_CONFIG_HEVC, spsId),
    FIELD(NV_ENC_CONFIG_HEVC, ppsId),
    FIELD(NV_ENC_CONFIG_HEVC, sliceMode),
    FIELD(NV_ENC_CONFIG_HEVC, sliceModeData),
    FIELD(NV_ENC_CONFIG_HEVC, maxTemporalLayersMinus1),
    FIELD(NV_ENC_CONFIG_HEVC, hevcVUIParameters),
    FIELD(NV_ENC_CONFIG_HEVC, ltrTrustMode),
    FIELD(NV_ENC_CONFIG_HEVC, useBFramesAsRef),
    FIELD(NV_ENC_CONFIG_HEVC, numRefL0),
    FIELD(NV_ENC_CONFIG_HEVC, numRefL1),
    FIELD(NV_ENC_CONFIG_HEVC, tfLevel),
    FIELD(NV_ENC_CONFIG_HEVC, disableDeblockingFilterIDC),
    FIELD(NV_ENC_CONFIG_HEVC, outputBitDepth),
    FIELD(NV_ENC_CONFIG_HEVC, inputBitDepth),
    FIELD(NV_ENC_CONFIG_HEVC, reserved1),
    FIELD(NV_ENC_CONFIG_HEVC, reserved2),

    SIZE(NV_ENC_CONFIG_AV1),
    SIZE(NV_ENC_CONFIG_H264_MEONLY),
    SIZE(NV_ENC_CONFIG_HEVC_MEONLY),

    SIZE(NV_ENC_CODEC_CONFIG),
    FIELD(NV_ENC_CODEC_CONFIG, h264Config),
    FIELD(NV_ENC_CODEC_CONFIG, hevcConfig),
    FIELD(NV_ENC_CODEC_CONFIG, av1Config),
    FIELD(NV_ENC_CODEC_CONFIG, h264MeOnlyConfig),
    FIELD(NV_ENC_CODEC_CONFIG, hevcMeOnlyConfig),
    FIELD(NV_ENC_CODEC_CONFIG, reserved),

    SIZE(NV_ENC_CONFIG),
    FIELD(NV_ENC_CONFIG, version),
    FIELD(NV_ENC_CONFIG, profileGUID),
    FIELD(NV_ENC_CONFIG, gopLength),
    FIELD(NV_ENC_CONFIG, frameIntervalP),
    FIELD(NV_ENC_CONFIG, monoChromeEncoding),
    FIELD(NV_ENC_CONFIG, frameFieldMode),
    FIELD(NV_ENC_CONFIG, mvPrecision),
    FIELD(NV_ENC_CONFIG, rcParams),
    FIELD(NV_ENC_CONFIG, encodeCodecConfig),
    FIELD(NV_ENC_CONFIG, reserved),
    FIELD(NV_ENC_CONFIG, reserved2),

    SIZE(NV_ENC_INITIALIZE_PARAMS),
    FIELD(NV_ENC_INITIALIZE_PARAMS, version),
    FIELD(NV_ENC_INITIALIZE_PARAMS, encodeGUID),
    FIELD(NV_ENC_INITIALIZE_PARAMS, presetGUID),
    FIELD(NV_ENC_INITIALIZE_PARAMS, encodeWidth),
    FIELD(NV_ENC_INITIALIZE_PARAMS, encodeHeight),
    FIELD(NV_ENC_INITIALIZE_PARAMS, darWidth),
    FIELD(NV_ENC_INITIALIZE_PARAMS, darHeight),
    FIELD(NV_ENC_INITIALIZE_PARAMS, frameRateNum),
    FIELD(NV_ENC_INITIALIZE_PARAMS, frameRateDen),
    FIELD(NV_ENC_INITIALIZE_PARAMS, enableEncodeAsync),
    FIELD(NV_ENC_INITIALIZE_PARAMS, enablePTD),
    FIELD(NV_ENC_INITIALIZE_PARAMS, privDataSize),
    FIELD(NV_ENC_INITIALIZE_PARAMS, reserved),
    FIELD(NV_ENC_INITIALIZE_PARAMS, privData),
    FIELD(NV_ENC_INITIALIZE_PARAMS, encodeConfig),
    FIELD(NV_ENC_INITIALIZE_PARAMS, maxEncodeWidth),
    FIELD(NV_ENC_INITIALIZE_PARAMS, maxEncodeHeight),
    FIELD(NV_ENC_INITIALIZE_PARAMS, maxMEHintCountsPerBlock),
    FIELD(NV_ENC_INITIALIZE_PARAMS, tuningInfo),
    FIELD(NV_ENC_INITIALIZE_PARAMS, bufferFormat),
    FIELD(NV_ENC_INITIALIZE_PARAMS, numStateBuffers),
    FIELD(NV_ENC_INITIALIZE_PARAMS, outputStatsLevel),
    FIELD(NV_ENC_INITIALIZE_PARAMS, reserved1),
    FIELD(NV_ENC_INITIALIZE_PARAMS, reserved2),

    SIZE(NV_ENC_RECONFIGURE_PARAMS),
    FIELD(NV_ENC_RECONFIGURE_PARAMS, version),
    FIELD(NV_ENC_RECONFIGURE_PARAMS, reserved),
    FIELD(NV_ENC_RECONFIGURE_PARAMS, reInitEncodeParams),
    FIELD(NV_ENC_RECONFIGURE_PARAMS, reserved2),

    SIZE(NV_ENC_PRESET_CONFIG),
    FIELD(NV_ENC_PRESET_CONFIG, version),
    FIELD(NV_ENC_PRESET_CONFIG, reserved),
    FIELD(NV_ENC_PRESET_CONFIG, presetCfg),
    FIELD(NV_ENC_PRESET_CONFIG, reserved1),
    FIELD(NV_ENC_PRESET_CONFIG, reserved2),

    SIZE(NV_ENC_PIC_PARAMS_MVC),

    SIZE(NV_ENC_PIC_PARAMS_H264_EXT),
    FIELD(NV_ENC_PIC_PARAMS_H264_EXT, mvcPicParams),
    FIELD(NV_ENC_PIC_PARAMS_H264_EXT, reserved1),

    SIZE(NV_ENC_PIC_PARAMS_H264),
    FIELD(NV_ENC_PIC_PARAMS_H264, displayPOCSyntax),
    FIELD(NV_ENC_PIC_PARAMS_H264, reserved3),
    FIELD(NV_ENC_PIC_PARAMS_H264, refPicFlag),
    FIELD(NV_ENC_PIC_PARAMS_H264, colourPlaneId),
    FIELD(NV_ENC_PIC_PARAMS_H264, forceIntraRefreshWithFrameCnt),
    FIELD(NV_ENC_PIC_PARAMS_H264, sliceTypeData),
    FIELD(NV_ENC_PIC_PARAMS_H264, sliceTypeArrayCnt),
    FIELD(NV_ENC_PIC_PARAMS_H264, seiPayloadArrayCnt),
    FIELD(NV_ENC_PIC_PARAMS_H264, seiPayloadArray),
    FIELD(NV_ENC_PIC_PARAMS_H264, sliceMode),
    FIELD(NV_ENC_PIC_PARAMS_H264, sliceModeData),
    FIELD(NV_ENC_PIC_PARAMS_H264, ltrMarkFrameIdx),
    FIELD(NV_ENC_PIC_PARAMS_H264, ltrUseFrameBitmap),
    FIELD(NV_ENC_PIC_PARAMS_H264, ltrUsageMode),
    FIELD(NV_ENC_PIC_PARAMS_H264, forceIntraSliceCount),
    FIELD(NV_ENC_PIC_PARAMS_H264, forceIntraSliceIdx),
    FIELD(NV_ENC_PIC_PARAMS_H264, h264ExtPicParams),
    FIELD(NV_ENC_PIC_PARAMS_H264, timeCode),
    FIELD(NV_ENC_PIC_PARAMS_H264, reserved),
    FIELD(NV_ENC_PIC_PARAMS_H264, reserved2),

    SIZE(NV_ENC_PIC_PARAMS_HEVC),
    SIZE(NV_ENC_PIC_PARAMS_AV1),

    SIZE(NV_ENC_CODEC_PIC_PARAMS),
    FIELD(NV_ENC_CODEC_PIC_PARAMS, h264PicParams),
    FIELD(NV_ENC_CODEC_PIC_PARAMS, hevcPicParams),
    FIELD(NV_ENC_CODEC_PIC_PARAMS, av1PicParams),
    FIELD(NV_ENC_CODEC_PIC_PARAMS, reserved),

    SIZE(NV_ENC_PIC_PARAMS),
    FIELD(NV_ENC_PIC_PARAMS, version),
    FIELD(NV_ENC_PIC_PARAMS, inputWidth),
    FIELD(NV_ENC_PIC_PARAMS, inputHeight),
    FIELD(NV_ENC_PIC_PARAMS, inputPitch),
    FIELD(NV_ENC_PIC_PARAMS, encodePicFlags),
    FIELD(NV_ENC_PIC_PARAMS, frameIdx),
    FIELD(NV_ENC_PIC_PARAMS, inputTimeStamp),
    FIELD(NV_ENC_PIC_PARAMS, inputDuration),
    FIELD(NV_ENC_PIC_PARAMS, inputBuffer),
    FIELD(NV_ENC_PIC_PARAMS, outputBitstream),
    FIELD(NV_ENC_PIC_PARAMS, completionEvent),
    FIELD(NV_ENC_PIC_PARAMS, bufferFmt),
    FIELD(NV_ENC_PIC_PARAMS, pictureStruct),
    FIELD(NV_ENC_PIC_PARAMS, pictureType),
    FIELD(NV_ENC_PIC_PARAMS, codecPicParams),
    FIELD(NV_ENC_PIC_PARAMS, meHintCountsPerBlock),
    FIELD(NV_ENC_PIC_PARAMS, meExternalHints),
    FIELD(NV_ENC_PIC_PARAMS, reserved2),
    FIELD(NV_ENC_PIC_PARAMS, reserved5),
    FIELD(NV_ENC_PIC_PARAMS, qpDeltaMap),
    FIELD(NV_ENC_PIC_PARAMS, qpDeltaMapSize),
    FIELD(NV_ENC_PIC_PARAMS, reservedBitFields),
    FIELD(NV_ENC_PIC_PARAMS, meHintRefPicDist),
    FIELD(NV_ENC_PIC_PARAMS, reserved4),
    FIELD(NV_ENC_PIC_PARAMS, alphaBuffer),
    FIELD(NV_ENC_PIC_PARAMS, meExternalSbHints),
    FIELD(NV_ENC_PIC_PARAMS, meSbHintsCount),
    FIELD(NV_ENC_PIC_PARAMS, stateBufferIdx),
    FIELD(NV_ENC_PIC_PARAMS, outputReconBuffer),
    FIELD(NV_ENC_PIC_PARAMS, reserved3),
    FIELD(NV_ENC_PIC_PARAMS, reserved6),

    SIZE(NV_ENC_LOCK_BITSTREAM),
    FIELD(NV_ENC_LOCK_BITSTREAM, version),
    FIELD(NV_ENC_LOCK_BITSTREAM, outputBitstream),
    FIELD(NV_ENC_LOCK_BITSTREAM, sliceOffsets),
    FIELD(NV_ENC_LOCK_BITSTREAM, frameIdx),
    FIELD(NV_ENC_LOCK_BITSTREAM, hwEncodeStatus),
    FIELD(NV_ENC_LOCK_BITSTREAM, numSlices),
    FIELD(NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes),
    FIELD(NV_ENC_LOCK_BITSTREAM, outputTimeStamp),
    FIELD(NV_ENC_LOCK_BITSTREAM, outputDuration),
    FIELD(NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr),
    FIELD(NV_ENC_LOCK_BITSTREAM, pictureType),
    FIELD(NV_ENC_LOCK_BITSTREAM, pictureStruct),
    FIELD(NV_ENC_LOCK_BITSTREAM, frameAvgQP),
    FIELD(NV_ENC_LOCK_BITSTREAM, frameSatd),
    FIELD(NV_ENC_LOCK_BITSTREAM, ltrFrameIdx),
    FIELD(NV_ENC_LOCK_BITSTREAM, ltrFrameBitmap),
    FIELD(NV_ENC_LOCK_BITSTREAM, temporalId),
    FIELD(NV_ENC_LOCK_BITSTREAM, intraMBCount),
    FIELD(NV_ENC_LOCK_BITSTREAM, interMBCount),
    FIELD(NV_ENC_LOCK_BITSTREAM, averageMVX),
    FIELD(NV_ENC_LOCK_BITSTREAM, averageMVY),
    FIELD(NV_ENC_LOCK_BITSTREAM, alphaLayerSizeInBytes),
    FIELD(NV_ENC_LOCK_BITSTREAM, outputStatsPtrSize),
    FIELD(NV_ENC_LOCK_BITSTREAM, reserved),
    FIELD(NV_ENC_LOCK_BITSTREAM, outputStatsPtr),
    FIELD(NV_ENC_LOCK_BITSTREAM, frameIdxDisplay),
    FIELD(NV_ENC_LOCK_BITSTREAM, reserved1),
    FIELD(NV_ENC_LOCK_BITSTREAM, reserved2),
    FIELD(NV_ENC_LOCK_BITSTREAM, reservedInternal),

    SIZE(NV_ENC_MAP_INPUT_RESOURCE),
    FIELD(NV_ENC_MAP_INPUT_RESOURCE, version),
    FIELD(NV_ENC_MAP_INPUT_RESOURCE, subResourceIndex),
    FIELD(NV_ENC_MAP_INPUT_RESOURCE, inputResource),
    FIELD(NV_ENC_MAP_INPUT_RESOURCE, registeredResource),
    FIELD(NV_ENC_MAP_INPUT_RESOURCE, mappedResource),
    FIELD(NV_ENC_MAP_INPUT_RESOURCE, mappedBufferFmt),
    FIELD(NV_ENC_MAP_INPUT_RESOURCE, reserved1),
    FIELD(NV_ENC_MAP_INPUT_RESOURCE, reserved2),

    SIZE(NV_ENC_REGISTER_RESOURCE),
    FIELD(NV_ENC_REGISTER_RESOURCE, version),
    FIELD(NV_ENC_REGISTER_RESOURCE, resourceType),
    FIELD(NV_ENC_REGISTER_RESOURCE, width),
    FIELD(NV_ENC_REGISTER_RESOURCE, height),
    FIELD(NV_ENC_REGISTER_RESOURCE, pitch),
    FIELD(NV_ENC_REGISTER_RESOURCE, subResourceIndex),
    FIELD(NV_ENC_REGISTER_RESOURCE, resourceToRegister),
    FIELD(NV_ENC_REGISTER_RESOURCE, registeredResource),
    FIELD(NV_ENC_REGISTER_RESOURCE, bufferFormat),
    FIELD(NV_ENC_REGISTER_RESOURCE, bufferUsage),
    FIELD(NV_ENC_REGISTER_RESOURCE, pInputFencePoint),
    FIELD(NV_ENC_REGISTER_RESOURCE, chromaOffset),
    FIELD(NV_ENC_REGISTER_RESOURCE, reserved1),
    FIELD(NV_ENC_REGISTER_RESOURCE, reserved2),

    SIZE(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS),
    FIELD(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, version),
    FIELD(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, deviceType),
    FIELD(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, device),
    FIELD(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, reserved),
    FIELD(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, apiVersion),
    FIELD(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, reserved1),
    FIELD(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, reserved2),

    SIZE(NV_ENCODE_API_FUNCTION_LIST),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, version),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, reserved),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSession),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeGUIDCount),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeProfileGUIDCount),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeProfileGUIDs),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeGUIDs),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetInputFormatCount),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetInputFormats),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeCaps),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetCount),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetGUIDs),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfig),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncInitializeEncoder),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateInputBuffer),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyInputBuffer),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateBitstreamBuffer),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyBitstreamBuffer),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncEncodePicture),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncLockBitstream),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockBitstream),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncLockInputBuffer),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockInputBuffer),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeStats),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetSequenceParams),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncRegisterAsyncEvent),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncUnregisterAsyncEvent),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncMapInputResource),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncUnmapInputResource),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyEncoder),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncInvalidateRefFrames),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSessionEx),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncRegisterResource),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncUnregisterResource),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncReconfigureEncoder),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, reserved1),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateMVBuffer),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyMVBuffer),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncRunMotionEstimationOnly),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetLastErrorString),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncSetIOCudaStreams),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfigEx),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncGetSequenceParamEx),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncRestoreEncoderState),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, nvEncLookaheadPicture),
    FIELD(NV_ENCODE_API_FUNCTION_LIST, reserved2),

    VALUE(NVENCAPI_MAJOR_VERSION),
    VALUE(NVENCAPI_MINOR_VERSION),
    VALUE(NVENCAPI_VERSION),
    VALUE(NVENC_INFINITE_GOPLENGTH),

    VALUE(NV_ENC_CAPS_PARAM_VER),
    VALUE(NV_ENC_CREATE_BITSTREAM_BUFFER_VER),
    VALUE(NV_ENC_CONFIG_VER),
    VALUE(NV_ENC_INITIALIZE_PARAMS_VER),
    VALUE(NV_ENC_RECONFIGURE_PARAMS_VER),
    VALUE(NV_ENC_PRESET_CONFIG_VER),
    VALUE(NV_ENC_PIC_PARAMS_VER),
    VALUE(NV_ENC_LOCK_BITSTREAM_VER),
    VALUE(NV_ENC_MAP_INPUT_RESOURCE_VER),
    VALUE(NV_ENC_REGISTER_RESOURCE_VER),
    VALUE(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER),
    VALUE(NV_ENCODE_API_FUNCTION_LIST_VER),

    VALUE(NV_ENC_CAPS_LEVEL_MAX),
    VALUE(NV_ENC_CAPS_WIDTH_MAX),
    VALUE(NV_ENC_CAPS_HEIGHT_MAX),
    VALUE(NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE),
    VALUE(NV_ENC_CAPS_SUPPORT_INTRA_REFRESH),
    VALUE(NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE),
    VALUE(NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION),
    VALUE(NV_ENC_CAPS_WIDTH_MIN),
    VALUE(NV_ENC_CAPS_HEIGHT_MIN),
    VALUE(NV_ENC_CAPS_SINGLE_SLICE_INTRA_REFRESH),

    VALUE(NV_ENC_PARAMS_RC_CBR),
    VALUE(NV_ENC_TWO_PASS_QUARTER_RESOLUTION),
    VALUE(NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY),

    VALUE(NV_ENC_PIC_FLAG_FORCEIDR),
    VALUE(NV_ENC_PIC_FLAG_OUTPUT_SPSPPS),
    VALUE(NV_ENC_PIC_FLAG_EOS),
    VALUE(NV_ENC_PIC_STRUCT_FRAME),
    VALUE(NV_ENC_PIC_TYPE_IDR),

    VALUE(NV_ENC_BUFFER_FORMAT_NV12),
    VALUE(NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX),
    VALUE(NV_ENC_INPUT_IMAGE),
    VALUE(NV_ENC_DEVICE_TYPE_DIRECTX),

    VALUE(NV_ENC_LEVEL_HEVC_1),
    VALUE(NV_ENC_LEVEL_HEVC_2),
    VALUE(NV_ENC_LEVEL_HEVC_21),
    VALUE(NV_ENC_LEVEL_HEVC_3),
    VALUE(NV_ENC_LEVEL_HEVC_31),
    VALUE(NV_ENC_LEVEL_HEVC_4),
    VALUE(NV_ENC_LEVEL_HEVC_41),
    VALUE(NV_ENC_LEVEL_HEVC_5),
    VALUE(NV_ENC_LEVEL_HEVC_51),
    VALUE(NV_ENC_LEVEL_HEVC_52),
    VALUE(NV_ENC_LEVEL_HEVC_6),
    VALUE(NV_ENC_LEVEL_HEVC_61),
    VALUE(NV_ENC_LEVEL_HEVC_62),
    VALUE(NV_ENC_TIER_HEVC_MAIN),
    VALUE(NV_ENC_TIER_HEVC_HIGH),
    VALUE(NV_ENC_BIT_DEPTH_8),

    VALUE(NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED),
    VALUE(NV_ENC_VUI_COLOR_PRIMARIES_BT709),
    VALUE(NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709),
    VALUE(NV_ENC_VUI_MATRIX_COEFFS_BT709),

    VALUE(NV_ENC_SUCCESS),
    VALUE(NV_ENC_ERR_NO_ENCODE_DEVICE),
    VALUE(NV_ENC_ERR_UNSUPPORTED_DEVICE),
    VALUE(NV_ENC_ERR_INVALID_ENCODERDEVICE),
    VALUE(NV_ENC_ERR_INVALID_DEVICE),
    VALUE(NV_ENC_ERR_DEVICE_NOT_EXIST),
    VALUE(NV_ENC_ERR_INVALID_PTR),
    VALUE(NV_ENC_ERR_INVALID_EVENT),
    VALUE(NV_ENC_ERR_INVALID_PARAM),
    VALUE(NV_ENC_ERR_INVALID_CALL),
    VALUE(NV_ENC_ERR_OUT_OF_MEMORY),
    VALUE(NV_ENC_ERR_ENCODER_NOT_INITIALIZED),
    VALUE(NV_ENC_ERR_UNSUPPORTED_PARAM),
    VALUE(NV_ENC_ERR_LOCK_BUSY),
    VALUE(NV_ENC_ERR_NOT_ENOUGH_BUFFER),
    VALUE(NV_ENC_ERR_INVALID_VERSION),
    VALUE(NV_ENC_ERR_MAP_FAILED),
    VALUE(NV_ENC_ERR_NEED_MORE_INPUT),
    VALUE(NV_ENC_ERR_ENCODER_BUSY),
    VALUE(NV_ENC_ERR_EVENT_NOT_REGISTERD),
    VALUE(NV_ENC_ERR_GENERIC),
    VALUE(NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY),
    VALUE(NV_ENC_ERR_UNIMPLEMENTED),
    VALUE(NV_ENC_ERR_RESOURCE_REGISTER_FAILED),
    VALUE(NV_ENC_ERR_RESOURCE_NOT_REGISTERED),
    VALUE(NV_ENC_ERR_RESOURCE_NOT_MAPPED),
    VALUE(NV_ENC_ERR_NEED_MORE_OUTPUT),
};

const struct booth_entry *booth_nvenc_layout(size_t *count)
{
    *count = sizeof layout / sizeof layout[0];
    return layout;
}

/* The header's GUIDs are const variables, which C does not accept in a
 * static initializer, so they are copied out at run time. */
#define GUID_VALUE(g)                    \
    do {                                 \
        if (n < capacity) {              \
            out[n].name = #g;            \
            out[n].value = g;            \
        }                                \
        n++;                             \
    } while (0)

size_t booth_nvenc_guids(struct booth_guid *out, size_t capacity)
{
    size_t n = 0;
    GUID_VALUE(NV_ENC_CODEC_H264_GUID);
    GUID_VALUE(NV_ENC_CODEC_HEVC_GUID);
    GUID_VALUE(NV_ENC_H264_PROFILE_HIGH_GUID);
    GUID_VALUE(NV_ENC_HEVC_PROFILE_MAIN_GUID);
    GUID_VALUE(NV_ENC_PRESET_P1_GUID);
    GUID_VALUE(NV_ENC_PRESET_P2_GUID);
    GUID_VALUE(NV_ENC_PRESET_P3_GUID);
    GUID_VALUE(NV_ENC_PRESET_P4_GUID);
    return n;
}

/*
 * offsetof cannot name a bitfield, so bitfields are found the long way: set
 * the members to all ones in a zeroed struct and see which bits of which
 * 32-bit word changed. A run that does not fit one word, or leaves stray
 * bits elsewhere, reports an offset of UINT32_MAX.
 */
static void record(struct booth_bits *out, size_t *n, size_t capacity, const char *name,
                   const void *object, size_t size)
{
    const unsigned char *bytes = object;
    size_t first = size;
    size_t last = 0;
    size_t i;
    uint32_t word;

    for (i = 0; i < size; i++) {
        if (bytes[i] != 0) {
            if (first == size) first = i;
            last = i;
        }
    }
    if (*n >= capacity) {
        (*n)++;
        return;
    }
    out[*n].name = name;
    if (first == size || first / 4 != last / 4) {
        out[*n].offset = UINT32_MAX;
        out[*n].mask = 0;
    } else {
        memcpy(&word, bytes + first / 4 * 4, sizeof word);
        out[*n].offset = (uint32_t)(first / 4 * 4);
        out[*n].mask = word;
    }
    (*n)++;
}

/* Unsigned bitfields wrap on decrement, so 0 minus 1 is all ones at any width. */
#define ONES(s, f) ((s).f = 0, (s).f -= 1)

#define BIT(t, f)                                          \
    do {                                                   \
        t s_;                                              \
        memset(&s_, 0, sizeof s_);                         \
        ONES(s_, f);                                       \
        record(out, &n, capacity, #t "." #f, &s_, sizeof s_); \
    } while (0)

size_t booth_nvenc_bitfields(struct booth_bits *out, size_t capacity)
{
    size_t n = 0;

    /* Each whole run: every member at once must fill exactly one word. */
    {
        NV_ENC_RC_PARAMS s;
        memset(&s, 0, sizeof s);
        ONES(s, enableMinQP);
        ONES(s, enableMaxQP);
        ONES(s, enableInitialRCQP);
        ONES(s, enableAQ);
        ONES(s, reservedBitField1);
        ONES(s, enableLookahead);
        ONES(s, disableIadapt);
        ONES(s, disableBadapt);
        ONES(s, enableTemporalAQ);
        ONES(s, zeroReorderDelay);
        ONES(s, enableNonRefP);
        ONES(s, strictGOPTarget);
        ONES(s, aqStrength);
        ONES(s, enableExtLookahead);
        ONES(s, reservedBitFields);
        record(out, &n, capacity, "NV_ENC_RC_PARAMS.bitfields", &s, sizeof s);
    }
    {
        NV_ENC_CLOCK_TIMESTAMP_SET s;
        memset(&s, 0, sizeof s);
        ONES(s, countingType);
        ONES(s, discontinuityFlag);
        ONES(s, cntDroppedFrames);
        ONES(s, nFrames);
        ONES(s, secondsValue);
        ONES(s, minutesValue);
        ONES(s, hoursValue);
        ONES(s, reserved2);
        record(out, &n, capacity, "NV_ENC_CLOCK_TIMESTAMP_SET.bitfields", &s, sizeof s);
    }
    {
        NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE s;
        memset(&s, 0, sizeof s);
        ONES(s, numCandsPerBlk16x16);
        ONES(s, numCandsPerBlk16x8);
        ONES(s, numCandsPerBlk8x16);
        ONES(s, numCandsPerBlk8x8);
        ONES(s, numCandsPerSb);
        ONES(s, reserved);
        record(out, &n, capacity, "NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE.bitfields", &s,
               sizeof s);
    }
    {
        NV_ENC_CONFIG_H264 s;
        memset(&s, 0, sizeof s);
        ONES(s, enableTemporalSVC);
        ONES(s, enableStereoMVC);
        ONES(s, hierarchicalPFrames);
        ONES(s, hierarchicalBFrames);
        ONES(s, outputBufferingPeriodSEI);
        ONES(s, outputPictureTimingSEI);
        ONES(s, outputAUD);
        ONES(s, disableSPSPPS);
        ONES(s, outputFramePackingSEI);
        ONES(s, outputRecoveryPointSEI);
        ONES(s, enableIntraRefresh);
        ONES(s, enableConstrainedEncoding);
        ONES(s, repeatSPSPPS);
        ONES(s, enableVFR);
        ONES(s, enableLTR);
        ONES(s, qpPrimeYZeroTransformBypassFlag);
        ONES(s, useConstrainedIntraPred);
        ONES(s, enableFillerDataInsertion);
        ONES(s, disableSVCPrefixNalu);
        ONES(s, enableScalabilityInfoSEI);
        ONES(s, singleSliceIntraRefresh);
        ONES(s, enableTimeCode);
        ONES(s, reservedBitFields);
        record(out, &n, capacity, "NV_ENC_CONFIG_H264.bitfields", &s, sizeof s);
    }
    {
        NV_ENC_CONFIG_HEVC s;
        memset(&s, 0, sizeof s);
        ONES(s, useConstrainedIntraPred);
        ONES(s, disableDeblockAcrossSliceBoundary);
        ONES(s, outputBufferingPeriodSEI);
        ONES(s, outputPictureTimingSEI);
        ONES(s, outputAUD);
        ONES(s, enableLTR);
        ONES(s, disableSPSPPS);
        ONES(s, repeatSPSPPS);
        ONES(s, enableIntraRefresh);
        ONES(s, chromaFormatIDC);
        ONES(s, reserved3);
        ONES(s, enableFillerDataInsertion);
        ONES(s, enableConstrainedEncoding);
        ONES(s, enableAlphaLayerEncoding);
        ONES(s, singleSliceIntraRefresh);
        ONES(s, outputRecoveryPointSEI);
        ONES(s, outputTimeCodeSEI);
        ONES(s, reserved);
        record(out, &n, capacity, "NV_ENC_CONFIG_HEVC.bitfields", &s, sizeof s);
    }
    {
        NV_ENC_INITIALIZE_PARAMS s;
        memset(&s, 0, sizeof s);
        ONES(s, reportSliceOffsets);
        ONES(s, enableSubFrameWrite);
        ONES(s, enableExternalMEHints);
        ONES(s, enableMEOnlyMode);
        ONES(s, enableWeightedPrediction);
        ONES(s, splitEncodeMode);
        ONES(s, enableOutputInVidmem);
        ONES(s, enableReconFrameOutput);
        ONES(s, enableOutputStats);
        ONES(s, enableUniDirectionalB);
        ONES(s, reservedBitFields);
        record(out, &n, capacity, "NV_ENC_INITIALIZE_PARAMS.bitfields", &s, sizeof s);
    }
    {
        NV_ENC_RECONFIGURE_PARAMS s;
        memset(&s, 0, sizeof s);
        ONES(s, resetEncoder);
        ONES(s, forceIDR);
        ONES(s, reserved1);
        record(out, &n, capacity, "NV_ENC_RECONFIGURE_PARAMS.bitfields", &s, sizeof s);
    }
    {
        NV_ENC_PIC_PARAMS_H264 s;
        memset(&s, 0, sizeof s);
        ONES(s, constrainedFrame);
        ONES(s, sliceModeDataUpdate);
        ONES(s, ltrMarkFrame);
        ONES(s, ltrUseFrames);
        ONES(s, reservedBitFields);
        record(out, &n, capacity, "NV_ENC_PIC_PARAMS_H264.bitfields", &s, sizeof s);
    }
    {
        NV_ENC_LOCK_BITSTREAM s;
        memset(&s, 0, sizeof s);
        ONES(s, doNotWait);
        ONES(s, ltrFrame);
        ONES(s, getRCStats);
        ONES(s, reservedBitFields);
        record(out, &n, capacity, "NV_ENC_LOCK_BITSTREAM.bitfields", &s, sizeof s);
    }

    /* The members Booth sets one by one. */
    BIT(NV_ENC_RC_PARAMS, enableLookahead);
    BIT(NV_ENC_RC_PARAMS, zeroReorderDelay);
    BIT(NV_ENC_RC_PARAMS, enableNonRefP);
    BIT(NV_ENC_CONFIG_H264, outputAUD);
    BIT(NV_ENC_CONFIG_H264, enableIntraRefresh);
    BIT(NV_ENC_CONFIG_H264, repeatSPSPPS);
    BIT(NV_ENC_CONFIG_H264, enableLTR);
    BIT(NV_ENC_CONFIG_H264, enableFillerDataInsertion);
    BIT(NV_ENC_CONFIG_H264, singleSliceIntraRefresh);
    BIT(NV_ENC_CONFIG_HEVC, outputAUD);
    BIT(NV_ENC_CONFIG_HEVC, enableLTR);
    BIT(NV_ENC_CONFIG_HEVC, repeatSPSPPS);
    BIT(NV_ENC_CONFIG_HEVC, enableIntraRefresh);
    BIT(NV_ENC_CONFIG_HEVC, chromaFormatIDC);
    BIT(NV_ENC_CONFIG_HEVC, enableFillerDataInsertion);
    BIT(NV_ENC_CONFIG_HEVC, singleSliceIntraRefresh);
    BIT(NV_ENC_RECONFIGURE_PARAMS, resetEncoder);
    BIT(NV_ENC_RECONFIGURE_PARAMS, forceIDR);

    return n;
}
