# H.264 parser regression fixture

`parser-continuation.h264` is a synthetic 24-frame test stream, generated with:

```sh
ffmpeg -f lavfi -i testsrc2=size=160x96:rate=30 -frames:v 24 \
  -c:v libx264 -preset veryfast -pix_fmt yuv420p \
  -x264-params aud=1:keyint=30:bframes=0 -f h264 parser-continuation.h264
```

Each access unit is supplied separately, like a dongle payload. FFmpeg can emit
its previous buffered frame with zero input consumption. The decoder must then
retry the current bytes, not drop them. Before the fix only 12 frames decoded;
after it 23 decode, with the final frame retained by the parser until EOF.
No user screen content is included in this fixture.
