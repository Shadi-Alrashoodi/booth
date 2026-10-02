// The decoder's only C: the few FFmpeg struct fields Rust needs, read and
// written through FFmpeg's own headers so no struct layout is ever copied
// into Rust. Nothing here calls an FFmpeg function or links against FFmpeg;
// Rust looks the functions up in the DLLs at runtime.

#include <d3d11.h>
#include <stdint.h>

#include <libavcodec/avcodec.h>
#include <libavcodec/version.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
#include <libavutil/log.h>
#include <libavutil/version.h>

// Rust reads these instead of copying the numbers by hand.
const int booth_avcodec_major = LIBAVCODEC_VERSION_MAJOR;
const int booth_avcodec_minor = LIBAVCODEC_VERSION_MINOR;
const int booth_avutil_major = LIBAVUTIL_VERSION_MAJOR;
const int booth_avutil_minor = LIBAVUTIL_VERSION_MINOR;
const int booth_codec_h264 = AV_CODEC_ID_H264;
const int booth_codec_hevc = AV_CODEC_ID_HEVC;
const int booth_hwdevice_d3d11va = AV_HWDEVICE_TYPE_D3D11VA;
const int booth_pix_fmt_d3d11 = AV_PIX_FMT_D3D11;
const int booth_error_again = AVERROR(EAGAIN);
// Parts of FFmpeg return a bare -1 on failure, which av_strerror reads as
// EPERM, "Operation not permitted".
const int booth_error_bare = AVERROR(EPERM);
const int booth_padding = AV_INPUT_BUFFER_PADDING_SIZE;
const int booth_log_quiet = AV_LOG_QUIET;

// Rust calls these functions through signatures written by hand
// (src/library.rs). Each line stops the build if FFmpeg's header declares
// the function differently, so an FFmpeg update that changes one fails here
// instead of corrupting the stack. _Generic does not evaluate its operand,
// so none of this references the functions or needs FFmpeg at link time.
#define SAME(function, type) \
    _Static_assert(_Generic(&function, type: 1, default: 0), #function " no longer matches src/library.rs")

SAME(avutil_version, unsigned (*)(void));
SAME(av_log_set_level, void (*)(int));
SAME(av_strerror, int (*)(int, char *, size_t));
SAME(av_hwdevice_ctx_alloc, AVBufferRef *(*)(enum AVHWDeviceType));
SAME(av_hwdevice_ctx_init, int (*)(AVBufferRef *));
SAME(av_buffer_unref, void (*)(AVBufferRef **));
SAME(av_frame_alloc, AVFrame *(*)(void));
SAME(av_frame_free, void (*)(AVFrame **));
SAME(av_frame_unref, void (*)(AVFrame *));
SAME(av_frame_move_ref, void (*)(AVFrame *, AVFrame *));
SAME(avcodec_version, unsigned (*)(void));
SAME(avcodec_find_decoder, const AVCodec *(*)(enum AVCodecID));
SAME(avcodec_profile_name, const char *(*)(enum AVCodecID, int));
SAME(avcodec_alloc_context3, AVCodecContext *(*)(const AVCodec *));
SAME(avcodec_open2, int (*)(AVCodecContext *, const AVCodec *, AVDictionary **));
SAME(avcodec_free_context, void (*)(AVCodecContext **));
SAME(avcodec_send_packet, int (*)(AVCodecContext *, const AVPacket *));
SAME(avcodec_receive_frame, int (*)(AVCodecContext *, AVFrame *));
SAME(avcodec_flush_buffers, void (*)(AVCodecContext *));
SAME(av_packet_alloc, AVPacket *(*)(void));
SAME(av_packet_free, void (*)(AVPacket **));

// Rust passes C enums as int.
_Static_assert(sizeof(enum AVCodecID) == sizeof(int), "enum AVCodecID is not an int");
_Static_assert(sizeof(enum AVHWDeviceType) == sizeof(int), "enum AVHWDeviceType is not an int");

// FFmpeg releases the device and context when the device context goes, so
// Rust hands over references of its own.
void booth_device_fill(AVBufferRef *device, ID3D11Device *d3d, ID3D11DeviceContext *context,
                       unsigned bind_flags)
{
    AVHWDeviceContext *hw = (AVHWDeviceContext *)device->data;
    AVD3D11VADeviceContext *d3d11 = hw->hwctx;
    d3d11->device = d3d;
    d3d11->device_context = context;
    d3d11->BindFlags = bind_flags;
}

// The frame size comes from the peer's SPS, and FFmpeg sizes its pool of
// about 20 decoder surfaces by it before the GPU gets a say. The largest
// frame of H.264 level 5.2, 36864 macroblocks (4096x2304; a 5120x1440
// ultrawide share is 28800), keeps that pool under 300 MB of video memory.
// Booth never sends more than 1440 lines.
//
// FFmpeg rounds an HEVC pool up to 128 in both directions rather than 16
// (libavcodec/dxva2.c), so for HEVC the limit counts 16x16 blocks of the
// size rounded that way, which bounds the pool the same. Rounded by 16 alone,
// a 648x14384 HEVC picture would pass and get a 768x14464 pool, about 330 MB.
// 4096x2304 is exactly the limit in both codecs.
//
// FFmpeg's H.264 decoder asks for a format before it sizes anything for a
// new SPS. Its HEVC decoder first sizes its own tables by the SPS in CPU
// memory, about 165 MB for about 25 ms at 16384x16000 (FFmpeg 8.1.3), so
// src/guard.rs applies this limit to every HEVC SPS in Rust before FFmpeg
// sees the access unit, and for HEVC the check in pick_d3d11 only backs it
// up. No video memory is taken for either.
const int booth_max_macroblocks = 36864;

// The probe and the HEVC guard ask this too, so neither passes a size the
// format callback would refuse.
int booth_too_large(int codec_id, int64_t width, int64_t height)
{
    int64_t round = codec_id == AV_CODEC_ID_HEVC ? 128 : 16;
    int64_t across = (width + round - 1) / round * round / 16;
    int64_t down = (height + round - 1) / round * round / 16;
    return across * down > booth_max_macroblocks;
}

// What pick_d3d11 did during one decode call, through AVCodecContext.opaque:
// chose d3d11va, or turned the stream down and why.
const int booth_chose_d3d11 = 1;
const int booth_refused_too_large = 2;
// Not 8-bit 4:2:0, the one format the viewer draws (NV12). FFmpeg has no
// hardware path for any other H.264 anyway. HEVC Main 10 decodes on most
// GPUs, but into P010, and Booth never sends it.
const int booth_refused_format = 3;
// d3d11va was chosen and FFmpeg could not set it up, so it asked again
// without it: the GPU's decoder did not take the size or profile, video
// memory ran out, or the device was lost. FFmpeg keeps the reason to itself.
const int booth_refused_setup = 4;

// FFmpeg asks each time the stream's size or profile changes (HEVC: each
// time a different SPS takes over), and offers the software formats only
// once d3d11va has turned the stream down. Software decode would be slow
// and hand back CPU memory the viewer cannot present, so the stream is
// refused instead.
static enum AVPixelFormat pick_d3d11(AVCodecContext *codec, const enum AVPixelFormat *formats)
{
    int *choice = codec->opaque;
    if (booth_too_large(codec->codec_id, codec->coded_width, codec->coded_height)) {
        *choice = booth_refused_too_large;
        return AV_PIX_FMT_NONE;
    }
    // FFmpeg sets it before it asks. The J format is its older name for the
    // same samples in full range.
    if (codec->sw_pix_fmt != AV_PIX_FMT_YUV420P && codec->sw_pix_fmt != AV_PIX_FMT_YUVJ420P) {
        *choice = booth_refused_format;
        return AV_PIX_FMT_NONE;
    }
    for (; *formats != AV_PIX_FMT_NONE; formats++) {
        if (*formats == AV_PIX_FMT_D3D11) {
            *choice = booth_chose_d3d11;
            return AV_PIX_FMT_D3D11;
        }
    }
    *choice = *choice == booth_chose_d3d11 ? booth_refused_setup : booth_refused_format;
    return AV_PIX_FMT_NONE;
}

// Takes over the reference to `device`. One thread, because every frame
// thread holds a frame back.
//
// HEVC gets output-corrupt, so a frame that predicts from a lost one comes
// out damaged, as H.264 frames after the first IDR do without it. Without
// it FFmpeg's HEVC decoder skips such a frame, and the skip drops every
// other reference from its picture buffer too (FFmpeg 8.1.3), so the frame
// after an invalidation, which predicts from one before the loss, did not
// decode either and nothing did until the next IDR. It also makes pictures
// out of P frames with nothing at all to predict from, before the first IDR
// or after a reset, where H.264 gives none; src/decoder.rs keeps those from
// FFmpeg.
void booth_codec_setup(AVCodecContext *codec, AVBufferRef *device, int *choice)
{
    codec->hw_device_ctx = device;
    codec->get_format = pick_d3d11;
    codec->opaque = choice;
    codec->flags |= AV_CODEC_FLAG_LOW_DELAY;
    if (codec->codec_id == AV_CODEC_ID_HEVC) {
        codec->flags |= AV_CODEC_FLAG_OUTPUT_CORRUPT;
    }
    codec->thread_count = 1;
}

void booth_codec_stream(const AVCodecContext *codec, int *width, int *height, int *profile,
                        int *level)
{
    *width = codec->coded_width;
    *height = codec->coded_height;
    *profile = codec->profile;
    *level = codec->level;
}

// After pick_d3d11 turns a stream down, FFmpeg has taken d3d11va down but
// leaves d3d11 as the context's format. The H.264 decoder takes a format that
// is still in its list without asking again, so once a 10-bit SPS had been
// refused, the next 8-bit IDR went straight to d3d11 with nothing behind it
// and every frame after failed in get_buffer (FFmpeg 8.1.3). With no format
// it has to ask.
void booth_codec_forget_format(AVCodecContext *codec)
{
    codec->pix_fmt = AV_PIX_FMT_NONE;
}

// Frames the decoder holds back to put them in display order. A stream can
// raise it through its SPS; Booth's never do.
int booth_codec_reorder(const AVCodecContext *codec)
{
    return codec->has_b_frames;
}

void booth_codec_clear_reorder(AVCodecContext *codec)
{
    codec->has_b_frames = 0;
}

void booth_packet_set(AVPacket *packet, uint8_t *data, int size, int64_t pts)
{
    packet->data = data;
    packet->size = size;
    packet->pts = pts;
}

// For AV_PIX_FMT_D3D11, data[0] is the texture and data[1] its index in the
// array (pixfmt.h).
int booth_frame_read(const AVFrame *frame, ID3D11Texture2D **texture, intptr_t *index, int *width,
                     int *height, int64_t *pts)
{
    *texture = (ID3D11Texture2D *)frame->data[0];
    *index = (intptr_t)frame->data[1];
    *width = frame->width;
    *height = frame->height;
    *pts = frame->pts;
    return frame->format;
}
