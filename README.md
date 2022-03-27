Vosk Speech Recognition GStreamer Plugin
========================================

Transcription of speech using [Vosk Toolkit](https://alphacephei.com/vosk/). Can be used to generate subtitles for
videos, transcription of audio notes, etc.

Usage
-----

```bash
GST_DEBUG=1,vosk_transcriber:5 gst-launch-1.0 filesrc location=/Users/rafaelcaricio/astronaut.mkv ! matroskademux name=d d.audio_0 ! decodebin ! audiorate ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=1 ! vosk_transcriber server-address=ws://192.168.178.20:2700 ! fakesink dump=true --gst-plugin-path=/Users/rafaelcaricio/development/gst-plugin-vosk/target/release/
```