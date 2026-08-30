#ifndef MMRECODE_H
#define MMRECODE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(MMR_BUILD_SHARED)
#define MMR_API __declspec(dllexport)
#elif defined(_WIN32) && defined(MMR_USE_SHARED)
#define MMR_API __declspec(dllimport)
#elif defined(__GNUC__) && __GNUC__ >= 4
#define MMR_API __attribute__((visibility("default")))
#else
#define MMR_API
#endif

#define MMR_ABI_VERSION 1u
#define MMR_MAX_PLANES 3u

typedef int32_t mmr_status;

#define MMR_STATUS_OK ((mmr_status)0)
#define MMR_STATUS_INVALID_ARGUMENT ((mmr_status)1)
#define MMR_STATUS_INVALID_DATA ((mmr_status)2)
#define MMR_STATUS_UNSUPPORTED ((mmr_status)3)
#define MMR_STATUS_INVALID_STATE ((mmr_status)4)
#define MMR_STATUS_IO_ERROR ((mmr_status)5)
#define MMR_STATUS_INTERNAL_ERROR ((mmr_status)100)

typedef uint32_t mmr_pixel_format;

#define MMR_PIXEL_FORMAT_GRAY8 ((mmr_pixel_format)1u)
#define MMR_PIXEL_FORMAT_YUV420P8 ((mmr_pixel_format)2u)
#define MMR_PIXEL_FORMAT_YUV422P8 ((mmr_pixel_format)3u)
#define MMR_PIXEL_FORMAT_YUV444P8 ((mmr_pixel_format)4u)
#define MMR_PIXEL_FORMAT_YUV411P8 ((mmr_pixel_format)5u)

typedef uint32_t mmr_color_range;

#define MMR_COLOR_RANGE_UNSPECIFIED ((mmr_color_range)0u)
#define MMR_COLOR_RANGE_FULL ((mmr_color_range)1u)
#define MMR_COLOR_RANGE_LIMITED ((mmr_color_range)2u)

/* A borrowed plane. The caller retains ownership for the duration of a call. */
typedef struct mmr_plane_view {
    const uint8_t *data;
    size_t data_len;
    size_t stride;
    size_t width;
    size_t height;
} mmr_plane_view;

/* A borrowed progressive video frame used as encoder input. */
typedef struct mmr_video_frame_view {
    size_t struct_size;
    mmr_pixel_format format;
    mmr_color_range range;
    size_t width;
    size_t height;
    size_t plane_count;
    mmr_plane_view planes[MMR_MAX_PLANES];
} mmr_video_frame_view;

/* One plane allocated by MMRecode. Release it through mmr_video_frame_free. */
typedef struct mmr_owned_plane {
    uint8_t *data;
    size_t data_len;
    size_t stride;
    size_t width;
    size_t height;
} mmr_owned_plane;

/* A progressive decoded frame allocated by MMRecode. */
typedef struct mmr_video_frame {
    size_t struct_size;
    mmr_pixel_format format;
    mmr_color_range range;
    size_t width;
    size_t height;
    size_t plane_count;
    mmr_owned_plane planes[MMR_MAX_PLANES];
} mmr_video_frame;

/* An encoded byte buffer allocated by MMRecode. */
typedef struct mmr_buffer {
    size_t struct_size;
    uint8_t *data;
    size_t len;
} mmr_buffer;

/* Settings for a complete MPEG-2 Video elementary-stream encode. */
typedef struct mmr_mpeg2_encode_options {
    size_t struct_size;
    uint32_t frame_rate_numerator;
    uint32_t frame_rate_denominator;
    uint32_t gop_size;
    uint32_t b_frames;
    uint32_t quantiser_scale_code;
    uint32_t motion_search_range;
    uint32_t progressive;
    uint32_t top_field_first;
} mmr_mpeg2_encode_options;

/* Returns the ABI version implemented by this library. */
MMR_API uint32_t mmr_abi_version(void);

/* Returns a process-lifetime, NUL-terminated library version string. */
MMR_API const char *mmr_version(void);

/*
 * Copies the current thread's last diagnostic, including a trailing NUL when
 * capacity is nonzero. Returns the required capacity including the NUL.
 */
MMR_API size_t mmr_last_error_message(char *buffer, size_t capacity);

/*
 * Decodes one complete baseline JPEG image. Before calling, zero-initialize
 * out_frame and set out_frame->struct_size = sizeof(*out_frame).
 */
MMR_API mmr_status mmr_mjpeg_decode(
    const uint8_t *data,
    size_t len,
    mmr_video_frame *out_frame);

/* Decodes one complete 120000- or 144000-byte raw DV25 frame. */
MMR_API mmr_status mmr_dv_decode(
    const uint8_t *data,
    size_t len,
    mmr_video_frame *out_frame);

/* Returns the number of pictures in a complete MPEG-2 Video elementary stream. */
MMR_API mmr_status mmr_mpeg2_picture_count(
    const uint8_t *data,
    size_t len,
    size_t *out_count);

/*
 * Decodes one picture by zero-based presentation order from a complete
 * MPEG-2 Video elementary stream.
 */
MMR_API mmr_status mmr_mpeg2_decode_picture(
    const uint8_t *data,
    size_t len,
    size_t presentation_index,
    mmr_video_frame *out_frame);

/* Releases all allocations in a frame returned by any MMRecode decoder. */
MMR_API void mmr_video_frame_free(mmr_video_frame *frame);

/*
 * Encodes one progressive planar frame as baseline JPEG. Quality is 1..100.
 * Before calling, zero-initialize out_buffer and set
 * out_buffer->struct_size = sizeof(*out_buffer).
 */
MMR_API mmr_status mmr_mjpeg_encode(
    const mmr_video_frame_view *frame,
    uint8_t quality,
    mmr_buffer *out_buffer);

/* Encodes a native 720x480 YUV411P8 or 720x576 YUV420P8 frame as raw DV25. */
MMR_API mmr_status mmr_dv_encode(
    const mmr_video_frame_view *frame,
    mmr_buffer *out_buffer);

/*
 * Encodes a complete array of YUV420P8 frames as an MPEG-2 Video elementary
 * stream. All frames and their plane storage remain caller-owned.
 */
MMR_API mmr_status mmr_mpeg2_encode(
    const mmr_video_frame_view *frames,
    size_t frame_count,
    const mmr_mpeg2_encode_options *options,
    mmr_buffer *out_buffer);

/* Wraps a complete MPEG-2 Video elementary stream in a single-program MPEG-TS. */
MMR_API mmr_status mmr_mpegts_mux_mpeg2(
    const uint8_t *data,
    size_t len,
    mmr_buffer *out_buffer);

/* Muxes complete MPEG-2 Video and MPEG-1 Layer II elementary streams with A/V timing. */
MMR_API mmr_status mmr_mpegts_mux_mpeg2_mp2(
    const uint8_t *video_data,
    size_t video_len,
    const uint8_t *audio_data,
    size_t audio_len,
    mmr_buffer *out_buffer);

/* Extracts the first MPEG-2 Video elementary stream from a complete MPEG-TS. */
MMR_API mmr_status mmr_mpegts_demux_mpeg2(
    const uint8_t *data,
    size_t len,
    mmr_buffer *out_buffer);

/* Extracts the first MPEG-1 Audio Layer II elementary stream from a complete MPEG-TS. */
MMR_API mmr_status mmr_mpegts_demux_mp2(
    const uint8_t *data,
    size_t len,
    mmr_buffer *out_buffer);

/* Releases the allocation in a buffer returned by any MMRecode encoder or muxer. */
MMR_API void mmr_buffer_free(mmr_buffer *buffer);

#ifdef __cplusplus
}
#endif

#endif
