//! Experimental C ABI for `MMRecode`.
//!
//! This crate is the only workspace component permitted to use unsafe Rust.
//! Its public contract is the checked-in `include/mmrecode.h` header. The ABI
//! is intentionally experimental while the codec and ownership APIs mature.

use std::{
    cell::RefCell,
    ffi::c_char,
    fmt,
    mem::{self, size_of},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice,
};

use mmrecode_core::{
    ColorDescription, ColorRange, Error, FieldOrder, FrameTiming, PixelFormat, Plane, VideoFrame,
};
use mmrecode_mjpeg::{JpegEncodeOptions, decode_jpeg, encode_jpeg};
use mmrecode_mpeg2::{FrameRate, Mpeg2EncodeOptions};

const ABI_VERSION: u32 = 1;
const MAX_PLANES: usize = 3;
const STATUS_OK: i32 = 0;
const STATUS_INVALID_ARGUMENT: i32 = 1;
const STATUS_INVALID_DATA: i32 = 2;
const STATUS_UNSUPPORTED: i32 = 3;
const STATUS_INVALID_STATE: i32 = 4;
const STATUS_IO_ERROR: i32 = 5;
const STATUS_INTERNAL_ERROR: i32 = 100;

const PIXEL_FORMAT_GRAY8: u32 = 1;
const PIXEL_FORMAT_YUV420P8: u32 = 2;
const PIXEL_FORMAT_YUV422P8: u32 = 3;
const PIXEL_FORMAT_YUV444P8: u32 = 4;
const PIXEL_FORMAT_YUV411P8: u32 = 5;

const COLOR_RANGE_UNSPECIFIED: u32 = 0;
const COLOR_RANGE_FULL: u32 = 1;
const COLOR_RANGE_LIMITED: u32 = 2;

static VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

#[derive(Debug)]
enum ApiError {
    InvalidArgument(String),
    Core(Error),
}

impl ApiError {
    const fn status(&self) -> i32 {
        match self {
            Self::InvalidArgument(_) => STATUS_INVALID_ARGUMENT,
            Self::Core(Error::InvalidData(_)) => STATUS_INVALID_DATA,
            Self::Core(Error::Unsupported(_)) => STATUS_UNSUPPORTED,
            Self::Core(Error::InvalidState(_)) => STATUS_INVALID_STATE,
            Self::Core(Error::Io(_)) => STATUS_IO_ERROR,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument(message) => write!(formatter, "invalid argument: {message}"),
            Self::Core(error) => error.fmt(formatter),
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        Self::Core(error)
    }
}

type ApiResult<T> = Result<T, ApiError>;

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(missing_docs)]
pub struct MmrPlaneView {
    pub data: *const u8,
    pub data_len: usize,
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

impl MmrPlaneView {
    #[cfg(test)]
    const fn empty() -> Self {
        Self {
            data: ptr::null(),
            data_len: 0,
            stride: 0,
            width: 0,
            height: 0,
        }
    }
}

#[repr(C)]
#[allow(missing_docs)]
pub struct MmrVideoFrameView {
    pub struct_size: usize,
    pub format: u32,
    pub range: u32,
    pub width: usize,
    pub height: usize,
    pub plane_count: usize,
    pub planes: [MmrPlaneView; MAX_PLANES],
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(missing_docs)]
pub struct MmrOwnedPlane {
    pub data: *mut u8,
    pub data_len: usize,
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

impl MmrOwnedPlane {
    const fn empty() -> Self {
        Self {
            data: ptr::null_mut(),
            data_len: 0,
            stride: 0,
            width: 0,
            height: 0,
        }
    }

    fn from_plane(plane: Plane) -> Self {
        let mut data = plane.data.into_boxed_slice();
        let data_ptr = data.as_mut_ptr();
        let data_len = data.len();
        mem::forget(data);
        Self {
            data: data_ptr,
            data_len,
            stride: plane.stride,
            width: plane.width,
            height: plane.height,
        }
    }

    unsafe fn free(self) {
        if !self.data.is_null() {
            let allocation = ptr::slice_from_raw_parts_mut(self.data, self.data_len);
            // SAFETY: `data` and `data_len` came from `Box<[u8]>` in
            // `from_plane`, and ownership is transferred exactly once.
            drop(unsafe { Box::from_raw(allocation) });
        }
    }
}

#[repr(C)]
#[allow(missing_docs)]
pub struct MmrVideoFrame {
    pub struct_size: usize,
    pub format: u32,
    pub range: u32,
    pub width: usize,
    pub height: usize,
    pub plane_count: usize,
    pub planes: [MmrOwnedPlane; MAX_PLANES],
}

impl MmrVideoFrame {
    const fn empty() -> Self {
        Self {
            struct_size: size_of::<Self>(),
            format: 0,
            range: COLOR_RANGE_UNSPECIFIED,
            width: 0,
            height: 0,
            plane_count: 0,
            planes: [MmrOwnedPlane::empty(); MAX_PLANES],
        }
    }

    fn from_video_frame(frame: VideoFrame) -> ApiResult<Self> {
        let format = pixel_format_to_c(frame.format)?;
        let range = color_range_to_c(frame.color.range);
        if frame.planes.len() > MAX_PLANES {
            return Err(ApiError::Core(Error::Unsupported(
                "C API supports at most three video planes".into(),
            )));
        }

        let mut planes = [MmrOwnedPlane::empty(); MAX_PLANES];
        let plane_count = frame.planes.len();
        for (destination, source) in planes.iter_mut().zip(frame.planes) {
            *destination = MmrOwnedPlane::from_plane(source);
        }
        Ok(Self {
            struct_size: size_of::<Self>(),
            format,
            range,
            width: frame.width,
            height: frame.height,
            plane_count,
            planes,
        })
    }

    unsafe fn free(self) {
        for plane in self
            .planes
            .into_iter()
            .take(self.plane_count.min(MAX_PLANES))
        {
            // SAFETY: the frame contract requires allocations returned by this
            // library to be passed back unchanged and freed at most once.
            unsafe { plane.free() };
        }
    }
}

#[repr(C)]
#[allow(missing_docs)]
pub struct MmrBuffer {
    pub struct_size: usize,
    pub data: *mut u8,
    pub len: usize,
}

#[repr(C)]
#[allow(missing_docs)]
pub struct MmrMpeg2EncodeOptions {
    pub struct_size: usize,
    pub frame_rate_numerator: u32,
    pub frame_rate_denominator: u32,
    pub gop_size: u32,
    pub b_frames: u32,
    pub quantiser_scale_code: u32,
    pub motion_search_range: u32,
    pub progressive: u32,
    pub top_field_first: u32,
}

impl MmrBuffer {
    const fn empty() -> Self {
        Self {
            struct_size: size_of::<Self>(),
            data: ptr::null_mut(),
            len: 0,
        }
    }

    fn from_vec(data: Vec<u8>) -> Self {
        let mut data = data.into_boxed_slice();
        let data_ptr = data.as_mut_ptr();
        let len = data.len();
        mem::forget(data);
        Self {
            struct_size: size_of::<Self>(),
            data: data_ptr,
            len,
        }
    }

    unsafe fn free(self) {
        if !self.data.is_null() {
            let allocation = ptr::slice_from_raw_parts_mut(self.data, self.len);
            // SAFETY: `data` and `len` came from `Box<[u8]>` in `from_vec`,
            // and ownership is transferred exactly once.
            drop(unsafe { Box::from_raw(allocation) });
        }
    }
}

/// Returns the version of the experimental C ABI.
#[unsafe(no_mangle)]
pub extern "C" fn mmr_abi_version() -> u32 {
    ABI_VERSION
}

/// Returns the `MMRecode` library version as a process-lifetime C string.
#[unsafe(no_mangle)]
pub extern "C" fn mmr_version() -> *const c_char {
    VERSION.as_ptr().cast()
}

/// Copies the calling thread's last diagnostic into a C buffer.
///
/// The return value is the required capacity including the terminating NUL.
/// Passing a null buffer is permitted and performs a size query.
///
/// # Safety
///
/// When `buffer` is non-null and `capacity` is nonzero, it must point to at
/// least `capacity` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_last_error_message(buffer: *mut c_char, capacity: usize) -> usize {
    LAST_ERROR.with(|last_error| {
        let last_error = last_error.borrow();
        let bytes = last_error.as_bytes();
        let required = bytes.len().saturating_add(1);
        if !buffer.is_null() && capacity > 0 {
            let copied = bytes.len().min(capacity - 1);
            // SAFETY: the caller guarantees `capacity` writable bytes and the
            // copy is bounded to at most `capacity - 1`.
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast(), copied);
                *buffer.add(copied) = 0;
            }
        }
        required
    })
}

/// Decodes one complete baseline JPEG image into library-owned planes.
///
/// # Safety
///
/// `data` must identify `len` readable bytes. `out_frame` must be writable,
/// zero-initialized, and have its `struct_size` initialized to the size of the
/// C `mmr_video_frame` type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_mjpeg_decode(
    data: *const u8,
    len: usize,
    out_frame: *mut MmrVideoFrame,
) -> i32 {
    ffi_status(|| {
        // SAFETY: pointer validation and all dereferences are centralized in
        // the implementation, under the caller contract above.
        unsafe { mjpeg_decode_impl(data, len, out_frame) }
    })
}

/// Decodes one complete raw DV25 frame into library-owned planes.
///
/// # Safety
///
/// `data` must identify `len` readable bytes. `out_frame` must be writable,
/// zero-initialized, and carry the correct structure size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_dv_decode(
    data: *const u8,
    len: usize,
    out_frame: *mut MmrVideoFrame,
) -> i32 {
    ffi_status(|| {
        // SAFETY: pointer validation is centralized in the implementation.
        unsafe { dv_decode_impl(data, len, out_frame) }
    })
}

/// Counts pictures in a complete MPEG-2 Video elementary stream.
///
/// # Safety
///
/// `data` must identify `len` readable bytes and `out_count` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_mpeg2_picture_count(
    data: *const u8,
    len: usize,
    out_count: *mut usize,
) -> i32 {
    ffi_status(|| {
        // SAFETY: pointer validation is centralized in the implementation.
        unsafe { mpeg2_picture_count_impl(data, len, out_count) }
    })
}

/// Decodes one MPEG-2 picture selected by presentation order.
///
/// # Safety
///
/// `data` must identify `len` readable bytes. `out_frame` must be writable,
/// zero-initialized, and carry the correct structure size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_mpeg2_decode_picture(
    data: *const u8,
    len: usize,
    presentation_index: usize,
    out_frame: *mut MmrVideoFrame,
) -> i32 {
    ffi_status(|| {
        // SAFETY: pointer validation is centralized in the implementation.
        unsafe { mpeg2_decode_picture_impl(data, len, presentation_index, out_frame) }
    })
}

/// Releases all allocations held by a decoded C video frame.
///
/// A null pointer is ignored. A valid frame is reset so repeated calls are
/// harmless when the caller does not modify the returned allocation fields.
///
/// # Safety
///
/// `frame` must be null or point to a frame initialized by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_video_frame_free(frame: *mut MmrVideoFrame) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if frame.is_null() {
            return;
        }
        // SAFETY: the pointer validity is guaranteed by the caller. Replacing
        // first prevents a second call from observing owned allocations.
        let owned = unsafe { mem::replace(&mut *frame, MmrVideoFrame::empty()) };
        // SAFETY: ownership originated from `MmrVideoFrame::from_video_frame`.
        unsafe { owned.free() };
    }));
}

/// Encodes one borrowed planar frame as a library-owned baseline JPEG buffer.
///
/// # Safety
///
/// `frame` and each populated plane must remain readable for the call.
/// `out_buffer` must be writable, zero-initialized, and have its `struct_size`
/// initialized to the size of the C `mmr_buffer` type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_mjpeg_encode(
    frame: *const MmrVideoFrameView,
    quality: u8,
    out_buffer: *mut MmrBuffer,
) -> i32 {
    ffi_status(|| {
        // SAFETY: pointer validation and all dereferences are centralized in
        // the implementation, under the caller contract above.
        unsafe { mjpeg_encode_impl(frame, quality, out_buffer) }
    })
}

/// Encodes one borrowed native-layout frame as a raw DV25 frame.
///
/// # Safety
///
/// `frame` and its planes must remain readable for the call. `out_buffer`
/// must be writable, zero-initialized, and carry the correct structure size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_dv_encode(
    frame: *const MmrVideoFrameView,
    out_buffer: *mut MmrBuffer,
) -> i32 {
    ffi_status(|| {
        // SAFETY: pointer validation is centralized in the implementation.
        unsafe { dv_encode_impl(frame, out_buffer) }
    })
}

/// Encodes complete borrowed frames as an MPEG-2 Video elementary stream.
///
/// # Safety
///
/// `frames` must identify `frame_count` readable frame views whose plane
/// storage remains readable for the call. `options` must be readable.
/// `out_buffer` must be writable, zero-initialized, and carry the correct
/// structure size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_mpeg2_encode(
    frames: *const MmrVideoFrameView,
    frame_count: usize,
    options: *const MmrMpeg2EncodeOptions,
    out_buffer: *mut MmrBuffer,
) -> i32 {
    ffi_status(|| {
        // SAFETY: pointer validation is centralized in the implementation.
        unsafe { mpeg2_encode_impl(frames, frame_count, options, out_buffer) }
    })
}

/// Wraps a complete MPEG-2 Video elementary stream in MPEG-TS.
///
/// # Safety
///
/// `data` must identify `len` readable bytes. `out_buffer` must be writable,
/// zero-initialized, and carry the correct structure size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_mpegts_mux_mpeg2(
    data: *const u8,
    len: usize,
    out_buffer: *mut MmrBuffer,
) -> i32 {
    ffi_status(|| {
        // SAFETY: pointer validation is centralized in the implementation.
        unsafe { mpegts_transform_impl(data, len, out_buffer, true) }
    })
}

/// Extracts the first MPEG-2 Video elementary stream from MPEG-TS.
///
/// # Safety
///
/// `data` must identify `len` readable bytes. `out_buffer` must be writable,
/// zero-initialized, and carry the correct structure size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_mpegts_demux_mpeg2(
    data: *const u8,
    len: usize,
    out_buffer: *mut MmrBuffer,
) -> i32 {
    ffi_status(|| {
        // SAFETY: pointer validation is centralized in the implementation.
        unsafe { mpegts_transform_impl(data, len, out_buffer, false) }
    })
}

/// Releases an encoded buffer allocated by `MMRecode`.
///
/// A null pointer is ignored. The buffer is reset after it is released.
///
/// # Safety
///
/// `buffer` must be null or point to a buffer initialized by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmr_buffer_free(buffer: *mut MmrBuffer) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if buffer.is_null() {
            return;
        }
        // SAFETY: the pointer validity is guaranteed by the caller. Replacing
        // first prevents a second call from observing the allocation.
        let owned = unsafe { mem::replace(&mut *buffer, MmrBuffer::empty()) };
        // SAFETY: ownership originated from `MmrBuffer::from_vec`.
        unsafe { owned.free() };
    }));
}

fn ffi_status(operation: impl FnOnce() -> ApiResult<()>) -> i32 {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => {
            set_last_error(String::new());
            STATUS_OK
        }
        Ok(Err(error)) => {
            let status = error.status();
            set_last_error(error.to_string());
            status
        }
        Err(_) => {
            set_last_error("panic contained at MMRecode C API boundary".into());
            STATUS_INTERNAL_ERROR
        }
    }
}

fn set_last_error(message: String) {
    LAST_ERROR.with(|last_error| *last_error.borrow_mut() = message);
}

unsafe fn mjpeg_decode_impl(
    data: *const u8,
    len: usize,
    out_frame: *mut MmrVideoFrame,
) -> ApiResult<()> {
    if out_frame.is_null() {
        return Err(ApiError::InvalidArgument("out_frame is null".into()));
    }
    // SAFETY: the caller guarantees a writable output pointer.
    let out_frame = unsafe { &mut *out_frame };
    if out_frame.struct_size != size_of::<MmrVideoFrame>() {
        return Err(ApiError::InvalidArgument(format!(
            "out_frame struct_size is {}, expected {}",
            out_frame.struct_size,
            size_of::<MmrVideoFrame>()
        )));
    }
    *out_frame = MmrVideoFrame::empty();

    let input = unsafe { borrowed_bytes(data, len, "data")? };
    let decoded = decode_jpeg(input)?.into_video_frame()?;
    *out_frame = MmrVideoFrame::from_video_frame(decoded)?;
    Ok(())
}

unsafe fn dv_decode_impl(
    data: *const u8,
    len: usize,
    out_frame: *mut MmrVideoFrame,
) -> ApiResult<()> {
    if out_frame.is_null() {
        return Err(ApiError::InvalidArgument("out_frame is null".into()));
    }
    // SAFETY: the caller guarantees a writable output pointer.
    let out_frame = unsafe { &mut *out_frame };
    if out_frame.struct_size != size_of::<MmrVideoFrame>() {
        return Err(ApiError::InvalidArgument(format!(
            "out_frame struct_size is {}, expected {}",
            out_frame.struct_size,
            size_of::<MmrVideoFrame>()
        )));
    }
    *out_frame = MmrVideoFrame::empty();
    let input = unsafe { borrowed_bytes(data, len, "data")? };
    let parsed = mmrecode_dv::parse_frame(input)?;
    let decoded = mmrecode_dv::decode_video(&parsed)?;
    *out_frame = MmrVideoFrame::from_video_frame(decoded)?;
    Ok(())
}

unsafe fn mpeg2_picture_count_impl(
    data: *const u8,
    len: usize,
    out_count: *mut usize,
) -> ApiResult<()> {
    if out_count.is_null() {
        return Err(ApiError::InvalidArgument("out_count is null".into()));
    }
    // SAFETY: the caller guarantees a writable output pointer.
    unsafe { *out_count = 0 };
    let input = unsafe { borrowed_bytes(data, len, "data")? };
    let stream = mmrecode_mpeg2::parse_stream(input)?;
    // SAFETY: the pointer remains valid for the call.
    unsafe { *out_count = stream.pictures().len() };
    Ok(())
}

unsafe fn mpeg2_decode_picture_impl(
    data: *const u8,
    len: usize,
    presentation_index: usize,
    out_frame: *mut MmrVideoFrame,
) -> ApiResult<()> {
    if out_frame.is_null() {
        return Err(ApiError::InvalidArgument("out_frame is null".into()));
    }
    // SAFETY: the caller guarantees a writable output pointer.
    let out_frame = unsafe { &mut *out_frame };
    validate_owned_frame_output(out_frame)?;
    *out_frame = MmrVideoFrame::empty();
    let input = unsafe { borrowed_bytes(data, len, "data")? };
    let pictures = mmrecode_mpeg2::decode_stream(input)?;
    let picture_count = pictures.len();
    let picture = pictures.into_iter().nth(presentation_index).ok_or_else(|| {
        ApiError::InvalidArgument(format!(
            "presentation_index {presentation_index} is outside the {picture_count}-picture stream"
        ))
    })?;
    *out_frame = MmrVideoFrame::from_video_frame(picture.frame)?;
    Ok(())
}

unsafe fn mjpeg_encode_impl(
    frame: *const MmrVideoFrameView,
    quality: u8,
    out_buffer: *mut MmrBuffer,
) -> ApiResult<()> {
    if frame.is_null() {
        return Err(ApiError::InvalidArgument("frame is null".into()));
    }
    if out_buffer.is_null() {
        return Err(ApiError::InvalidArgument("out_buffer is null".into()));
    }
    // SAFETY: the caller guarantees readable/writable structure pointers.
    let (frame, out_buffer) = unsafe { (&*frame, &mut *out_buffer) };
    if frame.struct_size != size_of::<MmrVideoFrameView>() {
        return Err(ApiError::InvalidArgument(format!(
            "frame struct_size is {}, expected {}",
            frame.struct_size,
            size_of::<MmrVideoFrameView>()
        )));
    }
    if out_buffer.struct_size != size_of::<MmrBuffer>() {
        return Err(ApiError::InvalidArgument(format!(
            "out_buffer struct_size is {}, expected {}",
            out_buffer.struct_size,
            size_of::<MmrBuffer>()
        )));
    }
    *out_buffer = MmrBuffer::empty();

    let frame = unsafe { video_frame_from_view(frame)? };
    let encoded = encode_jpeg(&frame, JpegEncodeOptions { quality })?;
    *out_buffer = MmrBuffer::from_vec(encoded.data);
    Ok(())
}

unsafe fn dv_encode_impl(
    frame: *const MmrVideoFrameView,
    out_buffer: *mut MmrBuffer,
) -> ApiResult<()> {
    if frame.is_null() {
        return Err(ApiError::InvalidArgument("frame is null".into()));
    }
    if out_buffer.is_null() {
        return Err(ApiError::InvalidArgument("out_buffer is null".into()));
    }
    // SAFETY: the caller guarantees readable/writable structure pointers.
    let (frame, out_buffer) = unsafe { (&*frame, &mut *out_buffer) };
    if frame.struct_size != size_of::<MmrVideoFrameView>() {
        return Err(ApiError::InvalidArgument(format!(
            "frame struct_size is {}, expected {}",
            frame.struct_size,
            size_of::<MmrVideoFrameView>()
        )));
    }
    if out_buffer.struct_size != size_of::<MmrBuffer>() {
        return Err(ApiError::InvalidArgument(format!(
            "out_buffer struct_size is {}, expected {}",
            out_buffer.struct_size,
            size_of::<MmrBuffer>()
        )));
    }
    *out_buffer = MmrBuffer::empty();
    let frame = unsafe { video_frame_from_view(frame)? };
    let encoded = mmrecode_dv::encode_video(&frame)?;
    *out_buffer = MmrBuffer::from_vec(encoded.data);
    Ok(())
}

unsafe fn mpeg2_encode_impl(
    frames: *const MmrVideoFrameView,
    frame_count: usize,
    options: *const MmrMpeg2EncodeOptions,
    out_buffer: *mut MmrBuffer,
) -> ApiResult<()> {
    if frame_count == 0 {
        return Err(ApiError::InvalidArgument("frame_count is zero".into()));
    }
    if frames.is_null() {
        return Err(ApiError::InvalidArgument("frames is null".into()));
    }
    if frame_count > isize::MAX as usize {
        return Err(ApiError::InvalidArgument(
            "frame_count exceeds the addressable object limit".into(),
        ));
    }
    if options.is_null() {
        return Err(ApiError::InvalidArgument("options is null".into()));
    }
    if out_buffer.is_null() {
        return Err(ApiError::InvalidArgument("out_buffer is null".into()));
    }
    // SAFETY: the caller guarantees readable input structures and a writable output structure.
    let (frames, options, out_buffer) = unsafe {
        (
            slice::from_raw_parts(frames, frame_count),
            &*options,
            &mut *out_buffer,
        )
    };
    if options.struct_size != size_of::<MmrMpeg2EncodeOptions>() {
        return Err(ApiError::InvalidArgument(format!(
            "options struct_size is {}, expected {}",
            options.struct_size,
            size_of::<MmrMpeg2EncodeOptions>()
        )));
    }
    validate_buffer_output(out_buffer)?;
    *out_buffer = MmrBuffer::empty();

    let mut owned_frames = Vec::with_capacity(frame_count);
    for frame in frames {
        if frame.struct_size != size_of::<MmrVideoFrameView>() {
            return Err(ApiError::InvalidArgument(format!(
                "frame struct_size is {}, expected {}",
                frame.struct_size,
                size_of::<MmrVideoFrameView>()
            )));
        }
        owned_frames.push(unsafe { video_frame_from_view(frame)? });
    }
    let encoded = mmrecode_mpeg2::encode_stream(
        &owned_frames,
        Mpeg2EncodeOptions {
            frame_rate: mpeg2_frame_rate(
                options.frame_rate_numerator,
                options.frame_rate_denominator,
            )?,
            gop_size: usize::try_from(options.gop_size)
                .map_err(|_| ApiError::InvalidArgument("gop_size does not fit size_t".into()))?,
            b_frames: usize::try_from(options.b_frames)
                .map_err(|_| ApiError::InvalidArgument("b_frames does not fit size_t".into()))?,
            quantiser_scale_code: u8::try_from(options.quantiser_scale_code).map_err(|_| {
                ApiError::InvalidArgument("quantiser_scale_code does not fit uint8_t".into())
            })?,
            motion_search_range: usize::try_from(options.motion_search_range).map_err(|_| {
                ApiError::InvalidArgument("motion_search_range does not fit size_t".into())
            })?,
            progressive: c_boolean(options.progressive, "progressive")?,
            top_field_first: c_boolean(options.top_field_first, "top_field_first")?,
        },
    )?;
    *out_buffer = MmrBuffer::from_vec(encoded.data);
    Ok(())
}

unsafe fn mpegts_transform_impl(
    data: *const u8,
    len: usize,
    out_buffer: *mut MmrBuffer,
    mux: bool,
) -> ApiResult<()> {
    if out_buffer.is_null() {
        return Err(ApiError::InvalidArgument("out_buffer is null".into()));
    }
    // SAFETY: the caller guarantees a writable output structure.
    let out_buffer = unsafe { &mut *out_buffer };
    validate_buffer_output(out_buffer)?;
    *out_buffer = MmrBuffer::empty();
    let input = unsafe { borrowed_bytes(data, len, "data")? };
    let output = if mux {
        mmrecode_mpeg2::parse_stream(input)?;
        mmrecode_mpegts::mux_mpeg2_video(input)?
    } else {
        mmrecode_mpegts::demux_transport_stream(input)?.mpeg2_video_bytes()?
    };
    *out_buffer = MmrBuffer::from_vec(output);
    Ok(())
}

fn validate_owned_frame_output(out_frame: &MmrVideoFrame) -> ApiResult<()> {
    if out_frame.struct_size != size_of::<MmrVideoFrame>() {
        return Err(ApiError::InvalidArgument(format!(
            "out_frame struct_size is {}, expected {}",
            out_frame.struct_size,
            size_of::<MmrVideoFrame>()
        )));
    }
    Ok(())
}

fn validate_buffer_output(out_buffer: &MmrBuffer) -> ApiResult<()> {
    if out_buffer.struct_size != size_of::<MmrBuffer>() {
        return Err(ApiError::InvalidArgument(format!(
            "out_buffer struct_size is {}, expected {}",
            out_buffer.struct_size,
            size_of::<MmrBuffer>()
        )));
    }
    Ok(())
}

fn mpeg2_frame_rate(numerator: u32, denominator: u32) -> ApiResult<FrameRate> {
    match (numerator, denominator) {
        (24_000, 1_001) => Ok(FrameRate::Fps23_976),
        (24, 1) => Ok(FrameRate::Fps24),
        (25, 1) => Ok(FrameRate::Fps25),
        (30_000, 1_001) => Ok(FrameRate::Fps29_97),
        (30, 1) => Ok(FrameRate::Fps30),
        (50, 1) => Ok(FrameRate::Fps50),
        (60_000, 1_001) => Ok(FrameRate::Fps59_94),
        (60, 1) => Ok(FrameRate::Fps60),
        _ => Err(ApiError::InvalidArgument(format!(
            "unsupported MPEG-2 frame rate {numerator}/{denominator}"
        ))),
    }
}

fn c_boolean(value: u32, name: &str) -> ApiResult<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ApiError::InvalidArgument(format!(
            "{name} must be zero or one"
        ))),
    }
}

unsafe fn video_frame_from_view(view: &MmrVideoFrameView) -> ApiResult<VideoFrame> {
    if view.plane_count > MAX_PLANES {
        return Err(ApiError::InvalidArgument(format!(
            "plane_count {} exceeds {MAX_PLANES}",
            view.plane_count
        )));
    }
    let format = pixel_format_from_c(view.format)?;
    let range = color_range_from_c(view.range)?;
    let expected_planes = match format {
        PixelFormat::Gray8 => 1,
        PixelFormat::Yuv420p8
        | PixelFormat::Yuv411p8
        | PixelFormat::Yuv422p8
        | PixelFormat::Yuv444p8 => 3,
        PixelFormat::Rgb24 => unreachable!("C API does not expose packed RGB"),
        _ => {
            return Err(ApiError::InvalidArgument(
                "unknown pixel format at C API boundary".into(),
            ));
        }
    };
    if view.plane_count != expected_planes {
        return Err(ApiError::InvalidArgument(format!(
            "pixel format requires {expected_planes} planes, received {}",
            view.plane_count
        )));
    }

    let mut planes = Vec::with_capacity(view.plane_count);
    for (index, plane) in view.planes[..view.plane_count].iter().enumerate() {
        let data = unsafe { borrowed_bytes(plane.data, plane.data_len, "plane data")? };
        validate_plane_storage(plane, index)?;
        planes.push(Plane {
            data: data.to_vec(),
            stride: plane.stride,
            width: plane.width,
            height: plane.height,
        });
    }

    Ok(VideoFrame {
        format,
        width: view.width,
        height: view.height,
        planes,
        timing: FrameTiming::default(),
        color: ColorDescription {
            range,
            primaries: None,
            transfer: None,
            matrix: None,
        },
        field_order: FieldOrder::Progressive,
    })
}

fn validate_plane_storage(plane: &MmrPlaneView, index: usize) -> ApiResult<()> {
    let rows_before_last = plane.height.saturating_sub(1);
    let required = rows_before_last
        .checked_mul(plane.stride)
        .and_then(|offset| offset.checked_add(plane.width))
        .ok_or_else(|| {
            ApiError::InvalidArgument(format!("plane {index} storage dimensions overflow"))
        })?;
    if required > plane.data_len {
        return Err(ApiError::InvalidArgument(format!(
            "plane {index} needs {required} bytes but data_len is {}",
            plane.data_len
        )));
    }
    Ok(())
}

unsafe fn borrowed_bytes<'a>(
    data: *const u8,
    len: usize,
    argument_name: &str,
) -> ApiResult<&'a [u8]> {
    if len == 0 {
        return Ok(&[]);
    }
    if data.is_null() {
        return Err(ApiError::InvalidArgument(format!(
            "{argument_name} is null while length is nonzero"
        )));
    }
    if len > isize::MAX as usize {
        return Err(ApiError::InvalidArgument(format!(
            "{argument_name} length exceeds the addressable object limit"
        )));
    }
    // SAFETY: the caller guarantees `len` readable bytes, null was rejected,
    // and the length is bounded to `isize::MAX`.
    Ok(unsafe { slice::from_raw_parts(data, len) })
}

fn pixel_format_from_c(format: u32) -> ApiResult<PixelFormat> {
    match format {
        PIXEL_FORMAT_GRAY8 => Ok(PixelFormat::Gray8),
        PIXEL_FORMAT_YUV420P8 => Ok(PixelFormat::Yuv420p8),
        PIXEL_FORMAT_YUV411P8 => Ok(PixelFormat::Yuv411p8),
        PIXEL_FORMAT_YUV422P8 => Ok(PixelFormat::Yuv422p8),
        PIXEL_FORMAT_YUV444P8 => Ok(PixelFormat::Yuv444p8),
        _ => Err(ApiError::InvalidArgument(format!(
            "unknown pixel format {format}"
        ))),
    }
}

fn pixel_format_to_c(format: PixelFormat) -> ApiResult<u32> {
    match format {
        PixelFormat::Gray8 => Ok(PIXEL_FORMAT_GRAY8),
        PixelFormat::Yuv420p8 => Ok(PIXEL_FORMAT_YUV420P8),
        PixelFormat::Yuv411p8 => Ok(PIXEL_FORMAT_YUV411P8),
        PixelFormat::Yuv422p8 => Ok(PIXEL_FORMAT_YUV422P8),
        PixelFormat::Yuv444p8 => Ok(PIXEL_FORMAT_YUV444P8),
        PixelFormat::Rgb24 => Err(ApiError::Core(Error::Unsupported(
            "C API does not expose packed RGB frames yet".into(),
        ))),
        _ => Err(ApiError::Core(Error::Unsupported(
            "C API does not expose this pixel format".into(),
        ))),
    }
}

const fn color_range_to_c(range: ColorRange) -> u32 {
    match range {
        ColorRange::Full => COLOR_RANGE_FULL,
        ColorRange::Limited => COLOR_RANGE_LIMITED,
        _ => COLOR_RANGE_UNSPECIFIED,
    }
}

fn color_range_from_c(range: u32) -> ApiResult<ColorRange> {
    match range {
        COLOR_RANGE_UNSPECIFIED => Ok(ColorRange::Unspecified),
        COLOR_RANGE_FULL => Ok(ColorRange::Full),
        COLOR_RANGE_LIMITED => Ok(ColorRange::Limited),
        _ => Err(ApiError::InvalidArgument(format!(
            "unknown color range {range}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = include_bytes!("../../../testdata/jpeg/valid/baseline-420.jpg");
    const DV: &[u8] = include_bytes!("../../../testdata/dv/valid/dv25-525-60-one-frame.dv");
    const MPEG2: &[u8] =
        include_bytes!("../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");

    #[test]
    fn c_boundary_decodes_and_reencodes_a_frame() {
        let mut decoded = MmrVideoFrame::empty();
        // SAFETY: all input and output pointers refer to live Rust objects.
        let status = unsafe { mmr_mjpeg_decode(JPEG.as_ptr(), JPEG.len(), &raw mut decoded) };
        assert_eq!(status, STATUS_OK);
        assert!(decoded.width > 0);
        assert!(decoded.height > 0);
        assert_eq!(decoded.plane_count, 3);

        let mut views = [MmrPlaneView::empty(); MAX_PLANES];
        for (view, plane) in views.iter_mut().zip(decoded.planes) {
            *view = MmrPlaneView {
                data: plane.data,
                data_len: plane.data_len,
                stride: plane.stride,
                width: plane.width,
                height: plane.height,
            };
        }
        let view = MmrVideoFrameView {
            struct_size: size_of::<MmrVideoFrameView>(),
            format: decoded.format,
            range: decoded.range,
            width: decoded.width,
            height: decoded.height,
            plane_count: decoded.plane_count,
            planes: views,
        };
        let mut encoded = MmrBuffer::empty();
        // SAFETY: the borrowed view remains live for the duration of the call.
        let status = unsafe { mmr_mjpeg_encode(&raw const view, 75, &raw mut encoded) };
        assert_eq!(status, STATUS_OK);
        assert!(encoded.len > 4);
        // SAFETY: both values were initialized by this library and are freed once.
        unsafe {
            mmr_buffer_free(&raw mut encoded);
            mmr_video_frame_free(&raw mut decoded);
        }
        assert!(encoded.data.is_null());
        assert!(decoded.planes[0].data.is_null());
    }

    #[test]
    fn c_boundary_decodes_and_reencodes_dv25() {
        let mut decoded = MmrVideoFrame::empty();
        // SAFETY: all input and output pointers refer to live Rust objects.
        let status = unsafe { mmr_dv_decode(DV.as_ptr(), DV.len(), &raw mut decoded) };
        assert_eq!(status, STATUS_OK);
        assert_eq!(decoded.format, PIXEL_FORMAT_YUV411P8);
        assert_eq!((decoded.width, decoded.height), (720, 480));

        let mut views = [MmrPlaneView::empty(); MAX_PLANES];
        for (view, plane) in views.iter_mut().zip(decoded.planes) {
            *view = MmrPlaneView {
                data: plane.data,
                data_len: plane.data_len,
                stride: plane.stride,
                width: plane.width,
                height: plane.height,
            };
        }
        let view = MmrVideoFrameView {
            struct_size: size_of::<MmrVideoFrameView>(),
            format: decoded.format,
            range: decoded.range,
            width: decoded.width,
            height: decoded.height,
            plane_count: decoded.plane_count,
            planes: views,
        };
        let mut encoded = MmrBuffer::empty();
        // SAFETY: all borrowed input storage remains live for the call.
        let status = unsafe { mmr_dv_encode(&raw const view, &raw mut encoded) };
        assert_eq!(status, STATUS_OK);
        assert_eq!(encoded.len, 120_000);
        // SAFETY: both allocations originated from this library and are freed once.
        unsafe {
            mmr_buffer_free(&raw mut encoded);
            mmr_video_frame_free(&raw mut decoded);
        }
    }

    #[test]
    fn c_boundary_counts_decodes_and_encodes_mpeg2() {
        let mut picture_count = 0_usize;
        // SAFETY: all pointers refer to live Rust objects.
        let status =
            unsafe { mmr_mpeg2_picture_count(MPEG2.as_ptr(), MPEG2.len(), &raw mut picture_count) };
        assert_eq!(status, STATUS_OK);
        assert_eq!(picture_count, 12);

        let mut decoded = MmrVideoFrame::empty();
        // SAFETY: all pointers refer to live Rust objects.
        let status =
            unsafe { mmr_mpeg2_decode_picture(MPEG2.as_ptr(), MPEG2.len(), 0, &raw mut decoded) };
        assert_eq!(status, STATUS_OK);
        assert_eq!(decoded.format, PIXEL_FORMAT_YUV420P8);
        assert_eq!((decoded.width, decoded.height), (96, 64));

        let mut views = [MmrPlaneView::empty(); MAX_PLANES];
        for (view, plane) in views.iter_mut().zip(decoded.planes) {
            *view = MmrPlaneView {
                data: plane.data,
                data_len: plane.data_len,
                stride: plane.stride,
                width: plane.width,
                height: plane.height,
            };
        }
        let view = MmrVideoFrameView {
            struct_size: size_of::<MmrVideoFrameView>(),
            format: decoded.format,
            range: decoded.range,
            width: decoded.width,
            height: decoded.height,
            plane_count: decoded.plane_count,
            planes: views,
        };
        let options = MmrMpeg2EncodeOptions {
            struct_size: size_of::<MmrMpeg2EncodeOptions>(),
            frame_rate_numerator: 25,
            frame_rate_denominator: 1,
            gop_size: 12,
            b_frames: 2,
            quantiser_scale_code: 8,
            motion_search_range: 4,
            progressive: 1,
            top_field_first: 0,
        };
        let mut encoded = MmrBuffer::empty();
        // SAFETY: the input frame, options, and output all remain live for the call.
        let status =
            unsafe { mmr_mpeg2_encode(&raw const view, 1, &raw const options, &raw mut encoded) };
        assert_eq!(status, STATUS_OK);
        assert!(encoded.len > 16);
        // SAFETY: both allocations originated from this library and are freed once.
        unsafe {
            mmr_buffer_free(&raw mut encoded);
            mmr_video_frame_free(&raw mut decoded);
        }
    }

    #[test]
    fn invalid_argument_sets_thread_local_diagnostic() {
        let mut decoded = MmrVideoFrame::empty();
        // SAFETY: output is live; the deliberately invalid null input is part
        // of the API's checked error contract.
        let status = unsafe { mmr_mjpeg_decode(ptr::null(), 4, &raw mut decoded) };
        assert_eq!(status, STATUS_INVALID_ARGUMENT);

        let required = unsafe { mmr_last_error_message(ptr::null_mut(), 0) };
        let mut message = vec![0_u8; required];
        // SAFETY: `message` provides exactly the advertised writable capacity.
        unsafe { mmr_last_error_message(message.as_mut_ptr().cast(), message.len()) };
        assert!(
            std::str::from_utf8(&message[..message.len() - 1])
                .expect("diagnostics are UTF-8")
                .contains("data is null")
        );
    }

    #[test]
    fn c_boundary_muxes_and_demuxes_mpegts() {
        let mut transport = MmrBuffer::empty();
        // SAFETY: all pointers refer to live storage for the duration of the call.
        let status =
            unsafe { mmr_mpegts_mux_mpeg2(MPEG2.as_ptr(), MPEG2.len(), &raw mut transport) };
        assert_eq!(status, STATUS_OK);
        assert!(
            transport
                .len
                .is_multiple_of(mmrecode_mpegts::TS_PACKET_SIZE)
        );
        let mut elementary = MmrBuffer::empty();
        // SAFETY: the transport allocation remains owned and readable until both calls finish.
        let status =
            unsafe { mmr_mpegts_demux_mpeg2(transport.data, transport.len, &raw mut elementary) };
        assert_eq!(status, STATUS_OK);
        // SAFETY: the returned allocation contains `len` initialized bytes.
        let extracted = unsafe { slice::from_raw_parts(elementary.data, elementary.len) };
        assert_eq!(extracted, MPEG2);
        // SAFETY: both allocations originated from this library and are freed once.
        unsafe {
            mmr_buffer_free(&raw mut elementary);
            mmr_buffer_free(&raw mut transport);
        }
    }

    #[test]
    fn rejects_an_incompatible_output_structure() {
        let mut decoded = MmrVideoFrame::empty();
        decoded.struct_size = 0;
        // SAFETY: all pointers are live; the deliberately wrong structure size
        // is a checked part of the ABI contract.
        let status = unsafe { mmr_mjpeg_decode(JPEG.as_ptr(), JPEG.len(), &raw mut decoded) };
        assert_eq!(status, STATUS_INVALID_ARGUMENT);
        assert_eq!(decoded.struct_size, 0);
    }

    #[test]
    fn versions_are_exposed() {
        assert_eq!(mmr_abi_version(), ABI_VERSION);
        assert!(!mmr_version().is_null());
    }
}
