# Go Live: the share as a Discord stream

A screen or window share can also be announced as a normal Discord stream ("Go Live") in
the voice channel, so people on the official Discord apps can watch it. The share between
Litecord clients and the browser link keep working next to it, on the same encoded
pictures. Watching other people's Discord streams from Litecord is not built yet.

**Status: sending works against Discord.** The first live run (2026-11-15) went through
on the first try with up to five people on official clients in the stream's DAVE group.
What that run and the three after it showed is under "What the live runs showed"; what
was changed for it has been checked offline only and waits for the next live run.

## Using it

- Share dialog: **Also stream on Discord (Go Live)**. Saved, off by default, not offered
  for cameras.
- While sharing: the broadcast button right of the globe starts or stops the Discord
  stream of the running share. Grey: off. Faded: connecting. Accent colour: live on
  Discord (the tooltip counts the people watching there).
- You must be in a voice channel or call. Leaving it, or ending the share, ends the stream.
- A Discord stream that fails or is ended by Discord never stops the share: a banner says
  why, and the direct stream goes on.

## What reaches Discord, and what does not

- Video and, when the share has audio, the shared audio go out end-to-end encrypted
  (DAVE) to the people watching. **Nothing is sent, and nothing is encoded for Discord,
  while nobody watches.** With DAVE the stream's group then has no other member and so no
  key. Without DAVE the lists of who is connected decide (the stream server's and the
  main gateway's).
- If the server negotiates no end-to-end encryption (`dave_protocol_version: 0`, which
  Discord documents for stage channels only) or announces a downgrade, frames go out with
  transport encryption only. The window says so (a banner once, and the button's tooltip
  for as long as it lasts), and so does the log. `LITECORD_GOLIVE_REQUIRE_DAVE=1` refuses
  to send instead. A downgrade message without a protocol version is ignored.
- No preview thumbnail is uploaded (previews are not end-to-end encrypted) and no "is
  live" notification is requested. Official viewers see the placeholder until they watch.
- While somebody watches on Discord the one encoder runs Constrained Baseline for
  everybody, at no more than what Discord's media server can take (see "Rate").
- This is a third-party client acting for a user account. Traffic is kept modest: one
  stream request per start. A stream gateway connection that fails, or is closed again
  within 30 s of coming up, counts as a failed attempt; the waits between attempts are 1,
  2, 4, 8 and 15 s, and the fifth failure within two minutes ends the stream with a
  banner. A stream deletion the main gateway could not take is tried again every second
  for a minute.

## What the live runs showed

All on 2026-11-15, from this machine to friends on official clients.

**Confirmed, and not to be changed:** stream gateway version 8; `streams: [{type:
"screen", rid: "100", quality: 100}]` in Identify; the MLS group id `rtc_server_id - 1`
(the gateway's proposals name it); our commits are the ones announced (opcode 29), and a
welcome (opcode 30) works too; payload types 101/102 and Opus 120; the full header
extension (and the minimal one, run 3); `type: "video"` in opcode 12; 8 Mbit/s announced;
stream audio without a header extension; `STREAM_*` events reach this gateway identity;
the voice session id is the main gateway's. The media server sends receiver reports (loss
0.0 %, round trip 16 to 18 ms), PLI, REMB (never above 9.0 Mbit/s) and NACK, also for the
audio SSRC. Its Ready names the experiment `fixed_keyframe_interval`, which this client
does not declare.

**The problem:** viewers saw the stream stutter and hang, at 8 Mbit/s and just the same
at 0.6 to 2.6 Mbit/s with no PLI, no NACK and no loss (run 2), and with the minimal
header extension, which has no playout-delay element (run 3). The packet log of run 4
shows why:

- **The capture's rhythm.** 15 303 pictures in 255 s, exactly 60 a second on average, but
  7, 14, 21 or 28 ms apart (multiples of the shared 144 Hz monitor's refresh): one in
  seven only half a frame interval after the one before. Timestamps and send times agreed
  to under a millisecond, so the rhythm reached viewers whichever of the two their client
  goes by. On a 60 Hz screen that shows pictures by their timestamps, 5.2 % of the
  refreshes repeat the picture before (each a short hang) and as many pictures are never
  shown.
- **A keyframe about every 2 s** (the share's own safety net), 135 to 205 kB at the
  quality floor of that run, sent within 7 to 13 ms.
- **The estimate of Discord's media server (REMB)** went as low as 1.9 Mbit/s while 3
  were being sent without loss, and was not used.

What was changed in the Discord path, all with a switch:

| What | Now | Before |
|---|---|---|
| Timestamps | one frame interval apart while the source keeps the share's pace but not its rhythm (`rate::EvenClock`); any other source keeps its own | the instant each picture was offered |
| Sending | on that grid, each picture 20 ms after its place there, so arrivals are even too | as they came |
| Playout delay told to receivers | 50 ms to 10 s: the receiver's jitter buffer smooths | 0 to 0: show as it arrives |
| Periodic keyframe while Discord watches | every 60 s | every 2 s |
| Keyframes for viewers (PLI, FIR, a join) | at most one a second for all together; a request inside that second is kept | each one, up to one per 300 ms |
| Packets of a picture | at 2.5 times the allowed rate, never queued longer than 150 ms | 48 at once, then 12 a millisecond |
| Rate | on the wire at most what was announced, and nine tenths of REMB | whatever 8 Mbit/s asked of the encoder became |
| NACK for an audio packet | the packet again, as it was | nothing (audio has no RTX stream) |

Measured on the pictures of run 4 (`golive/tests/rate.rs` holds the first 900 of them),
for a 60 Hz screen: with the timestamps as sent, 787 of 15 303 pictures are never shown
and 792 refreshes repeat one; on the even grid, none and 4. How evenly the scene itself
advances per refresh (16.7 ms would be perfect): standard deviation 7.1 ms as sent, about
5.5 ms on the grid.

**Two things are needed, and only one is here.** This worktree makes every picture reach
a viewer's screen once, at an even pace. *When* each picture is taken is the capture's
doing (the gate in `linux_gpu_tx.rs`), fixed on the branch `capture-cadence` (commit
a377729, not in this worktree): it passes one picture per whole number of refreshes, so a
144 Hz monitor gives a steady 48 pictures a second, a 165 Hz one 55, 60 and 120 Hz ones 60.
- Without that fix (this worktree's own build): the grid takes the capture's rhythm out
  of the timestamps and the sending; what moves in the picture still advances unevenly.
- With it: a steady 48 or 55 a second is not the share's pace, so the grid stays out of it
  and the pictures keep their own, even timestamps (a grid of 60 a second laid over 48
  would show three pictures 16.7 ms apart and the fourth 33 ms after). The 5 s line says
  which: `(on the even grid)` or `(not the share's pace: stamped as they come)`.

What official senders do, as far as public sources say:
- Keyframes: Discord's engineering blog aims at "one key frame per minute", says key
  frames are "typically 6 to 10 times the data size of a delta frame", and that each
  frame should be "no more than one-sixtieth of the average bitrate" at 60 FPS.
- Rate: the community documentation's example of opcode 12 from an official client
  announces 9 Mbit/s for a 60 FPS stream at source resolution; the media server's own
  estimate stopped at 9.0 Mbit/s in every run here. This client announces 8.
- The playout-delay element: the community documentation says official video packets
  carry it; no source says with which values. Unknown.

What libwebrtc (which official receivers are built on) does with that element, from its
source: min 0 and max 0 means "render as soon as possible", no smoothing at all (its
documentation names remote desktops and cloud gaming without sound); min 0 with a max up
to 500 ms is the same with decoding spaced 8 ms apart; anything else, and no element,
leaves the receiver's jitter buffer and lip sync in charge, within the limits. From
release M136 on (2025) a maximum below what lip sync wants wins over lip sync; before, the
two were in conflict ("undefined behaviour" in its own log line). Firefox has an open bug
about a 0/0 element freezing video. The other constant elements only feed
statistics and logs there: transmission offset (a periodic log line), video timing with
flags 0 (histograms), content type (statistics).

## Rate

- Opcode 12 announces the share's maximum, at most 8 Mbit/s (`LITECORD_GOLIVE_MAX_KBPS`
  for less). That is the ceiling for everything put on the wire: RTP headers, the header
  extension, the authentication tag, retransmissions, audio, reports and keyframes.
- The media server's REMB estimate holds the budget down once it has stayed below it for
  a second, with a tenth of headroom; the budget goes back up by a tenth a second. An
  estimate older than 10 s no longer counts. Never below 0.5 Mbit/s.
- The share's send loop reads `golive::rate_cap()` as a cap on what it asks of its
  encoder while somebody watches on Discord: 88 % of the video's part of the budget,
  less when more went out than was allowed over two windows of four seconds.
- That cap moves in steps, because the fork's GPU encoders start a new sequence (a
  keyframe) with every new bitrate: down at once for 6 % or more (with a keyframe
  request, so the encoder takes it now), up after 8 s for 15 % or more or to the top.
- A keyframe's size is the encoder's affair: its buffer is already the smallest
  `vah264enc` accepts (half a second of the bitrate), so the Discord path spaces
  keyframes out and paces their packets, it cannot shrink them.

## The 5 s line

```
5 s: 300 frames in, 300 sent, 0 dropped, 0 keyframes, 2940 packets, 7.5 Mbit/s on the wire
of 8.0 allowed (encoder at most 7.0), audio 250 packets, RTX 0, PLI 0 FIR 0 NACK 0, RR
loss 0.0 %, rtt 17 ms, REMB 9.0 Mbit/s (5 reports), frames offered 6.9 to 27.8 ms apart
(on the even grid) and sent 14.2 to 19.1 ms apart, last packet up to 36.0 ms after its
frame was offered, 0 send errors, 2 watching, DAVE epoch 2 (3 in the group)
```

`encoder at most` is the ceiling the Discord path gives the share's encoder. The share's
own rate (its maximum bitrate, the stream controls, loss its other viewers report) may
be lower: then `on the wire` is well under `allowed`, and that is as it should be.

Against the first live runs: `keyframes` is 0 most of the time and 1 when a viewer joined
or asked (it was 2 to 3 always); `on the wire` stays under `allowed` (it was 8.3 to 9.4
of 8.0); on the even grid `sent ... apart` stays near 16.7 ms whatever `offered ... apart`
says, and a source at another steady pace shows that pace in both; after `NACK` a note in
brackets says what was asked for and what was sent again.

The grid is taken up once the source's pace is known (24 pictures) and the hold comes in
over the 40 pictures after, so the first line of a stream on the grid shows a wide `sent
... apart` once. On the grid every picture leaves about 20 ms later than it could (the
hold); `LITECORD_GOLIVE_HOLD=0` takes that away, and the even arrivals with it.

## How it works

`src/screen_capture/golive.rs` is the public face; `golive/` holds the parts:

| File | What |
|---|---|
| `signal.rs` | main gateway: opcode 18 create, 19 delete, 22 unpause; `STREAM_*` events |
| `wire.rs` | the stream's own voice gateway connection, protocol version 8 |
| `session.rs` | the worker thread: state machine, websocket, media loop |
| `rate.rs` | the wire budget and REMB, the pacer, the keyframe gate, even timestamps, the audio clock of sender reports |
| `dave.rs` | DAVE for the stream (davey 0.2.0; voice stays on davey 0.1.4) |
| `h264.rs` | NAL split as libwebrtc does it, access-unit preparation, SPS normaliser |
| `rtp.rs`, `depacketize.rs` | RTP, header extension, FU-A, RTX, history; the way back |
| `rtcp.rs` | sender reports; receiver reports, NACK, PLI, FIR, REMB |
| `crypto.rs` | `aead_aes256_gcm_rtpsize` and `aead_xchacha20_poly1305_rtpsize` |
| `udp.rs` | the media socket and IP discovery |

Per access unit: private copy, drop delimiters and filler, make the SPS one libwebrtc's
receiver will not rewrite (a rewritten SPS no longer authenticates under DAVE, so every
keyframe would fail), DAVE-encrypt the whole frame, split at start codes, RTP (single NAL
unit packets and FU-A), header extension, transport encryption, held for its place on the
grid, paced UDP.

The send loop (`manager.rs`) has seven hooks: the share ended, the capture has a picture,
the loss Discord reports, whether Discord watches (profile), the rate cap, the periodic
keyframe's interval, and the access unit itself.

Speaking (opcode 5) with no flag is sent after Session Description and the first packet
waits 250 ms for the server to have read it; Speaking with the soundshare flag is sent
once per session when the stream starts sending, with or without sound.

Every log line starts with `[GO LIVE]`. Tokens and keys are never logged; the voice
session id is logged as its first eight characters.

## Switches for a live session

Read once per start and logged in the "Stream armed" line. The first value is the default.

| Variable | Values |
|---|---|
| `LITECORD_GOLIVE_TS` | `even`, `clock`: timestamps one frame interval apart while the source keeps the share's pace, or always the instant each picture was offered |
| `LITECORD_GOLIVE_HOLD` | `20`, `0` to `100`: milliseconds after its place on the even grid that a picture leaves; `0` sends as they come |
| `LITECORD_GOLIVE_PLAYOUT` | `5,1000`, `none`, `0`, `<min>,<max>` in units of 10 ms: the playout-delay element |
| `LITECORD_GOLIVE_KEYINT` | `60`, another number of seconds, `0` (none), `share` (every 2 s), `server` (what the gateway names) |
| `LITECORD_GOLIVE_REMB` | `1`, `0`: follow the media server's estimate, or only show it |
| `LITECORD_GOLIVE_PACE` | `2.5`, `1` to `20`, `burst`: how many times the allowed rate packets leave at |
| `LITECORD_GOLIVE_MAX_KBPS` | unset (the share's maximum, at most 8000), `500` to `8000` |
| `LITECORD_GOLIVE_EXT_OMIT` | `none`, or a list of `toffset`, `orientation`, `content`, `timing`: constant elements left out |
| `LITECORD_GOLIVE_EXT` | `full`, `min`, `none`: the RTP header extension on video (both `full` and `min` shown by official clients) |
| `LITECORD_GOLIVE_GW` | `8`, `9`: stream gateway version (9 adds `channel_id` to Identify and Resume) |
| `LITECORD_GOLIVE_PT` | `101`, `105`: H.264 payload type (RTX is the next number) |
| `LITECORD_GOLIVE_RTX` | `1`, `0`: declare and answer retransmissions |
| `LITECORD_GOLIVE_STREAM_TYPE` | `video`, `screen`: `streams[].type` in opcode 12 |
| `LITECORD_GOLIVE_REGION` | `null`, `omit`: `preferred_region` in opcode 18 |
| `LITECORD_GOLIVE_GROUP` | `auto`, `channel`, `server`, or a number: the MLS group id tried first |
| `LITECORD_GOLIVE_DAVE_CHECK` | `1`, `0`: only let announced users into the DAVE group |
| `LITECORD_GOLIVE_REQUIRE_DAVE` | `0`, `1`: send nothing without end-to-end encryption |
| `LITECORD_GOLIVE_AUDIO_EXT` | `none`, `full`: the header extension native clients put on audio packets |
| `LITECORD_GOLIVE_IDLE_SR` | `0`, `1`: a sender report with every keepalive packet while nobody watches |
| `LITECORD_GOLIVE_DUMP` | unset, or a directory for the prepared access units and a log of packets and pictures |

### If viewers still see stutter

One switch at a time, in this order. After each, look at the 5 s line and ask a viewer.

1. No switch. The 5 s line must show 0 or 1 keyframes, and either `(on the even grid)`
   with `sent ... apart` within a few milliseconds of 16.7, or `(not the share's pace:
   stamped as they come)` with `offered` and `sent` both within a few milliseconds of one
   value (20.8 ms for a 144 Hz monitor with the capture fix). `offered 7 to 28 ms apart`
   means the build has no capture fix: motion is then still a little uneven.
2. `LITECORD_GOLIVE_HOLD=40`, when on the grid `sent ... apart` is wider than about 13 to
   20 ms.
3. `LITECORD_GOLIVE_PLAYOUT=20,1000`: a floor of 200 ms. Better means the receivers go by
   the element and needed more room.
4. `LITECORD_GOLIVE_KEYINT=0`: no periodic keyframe at all.
5. `LITECORD_GOLIVE_REMB=0`, when `allowed` keeps falling although `RR loss` stays 0.0 %.
6. `LITECORD_GOLIVE_PACE=burst`: the old pacing.
7. `LITECORD_GOLIVE_PLAYOUT=none`, then `LITECORD_GOLIVE_EXT_OMIT=timing,toffset,content`.

To see the old behaviour again, for comparison: `LITECORD_GOLIVE_TS=clock
LITECORD_GOLIVE_PLAYOUT=0 LITECORD_GOLIVE_KEYINT=share LITECORD_GOLIVE_PACE=burst
LITECORD_GOLIVE_REMB=0`.

A viewer on the official desktop client can read what their receiver does: Voice
Connected, Debug, the stream's entry: "Frame Rate (Render)", "Freeze Count", "Minimum
Playout Delay", "Jitter Buffer", "Target Delay".

## Checking it offline

No test talks to Discord, opens a capture, the GPU or an audio device. Sockets are on
127.0.0.1 only.

```
nice -n 15 cargo test --release --lib -- golive
LITECORD_GOLIVE_REQUIRE=1 \
LITECORD_GOLIVE_SAMPLES=<directories of .h264 files, separated by ':'> \
LITECORD_GOLIVE_WORK=<a directory on disk for the files written> \
  nice -n 15 cargo test --release --lib -- golive
```

The tests cover the packet layouts against known answers, the packetizer against
GStreamer's depacketizer and ffmpeg's software decoder, DAVE against viewers on two
versions of the MLS library, the rate and timing rules on a made-up clock and on the
pictures of the fourth live run, and the whole worker against a scripted main gateway,
stream gateway and media server (`golive/tests/`), which sends REMB, PLI, FIR and NACK
and times what arrives.

## Sources

Read to understand the protocol; no code is taken from GPL, AGPL or unlicensed projects.

- Discord developer docs, voice connections (CC BY-SA 4.0): https://docs.discord.com/developers/topics/voice-connections
- DAVE protocol (CC BY-NC-SA 4.0) and libdave (MIT): https://daveprotocol.com/ , https://github.com/discord/libdave
- Discord Userdoccers (CC BY-SA 4.0): https://docs.discord.food/topics/voice-connections
- Discord engineering blog, on keyframes and frame sizes of Go Live: https://discord.com/blog/from-blocky-to-brilliant-improving-video-quality-on-discord-go-live-on-amd-gpus
- Discord-video-stream (ISC), also its issue 201 (freezes from bursts of large keyframes, and a viewer's receive statistics): https://github.com/Discord-RE/Discord-video-stream
- discord.py-self and discord-native-voice (MIT): https://github.com/dolfies/discord.py-self
- utsuru (MIT): https://github.com/VincentVerdynanta/utsuru
- davey (MIT): https://github.com/Snazzah/davey
- libwebrtc (BSD-3): `common_video/h264/sps_vui_rewriter.cc`, `modules/rtp_rtcp/source/video_rtp_depacketizer_h264.cc`, `docs/native-code/rtp-hdrext/playout-delay/README.md`, `modules/video_coding/timing/timing.cc`, `video/frame_decode_timing.cc`, `video/video_receive_stream2.cc`, `video/stream_synchronization.cc`, `video/rtp_video_stream_receiver2.cc`, `modules/video_coding/generic_decoder.cc`, `modules/rtp_rtcp/source/rtp_sender_video.cc`
- Mozilla bug 1771101, a playout-delay element of 0 freezing video: https://bugzilla.mozilla.org/show_bug.cgi?id=1771101
