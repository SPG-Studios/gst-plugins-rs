use std::fmt::Display;

use gst_video::{
    VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoColorimetry, VideoTransferFunction,
};
use image::metadata::{
    Cicp, CicpColorPrimaries, CicpMatrixCoefficients, CicpTransferCharacteristics,
    CicpVideoFullRangeFlag,
};

#[derive(Debug, Copy, Clone)]
pub(crate) struct ImageCicp(pub Cicp);

impl From<ImageCicp> for Cicp {
    fn from(value: ImageCicp) -> Self {
        value.0
    }
}

pub(crate) trait CanCicpRgb {
    /// Implements publicly Cicp::from(self.into_rgb()) == self
    /// (which checks for the two conditions below).
    fn is_rgb(&self) -> bool;
}

impl CanCicpRgb for ImageCicp {
    fn is_rgb(&self) -> bool {
        self.0.matrix == image::metadata::CicpMatrixCoefficients::Identity
            && self.0.full_range == image::metadata::CicpVideoFullRangeFlag::FullRange
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum UnsupportedCicp {
    ColorRange(CicpVideoFullRangeFlag),
    ColorMatrix(CicpMatrixCoefficients),
    TransferFunction(CicpTransferCharacteristics),
    Primaries(CicpColorPrimaries),
}

impl Display for UnsupportedCicp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            UnsupportedCicp::ColorRange(v) => gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color range {v:?}"]
            ),
            UnsupportedCicp::ColorMatrix(v) => gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color matrix {v:?}"]
            ),
            UnsupportedCicp::TransferFunction(v) => gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown transfer function {v:?}"]
            ),
            UnsupportedCicp::Primaries(v) => gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color primaries {v:?}"]
            ),
        };
        write!(f, "{}", msg)?;
        Ok(())
    }
}

impl From<UnsupportedCicp> for Result<VideoColorimetry, UnsupportedCicp> {
    fn from(value: UnsupportedCicp) -> Self {
        Err(value)
    }
}

impl TryFrom<ImageCicp> for VideoColorimetry {
    type Error = UnsupportedCicp;

    fn try_from(value: ImageCicp) -> Result<Self, Self::Error> {
        use UnsupportedCicp::*;

        let rg = match value.0.full_range {
            CicpVideoFullRangeFlag::NarrowRange => VideoColorRange::Range16_235,
            CicpVideoFullRangeFlag::FullRange => VideoColorRange::Range0_255,
            v => return ColorRange(v).into(),
        };

        let mx = match value.0.matrix {
            CicpMatrixCoefficients::Unspecified => VideoColorMatrix::Unknown,
            v => match VideoColorMatrix::from_iso(v as u32) {
                VideoColorMatrix::Unknown => return ColorMatrix(v).into(),
                v => v,
            },
        };

        let tf = match value.0.transfer {
            CicpTransferCharacteristics::Unspecified => VideoTransferFunction::Unknown,
            v => match VideoTransferFunction::from_iso(v as u32) {
                VideoTransferFunction::Unknown => return TransferFunction(v).into(),
                v => v,
            },
        };

        // See Rec. ITU-T H.273 (V4) (07/2024) table 2, p. 5
        // and the image-rs docs
        let pr = match value.0.primaries {
            CicpColorPrimaries::Unspecified => VideoColorPrimaries::Unknown,
            v => match VideoColorPrimaries::from_iso(v as u32) {
                VideoColorPrimaries::Unknown => return Primaries(v).into(),
                v => v,
            },
        };

        Ok(VideoColorimetry::new(rg, mx, tf, pr))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum UnsupportedVideoColorimetry {
    ColorRange(VideoColorRange),
    ColorMatrix(VideoColorMatrix),
    TransferFunction(VideoTransferFunction),
    Primaries(VideoColorPrimaries),
}

impl Display for UnsupportedVideoColorimetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use UnsupportedVideoColorimetry::*;

        let msg = match self {
            ColorRange(v) => gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color range {v:?}"]
            ),
            ColorMatrix(v) => gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color matrix {v:?}"]
            ),
            TransferFunction(v) => gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown transfer function {v:?}"]
            ),
            Primaries(v) => gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color primaries {v:?}"]
            ),
        };
        write!(f, "{}", msg)?;
        Ok(())
    }
}

impl From<UnsupportedVideoColorimetry> for Result<ImageCicp, UnsupportedVideoColorimetry> {
    fn from(value: UnsupportedVideoColorimetry) -> Self {
        Err(value)
    }
}

impl TryFrom<VideoColorimetry> for ImageCicp {
    type Error = UnsupportedVideoColorimetry;

    fn try_from(value: VideoColorimetry) -> Result<Self, Self::Error> {
        use UnsupportedVideoColorimetry::*;

        // This can NOT be done with VideoColorPrimaries::to_iso because it
        // is unsafe to convert an integer to an enum value.
        let mx = match value.matrix() {
            VideoColorMatrix::Unknown => CicpMatrixCoefficients::Unspecified,
            VideoColorMatrix::Rgb => CicpMatrixCoefficients::Identity,
            VideoColorMatrix::Fcc => CicpMatrixCoefficients::UsFCC,
            VideoColorMatrix::Bt709 => CicpMatrixCoefficients::Bt709,
            VideoColorMatrix::Bt601 => CicpMatrixCoefficients::Smpte170m,
            VideoColorMatrix::Smpte240m => CicpMatrixCoefficients::Smpte240m,
            VideoColorMatrix::Bt2020 => CicpMatrixCoefficients::Bt2020NonConstant,
            v => return ColorMatrix(v).into(),
        };

        let tf = match value.transfer() {
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
            v => return TransferFunction(v).into(),
        };

        let pr = match value.primaries() {
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
            v => return Primaries(v).into(),
        };

        let rg = match value.range() {
            gst_video::VideoColorRange::Range0_255 => {
                image::metadata::CicpVideoFullRangeFlag::FullRange
            }
            gst_video::VideoColorRange::Range16_235 => {
                image::metadata::CicpVideoFullRangeFlag::NarrowRange
            }
            v => return ColorRange(v).into(),
        };

        Ok(ImageCicp(Cicp {
            full_range: rg,
            matrix: mx,
            primaries: pr,
            transfer: tf,
        }))
    }
}
