use std::ffi::c_void;

use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11PixelShader, ID3D11VertexShader};
use windows::core::PCSTR;

use crate::error::{ViewerError, meaning};

// Compiled at runtime with d3dcompiler_47.dll, which ships with Windows,
// the way the capture crate does it: no build step and no compiled blobs in
// the repository.
pub(crate) fn compile(
    source: &'static str,
    file: PCSTR,
    entry: PCSTR,
    target: PCSTR,
) -> Result<Vec<u8>, ViewerError> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source pointer and length describe a static string; the
    // names are NUL-terminated literals; both out parameters are live.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            file,
            None,
            None,
            entry,
            target,
            D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    // SAFETY: both names are NUL-terminated literals.
    let (file, entry) = unsafe {
        (
            file.to_string().unwrap_or_default(),
            entry.to_string().unwrap_or_default(),
        )
    };
    match (compiled, code) {
        (Ok(()), Some(code)) => Ok(blob_bytes(&code).to_vec()),
        (result, _) => {
            let detail = errors
                .map(|blob| {
                    String::from_utf8_lossy(blob_bytes(&blob))
                        .trim()
                        .to_string()
                })
                .filter(|text| !text.is_empty())
                .or_else(|| result.err().map(|err| meaning(&err)))
                .unwrap_or_else(|| "no code came back".to_string());
            Err(ViewerError::other(format!(
                "could not compile the viewer's shader {file} ({entry}) with d3dcompiler_47.dll: {detail}"
            )))
        }
    }
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob owns this many bytes at this pointer for as long as
    // it lives, and the slice borrows the blob.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

pub(crate) fn vertex(
    device: &ID3D11Device,
    code: &[u8],
    what: &str,
) -> Result<ID3D11VertexShader, ViewerError> {
    let mut shader = None;
    // SAFETY: a whole compiled shader and a live out parameter.
    unsafe { device.CreateVertexShader(code, None, Some(&mut shader)) }
        .map_err(|err| ViewerError::windows(format!("load the {what} shader"), &err))?;
    shader.ok_or_else(|| ViewerError::missing(format!("load the {what} shader"), "shader"))
}

pub(crate) fn pixel(
    device: &ID3D11Device,
    code: &[u8],
    what: &str,
) -> Result<ID3D11PixelShader, ViewerError> {
    let mut shader = None;
    // SAFETY: a whole compiled shader and a live out parameter.
    unsafe { device.CreatePixelShader(code, None, Some(&mut shader)) }
        .map_err(|err| ViewerError::windows(format!("load the {what} shader"), &err))?;
    shader.ok_or_else(|| ViewerError::missing(format!("load the {what} shader"), "shader"))
}
