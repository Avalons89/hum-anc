# hum-anc

Noise masking and hum cancelling for in-ear monitors on Windows.

A mic on your desk measures the steady background noise in the room, and hum-anc plays soft brown, pink or white noise into your IEMs at that level plus an offset you choose. When the room gets louder the masking follows; when it gets quieter the masking drops. Typing and talking near the mic barely move it, because the meter tracks the quietest moments of the last few seconds rather than overall loudness.

A second mode cancels one steady hum, such as a fan, fridge or mains buzz. It locks onto the tone through the mic and plays it back inverted, and you set the phase and gain by ear until the hum fades.

## What it can't do

It can't cancel voices, traffic or other changing noise. Sound takes about 50 ms to get from the mic, through Windows audio and out of the IEMs, and anything that isn't a steady tone has changed by then. Real broadband noise cancelling needs dedicated hardware with microsecond delays. For that kind of noise, foam ear tips work better than any software.

## Features

- Masking that follows the room's background level, with an offset and a max volume you choose
- Brown, pink or white noise, switchable while it plays
- A 30 s graph of background and masking levels
- Hum cancelling with a scan that only reports tones standing 20 dB above the surrounding noise
- Tray icon, and Ctrl+Alt+M to start or stop from inside a full-screen game
- Optional start with Windows, hidden in the tray
- Settings saved between launches in `%APPDATA%\hum-anc`

## Hearing safety

Masking never goes above -30 dBFS RMS, and individual samples are clipped at -20 dBFS. The hum canceller's output is clipped at about -30 dBFS. IEMs sit inside the ear canal, so keep the Windows volume moderate too.

## Requirements

- Windows 10 or 11
- A microphone and an output device running at the same sample rate (48 kHz is typical), in 32-bit float shared mode, which is the Windows default

## Build

```
cargo build --release
```

The app is `target\release\hum-anc.exe`. Pass `--tray` to start hidden in the tray.

## Tests

```
cargo test
```

The unit tests cover tone detection, the hum tracker in a simulated room, the clock bridge between mic and output, the three noise colours, the background meter and the volume cap.

## License

MIT
