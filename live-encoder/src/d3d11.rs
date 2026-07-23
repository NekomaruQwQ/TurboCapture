//! D3D11 texture allocation local to the encoding pipeline.

use nkcore::prelude::*;
use nkcore::debug::*;
use nkcore::*;

use euclid::Size2D;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Create a GPU-default 2D texture with the given format and bind flags.
pub fn create_texture_2d(
    device: &ID3D11Device,
    size: Size2D<u32>,
    format: DXGI_FORMAT,
    bind_flags: &[D3D11_BIND_FLAG])
    -> anyhow::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.width,
        Height: size.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags:
            bind_flags
                .iter()
                .map(|flag| flag.0 as u32)
                .sum(),
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    // SAFETY: `device` is valid; `desc` is a stack-local struct with valid fields.
    out_var_or_err(|out| api_call!(unsafe {
        device.CreateTexture2D(
            &raw const desc,
            None,
            Some(out))
    }))?.ok_or_else(|| anyhow::anyhow!("failed to create texture"))
}

