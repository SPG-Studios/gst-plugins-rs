# Whisper transcriber
A rust plugin based on whisper.cpp library, which is port of OpenAI's Whisper model in C/C++

## Build
```bash
cargo cbuild -p gst-plugin-whisper
```

## Downloading the language model, the example resource and run the pipeline
First you should have to build the plugin to have the dependencies already downloaded.
```bash
cd <gst-plugins-rs>
$(find . -iname download-ggml-model.sh -print -quit | xargs realpath) base.en
export LM=$(find . -iname ggml-base.en.bin -print -quit | xargs realpath)
export FILE=$(find . -iname jfk.wav -print -quit | xargs realpath)
gst-launch-1.0 \
    filesrc location=${FILE} ! \
    decodebin ! \
    audioconvert ! \
    audio/x-raw,format=F32LE ! \
    whispertranscriber model-path=${LM} ! \
    textrender ! \
    videoconvert ! \
    autovideosink
    textrender ! \
    videoconvert ! \
    autovideosink
```
