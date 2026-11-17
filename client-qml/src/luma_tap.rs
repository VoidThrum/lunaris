//! Reads the luma of a part of a decoded frame on the CPU, for
//! `pointer_in_picture` (looking for the host's pointer in the picture).
//!
//! Called on the decode thread, before the frame goes to the renderer as it
//! always does, and only while the matcher wants a frame: during a drag in
//! which the host said its pointer may be in the picture, at most once per
//! 30 ms. Per kind of frame:
//!  - software frames: the rows of the Y plane are copied directly;
//!  - VAAPI: the surface is mapped (`av_hwframe_map`, the driver's
//!    `vaDeriveImage` when it has one) and only the rows wanted are copied;
//!  - other hardware frames (D3D11VA, CUDA, ...): `av_hwframe_transfer_data`
//!    of this one frame into a software frame, then the rows wanted.
//! A frame that cannot be read gives no check: the viewer's own pointer
//! stays (two pointers), it is never hidden blindly.

use ffmpeg_next::sys as ffi;

use crate::pointer_in_picture::Plan;

/// Studio range (16..235) to full range (0..255).
fn studio_to_full(value: u8) -> u8 {
    ((value as i32 - 16) * 255 + 109).div_euclid(219).clamp(0, 255) as u8
}

/// How a hardware frame is brought to the CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// `av_hwframe_map`, the surface itself where the driver allows it.
    MapDirect,
    /// `av_hwframe_map` of a copy the driver makes.
    Map,
    /// `av_hwframe_transfer_data` into a software frame.
    Transfer,
}

/// A reference to a decoded frame, read on the matcher's thread.
struct KeptFrame(*mut ffi::AVFrame);
// The frame is a reference of its own (av_frame_clone); FFmpeg's frame
// buffers and hardware frame pools are reference counted across threads.
unsafe impl Send for KeptFrame {}
impl Drop for KeptFrame {
    fn drop(&mut self) {
        unsafe { ffi::av_frame_free(&mut self.0) };
    }
}

/// Offers a decoded frame to the matcher. `bt709` and `full_range` are the
/// frame's colours as the renderers use them. Returns at once when no check
/// is due.
///
/// A VAAPI frame is read on the matcher's thread through a reference of its
/// own: mapping a surface waits until the GPU has finished decoding it
/// (3 to 5 ms for a 1080p frame, see the test below), which the decode
/// thread must not do. Other frames are copied here: software frames cost
/// microseconds; D3D11VA and CUDA frames are transferred here (about 1 ms
/// with CUDA) because their surface pools have no surface to spare for a
/// second holder.
///
/// # Safety
/// `frame` must be a valid decoded `AVFrame`; it is only read.
pub unsafe fn offer(frame: *mut ffi::AVFrame, bt709: bool, full_range: bool) {
    let Some(plan) = crate::pointer_in_picture::plan((*frame).width, (*frame).height, bt709) else {
        return;
    };
    if (*frame).format == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
        let kept = ffi::av_frame_clone(frame);
        if kept.is_null() {
            crate::pointer_in_picture::abandon(plan);
            return;
        }
        let kept = KeptFrame(kept);
        crate::pointer_in_picture::submit(
            plan,
            Box::new(move |plan| {
                let kept = kept;
                unsafe { read_luma(kept.0, plan, full_range) }
            }),
        );
        return;
    }
    match read_luma(frame, &plan, full_range) {
        Some(luma) => crate::pointer_in_picture::submit(plan, Box::new(move |_| Some(luma))),
        None => crate::pointer_in_picture::abandon(plan),
    }
}

/// The luma of the part of `frame` that `plan` asks for, full range, row
/// after row; `None` when this frame cannot be read.
///
/// # Safety
/// `frame` must be a valid decoded `AVFrame`.
pub unsafe fn read_luma(frame: *mut ffi::AVFrame, plan: &Plan, full_range: bool) -> Option<Vec<u8>> {
    let region = (plan.x0, plan.y0, plan.width, plan.height);
    if (*frame).hw_frames_ctx.is_null() {
        return copy_region(frame, region, full_range);
    }
    if (*frame).format == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
        if let Some(luma) = read_hw(frame, region, full_range, Read::MapDirect) {
            return Some(luma);
        }
    }
    read_hw(frame, region, full_range, Read::Transfer)
}

/// # Safety
/// `frame` must be a valid hardware `AVFrame` (with `hw_frames_ctx`).
pub unsafe fn read_hw(
    frame: *mut ffi::AVFrame,
    region: (i32, i32, i32, i32),
    full_range: bool,
    how: Read,
) -> Option<Vec<u8>> {
    let mut cpu = ffi::av_frame_alloc();
    if cpu.is_null() {
        return None;
    }
    let err = match how {
        Read::MapDirect | Read::Map => {
            let frames = (*(*frame).hw_frames_ctx).data as *const ffi::AVHWFramesContext;
            (*cpu).format = (*frames).sw_format as i32;
            let mut flags = ffi::AV_HWFRAME_MAP_READ as i32;
            if how == Read::MapDirect {
                flags |= ffi::AV_HWFRAME_MAP_DIRECT as i32;
            }
            ffi::av_hwframe_map(cpu, frame, flags)
        }
        Read::Transfer => ffi::av_hwframe_transfer_data(cpu, frame, 0),
    };
    let luma = if err >= 0 {
        if (*cpu).width <= 0 || (*cpu).height <= 0 {
            (*cpu).width = (*frame).width;
            (*cpu).height = (*frame).height;
        }
        copy_region(cpu, region, full_range)
    } else {
        None
    };
    ffi::av_frame_free(&mut cpu);
    luma
}

/// Copies `region` (x, y, width, height) of the luma of a software frame.
unsafe fn copy_region(frame: *const ffi::AVFrame, region: (i32, i32, i32, i32), full_range: bool) -> Option<Vec<u8>> {
    let (x0, y0, width, height) = region;
    if x0 < 0
        || y0 < 0
        || width <= 0
        || height <= 0
        || x0 + width > (*frame).width
        || y0 + height > (*frame).height
        || (*frame).data[0].is_null()
        || (*frame).linesize[0] <= 0
    {
        return None;
    }
    let format: ffi::AVPixelFormat = std::mem::transmute((*frame).format);
    let desc = ffi::av_pix_fmt_desc_get(format);
    if desc.is_null() {
        return None;
    }
    // Luma first in a plane of its own bytes: the planar and semi-planar
    // YCbCr formats (YUV420P, NV12, YUV444P, P010, ...).
    let luma = (*desc).comp[0];
    let flags = (*desc).flags as u64;
    if flags & (ffi::AV_PIX_FMT_FLAG_RGB as u64) != 0
        || flags & (ffi::AV_PIX_FMT_FLAG_BITSTREAM as u64) != 0
        || flags & (ffi::AV_PIX_FMT_FLAG_PAL as u64) != 0
        || flags & (ffi::AV_PIX_FMT_FLAG_HWACCEL as u64) != 0
        || flags & (ffi::AV_PIX_FMT_FLAG_BE as u64) != 0
        || luma.plane != 0
        || luma.offset != 0
    {
        return None;
    }
    let wide = match (luma.depth, luma.step) {
        (8, 1) => false,
        (9..=16, 2) => true,
        _ => return None,
    };
    let stride = (*frame).linesize[0] as usize;
    let mut out = Vec::with_capacity(width as usize * height as usize);
    for y in y0..y0 + height {
        let row = (*frame).data[0].add(y as usize * stride + x0 as usize * luma.step as usize) as *const u8;
        if wide {
            // Little-endian samples: the top eight bits.
            let drop = luma.shift + luma.depth - 8;
            let samples = std::slice::from_raw_parts(row, width as usize * 2);
            out.extend(samples.chunks_exact(2).map(|s| (u16::from_le_bytes([s[0], s[1]]) >> drop) as u8));
        } else {
            out.extend_from_slice(std::slice::from_raw_parts(row, width as usize));
        }
    }
    if !full_range {
        for value in &mut out {
            *value = studio_to_full(*value);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::time::Instant;

    #[test]
    fn studio_range_is_stretched() {
        assert_eq!(studio_to_full(16), 0);
        assert_eq!(studio_to_full(235), 255);
        assert_eq!(studio_to_full(0), 0);
        assert_eq!(studio_to_full(255), 255);
        assert_eq!(studio_to_full(126), 128);
    }

    /// A short 1080p H.264 stream made with the `ffmpeg` program; `None`
    /// when there is none.
    fn test_stream() -> Option<std::path::PathBuf> {
        let path = std::env::temp_dir().join(format!("lunaris-luma-tap-{}.h264", std::process::id()));
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30", "-frames:v", "12"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-bf", "0", "-f", "h264"])
            .arg(&path)
            .status()
            .ok()?;
        (status.success() && path.exists()).then_some(path)
    }

    /// Decodes the stream (on `device`, or in software) and calls `each`
    /// with every frame. False when the device or the decoder is not there.
    unsafe fn decode_all(
        path: &std::path::Path,
        device: Option<(ffi::AVHWDeviceType, Option<&str>)>,
        mut each: impl FnMut(*mut ffi::AVFrame),
    ) -> bool {
        let mut hw: *mut ffi::AVBufferRef = std::ptr::null_mut();
        if let Some((kind, node)) = device {
            let node = node.map(|n| CString::new(n).unwrap());
            let err = ffi::av_hwdevice_ctx_create(
                &mut hw,
                kind,
                node.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                std::ptr::null_mut(),
                0,
            );
            if err < 0 || hw.is_null() {
                return false;
            }
        }
        let c_path = CString::new(path.to_str().unwrap()).unwrap();
        let mut input: *mut ffi::AVFormatContext = std::ptr::null_mut();
        if ffi::avformat_open_input(&mut input, c_path.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) < 0 {
            ffi::av_buffer_unref(&mut hw);
            return false;
        }
        let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264);
        let mut ctx = ffi::avcodec_alloc_context3(codec);
        (*ctx).thread_count = 1;
        if !hw.is_null() {
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw);
        }
        let mut ok = ffi::avcodec_open2(ctx, codec, std::ptr::null_mut()) >= 0;
        let mut packet = ffi::av_packet_alloc();
        let mut frame = ffi::av_frame_alloc();
        let mut frames = 0;
        let mut hardware = false;
        while ok {
            let end = ffi::av_read_frame(input, packet) < 0;
            let sent = ffi::avcodec_send_packet(ctx, if end { std::ptr::null() } else { packet });
            ffi::av_packet_unref(packet);
            if sent < 0 && !end {
                ok = false;
                break;
            }
            while ffi::avcodec_receive_frame(ctx, frame) >= 0 {
                hardware |= !(*frame).hw_frames_ctx.is_null();
                each(frame);
                frames += 1;
                ffi::av_frame_unref(frame);
            }
            if end {
                break;
            }
        }
        ffi::av_frame_free(&mut frame);
        ffi::av_packet_free(&mut packet);
        ffi::avcodec_free_context(&mut ctx);
        ffi::avformat_close_input(&mut input);
        ffi::av_buffer_unref(&mut hw);
        // A device that opened but does not decode H.264 gives software frames.
        ok && frames > 0 && (device.is_none() || hardware)
    }

    /// Decodes a 1080p stream on each hardware device this machine has (no
    /// window is opened) and reads luma back as the matcher would: the
    /// values must be those of the software decoder, and the time each way
    /// of reading takes is printed (`cargo test -- --nocapture`). Skipped
    /// where there is no `ffmpeg` program or no device.
    #[test]
    fn hardware_frames_give_the_luma_of_the_picture() {
        let Some(path) = test_stream() else {
            eprintln!("luma tap: no ffmpeg program, skipped");
            return;
        };
        // The part a check reads: around a few positions (here 300 x 200),
        // and the whole plane as the worst case.
        let small = (800, 400, 300, 200);
        let whole = (0, 0, 1920, 1080);

        let mut software: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut software_times = Vec::new();
        let decoded = unsafe {
            decode_all(&path, None, |frame| {
                let start = Instant::now();
                let part = copy_region(frame, small, true).expect("software frames can be read");
                software_times.push(start.elapsed());
                software.push((part, copy_region(frame, whole, true).unwrap()));
            })
        };
        assert!(decoded, "software decoding of the test stream");
        assert_eq!(software.len(), 12);
        assert_eq!(software[0].0.len(), 300 * 200);
        // The part is the same pixels as in the whole plane.
        assert_eq!(&software[3].0[..300], &software[3].1[400 * 1920 + 800..][..300]);
        eprintln!("luma tap: software frame, 300x200: {:?} (median)", median(&mut software_times));

        let devices: [(&str, ffi::AVHWDeviceType, Option<&str>); 3] = [
            ("VAAPI renderD129", ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, Some("/dev/dri/renderD129")),
            ("VAAPI renderD128", ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, Some("/dev/dri/renderD128")),
            ("CUDA", ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA, None),
        ];
        for (name, kind, node) in devices {
            let ways: &[Read] = if kind == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI {
                &[Read::MapDirect, Read::Map, Read::Transfer]
            } else {
                &[Read::Transfer]
            };
            // Each way and each part in a decoding pass of its own: the
            // first read of a frame also waits for the GPU to finish
            // decoding it, which is part of what a check costs.
            let mut device_there = true;
            let mut worst_difference = 0u8;
            let mut last_way_worked = false;
            for way in ways {
                let mut medians = Vec::new();
                for (r, region) in [small, whole].into_iter().enumerate() {
                    let mut times = Vec::new();
                    let mut failed = 0;
                    let mut index = 0;
                    let decoded = unsafe {
                        decode_all(&path, Some((kind, node)), |frame| {
                            if (*frame).hw_frames_ctx.is_null() {
                                return;
                            }
                            let start = Instant::now();
                            let luma = read_hw(frame, region, true, *way);
                            let took = start.elapsed();
                            match luma {
                                Some(luma) => {
                                    times.push(took);
                                    let expected = if r == 0 { &software[index].0 } else { &software[index].1 };
                                    assert_eq!(luma.len(), expected.len());
                                    let difference =
                                        luma.iter().zip(expected.iter()).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
                                    worst_difference = worst_difference.max(difference);
                                }
                                None => failed += 1,
                            }
                            index += 1;
                        })
                    };
                    if !decoded || index == 0 {
                        device_there = false;
                        break;
                    }
                    medians.push((failed == 0 && !times.is_empty()).then(|| median(&mut times)));
                }
                if !device_there {
                    break;
                }
                last_way_worked = medians.iter().all(|m| m.is_some());
                match (medians[0], medians[1]) {
                    (Some(part), Some(plane)) => eprintln!(
                        "luma tap: {} {:?}: 300x200 of a 1080p frame {:?}, the whole plane {:?} (median of 12 frames)",
                        name, way, part, plane
                    ),
                    _ => eprintln!("luma tap: {} {:?}: not possible here", name, way),
                }
            }
            if !device_there {
                eprintln!("luma tap: {}: no such device or no H.264 decoding there, skipped", name);
                continue;
            }
            // A hardware decoder gives the same picture as the software one.
            assert!(worst_difference <= 2, "{}: luma differs from the software decoder by {}", name, worst_difference);
            // The way every device has (the last resort of read_luma) works.
            assert!(last_way_worked, "{}: the frame could not be transferred", name);
        }
        let _ = std::fs::remove_file(&path);
    }

    fn median(times: &mut [std::time::Duration]) -> std::time::Duration {
        times.sort();
        times.get(times.len() / 2).copied().unwrap_or_default()
    }
}
