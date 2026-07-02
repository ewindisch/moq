---
title: moq-srt
description: SRT / MPEG-TS <-> MoQ gateway (ingest and egress)
---

# moq-srt

`moq-srt` bridges [SRT](https://www.haivision.com/products/srt-secure-reliable-transport/)
(the low-latency transport ffmpeg, OBS, VLC, and most contribution hardware speak)
and Media over QUIC, in **both directions**:

- **Publish (ingest):** an encoder pushes a stream in, and `moq-srt` publishes it
  into MoQ as an ordinary broadcast.
- **Request (egress):** a player pulls a broadcast back out, and `moq-srt`
  subscribes to it from MoQ and re-muxes it down as MPEG-TS. VLC and ffmpeg can
  play it (browsers can't).

SRT carries media as an MPEG-TS byte stream. `moq-srt` runs the SRT session (via
the pure-Rust [`srt-tokio`](https://crates.io/crates/srt-tokio), no libsrt or
ffmpeg). On ingest it feeds the transport stream to `moq-mux`'s TS demuxer; on
egress it muxes the broadcast back to TS with `moq-mux` and paces the packets out
on the media clock. It's the sibling of `moq-rtmp` (RTMP/FLV) and `moq-rtc`
(WHIP/WHEP).

## CLI shape

The binary has two modes, mirroring `moq-rtmp`:

```bash
# serve: ingest SRT and serve it directly as a local relay
moq-srt serve --server-bind [::]:443 --tls-generate localhost \
  --srt-listen 0.0.0.0:9000 --srt-prefix live/

# publish: ingest SRT and forward broadcasts to a remote relay
moq-srt publish --relay https://relay.example.com \
  --srt-listen 0.0.0.0:9000 --srt-prefix live/
```

Point any SRT source at it. The stream id selects the broadcast and the
direction:

```bash
# Publish: lands at broadcast `live/cam0`.
ffmpeg -re -i input.mp4 -c copy -f mpegts \
  'srt://127.0.0.1:9000?streamid=#!::r=cam0,m=publish'

# Request: play `live/cam0` back out as MPEG-TS.
ffplay 'srt://127.0.0.1:9000?streamid=#!::r=cam0,m=request'
vlc    'srt://127.0.0.1:9000?streamid=#!::r=cam0,m=request'
```

A request waits for the broadcast to be announced, so a player may connect before
the publisher does.

### `serve` flags

- `--server-bind`: QUIC/WebTransport bind address (default `[::]:443`). Also
  serves the `/certificate.sha256` endpoint browsers need for self-signed
  `http://` origins, and a static player directory with `--dir`.
- `--tls-generate <hostname>` / `--tls-cert` / `--tls-key`: server TLS.

### `publish` flags

- `--relay`: upstream MoQ relay to publish every ingested broadcast into.

### SRT flags

- `--srt-listen`: UDP bind address for the SRT server (SRT has no well-known
  port; 9000 is common).
- `--srt-prefix`: prepended to every broadcast path, to namespace a listener's
  streams (e.g. `live/`).
- `--srt-latency`: SRT receive latency, the handshake-negotiated buffer that
  trades delay for loss recovery (default `200ms`).
- `--srt-egress-buffer`: on the request (egress) path, how long the TS re-muxer
  holds a per-track group before emitting it (default `200ms`). This absorbs
  delivery jitter at group boundaries so a slightly-late next group doesn't
  truncate the current one mid-GoP (which shows up as decode artifacts). `0`
  disables buffering; raise it to trade egress latency for robustness on a lossy
  or bursty path.

## Routing

Each connection's broadcast path and direction come from its SRT stream id:

- Standard form `#!::r=<resource>,m=<mode>` selects `<resource>` as the broadcast,
  with `m=request` selecting egress and anything else (including absent) selecting
  ingest.
- Otherwise the raw stream id (e.g. OBS-style `app/key`), always ingest.

`--srt-prefix` is prepended to namespace a listener's streams, so the URL
round-trips: publish to `r=cam0` with prefix `live/`, then request `r=cam0` to
pull `live/cam0` back. First **publisher** on a path wins (a second publish to a
live path is rejected); **requests** don't claim a path, so any number of players
can pull the same broadcast at once. In `serve` mode requests are served from the
same origin the server exposes, so anything in it -- SRT ingests and otherwise --
can be pulled back out over SRT.

## Notes and limitations

- **Auth.** The binary (and the `moq_srt::run` convenience) is unauthenticated:
  anyone who can reach the UDP port can publish or request any broadcast. Gate it
  with a host firewall or a private network. To authenticate, embed the library
  and drive its `Server` / `Request` API: `Server::accept` yields a `Request` that
  is either a `Publish` or a `Subscribe`, and you verify the resource / stream id
  (e.g. the stream id as a moq-token JWT) before accepting it into / out of an
  origin at a path of your choosing, or rejecting it -- no callback, the policy
  lives in your loop.
- **Embedding.** A relay can run the gateway in-process by depending on the
  `moq-srt` library (`default-features = false`). Call `moq_srt::run` against its
  own origin for the unauthenticated case (publishers ingest into it, requests are
  served out of it), or use `Server` / `Request` to plug in the relay's existing
  JWT/path auth and scope the origin per token. Either way the media stays local
  with no extra hop.
- **Encryption.** SRT passphrase encryption is a separate, planned next step.
- **Codecs.** Whatever the `moq-mux` TS demuxer/muxer supports: H.264/H.265 video
  and AAC/AC-3/Opus/MP2 audio.
