//! Bounded AAC-LC `raw_data_block` syntax. No container or platform dependencies.
//!
//! Syntax terminology follows ISO/IEC 14496-3: `raw_data_block`, `channel_pair_element`,
//! `ics_info`, `section_data`, `data_stream_element` and `fill_element`.

use mmrecode_bitstream::BitReader;
use mmrecode_core::{Error, Result};

use crate::{AudioSpecificConfig, huffman, tables};

/// One channel's dequantized spectrum, in chronological window order.
#[derive(Debug)]
pub(crate) struct SpectralChannel {
    pub sequence: u8,
    pub kbd: bool,
    pub coefficients: Vec<f64>,
}

#[derive(Clone, Debug)]
struct IcsInfo {
    short: bool,
    sequence: u8,
    kbd: bool,
    max_sfb: usize,
    group_lengths: Vec<usize>,
}

#[derive(Debug)]
struct ChannelData {
    info: IcsInfo,
    books: Vec<u8>,
    scales: Vec<i16>,
    coefficients: Vec<f64>,
    tns: Vec<Vec<TnsFilter>>,
}

#[derive(Debug)]
struct Pulse {
    positions: Vec<(usize, i16)>,
}

#[derive(Debug)]
struct TnsFilter {
    length: usize,
    direction: bool,
    reflection: Vec<f64>,
}

impl IcsInfo {
    fn read(reader: &mut BitReader<'_>, config: &AudioSpecificConfig) -> Result<Self> {
        if reader.read_bit()? {
            return Err(invalid("reserved ICS bit is set"));
        }
        let sequence = u8::try_from(reader.read_bits(2)?).expect("two-bit window sequence");
        let short = sequence == 2;
        let kbd = reader.read_bit()?;
        let max_sfb = bits(reader, if short { 4 } else { 6 })?;
        let mut group_lengths = vec![1];
        if short {
            for _ in 0..7 {
                if reader.read_bit()? {
                    *group_lengths.last_mut().expect("at least one group") += 1;
                } else {
                    group_lengths.push(1);
                }
            }
        } else if reader.read_bit()? {
            return Err(invalid("prediction is forbidden in AAC-LC"));
        }
        let limit = band_offsets(config, short)?.len() - 1;
        if max_sfb > limit {
            return Err(invalid("max_sfb exceeds the sample-rate band count"));
        }
        Ok(Self {
            short,
            sequence,
            kbd,
            max_sfb,
            group_lengths,
        })
    }
}

/// Decodes a complete access unit before any synthesis/overlap state may be mutated.
pub(crate) fn decode_spectrum(
    data: &[u8],
    config: &AudioSpecificConfig,
    noise_state: &mut u32,
) -> Result<Vec<SpectralChannel>> {
    let mut reader = BitReader::new(data);
    let mut channels = Vec::new();
    loop {
        match reader.read_bits(3)? {
            element @ (0 | 1) => {
                if !channels.is_empty() {
                    return Err(invalid("duplicate audio element"));
                }
                if (element == 0 && config.channels != 1) || (element == 1 && config.channels != 2)
                {
                    return Err(unsupported(
                        "audio element does not match mono/SCE or stereo/CPE layout",
                    ));
                }
                reader.skip_bits(4)?; // element_instance_tag
                if element == 1 {
                    channels = channel_pair(&mut reader, config, noise_state)?;
                } else {
                    let mut channel = channel(&mut reader, config, None, noise_state)?;
                    apply_tns(&mut channel, config)?;
                    channels.push(spectral(channel));
                }
            }
            2 => return Err(unsupported("coupling channel element")),
            3 => return Err(unsupported("LFE element")),
            4 => data_stream(&mut reader)?,
            5 => return Err(unsupported("program configuration element")),
            6 => fill(&mut reader)?,
            7 => break,
            _ => unreachable!("three-bit element identifier"),
        }
    }
    if channels.is_empty() {
        return Err(invalid("access unit contains no audio element"));
    }
    reader.align_to_byte();
    if reader.bits_remaining() != 0 {
        return Err(invalid("trailing bytes after raw_data_block END"));
    }
    Ok(channels)
}

fn channel_pair(
    reader: &mut BitReader<'_>,
    config: &AudioSpecificConfig,
    noise_state: &mut u32,
) -> Result<Vec<SpectralChannel>> {
    let mut mask = Vec::new();
    let mut ms_present = 0;
    let common = if reader.read_bit()? {
        let info = IcsInfo::read(reader, config)?;
        ms_present = reader.read_bits(2)?;
        if ms_present == 3 {
            return Err(invalid("reserved mid/side mask"));
        }
        for _ in 0..info.group_lengths.len() * info.max_sfb {
            mask.push(if ms_present == 1 {
                reader.read_bit()?
            } else {
                ms_present == 2
            });
        }
        Some(info)
    } else {
        None
    };
    let mut left = channel(reader, config, common.as_ref(), noise_state)?;
    let mut right = channel(reader, config, common.as_ref(), noise_state)?;
    if let Some(info) = common {
        let offsets = band_offsets(config, info.short)?;
        let mut window = 0;
        for (group, &length) in info.group_lengths.iter().enumerate() {
            for band in 0..info.max_sfb {
                let band_index = group * info.max_sfb + band;
                if !mask[band_index]
                    || left.books[band_index] >= 13
                    || right.books[band_index] >= 13
                {
                    continue;
                }
                for local_window in 0..length {
                    let base = (window + local_window) * 128;
                    for index in base + offsets[band]..base + offsets[band + 1] {
                        let mid = left.coefficients[index];
                        let side = right.coefficients[index];
                        left.coefficients[index] = mid + side;
                        right.coefficients[index] = mid - side;
                    }
                }
            }
            window += length;
        }
    }
    apply_intensity(&left, &mut right, &mask, ms_present, config)?;
    apply_tns(&mut left, config)?;
    apply_tns(&mut right, config)?;
    Ok(vec![spectral(left), spectral(right)])
}

fn channel(
    reader: &mut BitReader<'_>,
    config: &AudioSpecificConfig,
    common: Option<&IcsInfo>,
    noise_state: &mut u32,
) -> Result<ChannelData> {
    let global_gain = i16::try_from(reader.read_bits(8)?).expect("eight-bit global gain");
    let own_info;
    let info = if let Some(common) = common {
        common
    } else {
        own_info = IcsInfo::read(reader, config)?;
        &own_info
    };
    let books = sections(reader, info)?;
    let scales = scalefactors(reader, &books, global_gain)?;
    let offsets = band_offsets(config, info.short)?;
    let pulse = if reader.read_bit()? {
        if info.short {
            return Err(invalid("pulse data in a short window"));
        }
        Some(pulse(reader, offsets)?)
    } else {
        None
    };
    let tns = if reader.read_bit()? {
        tns(reader, info.short)?
    } else {
        vec![]
    };
    if reader.read_bit()? {
        gain_control(reader, info.sequence)?;
    }
    let mut quantized = vec![0_i16; 1_024];
    let mut window = 0;
    for (group, &length) in info.group_lengths.iter().enumerate() {
        for band in 0..info.max_sfb {
            let index = group * info.max_sfb + band;
            let book = books[index];
            if !matches!(book, 1..=11) {
                continue;
            }
            for local_window in 0..length {
                let base = (window + local_window) * 128;
                let output = &mut quantized[base + offsets[band]..base + offsets[band + 1]];
                for tuple in output.chunks_mut(huffman::dimension(book)) {
                    let values = huffman::spectral(reader, book)?;
                    for (destination, value) in tuple.iter_mut().zip(values) {
                        *destination = value;
                    }
                }
            }
        }
        window += length;
    }
    if let Some(pulse) = pulse {
        apply_pulse(&mut quantized, &books, info.max_sfb, offsets, &pulse)?;
    }
    let mut coefficients = vec![0.0; 1_024];
    window = 0;
    for (group, &length) in info.group_lengths.iter().enumerate() {
        for band in 0..info.max_sfb {
            let index = group * info.max_sfb + band;
            match books[index] {
                1..=11 => {
                    for local_window in 0..length {
                        let base = (window + local_window) * 128;
                        for position in base + offsets[band]..base + offsets[band + 1] {
                            coefficients[position] =
                                huffman::inverse_quantize(quantized[position], scales[index]);
                        }
                    }
                }
                13 => {
                    for local_window in 0..length {
                        let base = (window + local_window) * 128;
                        synthesize_noise(
                            &mut coefficients[base + offsets[band]..base + offsets[band + 1]],
                            scales[index],
                            noise_state,
                        );
                    }
                }
                _ => {}
            }
        }
        window += length;
    }
    Ok(ChannelData {
        info: info.clone(),
        books,
        scales,
        coefficients,
        tns,
    })
}

fn spectral(channel: ChannelData) -> SpectralChannel {
    SpectralChannel {
        sequence: channel.info.sequence,
        kbd: channel.info.kbd,
        coefficients: channel.coefficients,
    }
}

fn pulse(reader: &mut BitReader<'_>, offsets: &[usize]) -> Result<Pulse> {
    let count = bits(reader, 2)? + 1;
    let start_band = bits(reader, 6)?;
    if start_band >= offsets.len() - 1 {
        return Err(invalid("pulse start band is outside the spectrum"));
    }
    let mut position = offsets[start_band];
    let mut positions = Vec::with_capacity(count);
    for _ in 0..count {
        position += bits(reader, 5)?;
        if position >= *offsets.last().expect("AAC band table is nonempty") {
            return Err(invalid("pulse position is outside the spectrum"));
        }
        positions.push((
            position,
            i16::try_from(reader.read_bits(4)?).expect("four-bit pulse"),
        ));
    }
    Ok(Pulse { positions })
}

fn apply_pulse(
    quantized: &mut [i16],
    books: &[u8],
    max_sfb: usize,
    offsets: &[usize],
    pulse: &Pulse,
) -> Result<()> {
    for &(position, amplitude) in &pulse.positions {
        let band = offsets
            .windows(2)
            .position(|range| position >= range[0] && position < range[1])
            .ok_or_else(|| invalid("pulse position has no scalefactor band"))?;
        // Pulse positions may legally land above max_sfb, where the implicit ZERO_HCB means the
        // pulse has no effect. Noise bands are excluded by the AAC reconstruction rule.
        if band >= max_sfb || books[band] == 13 {
            continue;
        }
        let value = &mut quantized[position];
        *value = match (*value).cmp(&0) {
            std::cmp::Ordering::Greater => value.saturating_add(amplitude),
            std::cmp::Ordering::Less => value.saturating_sub(amplitude),
            std::cmp::Ordering::Equal => -amplitude,
        };
    }
    Ok(())
}

fn synthesize_noise(output: &mut [f64], scale: i16, state: &mut u32) {
    let mut energy = 0.0;
    for value in output.iter_mut() {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *value = f64::from((*state).cast_signed());
        energy += *value * *value;
    }
    if energy != 0.0 {
        // Our Huffman vectors use the ISO sign directly (the reference tables fold a negation
        // into their vector values), so PNS uses the corresponding positive amplitude here.
        let factor = (f64::from(scale) / 4.0).exp2() / energy.sqrt();
        for value in output {
            *value *= factor;
        }
    }
}

fn apply_intensity(
    left: &ChannelData,
    right: &mut ChannelData,
    mask: &[bool],
    ms_present: u64,
    config: &AudioSpecificConfig,
) -> Result<()> {
    let offsets = band_offsets(config, right.info.short)?;
    let mut window = 0;
    for (group, &length) in right.info.group_lengths.iter().enumerate() {
        for band in 0..right.info.max_sfb {
            let index = group * right.info.max_sfb + band;
            let book = right.books[index];
            if !matches!(book, 14 | 15) {
                continue;
            }
            let mut sign = if book == 14 { -1.0 } else { 1.0 };
            if ms_present != 0 && mask.get(index).copied().unwrap_or(false) {
                sign = -sign;
            }
            let scale = sign * (-f64::from(right.scales[index]) / 4.0).exp2();
            for local_window in 0..length {
                let base = (window + local_window) * 128;
                for position in base + offsets[band]..base + offsets[band + 1] {
                    right.coefficients[position] = left.coefficients[position] * scale;
                }
            }
        }
        window += length;
    }
    Ok(())
}

fn tns(reader: &mut BitReader<'_>, short: bool) -> Result<Vec<Vec<TnsFilter>>> {
    let windows = if short { 8 } else { 1 };
    let mut result = Vec::with_capacity(windows);
    for _ in 0..windows {
        let count = bits(reader, if short { 1 } else { 2 })?;
        let mut filters = Vec::with_capacity(count);
        if count != 0 {
            let coefficient_resolution = bits(reader, 1)?;
            for _ in 0..count {
                let length = bits(reader, if short { 4 } else { 6 })?;
                let order = bits(reader, if short { 3 } else { 5 })?;
                if order > if short { 7 } else { 12 } {
                    return Err(invalid("TNS filter order exceeds the AAC-LC limit"));
                }
                let mut direction = false;
                let mut reflection = Vec::with_capacity(order);
                if order != 0 {
                    direction = reader.read_bit()?;
                    let compressed = bits(reader, 1)?;
                    let width = coefficient_resolution + 3 - compressed;
                    let map = tns_map(compressed, coefficient_resolution);
                    for _ in 0..order {
                        reflection
                            .push(map[bits(reader, u8::try_from(width).expect("TNS width"))?]);
                    }
                }
                filters.push(TnsFilter {
                    length,
                    direction,
                    reflection,
                });
            }
        }
        result.push(filters);
    }
    Ok(result)
}

fn tns_map(compressed: usize, resolution: usize) -> &'static [f64] {
    const MAP_0_3: [f64; 8] = [
        0.0,
        -0.433_883_73,
        -0.781_831_50,
        -0.974_927_90,
        0.984_807_73,
        0.866_025_39,
        0.642_787_58,
        0.342_020_15,
    ];
    const MAP_0_4: [f64; 16] = [
        0.0,
        -0.207_911_70,
        -0.406_736_64,
        -0.587_785_24,
        -0.743_144_81,
        -0.866_025_39,
        -0.951_056_54,
        -0.994_521_92,
        0.995_734_16,
        0.961_825_61,
        0.895_163_30,
        0.798_017_20,
        0.673_695_62,
        0.526_432_16,
        0.361_241_67,
        0.183_749_51,
    ];
    const MAP_1_3: [f64; 4] = [0.0, -0.433_883_73, 0.642_787_58, 0.342_020_15];
    const MAP_1_4: [f64; 8] = [
        0.0,
        -0.207_911_70,
        -0.406_736_64,
        -0.587_785_24,
        0.673_695_62,
        0.526_432_16,
        0.361_241_67,
        0.183_749_51,
    ];
    match (compressed, resolution) {
        (0, 0) => &MAP_0_3,
        (0, 1) => &MAP_0_4,
        (1, 0) => &MAP_1_3,
        (1, 1) => &MAP_1_4,
        _ => unreachable!("one-bit TNS fields"),
    }
}

fn apply_tns(channel: &mut ChannelData, config: &AudioSpecificConfig) -> Result<()> {
    const MAX_LONG: [usize; 13] = [31, 31, 34, 40, 42, 51, 46, 46, 42, 42, 42, 39, 39];
    const MAX_SHORT: [usize; 13] = [9, 9, 10, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14];
    if channel.tns.is_empty() {
        return Ok(());
    }
    let rate = crate::SAMPLE_RATES
        .iter()
        .position(|value| *value == config.sample_rate)
        .ok_or_else(|| unsupported("nonstandard sample rate"))?;
    let offsets = band_offsets(config, channel.info.short)?;
    let maximum = (if channel.info.short {
        MAX_SHORT[rate]
    } else {
        MAX_LONG[rate]
    })
    .min(channel.info.max_sfb);
    for (window, filters) in channel.tns.iter().enumerate() {
        let mut bottom = offsets.len() - 1;
        for filter in filters {
            let top = bottom;
            bottom = top.saturating_sub(filter.length);
            if filter.reflection.is_empty() {
                continue;
            }
            let start = offsets[bottom.min(maximum)];
            let end = offsets[top.min(maximum)];
            if start >= end {
                continue;
            }
            let mut lpc = Vec::with_capacity(filter.reflection.len());
            for &coefficient in &filter.reflection {
                let reflection = -coefficient;
                let previous = lpc.clone();
                for index in 0..previous.len() {
                    lpc[index] =
                        previous[index] + reflection * previous[previous.len() - 1 - index];
                }
                lpc.push(reflection);
            }
            let base = window * 128;
            let size = end - start;
            for step in 0..size {
                let position = if filter.direction {
                    base + end - 1 - step
                } else {
                    base + start + step
                };
                for order in 1..=step.min(lpc.len()) {
                    let previous = if filter.direction {
                        position + order
                    } else {
                        position - order
                    };
                    channel.coefficients[position] -=
                        channel.coefficients[previous] * lpc[order - 1];
                }
            }
        }
    }
    Ok(())
}

fn gain_control(reader: &mut BitReader<'_>, sequence: u8) -> Result<()> {
    let (windows, start_sequence, location_width) = match sequence {
        0 => (1, false, 5),
        1 => (2, true, 2),
        2 => (8, false, 2),
        3 => (2, true, 5),
        _ => unreachable!("two-bit window sequence"),
    };
    let bands = bits(reader, 2)?;
    for _ in 0..bands {
        for window in 0..windows {
            let adjustments = bits(reader, 3)?;
            let location = if window == 0 && start_sequence {
                4
            } else {
                location_width
            };
            for _ in 0..adjustments {
                reader.skip_bits(4 + location)?;
            }
        }
    }
    Ok(())
}

fn sections(reader: &mut BitReader<'_>, info: &IcsInfo) -> Result<Vec<u8>> {
    let width = if info.short { 3 } else { 5 };
    let escape = (1 << width) - 1;
    let mut books = Vec::with_capacity(info.max_sfb * info.group_lengths.len());
    for _ in &info.group_lengths {
        let mut band = 0;
        while band < info.max_sfb {
            let book = u8::try_from(reader.read_bits(4)?).expect("four-bit codebook");
            if book == 12 {
                return Err(invalid("reserved spectral codebook 12"));
            }
            let start = band;
            loop {
                let increment = bits(reader, width)?;
                band += increment;
                if band > info.max_sfb {
                    return Err(invalid("section exceeds max_sfb"));
                }
                if increment != escape {
                    break;
                }
            }
            if band == start {
                return Err(invalid("zero-length section"));
            }
            books.extend(std::iter::repeat_n(book, band - start));
        }
    }
    Ok(books)
}

fn scalefactors(reader: &mut BitReader<'_>, books: &[u8], global_gain: i16) -> Result<Vec<i16>> {
    let mut spectral = global_gain;
    let mut noise = global_gain - 90;
    let mut intensity = 0_i16;
    let mut first_noise = true;
    let mut scales = Vec::with_capacity(books.len());
    for &book in books {
        match book {
            0 => scales.push(0),
            1..=11 => {
                spectral += huffman::scalefactor(reader)?;
                if !(0..=255).contains(&spectral) {
                    return Err(invalid("spectral scalefactor outside 0..255"));
                }
                scales.push(spectral);
            }
            13 => {
                noise += if first_noise {
                    first_noise = false;
                    i16::try_from(reader.read_bits(9)?).expect("nine-bit noise energy") - 256
                } else {
                    huffman::scalefactor(reader)?
                };
                noise = noise.clamp(-100, 155);
                scales.push(noise);
            }
            14 | 15 => {
                intensity += huffman::scalefactor(reader)?;
                intensity = intensity.clamp(-155, 100);
                scales.push(intensity);
            }
            _ => return Err(invalid("reserved spectral codebook")),
        }
    }
    Ok(scales)
}

fn band_offsets(config: &AudioSpecificConfig, short: bool) -> Result<&'static [usize]> {
    let index = crate::SAMPLE_RATES
        .iter()
        .position(|rate| *rate == config.sample_rate)
        .ok_or_else(|| unsupported("nonstandard sample rate"))?;
    if config.samples_per_frame != 1_024 {
        return Err(unsupported("unsupported AAC frame length"));
    }
    Ok(if short {
        tables::BANDS_128[index]
    } else {
        tables::BANDS_1024[index]
    })
}

fn data_stream(reader: &mut BitReader<'_>) -> Result<()> {
    reader.skip_bits(4)?;
    let align = reader.read_bit()?;
    let mut count = bits(reader, 8)?;
    if count == 255 {
        count += bits(reader, 8)?;
    }
    if align {
        reader.align_to_byte();
    }
    reader.skip_bits(count * 8)
}

fn fill(reader: &mut BitReader<'_>) -> Result<()> {
    let mut count = bits(reader, 4)?;
    if count == 15 {
        count = 14 + bits(reader, 8)?;
    }
    if count == 0 {
        return Ok(());
    }
    if reader.bits_remaining() < count * 8 {
        return Err(invalid("truncated fill element"));
    }
    let extension = reader.read_bits(4)?;
    // Only payloads consuming the entire fill element are skipped. In particular, do not
    // skip DRC blindly: a subsequent SBR extension could otherwise go undetected.
    match extension {
        0 | 1 => reader.skip_bits(count * 8 - 4),
        13 | 14 => Err(unsupported("SBR fill extension")),
        _ => Err(unsupported(&format!("fill extension {extension}"))),
    }
}

fn bits(reader: &mut BitReader<'_>, width: u8) -> Result<usize> {
    Ok(usize::try_from(reader.read_bits(width)?).expect("small syntax field fits usize"))
}

fn invalid(message: &str) -> Error {
    Error::InvalidData(format!("AAC: {message}"))
}
fn unsupported(message: &str) -> Error {
    Error::Unsupported(format!("native AAC-LC: {message}"))
}

#[cfg(test)]
mod tests {
    use mmrecode_bitstream::BitWriter;

    use super::*;

    fn packed(fields: &[(u64, u8)]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        for &(value, width) in fields {
            writer.write_bits(value, width).unwrap();
        }
        writer.into_bytes()
    }

    #[test]
    fn rejects_invalid_ics_fields_at_the_syntax_boundary() {
        let config = AudioSpecificConfig::parse(&[0x12, 0x10]).unwrap();
        for data in [
            packed(&[(1, 1)]),                                  // reserved bit
            packed(&[(0, 1), (0, 2), (0, 1), (0, 6), (1, 1)]),  // LC prediction
            packed(&[(0, 1), (0, 2), (0, 1), (50, 6), (0, 1)]), // long max_sfb > 49
            packed(&[(0, 1), (2, 2), (0, 1), (15, 4), (0, 7)]), // short max_sfb > 14
        ] {
            assert!(matches!(
                IcsInfo::read(&mut BitReader::new(&data), &config),
                Err(Error::InvalidData(_))
            ));
        }
    }

    #[test]
    fn rejects_nonprogressing_and_overrunning_sections() {
        let info = IcsInfo {
            short: false,
            sequence: 0,
            kbd: false,
            max_sfb: 1,
            group_lengths: vec![1],
        };
        for data in [
            packed(&[(0, 4), (0, 5)]),  // zero length must not loop forever
            packed(&[(0, 4), (2, 5)]),  // crosses max_sfb
            packed(&[(0, 4), (31, 5)]), // escaped length already exceeds max_sfb
            packed(&[(12, 4), (1, 5)]), // reserved book
            vec![0],                    // truncated section length
        ] {
            assert!(matches!(
                sections(&mut BitReader::new(&data), &info),
                Err(Error::InvalidData(_))
            ));
        }
    }

    #[test]
    fn rejects_reserved_stereo_masks() {
        let stereo = AudioSpecificConfig::parse(&[0x12, 0x10]).unwrap();
        let data = packed(&[(1, 1), (0, 1), (0, 2), (0, 1), (0, 6), (0, 1), (3, 2)]);
        assert!(matches!(
            channel_pair(&mut BitReader::new(&data), &stereo, &mut 0x1f2e_3d4c),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn parses_and_bounds_pulse_tns_and_gain_control() {
        let offsets = tables::BANDS_1024[4];
        let data = packed(&[(0, 2), (0, 6), (0, 5), (3, 4)]);
        let decoded = pulse(&mut BitReader::new(&data), offsets).unwrap();
        assert_eq!(decoded.positions, vec![(0, 3)]);
        assert!(pulse(&mut BitReader::new(&packed(&[(0, 2), (63, 6)])), offsets).is_err());

        // One forward, order-one long-window filter with a zero reflection coefficient.
        let data = packed(&[(1, 2), (1, 1), (4, 6), (1, 5), (0, 1), (0, 1), (0, 4)]);
        let decoded = tns(&mut BitReader::new(&data), false).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].len(), 1);
        assert_eq!(decoded[0][0].length, 4);
        assert_eq!(decoded[0][0].reflection, vec![0.0]);
        let excessive = packed(&[(1, 2), (0, 1), (1, 6), (13, 5)]);
        assert!(tns(&mut BitReader::new(&excessive), false).is_err());

        let gain = packed(&[(1, 2), (1, 3), (7, 4), (31, 5)]);
        gain_control(&mut BitReader::new(&gain), 0).unwrap();
        assert!(gain_control(&mut BitReader::new(&packed(&[(1, 2), (1, 3)])), 0).is_err());
    }

    #[test]
    fn bounded_ancillary_elements_reject_truncated_payloads() {
        let data = packed(&[(2, 4), (0, 4)]); // FIL says two bytes; less than one remains
        assert!(matches!(
            fill(&mut BitReader::new(&data)),
            Err(Error::InvalidData(_))
        ));
        let data = packed(&[(0, 4), (1, 1), (255, 8), (255, 8)]);
        assert!(matches!(
            data_stream(&mut BitReader::new(&data)),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn band_tables_partition_long_and_short_transforms_at_every_rate() {
        for (bands, length) in [(tables::BANDS_1024, 1_024), (tables::BANDS_128, 128)] {
            for offsets in bands {
                assert_eq!(offsets[0], 0);
                assert_eq!(offsets.last(), Some(&length));
                assert!(
                    offsets
                        .windows(2)
                        .all(|pair| pair[1] > pair[0] && (pair[1] - pair[0]).is_multiple_of(4))
                );
            }
        }
    }

    #[test]
    fn scalefactors_accumulate_across_zero_bands_and_reject_overflow() {
        // AAC scalefactor deltas +1 = 1010, -1 = 100, 0 = 0.
        let data = packed(&[(0b1010, 4), (0b100, 3)]);
        assert_eq!(
            scalefactors(&mut BitReader::new(&data), &[1, 0, 11], 100).unwrap(),
            vec![101, 0, 100]
        );
        assert!(scalefactors(&mut BitReader::new(&packed(&[(0b1010, 4)])), &[1], 255).is_err());
        assert!(scalefactors(&mut BitReader::new(&packed(&[(0b100, 3)])), &[1], 0).is_err());

        let special = packed(&[
            (256, 9),    // first PNS band: global_gain - 90
            (0b1010, 4), // subsequent PNS delta +1
            (0, 1),      // intensity delta zero
            (0b100, 3),  // intensity delta -1
        ]);
        assert_eq!(
            scalefactors(&mut BitReader::new(&special), &[13, 0, 13, 14, 15], 160).unwrap(),
            vec![70, 0, 71, 0, -1]
        );
    }
}
