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
