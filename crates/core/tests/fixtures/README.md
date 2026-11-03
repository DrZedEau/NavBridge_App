# H.264 and H.265 access-unit fixtures

`parser-continuation.h264` is a synthetic 24-frame test stream, generated with:

```sh
ffmpeg -f lavfi -i testsrc2=size=160x96:rate=30 -frames:v 24 \
  -c:v libx264 -preset veryfast -pix_fmt yuv420p \
  -x264-params aud=1:keyint=30:bframes=0 -f h264 parser-continuation.h264
```

`parser-continuation.h265` is the same picture in H.265, generated with:

```sh
ffmpeg -f lavfi -i testsrc2=size=160x96:rate=30 -frames:v 24 \
  -c:v libx265 -preset veryfast -tune zerolatency -pix_fmt yuv420p \
  -x265-params aud=1:keyint=30:bframes=0:log-level=error \
  -f hevc parser-continuation.h265
```

The tests split each at access-unit delimiters and push one access unit at a
time, like Android Auto video messages; the first unit's codec configuration
(SPS/PPS, and VPS for H.265) is pushed on its own, like the codec
configuration message. Every access unit must publish its frame as soon as it
is pushed, so all 24 decode. The earlier FFmpeg-parser pipeline held each frame
until the next unit arrived and decoded only 23. No user screen content is
included in these fixtures.
