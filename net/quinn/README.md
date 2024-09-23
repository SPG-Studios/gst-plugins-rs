# gst-plugin-quinn

This is a [GStreamer](https://gstreamer.freedesktop.org/) plugin for using [QUIC](https://www.rfc-editor.org/rfc/rfc9000.html) as the transport build using [quinn-rs](https://github.com/quinn-rs/quinn).

## Examples

Build the examples by running
```bash
cargo build -p gst-plugin-quinn --examples
```

### QUIC

QUIC multiplexing example can be tested as follows.
```bash
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example quic_mux
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example quic_mux -- --receiver
```

QUIC multiplexing example with WebTransport can be tested as follows.
```bash
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example quic_mux -- --webtransport
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example quic_mux -- --receiver --webtransport
```


### RTP over QUIC

RoQ example can be tested as follows. This tests H264 by default.
```bash
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example quic_roq
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example quic_roq -- --receiver
```

To test RoQ with VP8.
```bash
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example quic_roq -- --vp8
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example quic_roq -- --receiver --vp8
```

### Media over QUIC

Note that Media over QUIC specification is still under active development. Related `moqmux` and `moqdemux` plugins might need updates accordingly. As of this writing, Draft-14 is supported. Previous specifications won't be supported once an update to a later/recent specification is done.

The applicable specifications can be found at the below links.

- https://www.ietf.org/archive/id/draft-ietf-moq-transport-14.html
- https://www.ietf.org/archive/id/draft-ietf-moq-msf-00.html
- https://www.ietf.org/archive/id/draft-ietf-moq-cmsf-00.html

Media over QUIC needs a relay, a publisher and a subscriber.

Run the relay from [moq-rs](github.com/cloudflare/moq-rs) as follows.
```bash
RUST_LOG=moq_relay=trace dev/relay
```

Run the publisher example as follows. A media file must be provided using the `--uri` argument.
```bash
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example moq_pub -- --uri file:///gst-plugins-rs/HLS.mkv --webtransport
```

Run the subscriber example as follows.
```bash
GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example moq_sub -- --webtransport
```

To test with QUIC, instead of WebTransport, drop the `--webtransport` argument.
