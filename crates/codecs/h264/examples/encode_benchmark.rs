//! Small release-mode throughput benchmark for the YouTube-oriented encoder configuration.

#![allow(clippy::cast_precision_loss)]

use std::{collections::BTreeMap, env, time::Instant};

use mmrecode_core::{
    ColorDescription, Encoder, FieldOrder, FrameTiming, PixelFormat, Plane, Rational, Timestamp,
    VideoEncoderSettings, VideoFrame,
};
use mmrecode_h264::H264Encoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let width = argument(&mut arguments, "width", 320)?;
    let height = argument(&mut arguments, "height", 180)?;
    let frame_count = argument(&mut arguments, "frames", 4)?;
    let search_range = argument(&mut arguments, "search range", 16)?;
    let time_base = Rational::new(1, 30)?;
    let mut options = BTreeMap::new();
    for (name, value) in [
        ("mode", "inter".to_owned()),
        ("profile", "high".to_owned()),
        ("entropy", "cabac".to_owned()),
        ("b_frames", "2".to_owned()),
        ("max_refs", "2".to_owned()),
        ("search_range", search_range.to_string()),
        ("analysis", "fast".to_owned()),
        ("aq_strength", "6".to_owned()),
        ("scaling_matrix", "jvt".to_owned()),
        ("gop_size", "15".to_owned()),
        ("frame_duration_ticks", "1".to_owned()),
    ] {
        options.insert(name.into(), value);
    }
    let mut encoder = H264Encoder::default();
    encoder.configure(&VideoEncoderSettings {
        width,
        height,
        pixel_format: PixelFormat::Yuv420p8,
        time_base,
        bitrate: Some(8_000_000),
        options,
    })?;

    let started = Instant::now();
    let mut byte_count = 0_usize;
    for index in 0..frame_count {
        encoder.send_frame(moving_frame(width, height, index, time_base))?;
        drain_packets(&mut encoder, &mut byte_count)?;
    }
    encoder.flush()?;
    drain_packets(&mut encoder, &mut byte_count)?;
    let elapsed = started.elapsed();
    let megapixels = width as f64 * height as f64 * frame_count as f64 / 1_000_000.0;
    println!(
        "encoded {frame_count} frame(s) at {width}x{height}, range {search_range}: {:.3}s, {:.3} frame/s, {:.3} MP/s, {byte_count} bytes",
        elapsed.as_secs_f64(),
        frame_count as f64 / elapsed.as_secs_f64(),
        megapixels / elapsed.as_secs_f64(),
    );
    Ok(())
}

fn argument(
    arguments: &mut impl Iterator<Item = String>,
    name: &str,
    default: usize,
) -> Result<usize, String> {
    arguments.next().map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|_| format!("invalid {name} '{value}'"))
    })
}

fn drain_packets(encoder: &mut H264Encoder, byte_count: &mut usize) -> mmrecode_core::Result<()> {
    while let Some(packet) = encoder.receive_packet()? {
        *byte_count += packet.data.len();
        let _ = encoder.receive_reconstructed_frame()?;
    }
    Ok(())
}

fn moving_frame(width: usize, height: usize, index: usize, time_base: Rational) -> VideoFrame {
    let luma = (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                let checker = ((x / 16 + y / 16 + index) & 1) * 48;
                u8::try_from((x * 3 + y * 5 + index * 11 + checker) & 0xff)
                    .expect("masked sample fits u8")
            })
        })
        .collect();
    let chroma_width = width / 2;
    let chroma_height = height / 2;
    let chroma = |phase: usize| {
        (0..chroma_height)
            .flat_map(|y| {
                (0..chroma_width).map(move |x| {
                    u8::try_from((128 + x * 2 + y * 3 + index * phase) & 0xff)
                        .expect("masked sample fits u8")
                })
            })
            .collect()
    };
    VideoFrame {
        format: PixelFormat::Yuv420p8,
        width,
        height,
        planes: vec![
            Plane {
                data: luma,
                stride: width,
                width,
                height,
            },
            Plane {
                data: chroma(3),
                stride: chroma_width,
                width: chroma_width,
                height: chroma_height,
            },
            Plane {
                data: chroma(7),
                stride: chroma_width,
                width: chroma_width,
                height: chroma_height,
            },
        ],
        timing: FrameTiming {
            pts: Some(Timestamp {
                value: i64::try_from(index).expect("benchmark frame count fits i64"),
                time_base,
            }),
            duration: Some(Timestamp {
                value: 1,
                time_base,
            }),
        },
        color: ColorDescription::default(),
        field_order: FieldOrder::Progressive,
    }
}
