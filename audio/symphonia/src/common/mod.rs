// Copyright (C) 2022-2026 François Laignel <francois@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Symphonia audio decoders utilities.

use gst::glib::object::IsA;
use symphonia::core::audio::GenericAudioBufferRef;

/// Supported audio formats in preference order.
pub const AUDIO_FORMAT_LIST: [gst_audio::AudioFormat; 10] = [
    gst_audio::AUDIO_FORMAT_F32,
    gst_audio::AUDIO_FORMAT_F64,
    gst_audio::AUDIO_FORMAT_S32,
    gst_audio::AUDIO_FORMAT_U32,
    gst_audio::AUDIO_FORMAT_S24,
    gst_audio::AUDIO_FORMAT_U24,
    gst_audio::AUDIO_FORMAT_S16,
    gst_audio::AUDIO_FORMAT_U16,
    gst_audio::AUDIO_FORMAT_S8,
    gst_audio::AUDIO_FORMAT_U8,
];

/// Returns Symphonia compliant audio decoder src pad template.
pub fn src_caps_template() -> gst::PadTemplate {
    let mut src_caps =
        gst_audio::AudioCapsBuilder::for_encoding("audio/x-raw").format_list(AUDIO_FORMAT_LIST);

    #[cfg(feature = "allow-planar")]
    {
        src_caps = src_caps.layout_list([
            gst_audio::AudioLayout::Interleaved,
            gst_audio::AudioLayout::NonInterleaved,
        ]);
    }
    #[cfg(not(feature = "allow-planar"))]
    {
        src_caps = src_caps.layout(gst_audio::AudioLayout::Interleaved);
    }

    gst::PadTemplate::new(
        "src",
        gst::PadDirection::Src,
        gst::PadPresence::Always,
        &src_caps.build(),
    )
    .unwrap()
}

/// Builds a `Caps` filter with `audio_info` fields appearing first.
pub fn build_caps_filter(audio_info: gst_audio::AudioInfo) -> gst::Caps {
    use gst_audio::AudioChannelPosition;

    let mut caps = gst_audio::AudioCapsBuilder::for_encoding("audio/x-raw");

    let in_format = audio_info.format();
    let list_without_in_format = AUDIO_FORMAT_LIST
        .iter()
        .filter(move |&&f| f != in_format)
        .cloned();
    let out_formats = std::iter::once(in_format).chain(list_without_in_format);
    caps = caps.format_list(out_formats);

    #[cfg(feature = "allow-planar")]
    {
        caps = caps.layout_list([
            gst_audio::AudioLayout::Interleaved,
            gst_audio::AudioLayout::NonInterleaved,
        ]);
    }
    #[cfg(not(feature = "allow-planar"))]
    {
        caps = caps.layout(gst_audio::AudioLayout::Interleaved);
    }

    caps = caps.rate(audio_info.rate() as i32);

    let channels = audio_info.channels();
    caps = caps.channels(channels as i32);

    if channels > 1 {
        let channel_mask = audio_info
            .positions()
            .map(|positions| AudioChannelPosition::positions_to_mask(positions, true).unwrap())
            .unwrap_or_else(|| AudioChannelPosition::fallback_mask(channels));

        caps = caps.field("channel-mask", gst::Bitmask::new(channel_mask));
    }

    caps.build()
}

/// Exports decoded data to a `gst::Buffer` complying with `out_audio_info`.
// FIXME handle channels mapping
pub fn to_outbuf<D>(
    dec: &D,
    decoded: GenericAudioBufferRef,
    out_audio_info: &gst_audio::AudioInfo,
) -> Result<gst::Buffer, gst::FlowError>
where
    D: IsA<gst_audio::AudioDecoder> + IsA<gst::Element>,
{
    use symphonia::core::audio::conv::ConvertibleSample;
    use symphonia::core::audio::sample::{SampleBytes, i24, u24};

    fn inner<S, D>(
        dec: &D,
        decoded: GenericAudioBufferRef,
        out_audio_info: &gst_audio::AudioInfo,
    ) -> Result<gst::Buffer, gst::FlowError>
    where
        D: IsA<gst_audio::AudioDecoder> + IsA<gst::Element>,
        S: SampleBytes + ConvertibleSample,
    {
        use gst_audio::prelude::*;

        let rawbuf_len = decoded.byte_len_as::<S>();
        let mut outbuf = dec.allocate_output_buffer(rawbuf_len);
        {
            let outbuf = outbuf.get_mut().unwrap();
            let mut outbuf_mapped = outbuf
                .map_writable()
                .expect("newly obtained buffer is writable");

            if out_audio_info.layout() == gst_audio::AudioLayout::Interleaved {
                decoded.copy_bytes_interleaved_as::<S, _>(outbuf_mapped.as_mut_slice());
            } else if cfg!(feature = "allow-planar") {
                let mut outplans = outbuf_mapped
                    .chunks_exact_mut(rawbuf_len / (out_audio_info.channels() as usize))
                    .collect::<smallvec::SmallVec<[&mut [u8]; 8]>>();

                decoded.copy_bytes_planar_as::<S, _>(outplans.as_mut_slice());
            } else {
                panic!(
                    "Negotiated non-interleaved layout but the feature `allow-planar` is not activated"
                );
            }
            drop(outbuf_mapped);

            #[cfg(feature = "audio-meta")]
            gst_audio::AudioMeta::add(outbuf, out_audio_info, decoded.frames() as _, &[]).unwrap();
        }

        Ok(outbuf)
    }

    match out_audio_info.format() {
        gst_audio::AUDIO_FORMAT_U8 => inner::<u8, D>(dec, decoded, out_audio_info),
        gst_audio::AUDIO_FORMAT_U16 => inner::<u16, D>(dec, decoded, out_audio_info),
        gst_audio::AUDIO_FORMAT_U24 => inner::<u24, D>(dec, decoded, out_audio_info),
        gst_audio::AUDIO_FORMAT_U32 => inner::<u32, D>(dec, decoded, out_audio_info),
        gst_audio::AUDIO_FORMAT_S8 => inner::<i8, D>(dec, decoded, out_audio_info),
        gst_audio::AUDIO_FORMAT_S16 => inner::<i16, D>(dec, decoded, out_audio_info),
        gst_audio::AUDIO_FORMAT_S24 => inner::<i24, D>(dec, decoded, out_audio_info),
        gst_audio::AUDIO_FORMAT_S32 => inner::<i32, D>(dec, decoded, out_audio_info),
        gst_audio::AUDIO_FORMAT_F32 => inner::<f32, D>(dec, decoded, out_audio_info),
        gst_audio::AUDIO_FORMAT_F64 => inner::<f64, D>(dec, decoded, out_audio_info),
        other => panic!("Not a Symphonia compatible audio format: {other:?}"),
    }
}
