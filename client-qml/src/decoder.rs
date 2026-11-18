use ffmpeg_next::sys as ffi;

// SWS_BILINEAR is 2 in FFmpeg. We define it locally because on some platforms/FFmpeg versions
// bindgen generates it as a global constant, while on others it is an enum variant (SwsFlags::SWS_BILINEAR).
const SWS_BILINEAR: i32 = 2;
const DRM_FORMAT_NV12: u32 = 0x3231_564e;
const DRM_FORMAT_R8: u32 = 0x2020_3852; // 'R8  '
const DRM_FORMAT_GR88: u32 = 0x3838_5247; // 'GR88'

#[cfg(target_os = "linux")]
extern "C" {
    fn dup(fd: i32) -> i32;
    fn close(fd: i32) -> i32;
}

struct SendRawFrame {
    frame: *mut ffi::AVFrame,
    decoder_ptr: usize,
}
unsafe impl Send for SendRawFrame {}

static ACTIVE_CUDA_FRAME: std::sync::Mutex<Option<SendRawFrame>> = std::sync::Mutex::new(None);

pub fn clear_active_cuda_frame() {
    let mut lock = ACTIVE_CUDA_FRAME.lock().unwrap();
    if let Some(SendRawFrame { frame, .. }) = lock.take() {
        unsafe {
            ffi::av_frame_free(&mut (frame as *mut _));
        }
    }
}

pub fn clear_active_cuda_frame_for_decoder(decoder_ptr: usize) {
    let mut lock = ACTIVE_CUDA_FRAME.lock().unwrap();
    if let Some(ref r) = *lock {
        if r.decoder_ptr == decoder_ptr {
            let taken = lock.take();
            if let Some(SendRawFrame { frame, .. }) = taken {
                unsafe {
                    ffi::av_frame_free(&mut (frame as *mut _));
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CodecType {
    H264,
    H265,
    AV1,
}

/// The video codecs this client can take, as the names of the session
/// request ("h264", "h265", "av1"): those the receiver can unpack from RTP
/// (the three of `CodecType`; there is no VP8 or VP9 depacketizer here) and
/// that this FFmpeg has a decoder for.
pub fn decodable_codecs() -> Vec<String> {
    [
        ("h264", ffi::AVCodecID::AV_CODEC_ID_H264),
        ("h265", ffi::AVCodecID::AV_CODEC_ID_HEVC),
        ("av1", ffi::AVCodecID::AV_CODEC_ID_AV1),
    ]
    .into_iter()
    .filter(|(_, id)| !unsafe { ffi::avcodec_find_decoder(*id) }.is_null())
    .map(|(name, _)| name.to_string())
    .collect()
}

pub struct YUVFrame {
    pub width: i32,
    pub height: i32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub y_stride: i32,
    pub u_stride: i32,
    pub v_stride: i32,
}

pub enum DecodedFrame {
    CpuYuv(YUVFrame),
    /// Packed BGRX for formats QVideoSink can't take without losing chroma
    /// (4:4:4 streams).
    CpuBgrx(BgrxFrame),
    NativePresented,
}

pub struct BgrxFrame {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
    pub stride: i32,
}

/// The encoderCscMode asked of the host for this session (GameStream:
/// bit 0 = full range, bits 1.. = colourspace 0 Rec. 601, 1 Rec. 709,
/// 2 Rec. 2020); the default when nothing was asked.
static REQUESTED_ENCODER_CSC_MODE: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(crate::bridge::HOST_DEFAULT_ENCODER_CSC_MODE);

/// Store the request of the session (also handed to the C++ renderers).
pub fn set_requested_encoder_csc_mode(mode: i32) {
    REQUESTED_ENCODER_CSC_MODE.store(mode, std::sync::atomic::Ordering::Relaxed);
    crate::bridge::qobject::set_requested_csc_mode(mode);
}

/// Matrix (BT.709 or not: BT.601) and range (full or studio) of a frame
/// from its tags. Port of moonlight-qt renderer.h:245-275 (getFrameColorspace,
/// isFrameFullRange): "If the colorspace is not populated, assume the encoder
/// is sending the colorspace that we requested" (and the same for the range).
/// `requested_csc` is the encoderCscMode asked of the host. A requested
/// Rec. 2020 cannot be expressed in the (bt709, full) result: like a tagged
/// BT.2020 frame it counts as "not BT.601".
pub fn frame_color(colorspace: i32, color_range: i32, format: i32, requested_csc: i32) -> (bool, bool) {
    let bt601 = if colorspace == ffi::AVColorSpace::AVCOL_SPC_BT470BG as i32
        || colorspace == ffi::AVColorSpace::AVCOL_SPC_SMPTE170M as i32
    {
        true
    } else if colorspace == ffi::AVColorSpace::AVCOL_SPC_BT709 as i32
        || colorspace == ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL as i32
        || colorspace == ffi::AVColorSpace::AVCOL_SPC_BT2020_CL as i32
    {
        false
    } else {
        requested_csc >> 1 == 0
    };
    let tagged_full = if color_range == ffi::AVColorRange::AVCOL_RANGE_JPEG as i32 {
        true
    } else if color_range == ffi::AVColorRange::AVCOL_RANGE_MPEG as i32 {
        false
    } else {
        requested_csc & 1 != 0
    };
    // YUVJ pixel formats are full range.
    let full = tagged_full
        || format == ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32
        || format == ffi::AVPixelFormat::AV_PIX_FMT_YUVJ444P as i32;
    (!bt601, full)
}

/// get_format for the D3D11 path: moonlight-qt ffmpeg.cpp:187-226
/// (FFmpegVideoDecoder::ffGetFormat) with the hardware pixel format of the
/// D3D11VA config, AV_PIX_FMT_D3D11, as the desired one.
///
/// Where Moonlight returns AV_PIX_FMT_NONE when the preparation fails (and its
/// start-up test decode then moves to another decoder), Lunaris has no test
/// decode: it takes the first software format of the list, and the frames go
/// to the CPU presentation (the renderer is marked failed by the C++ side).
unsafe extern "C" fn d3d11_get_format(
    context: *mut ffi::AVCodecContext,
    pix_fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    let desired = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
    let mut p = pix_fmts;
    while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        // Only match our hardware decoding format.
        if *p == desired
            && crate::bridge::qobject::d3d11_prepare_decoder_context_in_get_format(context as usize as u64, *p as i32)
        {
            return *p;
        }
        p = p.add(1);
    }

    let mut p = pix_fmts;
    while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        let desc = ffi::av_pix_fmt_desc_get(*p);
        if !desc.is_null() && (*desc).flags & (ffi::AV_PIX_FMT_FLAG_HWACCEL as u64) == 0 {
            return *p;
        }
        p = p.add(1);
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

/// Whether a decoder that lists these hardware configurations
/// (`avcodec_get_hw_config`: device type, methods) decodes on a device of
/// the type `device` handed to it in `AVCodecContext.hw_device_ctx`.
///
/// FFmpeg libavcodec/codec.h, `AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX`:
/// "The codec supports this format via the hw_device_ctx interface. When
/// selecting this format, AVCodecContext.hw_device_ctx should have been set
/// to a device of the specified type before calling avcodec_open2()."
/// A decoder without such a configuration never looks at the device: a
/// software decoder such as libdav1d lists none at all.
fn codec_decodes_on(configs: &[(ffi::AVHWDeviceType, i32)], device: ffi::AVHWDeviceType) -> bool {
    device != ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE
        && configs
            .iter()
            .any(|(kind, methods)| *kind == device && methods & (ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) != 0)
}

/// The hardware configurations of a decoder (see `codec_decodes_on`).
fn codec_hw_configs(codec: *const ffi::AVCodec) -> Vec<(ffi::AVHWDeviceType, i32)> {
    let mut configs = Vec::new();
    for index in 0.. {
        let config = unsafe { ffi::avcodec_get_hw_config(codec, index) };
        if config.is_null() {
            break;
        }
        configs.push(unsafe { ((*config).device_type, (*config).methods) });
    }
    configs
}

/// Whether the pictures are decoded on the processor: there is no device
/// the decoder uses, or there is one and the last picture still came out in
/// ordinary memory (`frame_on_gpu` = it carried a hardware frames context;
/// `None` before the first picture). The second happens when the decoder
/// has the device but cannot use it for this stream and goes on by itself.
fn decodes_in_software(has_device: bool, frame_on_gpu: Option<bool>) -> bool {
    !has_device || frame_on_gpu == Some(false)
}

/// What the stats say of a software decoder: "Software (dav1d)" for
/// FFmpeg's `libdav1d`, "Software (h264)" for its own H.264 decoder,
/// "Software" when it has no name.
fn software_label(decoder_name: &str) -> String {
    let name = decoder_name.strip_prefix("lib").unwrap_or(decoder_name);
    if name.is_empty() {
        "Software".to_string()
    } else {
        format!("Software ({})", name)
    }
}

/// A device said that it has no AV1 decoding (FFmpeg's `av1` decoder
/// answered ENOSYS): AV1 goes to the software decoder at once from then on,
/// in this process (the GPUs do not change while it runs).
static AV1_HARDWARE_UNUSABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tests: of the devices to try, only this kind.
#[cfg(test)]
static TEST_ONLY_DEVICE: std::sync::Mutex<Option<ffi::AVHWDeviceType>> = std::sync::Mutex::new(None);

/// Key frames a hardware-only decoder may fail on, before any picture came
/// out, until the software decoder takes over (Lunaris's own number: one
/// can be a damaged frame, three in a row are the decoder).
const KEY_FRAME_FAILURES_TO_GIVE_UP: u32 = 3;

/// Whether the hardware-only decoder is given up for the software one,
/// asked after a failed packet while no picture has come out yet:
/// at once when FFmpeg said that there is no hardware decoding for the
/// stream (`no_hwaccel`: AVERROR(ENOSYS) of av1dec.c `get_pixel_format`),
/// else after `KEY_FRAME_FAILURES_TO_GIVE_UP` failed key frames.
fn hardware_decoder_gives_up(no_hwaccel: bool, key_frame_failures: u32) -> bool {
    no_hwaccel || key_frame_failures >= KEY_FRAME_FAILURES_TO_GIVE_UP
}

/// Whether an AV1 temporal unit (OBUs with size fields, av1_rtp.rs) holds a
/// sequence header (OBU type 1, AV1 specification 5.3.1 `obu_type`): the
/// units that begin a key frame do.
fn av1_unit_has_sequence_header(unit: &[u8]) -> bool {
    let mut offset = 0;
    while offset < unit.len() {
        let header = unit[offset];
        let obu_type = (header >> 3) & 0x0f;
        if obu_type == 1 {
            return true;
        }
        if header & 0x02 == 0 {
            return false; // no size field: the rest cannot be walked
        }
        offset += if header & 0x04 != 0 { 2 } else { 1 };
        let mut size = 0usize;
        let mut shift = 0;
        loop {
            let Some(&byte) = unit.get(offset) else { return false };
            offset += 1;
            size |= ((byte & 0x7f) as usize) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 35 {
                return false;
            }
        }
        offset = offset.saturating_add(size);
    }
    false
}

pub struct HardwareDecoder {
    codec_ctx: *mut ffi::AVCodecContext,
    hw_device_ctx: *mut ffi::AVBufferRef,
    hw_device_type: ffi::AVHWDeviceType,
    /// The device is the D3D11 renderer's decode device (frames can be
    /// handed to it).
    d3d11_device: bool,
    hardware_decode_requested: bool,
    /// Hardware decoding was asked for and this FFmpeg's decoder for the
    /// codec cannot do it on any device (`codec_decodes_on`).
    codec_without_hw: bool,
    /// `software_label` of the decoder.
    software_label: String,
    /// Whether the last decoded picture was on the GPU (`decodes_in_software`).
    last_frame_on_gpu: Option<bool>,
    codec_type: CodecType,
    /// The decoder only decodes on the device (FFmpeg's own `av1`) and a
    /// software decoder for the codec exists to go back to
    /// (`fall_back_to_software`).
    hardware_only: bool,
    /// The device could not decode this stream and the software decoder
    /// took over (said in `fallback_reason`).
    hardware_gave_up: bool,
    /// Pictures that came out, and key frames that failed before the first did.
    frames_out: u64,
    key_frame_failures: u32,
    /// FFmpeg's code of the last failed send or receive.
    last_error_code: i32,
    sws_ctx: *mut ffi::SwsContext,
    last_width: i32,
    last_height: i32,
    last_format: i32,
    /// YUV 4:4:4 -> BGRX converter and the (width, height, format, colorspace,
    /// range) it was made for.
    bgrx_sws_ctx: *mut ffi::SwsContext,
    bgrx_key: (i32, i32, i32, i32, i32),
    /// Why VAAPI frames went through the CPU was logged once.
    dmabuf_layout_logged: bool,
    /// (BT.709, full range) of the last frame, to log changes.
    last_color: Option<(bool, bool)>,
    /// The last presentation label with the (decode, present) names it was
    /// built from; rebuilt only when they change.
    presentation_label: std::sync::Mutex<(String, &'static str, String)>,
}

unsafe impl Send for HardwareDecoder {}

impl HardwareDecoder {
    fn linux_nvidia_cuda_present() -> bool {
        if !cfg!(target_os = "linux") {
            return false;
        }
        std::path::Path::new("/dev/nvidiactl").exists()
            || std::path::Path::new("/proc/driver/nvidia/version").exists()
    }

    pub fn new(codec_type: CodecType, disable_cuda: bool) -> Result<Self, anyhow::Error> {
        Self::open(codec_type, disable_cuda, false)
    }

    /// `software_av1`: AV1 is decoded by the software decoder whatever the
    /// devices are (the device was tried and could not decode the stream).
    fn open(codec_type: CodecType, disable_cuda: bool, software_av1: bool) -> Result<Self, anyhow::Error> {
        unsafe {
            ffi::av_log_set_level(ffi::AV_LOG_WARNING);
        }

        let codec_id = match codec_type {
            CodecType::H264 => ffi::AVCodecID::AV_CODEC_ID_H264,
            CodecType::H265 => ffi::AVCodecID::AV_CODEC_ID_HEVC,
            CodecType::AV1 => ffi::AVCodecID::AV_CODEC_ID_AV1,
        };

        // The decoder FFmpeg gives first for the codec. For AV1 that is a
        // software one where libdav1d is built in (see below).
        let mut default_codec = unsafe { ffi::avcodec_find_decoder(codec_id) };
        // FFmpeg's own AV1 decoder, the one that decodes on a GPU. It has no
        // software path: libavcodec/av1dec.c `get_pixel_format` ends with
        // "Your platform doesn't support hardware accelerated AV1 decoding."
        // and AVERROR(ENOSYS) when no hardware format could be set up.
        let native_av1 = if matches!(codec_type, CodecType::AV1) {
            unsafe { ffi::avcodec_find_decoder_by_name(c"av1".as_ptr()) }
        } else {
            std::ptr::null()
        };
        if default_codec.is_null() {
            default_codec = native_av1;
        }
        if default_codec.is_null() {
            return Err(anyhow::anyhow!(
                "Failed to find FFmpeg decoder for codec {:?}",
                codec_id
            ));
        }
        let mut codec = default_codec;

        let mut hw_device_ctx: *mut ffi::AVBufferRef = std::ptr::null_mut();
        let mut hw_device_type = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
        let hardware_decode_requested = !disable_cuda;
        let mut d3d11_device = false;

        // (device type, device to open: None = FFmpeg's default)
        let mut candidates: Vec<(ffi::AVHWDeviceType, Option<String>)> = Vec::new();
        if hardware_decode_requested {
            let cuda_disabled = std::env::var("LUNARIS_DISABLE_CUDA").is_ok();
            // Use the same logic as cuda_gl_requested() — auto-enable on
            // Linux, respect explicit env var overrides.
            let cuda_gl_on = Self::cuda_gl_requested();

            if cfg!(target_os = "linux") {
                // Displays on another GPU than NVIDIA (e.g. an AMD card with
                // the NVIDIA one driving no monitor): CUDA frames can't reach
                // the app's OpenGL without a copy through the CPU, so decode
                // with VAAPI on the display GPU first (frames go to OpenGL as
                // DMA-BUFs). FFmpeg's default VAAPI device is renderD128,
                // often an unrelated GPU.
                let display = crate::display_gpu::display_gpu();
                let display_node = display
                    .as_ref()
                    .filter(|g| g.vendor != crate::display_gpu::Vendor::Nvidia)
                    .and_then(|g| g.render_node.clone());
                if let Some(node) = &display_node {
                    println!("Displays run on {:?} ({}): trying VAAPI decoding there first", display.as_ref().map(|g| g.vendor), node);
                    candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, Some(node.clone())));
                }
                let prefer_cuda_decode = !cuda_disabled
                    && (cuda_gl_on || Self::linux_nvidia_cuda_present());
                if prefer_cuda_decode {
                    candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA, None));
                }
                candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, None));
                candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VDPAU, None));
                if !prefer_cuda_decode && !cuda_disabled {
                    candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA, None));
                } else if cuda_disabled {
                    println!("CUDA hardware decoding is disabled.");
                }
            } else if cfg!(target_os = "windows") {
                if cuda_gl_on && !cuda_disabled {
                    candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA, None));
                }
                candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, None));
                candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2, None));
                if !cuda_gl_on && !cuda_disabled {
                    candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA, None));
                }
            } else if cfg!(target_os = "macos") {
                candidates.push((ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX, None));
            }
        } else {
            println!("All hardware decoding is disabled (Software/FFmpeg mode).");
        }

        // Only a device the decoder decodes on is opened and given to it.
        // The decoder `avcodec_find_decoder` gives for a codec may be a
        // software one: for AV1 it is libdav1d on the FFmpeg this was
        // written with, which lists no device, while `h264` and `hevc` list
        // CUDA, VAAPI, VDPAU and Vulkan (the test `the_decoders_of_this_ffmpeg`
        // prints them). A device given to such a decoder would be left
        // unused while the stats named it; without one it also gets the
        // threads of software decoding below.
        #[cfg(test)]
        if let Some(only) = *TEST_ONLY_DEVICE.lock().unwrap_or_else(|e| e.into_inner()) {
            candidates.retain(|(dev_type, _)| *dev_type == only);
        }
        //
        // AV1: FFmpeg's own `av1` decoder is taken instead when it lists one
        // of the devices to try, so that AV1 pictures stay on the GPU like
        // H.264's and H.265's (the same DMA-BUF, CUDA or D3D11 present).
        // Whether the GPU behind the device decodes AV1 is not known here (a
        // device opens on any GPU): the first key frame tells, and
        // `fall_back_to_software` goes back to the software decoder then.
        // Not again in this process once a device said it cannot.
        let mut hw_configs = codec_hw_configs(codec);
        let d3d11_config = |configs: &[(ffi::AVHWDeviceType, i32)]| {
            cfg!(target_os = "windows") && codec_decodes_on(configs, ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA)
        };
        if !native_av1.is_null()
            && native_av1 != default_codec
            && hardware_decode_requested
            && !software_av1
            && !AV1_HARDWARE_UNUSABLE.load(std::sync::atomic::Ordering::Relaxed)
        {
            let native_configs = codec_hw_configs(native_av1);
            if candidates.iter().any(|(dev_type, _)| codec_decodes_on(&native_configs, *dev_type)) || d3d11_config(&native_configs) {
                codec = native_av1;
                hw_configs = native_configs;
            }
        }
        candidates.retain(|(dev_type, _)| codec_decodes_on(&hw_configs, *dev_type));

        // Windows: decode on a second D3D11 device on Qt's adapter, so that the
        // renderer can sample the decoder's textures (moonlight-qt d3d11va.cpp).
        // Not when CUDA is explicitly the first choice.
        if cfg!(target_os = "windows")
            && hardware_decode_requested
            && codec_decodes_on(&hw_configs, ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA)
            && !(Self::cuda_gl_requested() && std::env::var("LUNARIS_DISABLE_CUDA").is_err())
        {
            let dev = crate::bridge::qobject::d3d11_setup_decoder_device();
            if dev != 0 {
                hw_device_ctx = dev as usize as *mut ffi::AVBufferRef;
                hw_device_type = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;
                d3d11_device = true;
            }
        }

        for (dev_type, device) in &candidates {
            if hw_device_type != ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
                break;
            }
            let device_c = device.as_deref().and_then(|d| std::ffi::CString::new(d).ok());
            let err = unsafe {
                ffi::av_hwdevice_ctx_create(
                    &mut hw_device_ctx,
                    *dev_type,
                    device_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if err >= 0 && !hw_device_ctx.is_null() {
                hw_device_type = *dev_type;
                break;
            }
        }

        // No device opened: the hardware-only decoder is of no use.
        if hw_device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            codec = default_codec;
        }
        let hardware_only = codec != default_codec;
        let decoder_name = unsafe { std::ffi::CStr::from_ptr((*codec).name) }.to_string_lossy().into_owned();
        let codec_without_hw = hardware_decode_requested && codec_hw_configs(codec).is_empty();
        if codec_without_hw {
            println!("The decoder {} for {:?} does not decode on a GPU: decoding in software.", decoder_name, codec_type);
        }

        let codec_ctx = unsafe { ffi::avcodec_alloc_context3(codec) };
        if codec_ctx.is_null() {
            unsafe {
                if !hw_device_ctx.is_null() {
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                }
            }
            return Err(anyhow::anyhow!("Failed to allocate FFmpeg codec context"));
        }

        unsafe {
            (*codec_ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*codec_ctx).thread_count = 1;
        }

        if hw_device_type != ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            unsafe {
                (*codec_ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device_ctx);
                // Surfaces held outside the decoder: the frame on screen,
                // the frames waiting in the pacing queue (up to 8), and the
                // one frame the pointer matcher may be reading (luma_tap).
                #[cfg(target_os = "linux")]
                {
                    (*codec_ctx).extra_hw_frames = 11;
                }
                if d3d11_device {
                    (*codec_ctx).get_format = Some(d3d11_get_format);
                }
                let name = std::ffi::CStr::from_ptr(ffi::av_hwdevice_get_type_name(hw_device_type))
                    .to_string_lossy();
                println!(
                    "Successfully initialized GPU hardware decoding context: {} (decoder {})",
                    name, decoder_name
                );
            }
        } else {
            if hardware_decode_requested && !codec_without_hw {
                println!("Failed to create GPU hardware decoding context. Falling back to software decoding.");
            }
            // Software decoding: use slice threads. They decode the slices of
            // one frame in parallel without the extra frames of delay that
            // frame threading adds (x264 with zerolatency writes one slice per
            // encoder thread), which a 1080p60 4:4:4 stream needs.
            unsafe {
                (*codec_ctx).thread_type = ffi::FF_THREAD_SLICE as i32;
                (*codec_ctx).thread_count = 0;
            }
        }

        let open_err = unsafe { ffi::avcodec_open2(codec_ctx, codec, std::ptr::null_mut()) };
        if open_err < 0 {
            unsafe {
                if !hw_device_ctx.is_null() {
                    ffi::av_buffer_unref(&mut hw_device_ctx);
                }
                ffi::avcodec_free_context(&mut (codec_ctx as *mut _));
            }
            return Err(anyhow::anyhow!("Failed to open FFmpeg codec: {}", open_err));
        }

        Ok(HardwareDecoder {
            codec_ctx,
            hw_device_ctx,
            hw_device_type,
            d3d11_device,
            hardware_decode_requested,
            codec_without_hw,
            software_label: software_label(&decoder_name),
            last_frame_on_gpu: None,
            codec_type,
            hardware_only,
            hardware_gave_up: false,
            frames_out: 0,
            key_frame_failures: 0,
            last_error_code: 0,
            sws_ctx: std::ptr::null_mut(),
            last_width: 0,
            last_height: 0,
            last_format: -1,
            bgrx_sws_ctx: std::ptr::null_mut(),
            bgrx_key: (0, 0, -1, -1, -1),
            dmabuf_layout_logged: false,
            last_color: None,
            presentation_label: std::sync::Mutex::new((String::new(), "", String::new())),
        })
    }

    /// What decodes the pictures, for the stats: the device's name while
    /// the decoder decodes on it, else "Software (<decoder>)".
    pub fn decode_backend_label(&self) -> &str {
        if decodes_in_software(self.hw_device_type != ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE, self.last_frame_on_gpu) {
            return &self.software_label;
        }
        self.device_label()
    }

    fn device_label(&self) -> &'static str {
        match self.hw_device_type {
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA => "CUDA",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA => "D3D11VA",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2 => "DXVA2",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI => "VAAPI",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VDPAU => "VDPAU",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX => "VideoToolbox",
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE => "Software",
            _ => "GPU",
        }
    }

    /// Whether the CUDA-GL native present path is requested.
    /// Explicit `LUNARIS_CLIENT_CUDA_GL=1` forces it on; `=0` forces it off.
    /// When unset, off: `lib.rs` sets it when OpenGL runs on an NVIDIA GPU.
    fn cuda_gl_requested() -> bool {
        // lib.rs settles these variables at start-up, before any decoder
        // exists, and nothing changes them afterwards: read once.
        static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CACHED.get_or_init(Self::cuda_gl_requested_from_env)
    }

    fn cuda_gl_requested_from_env() -> bool {
        match std::env::var("LUNARIS_CLIENT_CUDA_GL") {
            Ok(ref v) if v == "1" || v.eq_ignore_ascii_case("true") => true,
            Ok(_) => false, // explicit "0" / "false" / etc.
            Err(_) => {
                // Not set: lib.rs found no OpenGL scenegraph or NVIDIA card,
                // or CUDA-GL was turned off (LUNARIS_CLIENT_CUDA_GL=0 is
                // cleared there). Interop only works with OpenGL on NVIDIA.
                false
            }
        }
    }

    /// Whether the VAAPI DMABUF-GL native present path is requested.
    /// Explicit `LUNARIS_CLIENT_DMABUF_GL=1` forces it on; `=0` forces it off.
    /// When unset, auto-enable on Linux (where VAAPI + EGL DMABUF may work),
    /// otherwise off.
    fn dmabuf_gl_requested() -> bool {
        // Read once, like cuda_gl_requested.
        static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CACHED.get_or_init(Self::dmabuf_gl_requested_from_env)
    }

    fn dmabuf_gl_requested_from_env() -> bool {
        match std::env::var("LUNARIS_CLIENT_DMABUF_GL") {
            Ok(ref v) if v == "1" || v.eq_ignore_ascii_case("true") => true,
            Ok(_) => false,
            Err(_) => cfg!(target_os = "linux"),
        }
    }

    pub fn gpu_decode_enabled(&self) -> bool {
        !decodes_in_software(self.hw_device_type != ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE, self.last_frame_on_gpu)
    }

    pub fn present_backend_label(&self) -> &'static str {
        if self.hw_device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            return "CPU/QVideoSink";
        }

        let direct_cuda_gl = self.hw_device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA
            && Self::cuda_gl_requested()
            && !crate::bridge::qobject::cuda_gl_render_failed();

        if direct_cuda_gl {
            return "CUDA-GL";
        }

        if self.hw_device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
            && Self::dmabuf_gl_requested()
            && !crate::bridge::qobject::dmabuf_render_failed()
        {
            return "DMABUF/OpenGL";
        }

        if self.d3d11_device && !crate::bridge::qobject::d3d11_render_failed() {
            return "D3D11";
        }

        "CPU/QVideoSink"
    }

    pub fn fallback_reason(&self) -> String {
        if !self.hardware_decode_requested {
            return "Software backend selected".to_string();
        }
        if self.hardware_gave_up {
            return format!("The GPU could not decode this stream; {}", self.software_label);
        }
        if self.codec_without_hw {
            return format!("{}: this decoder does not use the GPU", self.software_label);
        }
        if self.hw_device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            return "No GPU decoder available".to_string();
        }
        if self.present_backend_label() == "CPU/QVideoSink" {
            if self.hw_device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA
                && Self::cuda_gl_requested()
                && crate::bridge::qobject::cuda_gl_render_failed()
            {
                return "CUDA-GL present failed; using CPU/QVideoSink".to_string();
            }
            if self.hw_device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI {
                if crate::bridge::qobject::dmabuf_render_failed() {
                    return "DMABUF present failed; using CPU/QVideoSink".to_string();
                }
                if !Self::dmabuf_gl_requested() {
                    return "DMABUF/OpenGL not enabled; using CPU/QVideoSink".to_string();
                }
            }
            if self.hw_device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA
                && crate::bridge::qobject::d3d11_render_failed()
            {
                return "D3D11 present not available (see the 'D3D11 renderer disabled' log line); using CPU/QVideoSink".to_string();
            }
            return "GPU decode with CPU/QVideoSink present".to_string();
        }
        "None".to_string()
    }

    pub fn presentation_mode_label(&self) -> String {
        let decode = self.decode_backend_label();
        let present = self.present_backend_label();
        let mut cache = self
            .presentation_label
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.0 != decode || cache.1 != present || cache.2.is_empty() {
            *cache = (decode.to_string(), present, format!("{} decode + {} present", decode, present));
        }
        // The signature returns an owned String, so this still allocates; the
        // format! is what is saved. A caller that only compares labels could
        // use a borrowed variant instead.
        cache.2.clone()
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<DecodedFrame>, anyhow::Error> {
        self.decode_guarded(data, |decoder, frame| decoder.process_frame(frame))
    }

    /// `decode_with`, and when the hardware-only decoder turns out not to
    /// decode the stream, the software decoder takes its place
    /// (`fall_back_to_software`). The error is returned all the same: the
    /// caller asks the host for a key frame, which the new decoder needs.
    fn decode_guarded<T>(
        &mut self,
        data: &[u8],
        each: impl FnMut(&mut Self, *mut ffi::AVFrame) -> Result<T, anyhow::Error>,
    ) -> Result<Vec<T>, anyhow::Error> {
        let result = self.decode_with(data, each);
        if result.is_err() && self.hardware_only && self.frames_out == 0 {
            if av1_unit_has_sequence_header(data) {
                self.key_frame_failures += 1;
            }
            let no_hwaccel = self.last_error_code == ffi::AVERROR(ffmpeg_next::util::error::ENOSYS);
            if hardware_decoder_gives_up(no_hwaccel, self.key_frame_failures) {
                if no_hwaccel {
                    AV1_HARDWARE_UNUSABLE.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                self.fall_back_to_software();
            }
        }
        result
    }

    /// The software decoder for the codec in the place of the hardware-only
    /// one, once: the new decoder is not a hardware-only one, so nothing
    /// brings the old one back in this session.
    fn fall_back_to_software(&mut self) {
        println!(
            "The GPU decoder for {:?} could not decode the stream (FFmpeg error {}, {} key frames failed): going on with the software decoder.",
            self.codec_type, self.last_error_code, self.key_frame_failures
        );
        match Self::open(self.codec_type, !self.hardware_decode_requested, true) {
            Ok(mut software) => {
                software.hardware_gave_up = true;
                // The old decoder (and its device) is freed here.
                *self = software;
            }
            Err(e) => eprintln!("No software decoder to go on with: {:?}", e),
        }
    }

    /// One packet in, the pictures that come out of it handed to `each`
    /// (which owns the frame).
    fn decode_with<T>(
        &mut self,
        data: &[u8],
        mut each: impl FnMut(&mut Self, *mut ffi::AVFrame) -> Result<T, anyhow::Error>,
    ) -> Result<Vec<T>, anyhow::Error> {
        let packet = unsafe { ffi::av_packet_alloc() };
        if packet.is_null() {
            return Err(anyhow::anyhow!("Failed to allocate FFmpeg packet"));
        }

        unsafe {
            let ret = ffi::av_new_packet(packet, data.len() as i32);
            if ret < 0 {
                ffi::av_packet_free(&mut (packet as *mut _));
                return Err(anyhow::anyhow!("av_new_packet failed: {}", ret));
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), (*packet).data, data.len());
        }

        let send_ret = unsafe { ffi::avcodec_send_packet(self.codec_ctx, packet) };
        unsafe {
            ffi::av_packet_free(&mut (packet as *mut _));
        }

        if send_ret < 0 {
            self.last_error_code = send_ret;
            return Err(anyhow::anyhow!("avcodec_send_packet failed: {}", send_ret));
        }

        let mut decoded_frames = Vec::new();

        loop {
            let gpu_frame = unsafe { ffi::av_frame_alloc() };
            if gpu_frame.is_null() {
                return Err(anyhow::anyhow!("Failed to allocate FFmpeg frame"));
            }

            let recv_ret = unsafe { ffi::avcodec_receive_frame(self.codec_ctx, gpu_frame) };

            if recv_ret == ffi::AVERROR(ffmpeg_next::util::error::EAGAIN)
                || recv_ret == ffi::AVERROR_EOF
            {
                unsafe {
                    ffi::av_frame_free(&mut (gpu_frame as *mut _));
                }
                break;
            } else if recv_ret < 0 {
                unsafe {
                    ffi::av_frame_free(&mut (gpu_frame as *mut _));
                }
                self.last_error_code = recv_ret;
                return Err(anyhow::anyhow!(
                    "avcodec_receive_frame failed: {}",
                    recv_ret
                ));
            }

            self.frames_out += 1;
            // For the stats: a picture decoded on a device carries its frames
            // context (FFmpeg frame.h `hw_frames_ctx`: "For hwaccel-format
            // frames, this should be a reference to the AVHWFramesContext
            // describing the frame").
            self.last_frame_on_gpu = Some(unsafe { !(*gpu_frame).hw_frames_ctx.is_null() });
            match each(self, gpu_frame) {
                Ok(frame) => {
                    decoded_frames.push(frame);
                }
                Err(e) => {
                    eprintln!("Failed to process decoded frame: {:?}", e);
                }
            }
        }

        Ok(decoded_frames)
    }

    fn process_frame(
        &mut self,
        gpu_frame: *mut ffi::AVFrame,
    ) -> Result<DecodedFrame, anyhow::Error> {
        let format = unsafe { (*gpu_frame).format };
        // The renderers convert YCbCr with the stream's own matrix and range.
        let (bt709, full_range) =
            unsafe { frame_color(
                (*gpu_frame).colorspace as i32,
                (*gpu_frame).color_range as i32,
                format,
                REQUESTED_ENCODER_CSC_MODE.load(std::sync::atomic::Ordering::Relaxed),
            )
        };
        if self.last_color != Some((bt709, full_range)) {
            self.last_color = Some((bt709, full_range));
            println!("Video colours: {} {} range", if bt709 { "BT.709" } else { "BT.601" }, if full_range { "full" } else { "studio" });
        }
        crate::bridge::qobject::set_video_color(bt709, full_range);
        // The host's pointer is looked for in the picture during some drags
        // (pointer_in_picture): one atomic load otherwise. The frame goes
        // to the renderer below as always.
        if crate::pointer_in_picture::frame_wanted() {
            unsafe { crate::luma_tap::offer(gpu_frame, bt709, full_range) };
        }
        let is_cuda = format == ffi::AVPixelFormat::AV_PIX_FMT_CUDA as i32;
        let cuda_gl_failed = crate::bridge::qobject::cuda_gl_render_failed();
        if cuda_gl_failed {
            clear_active_cuda_frame_for_decoder(self as *const _ as usize);
        }
        let use_direct_cuda_gl = is_cuda && Self::cuda_gl_requested() && !cuda_gl_failed;

        if use_direct_cuda_gl {
            unsafe {
                let y_ptr = (*gpu_frame).data[0] as u64;
                let uv_ptr = (*gpu_frame).data[1] as u64;
                let y_stride = (*gpu_frame).linesize[0] as i32;
                let uv_stride = (*gpu_frame).linesize[1] as i32;
                let width = (*gpu_frame).width as i32;
                let height = (*gpu_frame).height as i32;

                let mut cuda_ctx = 0u64;
                if !self.hw_device_ctx.is_null() {
                    let hw_device_ctx_ptr =
                        (*self.hw_device_ctx).data as *mut ffi::AVHWDeviceContext;
                    if !hw_device_ctx_ptr.is_null() {
                        let hwctx = (*hw_device_ctx_ptr).hwctx;
                        if !hwctx.is_null() {
                            cuda_ctx = *(hwctx as *mut *mut std::ffi::c_void) as u64;
                        }
                    }
                }

                let mut lock = ACTIVE_CUDA_FRAME.lock().unwrap();
                let decoder_ptr = self as *const _ as usize;
                if let Some(SendRawFrame {
                    frame: old_frame, ..
                }) = lock.replace(SendRawFrame {
                    frame: gpu_frame,
                    decoder_ptr,
                }) {
                    ffi::av_frame_free(&mut (old_frame as *mut _));
                }

                crate::bridge::qobject::deliver_cuda_frame(
                    cuda_ctx, y_ptr, y_stride, uv_ptr, uv_stride, width, height,
                );
            }

            return Ok(DecodedFrame::NativePresented);
        }

        if self.try_present_dmabuf(gpu_frame)? {
            return Ok(DecodedFrame::NativePresented);
        }

        if self.try_present_d3d11(gpu_frame)? {
            return Ok(DecodedFrame::NativePresented);
        }

        let mut cpu_frame = gpu_frame;
        let mut is_hw_frame = false;

        unsafe {
            if !(*gpu_frame).hw_frames_ctx.is_null() {
                let temp_frame = ffi::av_frame_alloc();
                if temp_frame.is_null() {
                    ffi::av_frame_free(&mut (gpu_frame as *mut _));
                    return Err(anyhow::anyhow!(
                        "Failed to allocate temp frame for HW-to-CPU copy"
                    ));
                }

                let transfer_err = ffi::av_hwframe_transfer_data(temp_frame, gpu_frame, 0);
                if transfer_err >= 0 {
                    (*temp_frame).width = (*gpu_frame).width;
                    (*temp_frame).height = (*gpu_frame).height;
                    cpu_frame = temp_frame;
                    is_hw_frame = true;
                } else {
                    ffi::av_frame_free(&mut (temp_frame as *mut _));
                    if is_cuda {
                        ffi::av_frame_free(&mut (gpu_frame as *mut _));
                        return Err(anyhow::anyhow!(
                            "CUDA decoded frame transfer failed: {}",
                            transfer_err
                        ));
                    }
                }
            } else if is_cuda {
                ffi::av_frame_free(&mut (gpu_frame as *mut _));
                return Err(anyhow::anyhow!(
                    "CUDA decoded frame did not include hw_frames_ctx for CPU transfer"
                ));
            }
        }

        let width = unsafe { (*cpu_frame).width };
        let height = unsafe { (*cpu_frame).height };
        let format = unsafe { (*cpu_frame).format };

        if Self::is_yuv444(format) {
            let result = self.convert_to_bgrx(cpu_frame);
            unsafe {
                if is_hw_frame {
                    ffi::av_frame_free(&mut (cpu_frame as *mut _));
                }
                ffi::av_frame_free(&mut (gpu_frame as *mut _));
            }
            return result.map(DecodedFrame::CpuBgrx);
        }

        let y_size = (width * height) as usize;
        let u_size = ((width / 2) * (height / 2)) as usize;
        let v_size = u_size;

        let mut y_vec = vec![0u8; y_size];
        let mut u_vec = vec![0u8; u_size];
        let mut v_vec = vec![0u8; v_size];

        let is_yuv420p = format == ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32
            || format == ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32;

        if is_yuv420p {
            unsafe {
                let src_stride = (*cpu_frame).linesize[0] as usize;
                if src_stride == width as usize {
                    std::ptr::copy_nonoverlapping((*cpu_frame).data[0], y_vec.as_mut_ptr(), y_size);
                } else {
                    let mut src_ptr = (*cpu_frame).data[0];
                    let mut dst_ptr = y_vec.as_mut_ptr();
                    for _ in 0..height {
                        std::ptr::copy_nonoverlapping(src_ptr, dst_ptr, width as usize);
                        src_ptr = src_ptr.add(src_stride);
                        dst_ptr = dst_ptr.add(width as usize);
                    }
                }

                let src_stride = (*cpu_frame).linesize[1] as usize;
                let uv_h = height / 2;
                let uv_w = width / 2;
                if src_stride == uv_w as usize {
                    std::ptr::copy_nonoverlapping((*cpu_frame).data[1], u_vec.as_mut_ptr(), u_size);
                } else {
                    let mut src_ptr = (*cpu_frame).data[1];
                    let mut dst_ptr = u_vec.as_mut_ptr();
                    for _ in 0..uv_h {
                        std::ptr::copy_nonoverlapping(src_ptr, dst_ptr, uv_w as usize);
                        src_ptr = src_ptr.add(src_stride);
                        dst_ptr = dst_ptr.add(uv_w as usize);
                    }
                }

                let src_stride = (*cpu_frame).linesize[2] as usize;
                if src_stride == uv_w as usize {
                    std::ptr::copy_nonoverlapping((*cpu_frame).data[2], v_vec.as_mut_ptr(), v_size);
                } else {
                    let mut src_ptr = (*cpu_frame).data[2];
                    let mut dst_ptr = v_vec.as_mut_ptr();
                    for _ in 0..uv_h {
                        std::ptr::copy_nonoverlapping(src_ptr, dst_ptr, uv_w as usize);
                        src_ptr = src_ptr.add(src_stride);
                        dst_ptr = dst_ptr.add(uv_w as usize);
                    }
                }
            }
        } else {
            if self.sws_ctx.is_null()
                || self.last_width != width
                || self.last_height != height
                || self.last_format != format
            {
                if !self.sws_ctx.is_null() {
                    unsafe {
                        ffi::sws_freeContext(self.sws_ctx);
                    }
                }

                self.sws_ctx = unsafe {
                    ffi::sws_getContext(
                        width,
                        height,
                        std::mem::transmute(format),
                        width,
                        height,
                        ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
                        SWS_BILINEAR,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null(),
                    )
                };

                if self.sws_ctx.is_null() {
                    unsafe {
                        if is_hw_frame {
                            ffi::av_frame_free(&mut (cpu_frame as *mut _));
                        }
                        ffi::av_frame_free(&mut (gpu_frame as *mut _));
                    }
                    return Err(anyhow::anyhow!("Failed to initialize sws_scale context"));
                }

                self.last_width = width;
                self.last_height = height;
                self.last_format = format;
            }

            let dst_frame = unsafe { ffi::av_frame_alloc() };
            if dst_frame.is_null() {
                unsafe {
                    if is_hw_frame {
                        ffi::av_frame_free(&mut (cpu_frame as *mut _));
                    }
                    ffi::av_frame_free(&mut (gpu_frame as *mut _));
                }
                return Err(anyhow::anyhow!(
                    "Failed to allocate output destination frame"
                ));
            }

            unsafe {
                (*dst_frame).width = width;
                (*dst_frame).height = height;
                (*dst_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;

                let buffer_err = ffi::av_frame_get_buffer(dst_frame, 32);
                if buffer_err < 0 {
                    ffi::av_frame_free(&mut (dst_frame as *mut _));
                    if is_hw_frame {
                        ffi::av_frame_free(&mut (cpu_frame as *mut _));
                    }
                    ffi::av_frame_free(&mut (gpu_frame as *mut _));
                    return Err(anyhow::anyhow!(
                        "av_frame_get_buffer failed: {}",
                        buffer_err
                    ));
                }

                ffi::sws_scale(
                    self.sws_ctx,
                    (*cpu_frame).data.as_ptr() as *const *const u8,
                    (*cpu_frame).linesize.as_ptr(),
                    0,
                    height,
                    (*dst_frame).data.as_mut_ptr() as *mut *mut u8,
                    (*dst_frame).linesize.as_mut_ptr(),
                );

                let src_stride = (*dst_frame).linesize[0] as usize;
                if src_stride == width as usize {
                    std::ptr::copy_nonoverlapping((*dst_frame).data[0], y_vec.as_mut_ptr(), y_size);
                } else {
                    let mut src_ptr = (*dst_frame).data[0];
                    let mut dst_ptr = y_vec.as_mut_ptr();
                    for _ in 0..height {
                        std::ptr::copy_nonoverlapping(src_ptr, dst_ptr, width as usize);
                        src_ptr = src_ptr.add(src_stride);
                        dst_ptr = dst_ptr.add(width as usize);
                    }
                }

                let src_stride = (*dst_frame).linesize[1] as usize;
                let uv_h = height / 2;
                let uv_w = width / 2;
                if src_stride == uv_w as usize {
                    std::ptr::copy_nonoverlapping((*dst_frame).data[1], u_vec.as_mut_ptr(), u_size);
                } else {
                    let mut src_ptr = (*dst_frame).data[1];
                    let mut dst_ptr = u_vec.as_mut_ptr();
                    for _ in 0..uv_h {
                        std::ptr::copy_nonoverlapping(src_ptr, dst_ptr, uv_w as usize);
                        src_ptr = src_ptr.add(src_stride);
                        dst_ptr = dst_ptr.add(uv_w as usize);
                    }
                }

                let src_stride = (*dst_frame).linesize[2] as usize;
                if src_stride == uv_w as usize {
                    std::ptr::copy_nonoverlapping((*dst_frame).data[2], v_vec.as_mut_ptr(), v_size);
                } else {
                    let mut src_ptr = (*dst_frame).data[2];
                    let mut dst_ptr = v_vec.as_mut_ptr();
                    for _ in 0..uv_h {
                        std::ptr::copy_nonoverlapping(src_ptr, dst_ptr, uv_w as usize);
                        src_ptr = src_ptr.add(src_stride);
                        dst_ptr = dst_ptr.add(uv_w as usize);
                    }
                }

                ffi::av_frame_free(&mut (dst_frame as *mut _));
            }
        }

        unsafe {
            if is_hw_frame {
                ffi::av_frame_free(&mut (cpu_frame as *mut _));
            }
            ffi::av_frame_free(&mut (gpu_frame as *mut _));
        }

        Ok(DecodedFrame::CpuYuv(YUVFrame {
            width,
            height,
            y: y_vec,
            u: u_vec,
            v: v_vec,
            y_stride: width,
            u_stride: width / 2,
            v_stride: width / 2,
        }))
    }

    fn is_yuv444(format: i32) -> bool {
        format == ffi::AVPixelFormat::AV_PIX_FMT_YUV444P as i32
            || format == ffi::AVPixelFormat::AV_PIX_FMT_YUVJ444P as i32
            || format == ffi::AVPixelFormat::AV_PIX_FMT_NV24 as i32
    }

    /// Converts a 4:4:4 frame to packed BGRX with the frame's own matrix and
    /// range, keeping the full chroma resolution (the YUV420P path would
    /// throw away exactly what a 4:4:4 stream is for).
    fn convert_to_bgrx(&mut self, frame: *mut ffi::AVFrame) -> Result<BgrxFrame, anyhow::Error> {
        let (width, height, format, colorspace, range) = unsafe {
            (
                (*frame).width,
                (*frame).height,
                (*frame).format,
                (*frame).colorspace as i32,
                (*frame).color_range as i32,
            )
        };
        let key = (width, height, format, colorspace, range);
        if self.bgrx_sws_ctx.is_null() || self.bgrx_key != key {
            unsafe {
                if !self.bgrx_sws_ctx.is_null() {
                    ffi::sws_freeContext(self.bgrx_sws_ctx);
                }
                self.bgrx_sws_ctx = ffi::sws_getContext(
                    width,
                    height,
                    std::mem::transmute(format),
                    width,
                    height,
                    ffi::AVPixelFormat::AV_PIX_FMT_BGR0,
                    SWS_BILINEAR,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                if self.bgrx_sws_ctx.is_null() {
                    return Err(anyhow::anyhow!("Failed to create the 4:4:4 -> BGRX converter"));
                }
                // Unspecified colourspace: the agent's encoders tag BT.709.
                let matrix = match colorspace {
                    c if c == ffi::AVColorSpace::AVCOL_SPC_BT470BG as i32
                        || c == ffi::AVColorSpace::AVCOL_SPC_SMPTE170M as i32 => ffi::SWS_CS_ITU601,
                    _ => ffi::SWS_CS_ITU709,
                } as i32;
                let full_range = range == ffi::AVColorRange::AVCOL_RANGE_JPEG as i32
                    || format == ffi::AVPixelFormat::AV_PIX_FMT_YUVJ444P as i32;
                let coefficients = ffi::sws_getCoefficients(matrix);
                ffi::sws_setColorspaceDetails(
                    self.bgrx_sws_ctx,
                    coefficients,
                    full_range as i32,
                    coefficients,
                    1,
                    0,
                    1 << 16,
                    1 << 16,
                );
            }
            self.bgrx_key = key;
        }

        let stride = width * 4;
        let mut data = vec![0u8; (stride * height) as usize];
        unsafe {
            let dst_planes: [*mut u8; 4] = [data.as_mut_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()];
            let dst_strides: [i32; 4] = [stride, 0, 0, 0];
            ffi::sws_scale(
                self.bgrx_sws_ctx,
                (*frame).data.as_ptr() as *const *const u8,
                (*frame).linesize.as_ptr(),
                0,
                height,
                dst_planes.as_ptr(),
                dst_strides.as_ptr(),
            );
        }
        Ok(BgrxFrame { width, height, data, stride })
    }

    #[cfg(target_os = "linux")]
    fn try_present_dmabuf(&mut self, gpu_frame: *mut ffi::AVFrame) -> Result<bool, anyhow::Error> {
        if self.hw_device_type != ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
            || !Self::dmabuf_gl_requested()
            || crate::bridge::qobject::dmabuf_render_failed()
        {
            if self.hw_device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI && !self.dmabuf_layout_logged {
                self.dmabuf_layout_logged = true;
                println!(
                    "VAAPI frames go through the CPU: DMA-BUF present requested={} failed={}",
                    Self::dmabuf_gl_requested(),
                    crate::bridge::qobject::dmabuf_render_failed()
                );
            }
            return Ok(false);
        }

        unsafe {
            let mapped_frame = ffi::av_frame_alloc();
            if mapped_frame.is_null() {
                return Ok(false);
            }

            // Without a destination format FFmpeg maps the surface into
            // ordinary memory (NV12); ask for DMA-BUF handles instead.
            (*mapped_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let map_err =
                ffi::av_hwframe_map(mapped_frame, gpu_frame, ffi::AV_HWFRAME_MAP_READ as i32);
            if map_err < 0 {
                if !self.dmabuf_layout_logged {
                    self.dmabuf_layout_logged = true;
                    println!("VAAPI frames can't be mapped as DMA-BUFs ({}); copying through the CPU", map_err);
                }
                ffi::av_frame_free(&mut (mapped_frame as *mut _));
                return Ok(false);
            }

            if (*mapped_frame).format != ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
                || (*mapped_frame).data[0].is_null()
            {
                if !self.dmabuf_layout_logged {
                    self.dmabuf_layout_logged = true;
                    println!("VAAPI frames go through the CPU: mapped format {} is not DRM PRIME", (*mapped_frame).format);
                }
                ffi::av_frame_free(&mut (mapped_frame as *mut _));
                return Ok(false);
            }

            let desc = (*mapped_frame).data[0] as *const ffi::AVDRMFrameDescriptor;
            if desc.is_null() || (*desc).nb_layers < 1 || (*desc).nb_objects < 1 {
                ffi::av_frame_free(&mut (mapped_frame as *mut _));
                return Ok(false);
            }

            // NV12 comes as one layer with two planes, or (Mesa, which
            // exports separate layers) as an R8 layer for Y and a GR88 layer
            // for UV. Either way the renderer imports each plane on its own.
            let layer = (*desc).layers[0];
            let (plane0, plane1) = if layer.format == DRM_FORMAT_NV12 && layer.nb_planes >= 2 {
                (layer.planes[0], layer.planes[1])
            } else if (*desc).nb_layers >= 2
                && layer.format == DRM_FORMAT_R8
                && layer.nb_planes >= 1
                && (*desc).layers[1].format == DRM_FORMAT_GR88
                && (*desc).layers[1].nb_planes >= 1
            {
                (layer.planes[0], (*desc).layers[1].planes[0])
            } else {
                if !self.dmabuf_layout_logged {
                    self.dmabuf_layout_logged = true;
                    println!(
                        "VAAPI frames can't be shown as DMA-BUFs (layers={}, first format={:#x}, planes={}); copying through the CPU",
                        (*desc).nb_layers,
                        layer.format,
                        layer.nb_planes
                    );
                }
                ffi::av_frame_free(&mut (mapped_frame as *mut _));
                return Ok(false);
            };
            if plane0.object_index < 0
                || plane1.object_index < 0
                || plane0.object_index as usize >= (*desc).objects.len()
                || plane1.object_index as usize >= (*desc).objects.len()
                || plane0.object_index >= (*desc).nb_objects
                || plane1.object_index >= (*desc).nb_objects
            {
                ffi::av_frame_free(&mut (mapped_frame as *mut _));
                return Ok(false);
            }

            let object0 = (*desc).objects[plane0.object_index as usize];
            let object1 = (*desc).objects[plane1.object_index as usize];
            let fd0 = dup(object0.fd);
            let fd1 = dup(object1.fd);
            if fd0 < 0 || fd1 < 0 {
                if fd0 >= 0 {
                    close(fd0);
                }
                if fd1 >= 0 {
                    close(fd1);
                }
                ffi::av_frame_free(&mut (mapped_frame as *mut _));
                return Ok(false);
            }

            let modifier = object0.format_modifier;
            let delivered = crate::bridge::qobject::deliver_dmabuf_frame(
                fd0,
                fd1,
                DRM_FORMAT_NV12,
                modifier,
                plane0.offset as i32,
                plane0.pitch as i32,
                plane1.offset as i32,
                plane1.pitch as i32,
                (*gpu_frame).width as i32,
                (*gpu_frame).height as i32,
                // The video item keeps the frame (and with it the decoder's
                // surface) until it is no longer queued or shown, and frees
                // it with lunaris_release_video_frame.
                gpu_frame as usize as u64,
            );
            ffi::av_frame_free(&mut (mapped_frame as *mut _));
            if delivered {
                return Ok(true);
            }
        }

        Ok(false)
    }

    #[cfg(not(target_os = "linux"))]
    fn try_present_dmabuf(&mut self, _gpu_frame: *mut ffi::AVFrame) -> Result<bool, anyhow::Error> {
        Ok(false)
    }

    /// Hands a D3D11VA frame to the renderer. On success the renderer owns
    /// the frame (it frees it when replaced); the caller must not free it.
    fn try_present_d3d11(&mut self, gpu_frame: *mut ffi::AVFrame) -> Result<bool, anyhow::Error> {
        if !self.d3d11_device || crate::bridge::qobject::d3d11_render_failed() {
            return Ok(false);
        }

        unsafe {
            if (*gpu_frame).format != ffi::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
                return Ok(false);
            }
            if crate::bridge::qobject::deliver_d3d11_frame(gpu_frame as usize as u64) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Drop for HardwareDecoder {
    fn drop(&mut self) {
        let decoder_ptr = self as *const _ as usize;
        clear_active_cuda_frame_for_decoder(decoder_ptr);
        unsafe {
            if !self.codec_ctx.is_null() {
                ffi::avcodec_free_context(&mut self.codec_ctx);
            }
            if !self.hw_device_ctx.is_null() {
                ffi::av_buffer_unref(&mut self.hw_device_ctx);
            }
            if !self.sws_ctx.is_null() {
                ffi::sws_freeContext(self.sws_ctx);
            }
            if !self.bgrx_sws_ctx.is_null() {
                ffi::sws_freeContext(self.bgrx_sws_ctx);
            }
        }
    }
}

#[cfg(test)]
mod decode_label_tests {
    use super::*;

    const DEVICE_CTX: i32 = ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32;
    const VAAPI: ffi::AVHWDeviceType = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI;
    const CUDA: ffi::AVHWDeviceType = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA;
    const NONE: ffi::AVHWDeviceType = ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;

    #[test]
    fn a_device_counts_only_for_a_decoder_that_decodes_on_it() {
        // libdav1d: no hardware configuration at all.
        assert!(!codec_decodes_on(&[], VAAPI));
        assert!(!codec_decodes_on(&[], CUDA));
        // FFmpeg's own decoders list the devices they take.
        let h264 = [(VAAPI, DEVICE_CTX | ffi::AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX as i32), (CUDA, DEVICE_CTX)];
        assert!(codec_decodes_on(&h264, VAAPI));
        assert!(codec_decodes_on(&h264, CUDA));
        assert!(!codec_decodes_on(&h264, ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VDPAU));
        // A configuration that is not taken through `hw_device_ctx`.
        assert!(!codec_decodes_on(&[(VAAPI, ffi::AV_CODEC_HW_CONFIG_METHOD_INTERNAL as i32)], VAAPI));
        assert!(!codec_decodes_on(&h264, NONE));
    }

    #[test]
    fn the_label_says_software_unless_pictures_come_from_the_device() {
        // No device: software, before and after the first picture.
        assert!(decodes_in_software(false, None));
        assert!(decodes_in_software(false, Some(false)));
        // A device the decoder uses: hardware, until a picture says otherwise.
        assert!(!decodes_in_software(true, None));
        assert!(!decodes_in_software(true, Some(true)));
        assert!(decodes_in_software(true, Some(false)));

        assert_eq!(software_label("libdav1d"), "Software (dav1d)");
        assert_eq!(software_label("h264"), "Software (h264)");
        assert_eq!(software_label(""), "Software");
    }

    /// With this computer's FFmpeg: a decoder opened for software says
    /// "Software (<its name>)", and libdav1d lists no device.
    #[test]
    fn the_decoders_of_this_ffmpeg() {
        for (kind, id) in [
            (CodecType::H264, ffi::AVCodecID::AV_CODEC_ID_H264),
            (CodecType::H265, ffi::AVCodecID::AV_CODEC_ID_HEVC),
            (CodecType::AV1, ffi::AVCodecID::AV_CODEC_ID_AV1),
        ] {
            let codec = unsafe { ffi::avcodec_find_decoder(id) };
            if codec.is_null() {
                continue;
            }
            let decoder = unsafe { std::ffi::CStr::from_ptr((*codec).name) }.to_string_lossy().into_owned();
            let configs = codec_hw_configs(codec);
            println!("{:?}: decoder {} with hardware configurations {:?}", kind, decoder, configs.iter().map(|c| c.0 as i32).collect::<Vec<_>>());
            if decoder == "libdav1d" {
                assert!(configs.is_empty(), "libdav1d is a software decoder");
            }
            let software = HardwareDecoder::new(kind, true).expect("a software decoder");
            assert_eq!(software.decode_backend_label(), software_label(&decoder));
            assert!(!software.gpu_decode_enabled());
            assert_eq!(software.present_backend_label(), "CPU/QVideoSink");
            assert_eq!(software.fallback_reason(), "Software backend selected");
        }
    }
}

/// AV1 for real, without a window: frames encoded by the system's `ffmpeg`,
/// cut into RTP by the payloader the agent uses, put together again
/// (av1_rtp.rs) and decoded. Needs `ffmpeg` with libaom-av1 and, for the
/// hardware part, a GPU: `cargo test -p client-qml av1_offline -- --ignored --nocapture`.
#[cfg(test)]
mod av1_offline_tests {
    use super::*;
    use crate::av1_rtp::Av1Assembler;
    use rtc::rtp::packetizer::Payloader;

    /// The two tests change what the whole process tries (the device, AV1
    /// on a device at all): one at a time.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// What came out for one frame.
    #[derive(Debug, Clone, Copy)]
    struct Seen {
        on_gpu: bool,
        width: i32,
        height: i32,
        /// The picture maps to DMA-BUF handles (what `try_present_dmabuf` asks for).
        dmabuf: bool,
    }

    fn inspect(_: &mut HardwareDecoder, frame: *mut ffi::AVFrame) -> Result<Seen, anyhow::Error> {
        unsafe {
            let on_gpu = !(*frame).hw_frames_ctx.is_null();
            let mut dmabuf = false;
            if on_gpu {
                let mapped = ffi::av_frame_alloc();
                (*mapped).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
                dmabuf = ffi::av_hwframe_map(mapped, frame, ffi::AV_HWFRAME_MAP_READ as i32) >= 0
                    && (*mapped).format == ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
                    && !(*mapped).data[0].is_null();
                ffi::av_frame_free(&mut (mapped as *mut _));
            }
            let seen = Seen { on_gpu, width: (*frame).width, height: (*frame).height, dmabuf };
            ffi::av_frame_free(&mut (frame as *mut _));
            Ok(seen)
        }
    }

    /// The temporal units of a low-overhead AV1 stream (`ffmpeg -f obu`):
    /// cut at the temporal delimiters (OBU type 2), which are left out, as
    /// an RTP sender leaves them out (av1-rtp-spec 5: "MUST be removed").
    fn temporal_units(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut units: Vec<Vec<u8>> = Vec::new();
        let mut offset = 0;
        while offset < stream.len() {
            let start = offset;
            let header = stream[offset];
            assert!(header & 0x02 != 0, "an OBU without a size in a stream file");
            offset += if header & 0x04 != 0 { 2 } else { 1 };
            let (mut size, mut shift) = (0usize, 0);
            loop {
                let byte = stream[offset];
                offset += 1;
                size |= ((byte & 0x7f) as usize) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            offset += size;
            if (header >> 3) & 0x0f == 2 {
                units.push(Vec::new());
            } else {
                units.last_mut().expect("a stream begins with a temporal delimiter").extend_from_slice(&stream[start..offset]);
            }
        }
        units.retain(|unit| !unit.is_empty());
        units
    }

    fn encode(width: u32, height: u32, frames: u32) -> Option<Vec<u8>> {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("lunaris-av1-test-{}-{}x{}.obu", std::process::id(), width, height));
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={}x{}:rate=60", width, height))
            .args(["-frames:v", &frames.to_string(), "-pix_fmt", "yuv420p"])
            // Real time, no frames held back, one key frame: as a host encodes.
            .args(["-c:v", "libaom-av1", "-usage", "realtime", "-cpu-used", "8", "-lag-in-frames", "0", "-g", "600", "-b:v", "8M"])
            .args(["-f", "obu"])
            .arg(&path)
            .status()
            .ok()?;
        let data = status.success().then(|| std::fs::read(&path).ok()).flatten();
        let _ = std::fs::remove_file(&path);
        data
    }

    /// (pictures seen, mean milliseconds per frame in the decoder, the decoder at the end)
    fn run(units: &[Vec<u8>], hardware: bool) -> (Vec<Seen>, f64, HardwareDecoder) {
        let mut decoder = HardwareDecoder::new(CodecType::AV1, !hardware).expect("a decoder");
        let mut payloader = rtc::rtp::codec::av1::Av1Payloader::default();
        let mut assembler = Av1Assembler::default();
        let (mut seen, mut seq, mut spent) = (Vec::new(), 0u64, std::time::Duration::ZERO);
        for (index, unit) in units.iter().enumerate() {
            let payloads = payloader.payload(1200, &bytes::Bytes::copy_from_slice(unit)).expect("RTP payloads");
            let mut whole = None;
            for (i, payload) in payloads.iter().enumerate() {
                whole = assembler.push(seq, index as u32 * 1500, i + 1 == payloads.len(), payload);
                seq += 1;
            }
            let whole = whole.expect("the frame back from its packets");
            assert_eq!(&whole, unit, "frame {} comes back as it was sent", index);
            let started = std::time::Instant::now();
            match decoder.decode_guarded(&whole, inspect) {
                Ok(frames) => {
                    spent += started.elapsed();
                    seen.extend(frames);
                }
                Err(e) => println!("frame {}: {:?}", index, e),
            }
        }
        let mean = spent.as_secs_f64() * 1000.0 / seen.len().max(1) as f64;
        (seen, mean, decoder)
    }

    #[test]
    #[ignore = "needs the ffmpeg program with libaom-av1 and a GPU"]
    fn av1_offline_on_the_gpu_and_in_software() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        for (width, height) in [(1920u32, 1080u32), (1366, 766)] {
            let stream = encode(width, height, 60).expect("ffmpeg with libaom-av1");
            let units = temporal_units(&stream);
            assert_eq!(units.len(), 60);
            assert!(av1_unit_has_sequence_header(&units[0]));
            assert!(!av1_unit_has_sequence_header(&units[1]));

            let (seen, software_ms, decoder) = run(&units, false);
            assert_eq!(seen.len(), 60);
            assert!(seen.iter().all(|f| !f.on_gpu && f.width == width as i32 && f.height == height as i32), "{:?}", seen[0]);
            assert!(decoder.decode_backend_label().starts_with("Software ("), "{}", decoder.decode_backend_label());
            assert!(!decoder.gpu_decode_enabled());
            println!("{}x{} software: {} ({:.2} ms per frame)", width, height, decoder.decode_backend_label(), software_ms);

            let (seen, hardware_ms, decoder) = run(&units, true);
            println!(
                "{}x{} hardware asked: {} ({:.2} ms per frame, {} frames, on the GPU: {}, as DMA-BUF: {}), fallback reason: {}",
                width,
                height,
                decoder.decode_backend_label(),
                hardware_ms,
                seen.len(),
                seen.iter().filter(|f| f.on_gpu).count(),
                seen.iter().filter(|f| f.dmabuf).count(),
                decoder.fallback_reason()
            );
            // This computer's display GPU decodes AV1 (vainfo: VAProfileAV1Profile0 VLD).
            assert_eq!(decoder.decode_backend_label(), "VAAPI");
            assert_eq!(seen.len(), 60);
            assert!(seen.iter().all(|f| f.on_gpu && f.dmabuf && f.width == width as i32 && f.height == height as i32), "{:?}", seen[0]);
            assert!(decoder.gpu_decode_enabled());
        }
    }

    /// A device that opens but has no AV1 decoding (NVDEC of an RTX 20):
    /// the software decoder takes over at the first key frame, once.
    #[test]
    #[ignore = "needs the ffmpeg program with libaom-av1 and an NVIDIA GPU without AV1 decoding"]
    fn av1_offline_falls_back_when_the_device_cannot_decode_it() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let stream = encode(1280, 720, 10).expect("ffmpeg with libaom-av1");
        let units = temporal_units(&stream);
        *TEST_ONLY_DEVICE.lock().unwrap() = Some(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA);
        let mut decoder = HardwareDecoder::new(CodecType::AV1, false).expect("a decoder");
        *TEST_ONLY_DEVICE.lock().unwrap() = None;
        if decoder.device_label() != "CUDA" {
            println!("no CUDA device here: nothing to try");
            return;
        }
        assert!(decoder.hardware_only);
        // The key frame fails on the device ...
        let first = decoder.decode_guarded(&units[0], inspect);
        println!("the key frame on CUDA: {:?} (FFmpeg error {}, ENOSYS is {})", first.as_ref().map(|f| f.len()), decoder.last_error_code, ffi::AVERROR(ffmpeg_next::util::error::ENOSYS));
        assert!(first.is_err());
        // ... and the decoder is the software one from then on.
        assert!(!decoder.hardware_only);
        assert!(decoder.decode_backend_label().starts_with("Software ("));
        assert!(decoder.fallback_reason().starts_with("The GPU could not decode this stream"));
        // The key frame the viewer asks for again decodes, and what follows.
        let mut frames = 0;
        for unit in &units {
            frames += decoder.decode_guarded(unit, inspect).expect("software decoding").len();
        }
        assert_eq!(frames, units.len());
        // And AV1 is not tried on a device again in this process.
        assert!(AV1_HARDWARE_UNUSABLE.load(std::sync::atomic::Ordering::Relaxed));
        let next = HardwareDecoder::new(CodecType::AV1, false).expect("a decoder");
        assert!(!next.hardware_only);
        AV1_HARDWARE_UNUSABLE.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn when_the_hardware_decoder_is_given_up() {
        // FFmpeg said there is no hardware decoding: at once.
        assert!(hardware_decoder_gives_up(true, 0));
        // Other failures: not for one damaged key frame, after three.
        assert!(!hardware_decoder_gives_up(false, 0));
        assert!(!hardware_decoder_gives_up(false, 2));
        assert!(hardware_decoder_gives_up(false, 3));
        // Sequence header first (type 1, with size), then a frame (type 6).
        assert!(av1_unit_has_sequence_header(&[0x0A, 1, 0, 0x32, 2, 9, 9]));
        assert!(av1_unit_has_sequence_header(&[0x12, 0, 0x0A, 1, 0]));
        assert!(!av1_unit_has_sequence_header(&[0x32, 2, 9, 9]));
        assert!(!av1_unit_has_sequence_header(&[]));
        assert!(!av1_unit_has_sequence_header(&[0x32, 0x80]));
    }
}

#[cfg(test)]
mod color_tests {
    use super::{ffi, frame_color};

    #[test]
    fn tags_choose_matrix_and_range() {
        let nv12 = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        let unspec = ffi::AVColorSpace::AVCOL_SPC_UNSPECIFIED as i32;
        let mpeg = ffi::AVColorRange::AVCOL_RANGE_MPEG as i32;
        let jpeg = ffi::AVColorRange::AVCOL_RANGE_JPEG as i32;
        let no_range = ffi::AVColorRange::AVCOL_RANGE_UNSPECIFIED as i32;
        // The default request: Rec. 709, limited.
        let default = crate::bridge::HOST_DEFAULT_ENCODER_CSC_MODE;
        // Untagged: what was requested, here BT.709 studio.
        assert_eq!(frame_color(unspec, no_range, nv12, default), (true, false));
        assert_eq!(frame_color(ffi::AVColorSpace::AVCOL_SPC_BT709 as i32, jpeg, nv12, default), (true, true));
        assert_eq!(frame_color(ffi::AVColorSpace::AVCOL_SPC_SMPTE170M as i32, mpeg, nv12, default), (false, false));
        assert_eq!(frame_color(unspec, no_range, ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32, default), (true, true));
    }

    #[test]
    fn untagged_frames_take_the_requested_colours() {
        let nv12 = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        let unspec = ffi::AVColorSpace::AVCOL_SPC_UNSPECIFIED as i32;
        let mpeg = ffi::AVColorRange::AVCOL_RANGE_MPEG as i32;
        let no_range = ffi::AVColorRange::AVCOL_RANGE_UNSPECIFIED as i32;
        // Rec. 601 full range requested (encoderCscMode 1).
        assert_eq!(frame_color(unspec, no_range, nv12, 1), (false, true));
        // Rec. 2020 limited requested (4): not BT.601.
        assert_eq!(frame_color(unspec, no_range, nv12, 4), (true, false));
        // Tags win over the request.
        assert_eq!(frame_color(ffi::AVColorSpace::AVCOL_SPC_BT709 as i32, mpeg, nv12, 1), (true, false));
    }
}

/// Frees a decoder frame that was handed to the video item with
/// `deliver_dmabuf_frame` (called from C++ when the frame is dropped from the
/// pacing queue or replaced on screen).
#[no_mangle]
pub extern "C" fn lunaris_release_video_frame(owner: u64) {
    let mut frame = owner as usize as *mut ffi::AVFrame;
    if !frame.is_null() {
        unsafe { ffi::av_frame_free(&mut frame) };
    }
}
