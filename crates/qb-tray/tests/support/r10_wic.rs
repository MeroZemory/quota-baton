//! Shared production dashboard renderer at genuine WIC 192 DPI, not HWND evidence.
use qb_tray::{
    geometry::Scene,
    render::{Interaction, Renderer},
};
use std::path::Path;
use windows::{
    Win32::{
        Foundation::E_FAIL,
        Graphics::{
            Direct2D::{
                Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT},
                D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1CreateFactory, ID2D1Factory,
            },
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Imaging::{
                CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppPBGRA,
                IWICImagingFactory, WICBitmapCacheOnLoad, WICBitmapEncoderNoCache,
            },
        },
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
    },
    core::{Error, PCWSTR, Result},
};

pub fn save(
    path: &Path,
    scene: &Scene,
    interaction: &Interaction,
    dark: bool,
    size: [f32; 2],
) -> Result<()> {
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let width = size[0] as u32 * 2;
        let height = size[1] as u32 * 2;
        let bitmap = wic.CreateBitmap(
            width,
            height,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnLoad,
        )?;
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let target = factory.CreateWicBitmapRenderTarget(
            &bitmap,
            &D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 192.,
                dpiY: 192.,
                ..Default::default()
            },
        )?;
        Renderer::new(dark)?.paint(&target, scene, interaction)?;
        let stream = wic.CreateStream()?;
        let wide: Vec<_> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), 0x40000000)?;
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let (mut frame, mut options) = (None, None);
        encoder.CreateNewFrame(&mut frame, &mut options)?;
        let frame = frame.ok_or_else(|| Error::from_hresult(E_FAIL))?;
        frame.Initialize(None)?;
        frame.SetSize(width, height)?;
        frame.SetResolution(192., 192.)?;
        let mut format = GUID_WICPixelFormat32bppPBGRA;
        frame.SetPixelFormat(&mut format)?;
        frame.WriteSource(&bitmap, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}
