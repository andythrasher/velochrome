//! JPEG decoding with Windows' own codec (WIC). Its JPEG decoder can decode straight to 1/2, 1/4
//! or 1/8 size by skipping most of the work, which is what makes previews cheap: a culling
//! screen rarely needs every pixel of a 24-megapixel JPEG.

use std::cell::RefCell;

use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::{GUID, Interface, Result};

/// Decoded pixels, 8-bit BGRA (alpha unused), rows packed.
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

thread_local! {
    // One factory per thread: COM is joined once per thread, and WIC objects stay on it.
    static FACTORY: RefCell<Option<IWICImagingFactory>> = const { RefCell::new(None) };
}

fn factory() -> Result<IWICImagingFactory> {
    FACTORY.with(|f| {
        if let Some(factory) = &*f.borrow() {
            return Ok(factory.clone());
        }
        // Fails harmlessly if this thread already joined COM some other way.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let factory: IWICImagingFactory = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
        *f.borrow_mut() = Some(factory.clone());
        Ok(factory)
    })
}

/// The size of a JPEG without decoding it.
pub fn size(bytes: &[u8]) -> Result<(u32, u32)> {
    let frame = open(bytes)?;
    let (mut w, mut h) = (0, 0);
    unsafe { frame.GetSize(&mut w, &mut h)? };
    Ok((w, h))
}

fn open(bytes: &[u8]) -> Result<IWICBitmapFrameDecode> {
    let factory = factory()?;
    unsafe {
        let stream = factory.CreateStream()?;
        stream.InitializeFromMemory(bytes)?;
        let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        decoder.GetFrame(0)
    }
}

/// Decodes a JPEG at the smallest size whose longer side is at least `min_long_edge` (or full
/// size, if it's smaller than that). Pass 0 for full size.
pub fn decode(bytes: &[u8], min_long_edge: u32) -> Result<Pixels> {
    let frame = open(bytes)?;
    let (mut full_w, mut full_h) = (0, 0);
    unsafe { frame.GetSize(&mut full_w, &mut full_h)? };

    // Halve while the result still covers what's needed (the codec only does powers of two).
    let (mut w, mut h) = (full_w, full_h);
    while min_long_edge > 0 && w.max(h) / 2 >= min_long_edge && w.max(h) / 2 >= 1 {
        w /= 2;
        h /= 2;
    }

    // The fast path: the codec scales while it decodes.
    if let Ok(transform) = frame.cast::<IWICBitmapSourceTransform>() {
        let (mut tw, mut th) = (w, h);
        let mut format: GUID = GUID_WICPixelFormat32bppBGRA;
        unsafe {
            if transform.GetClosestSize(&mut tw, &mut th).is_ok()
                && transform.GetClosestPixelFormat(&mut format).is_ok()
                && (format == GUID_WICPixelFormat32bppBGRA || format == GUID_WICPixelFormat32bppBGR)
            {
                let mut bgra = vec![0u8; (tw * th * 4) as usize];
                transform.CopyPixels(
                    std::ptr::null(),
                    tw,
                    th,
                    &format,
                    WICBitmapTransformRotate0,
                    tw * 4,
                    &mut bgra,
                )?;
                return Ok(Pixels { width: tw, height: th, bgra });
            }
        }
    }

    // Otherwise decode, then scale and convert.
    let factory = factory()?;
    unsafe {
        let mut source: IWICBitmapSource = frame.cast()?;
        if (w, h) != (full_w, full_h) {
            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(&source, w, h, WICBitmapInterpolationModeFant)?;
            source = scaler.cast()?;
        }
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &source,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut bgra)?;
        Ok(Pixels { width: w, height: h, bgra })
    }
}
