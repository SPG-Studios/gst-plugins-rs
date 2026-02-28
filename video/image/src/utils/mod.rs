use gst_video::{
    VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoColorimetry, VideoTransferFunction,
};
use image::metadata::{
    Cicp, CicpColorPrimaries, CicpMatrixCoefficients, CicpTransferCharacteristics,
    CicpVideoFullRangeFlag,
};

pub(crate) fn cicp_to_videoinfo(cicp: Cicp) -> VideoColorimetry {
    let rg = match cicp.full_range {
        CicpVideoFullRangeFlag::NarrowRange => VideoColorRange::Range16_235,
        CicpVideoFullRangeFlag::FullRange => VideoColorRange::Range0_255,
        _ => VideoColorRange::Unknown,
    };
    // Doing this match exhaustively so we know which ones
    // aren't supported by GStreamer (from_iso will return Unknown)
    let mx = match cicp.matrix {
        CicpMatrixCoefficients::Identity => VideoColorMatrix::Rgb,
        CicpMatrixCoefficients::Bt709 => VideoColorMatrix::Bt709,
        CicpMatrixCoefficients::Unspecified => VideoColorMatrix::Unknown,
        CicpMatrixCoefficients::UsFCC => VideoColorMatrix::Fcc,
        CicpMatrixCoefficients::Bt470BG | CicpMatrixCoefficients::Smpte170m => {
            VideoColorMatrix::Bt601
        }
        CicpMatrixCoefficients::Smpte240m => VideoColorMatrix::Smpte240m,
        CicpMatrixCoefficients::YCgCo => VideoColorMatrix::from_iso(8),
        CicpMatrixCoefficients::Bt2020NonConstant => VideoColorMatrix::Bt2020,
        CicpMatrixCoefficients::Bt2020Constant => VideoColorMatrix::from_iso(10),
        CicpMatrixCoefficients::Smpte2085 => VideoColorMatrix::from_iso(11),
        CicpMatrixCoefficients::ChromaticityDerivedNonConstant => VideoColorMatrix::from_iso(12),
        CicpMatrixCoefficients::ChromaticityDerivedConstant => VideoColorMatrix::from_iso(13),
        CicpMatrixCoefficients::Bt2100 => VideoColorMatrix::from_iso(14),
        CicpMatrixCoefficients::IptPqC2 => VideoColorMatrix::from_iso(15),
        CicpMatrixCoefficients::YCgCoRe => VideoColorMatrix::from_iso(16),
        CicpMatrixCoefficients::YCgCoRo => VideoColorMatrix::from_iso(17),
        _ => VideoColorMatrix::Unknown,
    };

    let tf = match cicp.transfer {
        CicpTransferCharacteristics::Bt709 => VideoTransferFunction::Bt709,
        CicpTransferCharacteristics::Unspecified => VideoTransferFunction::Unknown,
        CicpTransferCharacteristics::Bt470M => VideoTransferFunction::Gamma22,
        CicpTransferCharacteristics::Bt470BG => VideoTransferFunction::Gamma28,
        CicpTransferCharacteristics::Bt601 => VideoTransferFunction::Bt601,
        CicpTransferCharacteristics::Smpte240m => VideoTransferFunction::Smpte240m,
        CicpTransferCharacteristics::Linear => VideoTransferFunction::Gamma10,
        CicpTransferCharacteristics::Log100 => VideoTransferFunction::Log100,
        CicpTransferCharacteristics::LogSqrt => VideoTransferFunction::Log316,
        CicpTransferCharacteristics::Iec61966_2_4 => VideoTransferFunction::from_iso(11),
        CicpTransferCharacteristics::Bt1361 => VideoTransferFunction::from_iso(12),
        CicpTransferCharacteristics::SRgb => VideoTransferFunction::Srgb,
        CicpTransferCharacteristics::Bt2020_10bit => VideoTransferFunction::Bt202010,
        CicpTransferCharacteristics::Bt2020_12bit => VideoTransferFunction::Bt202012,
        CicpTransferCharacteristics::Smpte2084 => VideoTransferFunction::Smpte2084,
        CicpTransferCharacteristics::Smpte428 => VideoTransferFunction::from_iso(17),
        CicpTransferCharacteristics::Bt2100Hlg => VideoTransferFunction::AribStdB67,
        _ => VideoTransferFunction::Unknown,
    };

    // See Rec. ITU-T H.273 (V4) (07/2024) table 2, p. 5
    // and the image-rs docs
    let pr = match cicp.primaries {
        CicpColorPrimaries::SRgb => VideoColorPrimaries::Bt709,
        CicpColorPrimaries::Unspecified => VideoColorPrimaries::Unknown,
        CicpColorPrimaries::RgbM => VideoColorPrimaries::Bt470m,
        CicpColorPrimaries::RgbB => VideoColorPrimaries::Bt470bg,
        CicpColorPrimaries::Bt601 => VideoColorPrimaries::Smpte170m,
        CicpColorPrimaries::Rgb240m => VideoColorPrimaries::Smpte240m,
        CicpColorPrimaries::GenericFilm => VideoColorPrimaries::Film,
        CicpColorPrimaries::Rgb2020 => VideoColorPrimaries::Bt2020,
        CicpColorPrimaries::Xyz => VideoColorPrimaries::Smptest428,
        CicpColorPrimaries::SmpteRp431 => VideoColorPrimaries::Smpterp431,
        CicpColorPrimaries::SmpteRp432 => VideoColorPrimaries::Smpteeg432,
        CicpColorPrimaries::Industry22 => VideoColorPrimaries::Ebu3213,
        _ => VideoColorPrimaries::Unknown,
    };

    VideoColorimetry::new(rg, mx, tf, pr)
}

pub(crate) fn videoinfo_to_cicp(color_space: VideoColorimetry) -> Cicp {
    let mx = match color_space.matrix() {
        VideoColorMatrix::Unknown => CicpMatrixCoefficients::Unspecified,
        VideoColorMatrix::Rgb => CicpMatrixCoefficients::Identity,
        VideoColorMatrix::Fcc => CicpMatrixCoefficients::UsFCC,
        VideoColorMatrix::Bt709 => CicpMatrixCoefficients::Bt709,
        VideoColorMatrix::Bt601 => CicpMatrixCoefficients::Smpte170m,
        VideoColorMatrix::Smpte240m => CicpMatrixCoefficients::Smpte240m,
        VideoColorMatrix::Bt2020 => CicpMatrixCoefficients::Bt2020NonConstant,
        _ => CicpMatrixCoefficients::Unspecified,
    };

    let tf = match color_space.transfer() {
        VideoTransferFunction::Unknown => CicpTransferCharacteristics::Unspecified,
        VideoTransferFunction::Gamma10 => CicpTransferCharacteristics::Linear,
        VideoTransferFunction::Gamma18 => CicpTransferCharacteristics::Unspecified,
        VideoTransferFunction::Gamma20 => CicpTransferCharacteristics::Unspecified,
        VideoTransferFunction::Gamma22 => CicpTransferCharacteristics::Bt470M,
        VideoTransferFunction::Bt709 => CicpTransferCharacteristics::Bt709,
        VideoTransferFunction::Smpte240m => CicpTransferCharacteristics::Smpte240m,
        VideoTransferFunction::Srgb => CicpTransferCharacteristics::SRgb,
        VideoTransferFunction::Gamma28 => CicpTransferCharacteristics::Bt470BG,
        VideoTransferFunction::Log100 => CicpTransferCharacteristics::Log100,
        VideoTransferFunction::Log316 => CicpTransferCharacteristics::LogSqrt,
        VideoTransferFunction::Bt202012 => CicpTransferCharacteristics::Bt2020_12bit,
        VideoTransferFunction::Adobergb => CicpTransferCharacteristics::Unspecified,
        VideoTransferFunction::Bt202010 => CicpTransferCharacteristics::Bt2020_10bit,
        VideoTransferFunction::Smpte2084 => CicpTransferCharacteristics::Smpte2084,
        VideoTransferFunction::AribStdB67 => CicpTransferCharacteristics::Bt2100Hlg,
        VideoTransferFunction::Bt601 => CicpTransferCharacteristics::Bt601,
        _ => CicpTransferCharacteristics::Unspecified,
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
        _ => CicpColorPrimaries::Unspecified,
    };

    let rg = match color_space.range() {
        gst_video::VideoColorRange::Unknown => unimplemented!(),
        gst_video::VideoColorRange::Range0_255 => {
            image::metadata::CicpVideoFullRangeFlag::FullRange
        }
        gst_video::VideoColorRange::Range16_235 => {
            image::metadata::CicpVideoFullRangeFlag::NarrowRange
        }
        _ => unimplemented!(),
    };

    Cicp {
        full_range: rg,
        matrix: mx,
        primaries: pr,
        transfer: tf,
    }
}
