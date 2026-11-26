use std::{path::Path, ptr};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToInt64};
use windows::core::{HSTRING, Interface};

use crate::desktop::{BLOCK, EXCLUDED_OFFSET, EXCLUDED_RGB, INCLUDED_OFFSET, INCLUDED_RGB};
use crate::gpu::Size;
use crate::media::pair;
use crate::{Result, SECOND, require};

const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

pub fn verify(path: &Path, source: Size, output: Size, duration: i64, expected: i64) -> Result<()> {
    let mut attrs = None;
    // SAFETY: a writable output slot on the MF worker.
    unsafe { MFCreateAttributes(&mut attrs, 2)? };
    let attrs = require(attrs, "reader attributes")?;
    // SAFETY: enable basic software RGB conversion and use the software decoder.
    unsafe {
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0)?;
    }
    let path = HSTRING::from(path.to_str().ok_or("MP4 path is not Unicode")?);
    // SAFETY: the path and attributes live through the call.
    let reader = unsafe { MFCreateSourceReaderFromURL(&path, &attrs) }
        .map_err(|e| format!("MP4 opens: {e}"))?;
    println!("PASS MP4 opens with Media Foundation");

    // SAFETY: asks the live reader for its first native video media type and attributes.
    let (codec, dimensions) = unsafe {
        let native = reader.GetNativeMediaType(VIDEO, 0)?;
        (
            native.GetGUID(&MF_MT_SUBTYPE)?,
            native.GetUINT64(&MF_MT_FRAME_SIZE)?,
        )
    };
    let mut ok = true;
    report(
        &mut ok,
        codec == MFVideoFormat_H264 && dimensions == pair(output.0, output.1),
        format!(
            "native H.264 video, {}x{} (expected {}x{})",
            dimensions >> 32,
            dimensions as u32,
            output.0,
            output.1
        ),
    );
    // SAFETY: MF_PD_DURATION is a presentation attribute, returned as an owned variant.
    let mut value = unsafe {
        reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)?
    };
    // SAFETY: this initialized variant lives through conversion and is then cleared exactly once.
    let measured = unsafe {
        let converted = PropVariantToInt64(&value);
        PropVariantClear(&mut value)?;
        converted?
    };
    report(
        &mut ok,
        (measured - duration).abs() <= SECOND / 5,
        format!(
            "duration {:.3} s (expected {:.3} s)",
            measured as f64 / SECOND as f64,
            duration as f64 / SECOND as f64
        ),
    );

    // SAFETY: configure the reader's selected video stream to decode to RGB32.
    let format = unsafe {
        let ty = MFCreateMediaType()?;
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader.SetCurrentMediaType(VIDEO, None, &ty)?;
        reader.GetCurrentMediaType(VIDEO)?
    };
    // SAFETY: reads metadata on the configured format. RGB32 has a DWORD-aligned row size.
    let stride = unsafe {
        format
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|v| v as i32)
            .or_else(|_| MFGetStrideForBitmapInfoHeader(MFVideoFormat_RGB32.data1, output.0))?
    };

    let (mut count, mut max_gap) = (0, 0);
    let (mut first, mut previous) = (None, None);
    let (mut monotonic, mut eos) = (true, false);
    let (mut normal, mut excluded, mut zoomed) = (None, None, None);
    for _ in 0..expected + 120 {
        let (mut flags, mut time, mut sample) = (0, 0_i64, None);
        // SAFETY: reader is synchronous and every requested output has a writable slot.
        unsafe {
            reader.ReadSample(
                VIDEO,
                0,
                None,
                Some(&mut flags),
                Some(&mut time),
                Some(&mut sample),
            )?
        };
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err("source reader signalled a decode error".into());
        }
        if let Some(sample) = sample {
            count += 1;
            first.get_or_insert(time);
            if let Some(prior) = previous {
                let gap = time - prior;
                monotonic &= gap > 0;
                max_gap = max_gap.max(gap);
            }
            previous = Some(time);
            if normal.is_none() && (SECOND / 2..SECOND).contains(&time) {
                let blocks = with_frame(&sample, output, stride, |pixels| {
                    [
                        pixels.block(source, INCLUDED_OFFSET, INCLUDED_RGB, 1.0),
                        pixels.block(source, EXCLUDED_OFFSET, EXCLUDED_RGB, 1.0),
                    ]
                })?;
                [normal, excluded] = blocks.map(Some);
            } else if zoomed.is_none() && (22 * SECOND / 10..23 * SECOND / 10).contains(&time) {
                zoomed = Some(with_frame(&sample, output, stride, |pixels| {
                    pixels.block(source, INCLUDED_OFFSET, INCLUDED_RGB, 2.0)
                })?);
            }
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            eos = true;
            break;
        }
    }
    report(
        &mut ok,
        eos && count == expected,
        format!("decoded frame count {count} (expected {expected})"),
    );
    report(
        &mut ok,
        monotonic
            && first.is_some_and(|t| (0..=SECOND / 20).contains(&t))
            && previous.is_some_and(|t| (0..=SECOND / 20).contains(&(duration - t)))
            && max_gap <= SECOND / 20,
        format!(
            "strictly increasing timestamps, maximum gap {:.3} ms",
            max_gap as f64 / 10_000.0
        ),
    );

    let expected_width = f64::from(BLOCK * output.0) / f64::from(source.0);
    let expected_height = f64::from(BLOCK * output.1) / f64::from(source.1);
    let shape = normal.unwrap_or_default();
    report(
        &mut ok,
        (0.8..1.2).contains(&(f64::from(shape.width) / expected_width))
            && (0.8..1.2).contains(&(f64::from(shape.height) / expected_height))
            && shape.solid(),
        format!(
            "included green block {}x{} in first second (expected {:.0}x{:.0})",
            shape.width, shape.height, expected_width, expected_height
        ),
    );
    report(
        &mut ok,
        excluded
            .as_ref()
            .is_some_and(|block| f64::from(block.pixels) < expected_width * expected_height * 0.02),
        format!(
            "excluded magenta block absent ({} matching pixels in its region)",
            excluded.unwrap_or_default().pixels
        ),
    );
    let large = zoomed.unwrap_or_default();
    let ratio_w = f64::from(large.width) / f64::from(shape.width);
    let ratio_h = f64::from(large.height) / f64::from(shape.height);
    report(
        &mut ok,
        (1.8..=2.2).contains(&ratio_w) && (1.8..=2.2).contains(&ratio_h) && large.solid(),
        format!(
            "zoomed green block {}x{} around 2 s ({ratio_w:.2}x wide, {ratio_h:.2}x tall)",
            large.width, large.height
        ),
    );
    if ok {
        Ok(())
    } else {
        Err("selfcheck hard checks failed".into())
    }
}

fn report(ok: &mut bool, passed: bool, message: String) {
    *ok &= passed;
    println!("{} {message}", if passed { "PASS" } else { "FAIL" });
}

#[derive(Default)]
struct Block {
    width: u32,
    height: u32,
    pixels: u32,
}

impl Block {
    fn solid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && f64::from(self.pixels) / f64::from(self.width * self.height) > 0.7
    }
}

struct Pixels<'a> {
    size: Size,
    data: &'a [u8],
    first: isize,
    stride: isize,
}

impl Pixels<'_> {
    fn block(&self, source: Size, offset: (i32, i32), rgb: [u8; 3], zoom: f64) -> Block {
        let sx = f64::from(self.size.0) * zoom / f64::from(source.0);
        let sy = f64::from(self.size.1) * zoom / f64::from(source.1);
        let margin = if zoom > 1.0 { 48 } else { 24 };
        let x0 = ((f64::from(offset.0) * sx).floor() as u32).saturating_sub(margin);
        let y0 = ((f64::from(offset.1) * sy).floor() as u32).saturating_sub(margin);
        let x1 =
            (((f64::from(offset.0) + f64::from(BLOCK)) * sx).ceil() as u32 + 12).min(self.size.0);
        let y1 =
            (((f64::from(offset.1) + f64::from(BLOCK)) * sy).ceil() as u32 + 12).min(self.size.1);
        let (mut min_x, mut min_y, mut max_x, mut max_y, mut matches) = (x1, y1, x0, y0, 0);
        for y in y0..y1 {
            let line = (self.first + y as isize * self.stride) as usize;
            for x in x0..x1 {
                let i = line + 4 * x as usize;
                let pixel = &self.data[i..i + 3];
                if pixel
                    .iter()
                    .zip(rgb.iter().rev())
                    .all(|(a, b)| a.abs_diff(*b) <= 32)
                {
                    matches += 1;
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
        }
        if matches == 0 {
            Block::default()
        } else {
            Block {
                width: max_x - min_x + 1,
                height: max_y - min_y + 1,
                pixels: matches,
            }
        }
    }
}

fn with_frame<T>(
    sample: &IMFSample,
    size: Size,
    stride: i32,
    check: impl FnOnce(Pixels<'_>) -> T,
) -> Result<T> {
    // SAFETY: sample is an owned RGB32 frame, and the reader selected its current format.
    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    if let Ok(buffer) = buffer.cast::<IMF2DBuffer2>() {
        let (mut row, mut start, mut stride, mut len) = (ptr::null_mut(), ptr::null_mut(), 0, 0);
        // SAFETY: each output slot is valid for the lifetime of this lock.
        unsafe {
            buffer.Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut row,
                &mut stride,
                &mut start,
                &mut len,
            )?
        };
        // SAFETY: buffer describes len readable bytes between Lock2DSize and Unlock2D.
        let result = unsafe {
            scan(
                start,
                len,
                row.addr() as isize - start.addr() as isize,
                stride,
                size,
                check,
            )
        };
        // SAFETY: matches the one successful read lock above.
        unsafe { buffer.Unlock2D()? };
        return result;
    }
    let (mut data, mut len) = (ptr::null_mut(), 0);
    // SAFETY: both output slots are live. This legacy reader buffer is contiguous.
    unsafe { buffer.Lock(&mut data, None, Some(&mut len))? };
    let first = if stride < 0 {
        (size.1 - 1) as isize * -(stride as isize)
    } else {
        0
    };
    // SAFETY: buffer describes len readable bytes during the current Lock.
    let result = unsafe { scan(data, len, first, stride, size, check) };
    // SAFETY: matches the successful lock above.
    unsafe { buffer.Unlock()? };
    result
}

unsafe fn scan<T>(
    start: *const u8,
    len: u32,
    first: isize,
    stride: i32,
    size: Size,
    check: impl FnOnce(Pixels<'_>) -> T,
) -> Result<T> {
    let stride = stride as isize;
    let last = first + (size.1 - 1) as isize * stride;
    let row_bytes = size.0 as isize * 4;
    if start.is_null()
        || first.min(last) < 0
        || first.max(last) + row_bytes > len as isize
        || stride.unsigned_abs() < row_bytes as usize
    {
        return Err(format!(
            "invalid decoded RGB32 layout (offset {first}, stride {stride}, bytes {len})"
        )
        .into());
    }
    // SAFETY: caller holds the MF read lock, and each referenced row lies inside the reported allocation.
    let data = unsafe { std::slice::from_raw_parts(start, len as usize) };
    Ok(check(Pixels {
        size,
        data,
        first,
        stride,
    }))
}
