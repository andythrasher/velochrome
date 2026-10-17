//! JPEG decoding with Windows' own codec (WIC). Its JPEG decoder can decode straight to 1/2, 1/4
//! or 1/8 size by skipping most of the work, which is what makes previews cheap: a culling
//! screen rarely needs every pixel of a 24-megapixel JPEG.

use std::cell::RefCell;

use windows::Win32::Graphics::Imaging::*;
use windows::Win32::Foundation::{E_FAIL, HGLOBAL};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, PROPBAG2};
use windows::Win32::System::Ole::PROPBAG2_TYPE_DATA;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, IStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::System::Variant::{VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_R4, VT_UI1};
use windows::core::{GUID, Interface, PCWSTR, PWSTR, Result};

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

/// Writes a JPEG with WIC, a band of rows at a time, so rendering can carry on while the
/// encoder works on what's done. The file is built in memory (`finish` returns its bytes) so the
/// caller can add the camera's EXIF before it reaches the disk.
pub struct Encoder {
    stream: IStream,
    encoder: IWICBitmapEncoder,
    frame: IWICBitmapFrameEncode,
    width: u32,
    rows_written: u32,
    height: u32,
}

impl Encoder {
    /// `quality` is 1–100. Chroma is not subsampled (4:4:4), and the file is tagged sRGB with
    /// Windows' own profile when it's there.
    pub fn new(width: u32, height: u32, quality: u8) -> Result<Encoder> {
        let factory = factory()?;
        unsafe {
            let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
            let encoder = factory.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            let mut options = None;
            encoder.CreateNewFrame(&mut frame, &mut options)?;
            let (frame, options) = (frame.unwrap(), options.unwrap());

            let mut quality_name: Vec<u16> = "ImageQuality\0".encode_utf16().collect();
            let mut sampling_name: Vec<u16> = "JpegYCrCbSubsampling\0".encode_utf16().collect();
            let props = [
                PROPBAG2 { dwType: PROPBAG2_TYPE_DATA.0 as u32, vt: VT_R4, pstrName: PWSTR(quality_name.as_mut_ptr()), ..Default::default() },
                PROPBAG2 { dwType: PROPBAG2_TYPE_DATA.0 as u32, vt: VT_UI1, pstrName: PWSTR(sampling_name.as_mut_ptr()), ..Default::default() },
            ];
            let values = [
                variant(VT_R4, VARIANT_0_0_0 { fltVal: quality.clamp(1, 100) as f32 / 100.0 }),
                variant(VT_UI1, VARIANT_0_0_0 { bVal: WICJpegYCrCbSubsampling444.0 as u8 }),
            ];
            options.Write(2, props.as_ptr(), values.as_ptr())?;
            frame.Initialize(&options)?;
            frame.SetSize(width, height)?;
            frame.SetResolution(96.0, 96.0)?;
            let mut format = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut format)?;
            if format != GUID_WICPixelFormat24bppBGR {
                return Err(windows::core::Error::new(E_FAIL, "the JPEG encoder wants another pixel format"));
            }
            if let Ok(profile) = srgb_profile(&factory) {
                let _ = frame.SetColorContexts(&[Some(profile)]);
            }
            Ok(Encoder { stream, encoder, frame, width, rows_written: 0, height })
        }
    }

    /// The next `rows` rows, as 8-bit BGR with no padding.
    pub fn write_rows(&mut self, bgr: &[u8], rows: u32) -> Result<()> {
        assert!(rows > 0 && bgr.len() >= (rows * self.width * 3) as usize && self.rows_written + rows <= self.height);
        unsafe { self.frame.WritePixels(rows, self.width * 3, &bgr[..(rows * self.width * 3) as usize])? };
        self.rows_written += rows;
        Ok(())
    }

    /// Finishes the file and returns its bytes.
    pub fn finish(self) -> Result<Vec<u8>> {
        unsafe {
            self.frame.Commit()?;
            self.encoder.Commit()?;
            let mut stat = STATSTG::default();
            self.stream.Stat(&mut stat, STATFLAG_NONAME)?;
            self.stream.Seek(0, STREAM_SEEK_SET, None)?;
            let mut bytes = vec![0u8; stat.cbSize as usize];
            let mut read = 0u32;
            self.stream.Read(bytes.as_mut_ptr() as *mut _, bytes.len() as u32, Some(&mut read)).ok()?;
            bytes.truncate(read as usize);
            Ok(bytes)
        }
    }
}

/// Windows' standard sRGB profile, as a WIC color context.
fn srgb_profile(factory: &IWICImagingFactory) -> Result<IWICColorContext> {
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let path = format!("{windir}\\System32\\spool\\drivers\\color\\sRGB Color Space Profile.icm\0");
    let wide: Vec<u16> = path.encode_utf16().collect();
    unsafe {
        let context = factory.CreateColorContext()?;
        context.InitializeFromFilename(PCWSTR(wide.as_ptr()))?;
        Ok(context)
    }
}

fn variant(vt: VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 { vt, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: value }),
        },
    }
}
