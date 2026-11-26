use std::path::Path;
use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoTaskMemFree, IPersist};
use windows::core::{GUID, HSTRING, Interface};

use crate::gpu::{Size, Timing};
use crate::{FPS, Result, require};

pub struct Media;

impl Media {
    pub fn start() -> Result<Self> {
        // SAFETY: called on the recording MTA. Drop runs once after all MF objects are gone.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
        Ok(Self)
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        // SAFETY: matches this thread's successful MFStartup.
        let _ = unsafe { MFShutdown() };
    }
}

pub struct Writer {
    inner: IMFSinkWriter,
    stream: u32,
    hardware: bool,
    pub timing: Timing,
}

impl Writer {
    pub fn open(
        path: &Path,
        size: Size,
        device: &ID3D11Device,
        first: &ID3D11Texture2D,
        duration: i64,
    ) -> Result<Self> {
        let mut hardware = Self::configure(path, size, device, true);
        if let Ok(writer) = &mut hardware {
            let first_result = writer.write(first, 0, duration);
            match first_result {
                Ok(()) => {
                    writer.print_encoder();
                    return hardware;
                }
                Err(error) => {
                    let _ = writer.finalize();
                    hardware = Err(error);
                }
            }
        }
        if let Err(error) = hardware {
            println!("Hardware setup/first sample failed, trying software ({error})");
        }
        let mut writer = Self::configure(path, size, device, false)?;
        if let Err(error) = writer.write(first, 0, duration) {
            let _ = writer.finalize();
            return Err(error);
        }
        writer.print_encoder();
        Ok(writer)
    }

    fn configure(path: &Path, size: Size, device: &ID3D11Device, hardware: bool) -> Result<Self> {
        let mut attrs = None;
        // SAFETY: the attributes output slot is writable.
        unsafe { MFCreateAttributes(&mut attrs, 5)? };
        let attrs = require(attrs, "writer attributes")?;
        // SAFETY: attrs is an owned attribute store. Index zero will be used by the single stream.
        unsafe {
            attrs.SetUINT32(
                &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                u32::from(hardware),
            )?;
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        }
        if hardware {
            let (mut token, mut manager) = (0, None);
            // SAFETY: both output slots are initialized and writable.
            unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
            let manager = require(manager, "DXGI device manager")?;
            // SAFETY: the video-enabled device is live and token came from this manager.
            unsafe { manager.ResetDevice(device, token)? };
            // SAFETY: the attribute store takes a COM reference on the manager.
            unsafe { attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)? };
        } else {
            // SAFETY: this attribute is valid when hardware transforms are disabled.
            unsafe { attrs.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)? };
        }

        // About 0.10 bit/pixel/frame at 60 fps, with a floor for smaller monitors.
        let bitrate = (size.0 * size.1 * 6).max(2_000_000);
        let output = video_type(size, &MFVideoFormat_H264)?;
        let input = video_type(size, &MFVideoFormat_NV12)?;
        // SAFETY: these are H.264 output and NV12 input media types, both specified as BT.709.
        unsafe {
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, size.0)?;
        }

        let url = path.to_str().ok_or("MP4 output path is not Unicode")?;
        let url = HSTRING::from(url);
        // SAFETY: the null-terminated path and the attribute store are live during creation.
        let inner = unsafe { MFCreateSinkWriterFromURL(&url, None::<&IMFByteStream>, &attrs) }
            .map_err(|e| format!("create Sink Writer: {e}"))?;
        // SAFETY: the writer is configured in order with compatible media types.
        let stream =
            unsafe { inner.AddStream(&output) }.map_err(|e| format!("add H.264 stream: {e}"))?;
        // SAFETY: stream was returned by AddStream. No custom encoder parameters are passed.
        unsafe { inner.SetInputMediaType(stream, &input, None::<&IMFAttributes>) }
            .map_err(|e| format!("set NV12 input: {e}"))?;
        // SAFETY: the sole stream is configured before writing starts.
        unsafe { inner.BeginWriting() }.map_err(|e| format!("begin MP4: {e}"))?;
        Ok(Self {
            inner,
            stream,
            hardware,
            timing: Timing::default(),
        })
    }

    pub fn write(&mut self, nv12: &ID3D11Texture2D, time: i64, duration: i64) -> Result<()> {
        // SAFETY: nv12 is a live single-subresource ID3D11Texture2D. MF takes a reference on it.
        let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)? };
        // SAFETY: the owned buffer type exposes its byte length without accessing pixel data.
        let len = unsafe { buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()? };
        // SAFETY: len is the buffer's full contiguous byte length.
        unsafe { buffer.SetCurrentLength(len)? };
        // SAFETY: MF is initialized on this thread and returns an owned sample.
        let sample = unsafe { MFCreateSample()? };
        // SAFETY: sample retains the live surface buffer and times use MF's 100 ns units.
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
        }
        let start = Instant::now();
        // SAFETY: this writer is active, and the stream and sample belong to this recording.
        let result = unsafe { self.inner.WriteSample(self.stream, &sample) };
        self.timing.add(start.elapsed());
        result.map_err(|e| format!("WriteSample: {e}").into())
    }

    pub fn finalize(&self) -> Result<()> {
        // SAFETY: called at most once on a started writer, after its last sample.
        Ok(unsafe { self.inner.Finalize() }?)
    }

    fn print_encoder(&self) {
        let (mode, name) = self.encoder_identity().unwrap_or_else(|| {
            if self.hardware {
                ("unverified", "unidentified H.264 MFT".into())
            } else {
                (
                    "software",
                    "Microsoft H.264 Video Encoder MFT (inbox default)".into(),
                )
            }
        });
        println!("Encoder: {mode} — {name}");
    }

    fn encoder_identity(&self) -> Option<(&'static str, String)> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: GUID_NULL requests QueryInterface from this stream's encoder.
        unsafe {
            self.inner.GetServiceForStream(
                self.stream,
                &GUID::zeroed(),
                &IMFTransform::IID,
                &mut raw,
            )
        }
        .ok()?;
        if raw.is_null() {
            return None;
        }
        // SAFETY: successful GetServiceForStream returned an owned IMFTransform reference.
        let transform = unsafe { IMFTransform::from_raw(raw) };
        // SAFETY: queries the configured encoder's own attribute store.
        let attrs = unsafe { transform.GetAttributes() }.ok();
        // SAFETY: both calls return value types from live COM objects.
        let class = unsafe {
            attrs
                .as_ref()
                .and_then(|attrs| attrs.GetGUID(&MFT_TRANSFORM_CLSID_Attribute).ok())
                .or_else(|| transform.cast::<IPersist>().ok()?.GetClassID().ok())
        };
        if class == Some(CMSH264EncoderMFT) {
            return Some(("software", "Microsoft H.264 Video Encoder MFT".into()));
        }
        let name = attrs
            .as_ref()
            .and_then(|attrs| attr_text(attrs, &MFT_FRIENDLY_NAME_Attribute));
        let url = attrs
            .as_ref()
            .and_then(|attrs| attr_text(attrs, &MFT_ENUM_HARDWARE_URL_Attribute));
        if self.hardware {
            if let Some(name) = registered_name(
                class,
                url.as_deref(),
                name.as_deref(),
                MFT_ENUM_FLAG_HARDWARE,
            ) {
                return Some(("hardware", name));
            }
        }
        if let Some(name) = registered_name(class, None, name.as_deref(), MFT_ENUM_FLAG_SYNCMFT) {
            return Some(("software", name));
        }
        let mode = if !self.hardware {
            "software"
        } else if url.is_some() {
            "hardware"
        } else {
            "unverified"
        };
        name.or_else(|| class.map(|class| format!("H.264 MFT {class:?}")))
            .map(|name| (mode, name))
    }
}

fn attr_text(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    // SAFETY: reads a string attribute from an owned object.
    let len = unsafe { attrs.GetStringLength(key) }.ok()?;
    let mut chars = vec![0; len as usize + 1];
    // SAFETY: the slice has room for the advertised UTF-16 string and terminating NUL.
    unsafe { attrs.GetString(key, &mut chars, None) }.ok()?;
    Some(String::from_utf16_lossy(&chars[..len as usize]))
}

fn registered_name(
    class: Option<GUID>,
    url: Option<&str>,
    name: Option<&str>,
    flags: MFT_ENUM_FLAG,
) -> Option<String> {
    if class.is_none() && url.is_none() && name.is_none() {
        return None;
    }
    let filter = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let (mut list, mut count) = (std::ptr::null_mut(), 0);
    // SAFETY: the filter and outputs are valid. MF allocates the activation array.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&filter),
            &mut list,
            &mut count,
        )
    }
    .ok()?;
    let mut found = None;
    if !list.is_null() {
        // SAFETY: MF returned count initialized activation slots, each with an owned reference.
        for slot in unsafe { std::slice::from_raw_parts_mut(list, count as usize) } {
            if let Some(activation) = slot.take() {
                // SAFETY: reads a value from the live activation interface.
                let id = unsafe { activation.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.ok();
                let link = attr_text(&activation, &MFT_ENUM_HARDWARE_URL_Attribute);
                let friendly = attr_text(&activation, &MFT_FRIENDLY_NAME_Attribute);
                if (class.is_some() && id == class)
                    || (url.is_some() && link.as_deref() == url)
                    || (name.is_some() && friendly.as_deref() == name)
                {
                    found = friendly;
                }
            }
        }
        // SAFETY: all activation references were released; MFTEnumEx used the COM allocator.
        unsafe { CoTaskMemFree(Some(list.cast())) };
    }
    found
}

pub fn pair(a: u32, b: u32) -> u64 {
    (u64::from(a) << 32) | u64::from(b)
}

fn video_type(size: Size, subtype: &GUID) -> Result<IMFMediaType> {
    // SAFETY: creates a media type on the initialized MTA.
    let ty = unsafe { MFCreateMediaType()? };
    // SAFETY: all values use the attribute formats defined by Media Foundation.
    unsafe {
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ty.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        ty.SetUINT64(&MF_MT_FRAME_SIZE, pair(size.0, size.1))?;
        ty.SetUINT64(&MF_MT_FRAME_RATE, pair(FPS as u32, 1))?;
        ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1))?;
        ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(ty)
}
