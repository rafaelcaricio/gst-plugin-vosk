# Vosk Speech Recognition GStreamer Plugin

Transcription of speech using [Vosk Toolkit](https://alphacephei.com/vosk/). Can be used to generate subtitles for
movies, live streams, lectures and interviews.

> Vosk is an offline open source speech recognition toolkit. It enables speech recognition for 20+ languages and
> dialects - English, Indian English, German, French, Spanish, Portuguese, Chinese, Russian, Turkish, Vietnamese,
> Italian, Dutch, Catalan, Arabic, Greek, Farsi, Filipino, Ukrainian, Kazakh, Swedish, Japanese, Esperanto, Hindi.
> More to come.
>
> https://github.com/alphacep/vosk-api

This GStreamer plugin was inspired by the work of [@MathieuDuponchelle](https://github.com/mathieuduponchelle) in the
[AwsTranscriber](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/tree/main/net/rusoto#awstranscriber) element.

## Build

Compiling this project will provide a shared library that can be used by your local GStreamer installation.

```bash
cargo build --release
```

The compiled shared library `./target/release/libgstvosk.dylib` must be made loadable to GStreamer. One possible
solution is to use the argument `--gst-plugin-path=` pointing to the location where the library file is every time you
run `gst-launch-1.0` command line tool.


## Example Usage

This plugin connects via websockets protocol to the [Vosk Server](https://alphacephei.com/vosk/server). The easiest
way to run the Vosk server is using [Docker](https://docs.docker.com/). You can run the server locally using
this command:

```bash
docker run --rm --name vosk-server -d -p 2700:2700 alphacep/kaldi-en:latest
```

Running the recognition server as a separated process comes with the additional benefit that you don't need to
install any special software. Plus the voice recognition work load is off your GStreamer pipeline process.

This example will just print out the raw text buffers that are published out by the Vosk transcriber:

```bash
gst-launch-1.0 \
  vosk_transcriber name=tc ! fakesink sync=true dump=true \
  uridecodebin uri=https://studio.blender.org/download-source/d1/d1f3b354a8f741c6afabf305489fa510/d1f3b354a8f741c6afabf305489fa510-1080p.mp4 ! audioconvert ! tc.
```
