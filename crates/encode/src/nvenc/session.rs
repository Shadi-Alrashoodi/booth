// The NVENC driver behind a small safe surface: loading nvEncodeAPI64.dll,
// one encode session, the input textures registered with it and its one
// bitstream buffer. Every unsafe call of the encoder is in this file.

use std::ffi::{CStr, c_void};
use std::ptr;
use std::time::Instant;

use windows::Win32::Foundation::{ERROR_MOD_NOT_FOUND, FreeLibrary, HMODULE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{HRESULT, Interface, PCSTR, s, w};

use super::ffi::*;
use super::status::OPEN_SESSION;
use crate::{Codec, EncodeError};

// The capture pool hands two or three textures around. More than this many
// means the pool was rebuilt, and the oldest registrations are for textures
// that will not come back.
const MAX_REGISTRATIONS: usize = 6;

struct Library(HMODULE);

impl Library {
    fn load() -> Result<Library, EncodeError> {
        // System32 only, never the exe's folder or the working directory:
        // a nvEncodeAPI64.dll dropped next to booth.exe must not be loaded.
        // SAFETY: the name is a NUL-terminated wide string literal.
        let module =
            unsafe { LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) };
        match module {
            Ok(module) => Ok(Library(module)),
            Err(e) if e.code() == HRESULT::from_win32(ERROR_MOD_NOT_FOUND.0) => {
                Err(EncodeError::NvencMissing)
            }
            Err(source) => Err(EncodeError::NvencLoad { source }),
        }
    }

    fn export(
        &self,
        name: PCSTR,
        text: &'static str,
    ) -> Result<unsafe extern "system" fn() -> isize, EncodeError> {
        // SAFETY: the module is loaded for as long as self lives and the name
        // is a NUL-terminated string literal.
        unsafe { GetProcAddress(self.0, name) }
            .ok_or(EncodeError::NvencExportMissing { name: text })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the handle came from LoadLibraryExW and is freed once. The
        // session that used code from it is destroyed before this runs.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}

/// The driver functions the encoder calls, checked present once at open.
struct Functions {
    get_encode_guid_count: PNVENCGETENCODEGUIDCOUNT,
    get_encode_guids: PNVENCGETENCODEGUIDS,
    get_encode_caps: PNVENCGETENCODECAPS,
    get_encode_preset_config_ex: PNVENCGETENCODEPRESETCONFIGEX,
    initialize_encoder: PNVENCINITIALIZEENCODER,
    create_bitstream_buffer: PNVENCCREATEBITSTREAMBUFFER,
    destroy_bitstream_buffer: PNVENCDESTROYBITSTREAMBUFFER,
    encode_picture: PNVENCENCODEPICTURE,
    lock_bitstream: PNVENCLOCKBITSTREAM,
    unlock_bitstream: PNVENCUNLOCKBITSTREAM,
    map_input_resource: PNVENCMAPINPUTRESOURCE,
    unmap_input_resource: PNVENCUNMAPINPUTRESOURCE,
    destroy_encoder: PNVENCDESTROYENCODER,
    invalidate_ref_frames: PNVENCINVALIDATEREFFRAMES,
    open_encode_session_ex: PNVENCOPENENCODESESSIONEX,
    register_resource: PNVENCREGISTERRESOURCE,
    unregister_resource: PNVENCUNREGISTERRESOURCE,
    reconfigure_encoder: PNVENCRECONFIGUREENCODER,
    get_last_error_string: PNVENCGETLASTERROR,
}

impl Functions {
    fn from_list(list: &NV_ENCODE_API_FUNCTION_LIST) -> Result<Functions, EncodeError> {
        fn need<T>(f: Option<T>, name: &'static str) -> Result<T, EncodeError> {
            f.ok_or(EncodeError::NvencExportMissing { name })
        }
        Ok(Functions {
            get_encode_guid_count: need(list.nvEncGetEncodeGUIDCount, "nvEncGetEncodeGUIDCount")?,
            get_encode_guids: need(list.nvEncGetEncodeGUIDs, "nvEncGetEncodeGUIDs")?,
            get_encode_caps: need(list.nvEncGetEncodeCaps, "nvEncGetEncodeCaps")?,
            get_encode_preset_config_ex: need(
                list.nvEncGetEncodePresetConfigEx,
                "nvEncGetEncodePresetConfigEx",
            )?,
            initialize_encoder: need(list.nvEncInitializeEncoder, "nvEncInitializeEncoder")?,
            create_bitstream_buffer: need(
                list.nvEncCreateBitstreamBuffer,
                "nvEncCreateBitstreamBuffer",
            )?,
            destroy_bitstream_buffer: need(
                list.nvEncDestroyBitstreamBuffer,
                "nvEncDestroyBitstreamBuffer",
            )?,
            encode_picture: need(list.nvEncEncodePicture, "nvEncEncodePicture")?,
            lock_bitstream: need(list.nvEncLockBitstream, "nvEncLockBitstream")?,
            unlock_bitstream: need(list.nvEncUnlockBitstream, "nvEncUnlockBitstream")?,
            map_input_resource: need(list.nvEncMapInputResource, "nvEncMapInputResource")?,
            unmap_input_resource: need(list.nvEncUnmapInputResource, "nvEncUnmapInputResource")?,
            destroy_encoder: need(list.nvEncDestroyEncoder, "nvEncDestroyEncoder")?,
            invalidate_ref_frames: need(list.nvEncInvalidateRefFrames, "nvEncInvalidateRefFrames")?,
            open_encode_session_ex: need(
                list.nvEncOpenEncodeSessionEx,
                "nvEncOpenEncodeSessionEx",
            )?,
            register_resource: need(list.nvEncRegisterResource, "nvEncRegisterResource")?,
            unregister_resource: need(list.nvEncUnregisterResource, "nvEncUnregisterResource")?,
            reconfigure_encoder: need(list.nvEncReconfigureEncoder, "nvEncReconfigureEncoder")?,
            get_last_error_string: need(list.nvEncGetLastErrorString, "nvEncGetLastErrorString")?,
        })
    }
}

struct Registration {
    // Held so the texture cannot be freed, and its address reused by a new
    // texture, while the driver still has it registered.
    texture: ID3D11Texture2D,
    handle: NV_ENC_REGISTERED_PTR,
    last_used: u64,
}

/// The settings a session was started with, kept to change the bitrate
/// later.
pub(crate) struct Params {
    pub(crate) init: NV_ENC_INITIALIZE_PARAMS,
    pub(crate) config: NV_ENC_CONFIG,
}

// SAFETY: the raw pointers in both structs are the header's reserved fields,
// which stay null, and init.encodeConfig, which this file sets only for the
// length of a driver call. Nothing they point at is shared.
unsafe impl Send for Params {}

/// What one frame asks of the encoder.
pub(crate) struct Picture {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) timestamp: u64,
    pub(crate) force_idr: bool,
}

pub(crate) struct Encoded {
    pub(crate) data: Vec<u8>,
    pub(crate) picture_type: u32,
    pub(crate) submitted: Instant,
    pub(crate) ready: Instant,
}

/// A frame that did not come out. Once the driver has the frame it may keep
/// it as a reference although its bytes never reach the caller, so the caller
/// needs to know which side of the submit the failure was on.
pub(crate) struct Failed {
    pub(crate) error: EncodeError,
    pub(crate) submitted: bool,
}

fn before_submit(error: EncodeError) -> Failed {
    Failed {
        error,
        submitted: false,
    }
}

fn after_submit(error: EncodeError) -> Failed {
    Failed {
        error,
        submitted: true,
    }
}

pub(crate) struct Session {
    encoder: *mut c_void,
    fns: Functions,
    bitstream: NV_ENC_OUTPUT_PTR,
    registrations: Vec<Registration>,
    uses: u64,
    device: ID3D11Device,
    // Makes the next unlock report a failure after the frame was encoded
    // and copied, the one driver failure a test cannot cause for real.
    #[cfg(any(test, feature = "fault"))]
    pub(crate) fail_next_unlock: bool,
    // Last, so the DLL outlives every call into it, Drop included.
    _library: Library,
}

// SAFETY: an NVENC session may be used from any thread as long as calls do
// not overlap. Session is not Sync, and every call that changes driver state
// takes &mut self, so they cannot. The Direct3D device the session works
// through has a rule of its own for threads, which the doc on open_codec()
// in lib.rs hands to the caller.
unsafe impl Send for Session {}

impl Session {
    pub(crate) fn open(device: &ID3D11Device) -> Result<Session, EncodeError> {
        let library = Library::load()?;

        let get_max = library.export(
            s!("NvEncodeAPIGetMaxSupportedVersion"),
            "NvEncodeAPIGetMaxSupportedVersion",
        )?;
        // SAFETY: the export has this signature in every driver that ships
        // it (nvEncodeAPI.h, NvEncodeAPIGetMaxSupportedVersion).
        let get_max: NvEncodeAPIGetMaxSupportedVersion = unsafe { std::mem::transmute(get_max) };
        let mut version = 0;
        // SAFETY: `version` is a valid u32 for the call to write.
        let status = unsafe { get_max(&mut version) };
        if status != NV_ENC_SUCCESS {
            return Err(nvenc_error("report its API version", status, String::new()));
        }
        // The low 4 bits are the minor version, the rest the major.
        let (major, minor) = (version >> 4, version & 0xf);
        if (major, minor) < (NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION) {
            return Err(EncodeError::NvencTooOld { major, minor });
        }

        let create =
            library.export(s!("NvEncodeAPICreateInstance"), "NvEncodeAPICreateInstance")?;
        // SAFETY: as above, the signature of NvEncodeAPICreateInstance.
        let create: NvEncodeAPICreateInstance = unsafe { std::mem::transmute(create) };
        let mut list: Box<NV_ENCODE_API_FUNCTION_LIST> = Box::new(zeroed());
        list.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        // SAFETY: `list` is a zeroed function list of the version this code
        // was written against, which the driver fills in.
        let status = unsafe { create(&mut *list) };
        if status != NV_ENC_SUCCESS {
            return Err(nvenc_error("list its functions", status, String::new()));
        }
        let fns = Functions::from_list(&list)?;

        let mut params: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = zeroed();
        params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
        params.device = device.as_raw();
        params.apiVersion = NVENCAPI_VERSION;
        let mut encoder = ptr::null_mut();
        // SAFETY: `params` is filled as the header asks and names a live
        // D3D11 device that `device` keeps alive for the session's life (a
        // clone is stored below). `encoder` is a valid out pointer.
        let status = unsafe { (fns.open_encode_session_ex)(&mut params, &mut encoder) };
        if status != NV_ENC_SUCCESS {
            let mut detail = String::new();
            if !encoder.is_null() {
                detail = last_error(&fns, encoder);
                // The header asks for the half-open session to be destroyed.
                // SAFETY: `encoder` came from the failed open and is not used
                // after this.
                unsafe { (fns.destroy_encoder)(encoder) };
            }
            return Err(nvenc_error(OPEN_SESSION, status, detail));
        }

        Ok(Session {
            encoder,
            fns,
            bitstream: ptr::null_mut(),
            registrations: Vec::new(),
            uses: 0,
            device: device.clone(),
            #[cfg(any(test, feature = "fault"))]
            fail_next_unlock: false,
            _library: library,
        })
    }

    fn error(&self, action: &'static str, status: NVENCSTATUS) -> EncodeError {
        nvenc_error(action, status, last_error(&self.fns, self.encoder))
    }

    /// Whether the GPU's encoder lists `codec` at all. Asked before any caps,
    /// so a GPU without HEVC gets a sentence saying so rather than whatever
    /// status its first caps call fails with.
    pub(crate) fn offers_codec(&self, codec: Codec) -> Result<bool, EncodeError> {
        let mut count = 0;
        // SAFETY: the session is open and `count` is a valid out pointer.
        let status = unsafe { (self.fns.get_encode_guid_count)(self.encoder, &mut count) };
        if status != NV_ENC_SUCCESS {
            return Err(self.error("list the codecs it encodes", status));
        }
        // Three today (H.264, HEVC, AV1); anything past 64 is not a list.
        let mut guids = vec![GUID::zeroed(); count.min(64) as usize];
        let mut written = 0;
        // SAFETY: `guids` has room for the length passed, and the driver
        // writes at most that many and says how many in `written`.
        let status = unsafe {
            (self.fns.get_encode_guids)(
                self.encoder,
                guids.as_mut_ptr(),
                guids.len() as u32,
                &mut written,
            )
        };
        if status != NV_ENC_SUCCESS {
            return Err(self.error("list the codecs it encodes", status));
        }
        guids.truncate(written as usize);
        Ok(guids.contains(&codec_guid(codec)))
    }

    pub(crate) fn cap(&self, codec: Codec, cap: NV_ENC_CAPS) -> Result<i32, EncodeError> {
        let mut params: NV_ENC_CAPS_PARAM = zeroed();
        params.version = NV_ENC_CAPS_PARAM_VER;
        params.capsToQuery = cap;
        let mut value = 0;
        // SAFETY: the session is open, `params` is a valid caps query and
        // `value` a valid out pointer.
        let status = unsafe {
            (self.fns.get_encode_caps)(self.encoder, codec_guid(codec), &mut params, &mut value)
        };
        if status != NV_ENC_SUCCESS {
            let action = match codec {
                Codec::H264 => "report what its H.264 encoder can do",
                Codec::Hevc => "report what its HEVC encoder can do",
            };
            return Err(self.error(action, status));
        }
        Ok(value)
    }

    pub(crate) fn preset_config(
        &self,
        codec: Codec,
        preset: GUID,
    ) -> Result<NV_ENC_CONFIG, EncodeError> {
        let mut params: Box<NV_ENC_PRESET_CONFIG> = Box::new(zeroed());
        params.version = NV_ENC_PRESET_CONFIG_VER;
        params.presetCfg.version = NV_ENC_CONFIG_VER;
        // SAFETY: the session is open and `params` is a zeroed preset config
        // with both version fields set, as the header asks.
        let status = unsafe {
            (self.fns.get_encode_preset_config_ex)(
                self.encoder,
                codec_guid(codec),
                preset,
                NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut *params,
            )
        };
        if status != NV_ENC_SUCCESS {
            return Err(self.error("read its preset settings", status));
        }
        Ok(params.presetCfg)
    }

    /// Initializes the encoder, then creates the bitstream buffer every frame
    /// is written to.
    pub(crate) fn initialize(&mut self, params: &mut Params) -> Result<(), EncodeError> {
        params.init.encodeConfig = &mut params.config;
        // SAFETY: the session is open and not yet initialized; `init` points
        // at `config` next to it, and both outlive the call.
        let status = unsafe { (self.fns.initialize_encoder)(self.encoder, &mut params.init) };
        params.init.encodeConfig = ptr::null_mut();
        if status != NV_ENC_SUCCESS {
            let action = if params.init.encodeGUID == NV_ENC_CODEC_HEVC_GUID {
                "start the HEVC encoder with Booth's settings"
            } else {
                "start the H.264 encoder with Booth's settings"
            };
            return Err(self.error(action, status));
        }

        let mut params: NV_ENC_CREATE_BITSTREAM_BUFFER = zeroed();
        params.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        // SAFETY: the encoder is initialized and `params` is valid.
        let status = unsafe { (self.fns.create_bitstream_buffer)(self.encoder, &mut params) };
        if status != NV_ENC_SUCCESS {
            return Err(self.error("create its output buffer", status));
        }
        self.bitstream = params.bitstreamBuffer;
        Ok(())
    }

    /// Applies changed settings from the next frame on, without an IDR and
    /// without resetting the rate control.
    pub(crate) fn reconfigure(&mut self, params: &mut Params) -> Result<(), EncodeError> {
        let mut reconfigure: Box<NV_ENC_RECONFIGURE_PARAMS> = Box::new(zeroed());
        reconfigure.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconfigure.reInitEncodeParams = params.init;
        reconfigure.reInitEncodeParams.encodeConfig = &mut params.config;
        reconfigure.set_reset_encoder(0);
        reconfigure.set_force_idr(0);
        // SAFETY: the encoder is initialized; `reconfigure` holds a copy of
        // the initialization parameters pointing at `params.config`, which
        // outlives the call.
        let status = unsafe { (self.fns.reconfigure_encoder)(self.encoder, &mut *reconfigure) };
        if status != NV_ENC_SUCCESS {
            return Err(self.error("change the bitrate", status));
        }
        Ok(())
    }

    /// Stops the encoder referencing the frame encoded with `timestamp`.
    pub(crate) fn invalidate(&mut self, timestamp: u64) -> Result<(), EncodeError> {
        // SAFETY: the encoder is initialized; the timestamp is plain data.
        let status = unsafe { (self.fns.invalidate_ref_frames)(self.encoder, timestamp) };
        if status != NV_ENC_SUCCESS {
            return Err(self.error("stop referencing a lost frame", status));
        }
        Ok(())
    }

    /// Encodes one frame and waits for it: map the texture, encode, lock the
    /// bitstream (which blocks until the frame is done), copy it out, unlock,
    /// unmap. One frame in flight, nothing queued.
    pub(crate) fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        picture: &Picture,
    ) -> Result<Encoded, Failed> {
        let registered = self
            .registration(texture, picture.width, picture.height)
            .map_err(before_submit)?;

        let mut map: NV_ENC_MAP_INPUT_RESOURCE = zeroed();
        map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        map.registeredResource = registered;
        // SAFETY: `registered` is a live registration of this session.
        let status = unsafe { (self.fns.map_input_resource)(self.encoder, &mut map) };
        if status != NV_ENC_SUCCESS {
            return Err(before_submit(self.error("take the frame texture", status)));
        }
        let mapped = Mapped {
            encoder: self.encoder,
            unmap: self.fns.unmap_input_resource,
            input: map.mappedResource,
        };

        let mut pic: Box<NV_ENC_PIC_PARAMS> = Box::new(zeroed());
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.inputWidth = picture.width;
        pic.inputHeight = picture.height;
        pic.inputPitch = picture.width;
        pic.inputBuffer = mapped.input;
        pic.outputBitstream = self.bitstream;
        pic.bufferFmt = map.mappedBufferFmt;
        pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        // The timestamp is what reference invalidation names a frame by.
        pic.inputTimeStamp = picture.timestamp;
        pic.frameIdx = picture.timestamp as u32;
        if picture.force_idr {
            pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        }

        let submitted = Instant::now();
        // SAFETY: the input is mapped, the bitstream buffer belongs to this
        // session and is not locked, and `pic` is valid for the call.
        let status = unsafe { (self.fns.encode_picture)(self.encoder, &mut *pic) };
        // Counted as submitted even when the call fails: nothing says how
        // far the driver got, and an unneeded IDR costs one big frame where
        // a reference the viewer never had costs a broken picture.
        if status != NV_ENC_SUCCESS {
            return Err(after_submit(self.error("encode a frame", status)));
        }

        let mut lock: Box<NV_ENC_LOCK_BITSTREAM> = Box::new(zeroed());
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream;
        // SAFETY: the bitstream buffer holds the frame just submitted; with
        // doNotWait left at 0 the call blocks until the frame is done.
        let status = unsafe { (self.fns.lock_bitstream)(self.encoder, &mut *lock) };
        if status != NV_ENC_SUCCESS {
            return Err(after_submit(self.error("read the encoded frame", status)));
        }
        let ready = Instant::now();

        let data = if lock.bitstreamSizeInBytes == 0 {
            Vec::new()
        } else {
            // SAFETY: while locked, bitstreamBufferPtr points at
            // bitstreamSizeInBytes readable bytes owned by the driver. They
            // are copied before the unlock below.
            unsafe {
                std::slice::from_raw_parts(
                    lock.bitstreamBufferPtr as *const u8,
                    lock.bitstreamSizeInBytes as usize,
                )
            }
            .to_vec()
        };
        // SAFETY: the buffer was locked above and nothing refers to its
        // memory any more.
        let status = unsafe { (self.fns.unlock_bitstream)(self.encoder, self.bitstream) };
        #[cfg(any(test, feature = "fault"))]
        let status = if std::mem::take(&mut self.fail_next_unlock) {
            NV_ENC_ERR_GENERIC
        } else {
            status
        };
        if status != NV_ENC_SUCCESS {
            return Err(after_submit(
                self.error("release its output buffer", status),
            ));
        }
        drop(mapped);

        Ok(Encoded {
            data,
            picture_type: lock.pictureType,
            submitted,
            ready,
        })
    }

    fn registration(
        &mut self,
        texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<NV_ENC_REGISTERED_PTR, EncodeError> {
        self.uses += 1;
        if let Some(r) = self
            .registrations
            .iter_mut()
            .find(|r| r.texture.as_raw() == texture.as_raw())
        {
            r.last_used = self.uses;
            return Ok(r.handle);
        }

        crate::gpu::check_texture(&self.device, texture, width, height)?;
        let mut params: NV_ENC_REGISTER_RESOURCE = zeroed();
        params.version = NV_ENC_REGISTER_RESOURCE_VER;
        params.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        params.width = width;
        params.height = height;
        params.resourceToRegister = texture.as_raw();
        params.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
        params.bufferUsage = NV_ENC_INPUT_IMAGE;
        // SAFETY: the texture is a live NV12 texture of this size on the
        // session's device (checked above), and the clone stored below keeps
        // it alive until it is unregistered.
        let status = unsafe { (self.fns.register_resource)(self.encoder, &mut params) };
        if status != NV_ENC_SUCCESS {
            return Err(self.error("register the frame texture", status));
        }

        if self.registrations.len() == MAX_REGISTRATIONS {
            let oldest = (0..self.registrations.len())
                .min_by_key(|&i| self.registrations[i].last_used)
                .unwrap_or(0);
            let old = self.registrations.swap_remove(oldest);
            // SAFETY: the handle is a live registration of this session and
            // no mapping of it is outstanding (mappings end with each frame).
            unsafe { (self.fns.unregister_resource)(self.encoder, old.handle) };
        }
        self.registrations.push(Registration {
            texture: texture.clone(),
            handle: params.registeredResource,
            last_used: self.uses,
        });
        Ok(params.registeredResource)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The header's order: flush, free everything created in the session,
        // then the session. Mappings and locks never outlive a frame.
        if !self.bitstream.is_null() {
            // An end of stream picture flushes the encoder. With one frame
            // in flight it holds nothing by now; the header asks for the
            // flush anyway, and an asynchronous encoder would need it.
            let mut eos: Box<NV_ENC_PIC_PARAMS> = Box::new(zeroed());
            eos.version = NV_ENC_PIC_PARAMS_VER;
            eos.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
            // SAFETY: the bitstream buffer exists only once the encoder is
            // initialized, and an end of stream picture names no buffers.
            unsafe { (self.fns.encode_picture)(self.encoder, &mut *eos) };
            // SAFETY: the buffer belongs to this session and is not locked.
            unsafe { (self.fns.destroy_bitstream_buffer)(self.encoder, self.bitstream) };
        }
        for r in self.registrations.drain(..) {
            // SAFETY: a live, unmapped registration of this session.
            unsafe { (self.fns.unregister_resource)(self.encoder, r.handle) };
        }
        // SAFETY: the session is destroyed once, after everything created in
        // it, and never used again.
        unsafe { (self.fns.destroy_encoder)(self.encoder) };
    }
}

/// Unmaps the frame's input when the frame is done, including on an error
/// path, since a mapped input cannot be registered again or unregistered.
struct Mapped {
    encoder: *mut c_void,
    unmap: PNVENCUNMAPINPUTRESOURCE,
    input: NV_ENC_INPUT_PTR,
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: `input` was mapped in this session and is unmapped once.
        unsafe { (self.unmap)(self.encoder, self.input) };
    }
}

fn last_error(fns: &Functions, encoder: *mut c_void) -> String {
    // SAFETY: `encoder` is a session handle from this driver. The returned
    // string belongs to the driver and is copied at once.
    let text = unsafe { (fns.get_last_error_string)(encoder) };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the driver returns a NUL-terminated string.
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    // With no error of its own to report, the driver can hand back whatever
    // its buffer held, control bytes and all, which is no text for a log
    // line: asked after a release of the output that had gone through, as
    // crate::fault's NVENC failure does, the RTX 4070 Ti SUPER's gave a run
    // of 0x03 and 0x04 bytes.
    text.chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect::<String>()
        .trim()
        .to_string()
}

pub(crate) fn codec_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => NV_ENC_CODEC_H264_GUID,
        Codec::Hevc => NV_ENC_CODEC_HEVC_GUID,
    }
}

fn nvenc_error(action: &'static str, status: NVENCSTATUS, detail: String) -> EncodeError {
    EncodeError::Nvenc {
        action,
        status,
        detail,
    }
}
