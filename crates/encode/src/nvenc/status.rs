use super::ffi::*;

pub(crate) fn name(status: NVENCSTATUS) -> &'static str {
    match status {
        NV_ENC_SUCCESS => "NV_ENC_SUCCESS",
        NV_ENC_ERR_NO_ENCODE_DEVICE => "NV_ENC_ERR_NO_ENCODE_DEVICE",
        NV_ENC_ERR_UNSUPPORTED_DEVICE => "NV_ENC_ERR_UNSUPPORTED_DEVICE",
        NV_ENC_ERR_INVALID_ENCODERDEVICE => "NV_ENC_ERR_INVALID_ENCODERDEVICE",
        NV_ENC_ERR_INVALID_DEVICE => "NV_ENC_ERR_INVALID_DEVICE",
        NV_ENC_ERR_DEVICE_NOT_EXIST => "NV_ENC_ERR_DEVICE_NOT_EXIST",
        NV_ENC_ERR_INVALID_PTR => "NV_ENC_ERR_INVALID_PTR",
        NV_ENC_ERR_INVALID_EVENT => "NV_ENC_ERR_INVALID_EVENT",
        NV_ENC_ERR_INVALID_PARAM => "NV_ENC_ERR_INVALID_PARAM",
        NV_ENC_ERR_INVALID_CALL => "NV_ENC_ERR_INVALID_CALL",
        NV_ENC_ERR_OUT_OF_MEMORY => "NV_ENC_ERR_OUT_OF_MEMORY",
        NV_ENC_ERR_ENCODER_NOT_INITIALIZED => "NV_ENC_ERR_ENCODER_NOT_INITIALIZED",
        NV_ENC_ERR_UNSUPPORTED_PARAM => "NV_ENC_ERR_UNSUPPORTED_PARAM",
        NV_ENC_ERR_LOCK_BUSY => "NV_ENC_ERR_LOCK_BUSY",
        NV_ENC_ERR_NOT_ENOUGH_BUFFER => "NV_ENC_ERR_NOT_ENOUGH_BUFFER",
        NV_ENC_ERR_INVALID_VERSION => "NV_ENC_ERR_INVALID_VERSION",
        NV_ENC_ERR_MAP_FAILED => "NV_ENC_ERR_MAP_FAILED",
        NV_ENC_ERR_NEED_MORE_INPUT => "NV_ENC_ERR_NEED_MORE_INPUT",
        NV_ENC_ERR_ENCODER_BUSY => "NV_ENC_ERR_ENCODER_BUSY",
        NV_ENC_ERR_EVENT_NOT_REGISTERD => "NV_ENC_ERR_EVENT_NOT_REGISTERD",
        NV_ENC_ERR_GENERIC => "NV_ENC_ERR_GENERIC",
        NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
        NV_ENC_ERR_UNIMPLEMENTED => "NV_ENC_ERR_UNIMPLEMENTED",
        NV_ENC_ERR_RESOURCE_REGISTER_FAILED => "NV_ENC_ERR_RESOURCE_REGISTER_FAILED",
        NV_ENC_ERR_RESOURCE_NOT_REGISTERED => "NV_ENC_ERR_RESOURCE_NOT_REGISTERED",
        NV_ENC_ERR_RESOURCE_NOT_MAPPED => "NV_ENC_ERR_RESOURCE_NOT_MAPPED",
        NV_ENC_ERR_NEED_MORE_OUTPUT => "NV_ENC_ERR_NEED_MORE_OUTPUT",
        _ => "an NVENC status this version of Booth does not know",
    }
}

pub(crate) const OPEN_SESSION: &str = "open a session";

pub(crate) fn meaning(action: &str, status: NVENCSTATUS) -> &'static str {
    match status {
        // GeForce drivers cap how many sessions can be open on the whole PC
        // at once, and report the cap this way when a session is opened.
        NV_ENC_ERR_OUT_OF_MEMORY if action == OPEN_SESSION => "too many encode sessions are open",
        NV_ENC_ERR_OUT_OF_MEMORY => "the GPU is out of memory",
        NV_ENC_ERR_NO_ENCODE_DEVICE => "this GPU has no video encoder",
        NV_ENC_ERR_UNSUPPORTED_DEVICE | NV_ENC_ERR_INVALID_DEVICE => {
            "the driver does not accept this Direct3D device"
        }
        NV_ENC_ERR_DEVICE_NOT_EXIST => "the GPU was removed or its driver restarted",
        NV_ENC_ERR_INVALID_VERSION => "the driver does not accept NVENC API 12.2 structures",
        NV_ENC_ERR_UNSUPPORTED_PARAM | NV_ENC_ERR_INVALID_PARAM => "the driver refused a setting",
        NV_ENC_ERR_ENCODER_BUSY | NV_ENC_ERR_LOCK_BUSY => "the encoder is busy",
        NV_ENC_ERR_MAP_FAILED => "the frame texture could not be handed to the encoder",
        NV_ENC_ERR_RESOURCE_REGISTER_FAILED => "the frame texture could not be registered",
        NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => "the driver refused this program",
        NV_ENC_ERR_GENERIC => "the driver reported an internal error",
        _ => "the driver reported an error",
    }
}
