macro_rules! append_caps {
    ($c:ident, $mime:literal) => (
        $c.append(gst::Caps::builder($mime).build());
    );
    ($c:ident, $mime:literal, $($mime2:literal),+) => (
        $c.append(gst::Caps::builder($mime).build());
        append_caps!($c, $($mime2),+)
    );
}

macro_rules! make_caps {
    ($mime:literal) => {
        gst::Caps::builder($mime).build()
    };

    ($($mime:literal),+) => {
        {
            let mut caps = gst::Caps::new_empty();
            let c = caps.make_mut();
            append_caps!(c, $($mime),+);
            caps
        }
    };

    ($format:expr) => {
        gst::Caps::builder($format.to_mime_type()).build()
    };
}

macro_rules! make_caps_with_extra_mimetypes {
    ($format:expr, $($mime:literal),+) => {
        {
            let mut caps = gst::Caps::new_empty();
            let c = caps.make_mut();
            c.append(gst::Caps::builder($format.to_mime_type()).build());
            append_caps!(c, $($mime),+);
            caps
        }
    };
}

macro_rules! append_pixel_caps {
    ($c:ident, $mime:literal, $formats:expr) => (
        $c.append(gst::Caps::builder($mime).field(
                    "format",
                    gst::List::new($formats.into_iter().map(|f| f.to_str())),
                ).build());
    );
    ($c:ident, $mime:literal, $($mime2:literal),+, $formats:expr) => (
        $c.append(gst::Caps::builder($mime).field(
                    "format",
                    gst::List::new($formats.into_iter().map(|f| f.to_str())),
                ).build());
        append_caps!($c, $($mime2),+, $formats)
    );
}

macro_rules! make_encoder_caps {
    ($mime:literal, $formats:expr) => {{
        let mut caps = gst::Caps::new_empty();
        let c = caps.make_mut();
        append_pixel_caps!(c, $mime, $formats);
        caps
    }};

    ($format:expr, $formats:expr) => {{
        let mut caps = gst::Caps::new_empty();
        let c = caps.make_mut();
        c.append(
            gst::Caps::builder($format.to_mime_type())
                .field(
                    "format",
                    gst::List::new($formats.into_iter().map(|f| f.to_str())),
                )
                .build(),
        );
        caps
    }};
}

macro_rules! make_encoder_caps_with_extra_mimetypes {
    ($format:expr, $($mime:literal),+, $formats:expr) => {{
        let mut caps = gst::Caps::new_empty();
        let c = caps.make_mut();
        c.append(
            gst::Caps::builder($format.to_mime_type())
                .field(
                    "format",
                    gst::List::new($formats.into_iter().map(|f| f.to_str())),
                )
                .build(),
        );
        append_pixel_caps!(c, $($mime),+, $formats);
        caps
    }};
}
