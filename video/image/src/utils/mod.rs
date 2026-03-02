use gst_video::{
    VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoColorimetry, VideoTransferFunction,
};
use image::metadata::{
    Cicp, CicpColorPrimaries, CicpMatrixCoefficients, CicpTransferCharacteristics,
    CicpVideoFullRangeFlag,
};

pub(crate) fn cicp_to_videoinfo(cicp: Cicp) -> Result<VideoColorimetry, gst::ErrorMessage> {
    let rg = match cicp.full_range {
        CicpVideoFullRangeFlag::NarrowRange => VideoColorRange::Range16_235,
        CicpVideoFullRangeFlag::FullRange => VideoColorRange::Range0_255,
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color range {v:?}"]
            ));
        }
    };

    let mx = match cicp.matrix {
        CicpMatrixCoefficients::Unspecified => VideoColorMatrix::Unknown,
        v => match VideoColorMatrix::from_iso(v as u32) {
            VideoColorMatrix::Unknown => {
                return Err(gst::error_msg!(
                    gst::CoreError::NotImplemented,
                    ["Unknown color matrix {v:?}"]
                ));
            }
            v => v,
        },
    };

    let tf = match cicp.transfer {
        CicpTransferCharacteristics::Unspecified => VideoTransferFunction::Unknown,
        v => match VideoTransferFunction::from_iso(v as u32) {
            VideoTransferFunction::Unknown => {
                return Err(gst::error_msg!(
                    gst::CoreError::NotImplemented,
                    ["Unknown transfer function {v:?}"]
                ));
            }
            v => v,
        },
    };

    // See Rec. ITU-T H.273 (V4) (07/2024) table 2, p. 5
    // and the image-rs docs
    let pr = match cicp.primaries {
        CicpColorPrimaries::Unspecified => VideoColorPrimaries::Unknown,
        v => match VideoColorPrimaries::from_iso(v as u32) {
            VideoColorPrimaries::Unknown => {
                return Err(gst::error_msg!(
                    gst::CoreError::NotImplemented,
                    ["Unknown color primaries {v:?}"]
                ));
            }
            v => v,
        },
    };

    Ok(VideoColorimetry::new(rg, mx, tf, pr))
}

pub(crate) fn videoinfo_to_cicp(color_space: VideoColorimetry) -> Result<Cicp, gst::ErrorMessage> {
    // This can NOT be done with VideoColorPrimaries::to_iso because it
    // is unsafe to convert an integer to an enum value.
    let mx = match color_space.matrix() {
        VideoColorMatrix::Unknown => CicpMatrixCoefficients::Unspecified,
        VideoColorMatrix::Rgb => CicpMatrixCoefficients::Identity,
        VideoColorMatrix::Fcc => CicpMatrixCoefficients::UsFCC,
        VideoColorMatrix::Bt709 => CicpMatrixCoefficients::Bt709,
        VideoColorMatrix::Bt601 => CicpMatrixCoefficients::Smpte170m,
        VideoColorMatrix::Smpte240m => CicpMatrixCoefficients::Smpte240m,
        VideoColorMatrix::Bt2020 => CicpMatrixCoefficients::Bt2020NonConstant,
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color matrix {v:?}"]
            ));
        }
    };

    let tf = match color_space.transfer() {
        VideoTransferFunction::Unknown => CicpTransferCharacteristics::Unspecified,
        VideoTransferFunction::Gamma10 => CicpTransferCharacteristics::Linear,
        VideoTransferFunction::Gamma22 => CicpTransferCharacteristics::Bt470M,
        VideoTransferFunction::Bt709 => CicpTransferCharacteristics::Bt709,
        VideoTransferFunction::Smpte240m => CicpTransferCharacteristics::Smpte240m,
        VideoTransferFunction::Srgb => CicpTransferCharacteristics::SRgb,
        VideoTransferFunction::Gamma28 => CicpTransferCharacteristics::Bt470BG,
        VideoTransferFunction::Log100 => CicpTransferCharacteristics::Log100,
        VideoTransferFunction::Log316 => CicpTransferCharacteristics::LogSqrt,
        VideoTransferFunction::Bt202012 => CicpTransferCharacteristics::Bt2020_12bit,
        VideoTransferFunction::Bt202010 => CicpTransferCharacteristics::Bt2020_10bit,
        VideoTransferFunction::Smpte2084 => CicpTransferCharacteristics::Smpte2084,
        VideoTransferFunction::AribStdB67 => CicpTransferCharacteristics::Bt2100Hlg,
        VideoTransferFunction::Bt601 => CicpTransferCharacteristics::Bt601,
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown transfer function {v:?}"]
            ));
        }
    };

    let pr = match color_space.primaries() {
        VideoColorPrimaries::Bt709 => CicpColorPrimaries::SRgb,
        VideoColorPrimaries::Unknown => CicpColorPrimaries::Unspecified,
        VideoColorPrimaries::Bt470m => CicpColorPrimaries::RgbM,
        VideoColorPrimaries::Bt470bg => CicpColorPrimaries::RgbB,
        VideoColorPrimaries::Smpte170m => CicpColorPrimaries::Bt601,
        VideoColorPrimaries::Smpte240m => CicpColorPrimaries::Rgb240m,
        VideoColorPrimaries::Film => CicpColorPrimaries::GenericFilm,
        VideoColorPrimaries::Bt2020 => CicpColorPrimaries::Rgb2020,
        VideoColorPrimaries::Smptest428 => CicpColorPrimaries::Xyz,
        VideoColorPrimaries::Smpterp431 => CicpColorPrimaries::SmpteRp431,
        VideoColorPrimaries::Smpteeg432 => CicpColorPrimaries::SmpteRp432,
        VideoColorPrimaries::Ebu3213 => CicpColorPrimaries::Industry22,
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color primaries {v:?}"]
            ));
        }
    };

    let rg = match color_space.range() {
        gst_video::VideoColorRange::Range0_255 => {
            image::metadata::CicpVideoFullRangeFlag::FullRange
        }
        gst_video::VideoColorRange::Range16_235 => {
            image::metadata::CicpVideoFullRangeFlag::NarrowRange
        }
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color range {v:?}"]
            ));
        }
    };

    Ok(Cicp {
        full_range: rg,
        matrix: mx,
        primaries: pr,
        transfer: tf,
    })
}
