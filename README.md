# Lossless Link

Lossless, low-latency audio from Windows to AirPlay speakers. Tested with the KEF LS LUXE.

Windows can't send audio to AirPlay speakers on its own. The usual workarounds are a poor fit for video:

- **AirPlay senders for Windows** buffer about 2 seconds. Apple devices hide that delay by holding back the picture, but Windows can't, so the sound trails the picture.
- **Bluetooth** keeps up with the picture, but it re-compresses the audio (SBC, AAC or aptX).

Lossless Link streams everything Windows plays as **uncompressed ALAC (16-bit/44.1 kHz)**. The latency is one you choose, 100 ms by default, so video stays in sync. The default works over ordinary Wi-Fi.

## Features

- **Bit-exact for CD-quality sources.** ALAC frames carry plain PCM. Samples that are already 16-bit pass through untouched, and anything finer is dithered once (TPDF) to 16 bit.
- **Clean capture path.** On start it turns off driver audio effects (Realtek, DTS and the like) on the capture output and sets that output to 44.1 kHz, so nothing alters or resamples the sound before it's captured.
- **Adjustable latency from 60 to 300 ms.** The KEF LS LUXE plays streams with as little as 80 ms of latency.
- **Volume keys control the speaker.** The Windows volume keys set the speaker's own volume, and the stream itself always goes out at full level. On KEF speakers, volume changes made with the remote also move the Windows slider.
- **Finds speakers automatically.** It discovers AirPlay speakers on the network with mDNS and follows a speaker whose IP address changes.
- **KEF integration.** It connects when you switch the speaker on and never wakes a speaker in standby. When Spotify, Tidal, Qobuz or Bluetooth takes over, it steps aside and comes back once the other source has gone quiet.
- **Recovers on its own.** It reconnects after sleep, Wi-Fi drops and speaker restarts. Audio threads run under the Windows multimedia scheduler (MMCSS), so CPU load elsewhere doesn't cause dropouts.
- **Updates itself.** A minute after start and every six hours it looks for a new release. It downloads it, checks it against the SHA-256 published with the release, and installs it once the audio has been quiet for a minute, so an update never cuts into a film. Turn this off with **Automatic updates** in the menu.
- **Small.** It's a single executable of about 4 MB with no runtime to install. It lives in the notification area and uses about 1% of one CPU core.
- **English and Turkish.** The language follows the Windows display language.

## Requirements

- **Windows 10 or 11 (x64).**
- **An output with nothing connected to it.** Windows plays into this output and Lossless Link records it. A motherboard's digital output (S/PDIF, "Realtek Digital Output") works, as does an HDMI output to a screen without speakers or a virtual audio cable. Lossless Link picks a digital output automatically when there is one.
- **An AirPlay speaker that accepts unencrypted AirPlay 1 (RAOP) streams.** Speakers built on Apple's AirPlay SDK usually do. Tested with a KEF LS LUXE on firmware 1.0.45. Apple TV and HomePod need pairing and aren't supported.

## Installation

1. Download `LosslessLink.exe` from [Releases](../../releases) and put it somewhere permanent, such as `%LOCALAPPDATA%\Programs\Lossless Link`.
2. Run it. When Windows asks whether to allow network access, allow it. The speaker sends timing messages to the PC from a port that changes every session, so they can only get in through a firewall rule. If the prompt never appeared, use **Allow through Windows Firewall…** in the menu.
3. If it found more than one speaker, pick yours under **Speaker**. To have it start at logon, tick **Start with Windows**.

Starting the program a second time stops the running copy, so one shortcut works as an on/off switch.

## The tray icon

| Colour | Meaning |
|---|---|
| Green | Streaming to the speaker |
| Blue | The speaker is playing another source; Lossless Link connects again when it stops |
| Grey | The speaker is off (standby) or can't be found |
| Orange | Starting, or a problem the tooltip explains |

If the sound trails the picture, lower **Latency**. If it drops out, raise it.

## How it works

1. **Capture.** WASAPI loopback records the chosen output in the Windows mix format. A silent stream into the same output keeps loopback delivering data when nothing else plays.
2. **Volume.** Loopback is recorded after the Windows volume is applied, so that gain is divided out again. The Windows volume is sent to the speaker as its AirPlay volume.
3. **Format.** The capture output runs at 44.1 kHz, so audio passes through without resampling. If that output can't be set to 44.1 kHz, an FFT resampler ([rubato](https://crates.io/crates/rubato)) converts it. Samples that aren't exact 16-bit values are TPDF-dithered to 16 bit. A small buffer absorbs capture jitter, and single frames are dropped or repeated to follow the few-ppm drift between the capture clock and the send clock.
4. **Streaming.** Every 352 frames go out as one RTP packet holding an uncompressed ALAC frame. The RAOP session is `OPTIONS`, `POST /auth-setup` (unencrypted), `ANNOUNCE`, `SETUP` and `RECORD`, followed by NTP-style timing replies, sync packets that set the latency, and retransmission on request.
5. **KEF integration.** On KEF speakers the program reads the speaker's local HTTP API, read-only, to see whether it is on, which source it uses and its volume.

## Building

```
cargo build --release
```

The executable ends up at `target\release\lossless-link.exe`. To run the tests, use `cargo test`.

## Limitations

- **CD quality is the ceiling.** AirPlay's real-time formats stop at 16-bit/44.1 kHz. For hi-res music, use the speaker's own Tidal or Qobuz Connect.
- **One speaker at a time.**
- **Receivers that require encryption or pairing aren't supported.**

## Trademarks

AirPlay is a trademark of Apple Inc. KEF is a trademark of GP Acoustics. This project is not affiliated with or endorsed by either company.

## License

[MIT](LICENSE)
