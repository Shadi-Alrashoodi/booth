// What src/fields.c exports. FFmpeg's structs stay opaque on this side:
// only the C file, compiled against FFmpeg's headers, knows their layout.

use std::ffi::{c_int, c_uint, c_void};

#[repr(C)]
pub(crate) struct AVCodec {
    _opaque: [u8; 0],
}

#[repr(C)]
pub(crate) struct AVCodecContext {
    _opaque: [u8; 0],
}

#[repr(C)]
pub(crate) struct AVFrame {
    _opaque: [u8; 0],
}

#[repr(C)]
pub(crate) struct AVPacket {
    _opaque: [u8; 0],
}

#[repr(C)]
pub(crate) struct AVBufferRef {
    _opaque: [u8; 0],
}

#[repr(C)]
pub(crate) struct AVDictionary {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    pub(crate) safe static booth_avcodec_major: c_int;
    pub(crate) safe static booth_avcodec_minor: c_int;
    pub(crate) safe static booth_avutil_major: c_int;
    pub(crate) safe static booth_avutil_minor: c_int;
    pub(crate) safe static booth_codec_h264: c_int;
    pub(crate) safe static booth_codec_hevc: c_int;
    pub(crate) safe static booth_hwdevice_d3d11va: c_int;
    pub(crate) safe static booth_pix_fmt_d3d11: c_int;
    pub(crate) safe static booth_error_again: c_int;
    pub(crate) safe static booth_error_bare: c_int;
    pub(crate) safe static booth_padding: c_int;
    pub(crate) safe static booth_log_quiet: c_int;
    // fields.c applies it; Rust only checks that the size the error text
    // names matches it.
    #[cfg(test)]
    pub(crate) safe static booth_max_macroblocks: c_int;
    pub(crate) safe static booth_chose_d3d11: c_int;
    pub(crate) safe static booth_refused_too_large: c_int;
    pub(crate) safe static booth_refused_format: c_int;
    pub(crate) safe static booth_refused_setup: c_int;

    // Hands FFmpeg one reference to each of `d3d` (an ID3D11Device) and
    // `context` (its ID3D11DeviceContext), which it releases itself.
    pub(crate) fn booth_device_fill(
        device: *mut AVBufferRef,
        d3d: *mut c_void,
        context: *mut c_void,
        bind_flags: c_uint,
    );
    // Takes over the reference to `device`. `choice` must outlive the codec
    // context: FFmpeg's format callback sets it to booth_chose_d3d11, or to
    // booth_refused_* when it turns a stream down.
    pub(crate) fn booth_codec_setup(
        codec: *mut AVCodecContext,
        device: *mut AVBufferRef,
        choice: *mut c_int,
    );
    pub(crate) fn booth_codec_stream(
        codec: *const AVCodecContext,
        width: *mut c_int,
        height: *mut c_int,
        profile: *mut c_int,
        level: *mut c_int,
    );
    // Whether a picture of that coded size is past the limit the format
    // callback applies, which the probe and the HEVC guard apply too.
    // Arithmetic only.
    pub(crate) safe fn booth_too_large(codec: c_int, width: i64, height: i64) -> c_int;
    // For after a refusal, so the next IDR chooses a format afresh.
    pub(crate) fn booth_codec_forget_format(codec: *mut AVCodecContext);
    pub(crate) fn booth_codec_reorder(codec: *const AVCodecContext) -> c_int;
    pub(crate) fn booth_codec_clear_reorder(codec: *mut AVCodecContext);
    pub(crate) fn booth_packet_set(packet: *mut AVPacket, data: *mut u8, size: c_int, pts: i64);
    // Returns the frame's pixel format. `texture` is borrowed from the frame.
    pub(crate) fn booth_frame_read(
        frame: *const AVFrame,
        texture: *mut *mut c_void,
        index: *mut isize,
        width: *mut c_int,
        height: *mut c_int,
        pts: *mut i64,
    ) -> c_int;
}
