# GStreamer DSC Plugin

A GStreamer plugin for Digitally Signed Content (DSC) that provides cryptographic signing and verification for encoded video data using Rust RSA and certificate parsing libraries.

Mechanisms for trustworthy authentication and verification of video content were recently developed by JVET for inclusion in video coding standards. This is realized by three supplemental enhancement information (SEI) messages that attach cryptographic signatures to flexible chunks of a video stream at the network abstraction layer (NAL) unit level.

The following are the references used for this implementation:
* Paper: https://www.hhi.fraunhofer.de/fileadmin/Events/2025/IBC_2025/IBC2025PaperAuthentication_HHI.pdf
* JVET Specs: https://www.jvet-experts.org/doc_end_user/documents/40_Geneva/wg11/JVET-AN1019-v1.zip
* DSC implementation in VVC VTM: https://vcgit.hhi.fraunhofer.de/jvet/VVCSoftware_VTM/-/releases/VTM-23.13

## Elements

- **DSC Signer**: Signs data packets based on the specified hash method
- **DSC Verifier**: Verifies the signed data

## Crypto stack

- Runtime implementation (`dscsigner` / `dscverifier`) uses pure-Rust crypto crates (`rsa`, `sha1`, `sha2`, `x509-parser`, `rustls-pemfile`).
- Test helper code is also OpenSSL-free and uses `rsa` + `rcgen`.
- OpenSSL is not required as a dependency for this plugin runtime or test helper logic.

## Generate certificate samples
The plugin accepts standard PEM key/certificate files. You can generate them with any tooling.

For tests and development, see the helper implementation in `tests/dsc.rs` (`create_test_keys()`), which generates RSA keys and self-signed certificates with Rust crates only.

If you prefer manual CLI generation, the following OpenSSL commands are optional:

### Create CA
```bash
openssl genrsa -out example_ca.key 4096
openssl genrsa -aes256 -out example_ca.key 4096
openssl req -x509 -new -nodes -key example_ca.key -sha256 -days 1826 -out example_ca.crt
openssl x509 -in example_ca.crt -noout -pubkey -out example_ca.pub
```

### Create Content Provider certificate
```bash
openssl genrsa -out example_content.key 4096
openssl req -new -key example_content.key -out example_content.csr
openssl x509 -req -in example_content.csr -CA example_ca.crt -CAkey example_ca.key -out example_content.crt -days 730 -sha256
openssl x509 -in example_content.crt -noout -pubkey -out example_content.pub
```

## Example
```bash
gst-launch-1.0 videotestsrc pattern=ball num-buffers=30 ! "video/x-raw,framerate=30/1" ! videoconvert ! x265enc key-int-max=5 ! dscsigner private-key-path=./example_content.key public-key-uri=./example_content.crt substream-length=5 ! dscverifier key-store-path="$(pwd)" ! h265parse ! avdec_h265 ! videoconvert ! autovideosink
```
