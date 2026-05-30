# Capture spike — how to run it (needs you at the keyboard)

This validates corti's core bet (ADR 0002): tap system output **and** the mic through one CoreAudio
aggregate device, producing a single synchronized multichannel WAV.

## Why the earlier runs got `io proc fired 0 times`

The process-tap calls all return `noErr` even without permission, then deliver no audio. macOS gates this
behind the **audio-capture** TCC permission (`NSAudioCaptureUsageDescription`, shown under **Privacy &
Security → Screen & System Audio Recording**), and — crucially — TCC anchors that grant to a **bundle
identity reached through LaunchServices**. Running the binary directly from a shell (even
`./corti-spike.app/Contents/MacOS/corti-spike`) makes the **terminal** the responsible process, so no prompt
appears and capture is silently denied. (Confirmed against Guilherme Rambo's AudioCap, the canonical
reference.) The fix: launch the signed `.app` with `open`.

## Run it (one command)

```sh
cd ~/code/vasovagal/corti
./crates/corti-coreaudio/bundle-spike.sh 15 /tmp/corti-spike.wav
```

This builds, bundles, signs, and `open`s the app, waits, and prints the diagnostics log.

1. **A permission prompt should appear** — approve it (Screen & System Audio Recording, and Microphone if
   asked). The **first** attempt is usually denied while you approve, so **run the script again** afterward.
2. While it records (15 s): **talk into your mic** and **play some audio** (a YouTube video, music — any
   system output) so both ends have signal.
3. The script prints `<out>.log`, which ends with `io proc fired N times; channels captured: 3` on success.
   (stdout is detached under `open`, so the log file is the source of truth.)

### Repeated runs

Ad-hoc signing gives a new code hash every rebuild, so macOS re-prompts and orphans the previous grant. To
avoid re-approving each time, make a stable self-signed cert once (Keychain Access → Certificate Assistant →
Create a Certificate → *Code Signing*), then:

```sh
CORTI_SIGN_ID="corti-dev" ./crates/corti-coreaudio/bundle-spike.sh 15 /tmp/corti-spike.wav
```

## What "go" looks like

```sh
afinfo /tmp/corti-spike.wav            # channel count + format
open -a Audacity /tmp/corti-spike.wav  # visual per-channel inspection
```

- **Channel layout:** first channel(s) = **mic** (your voice only), later channel(s) = **system tap** (the
  played audio only). Typically mic=1 + tap=2 = 3 channels.
- **Separation:** your voice not in the tap channels; system audio not in the mic channel.
- **Sync:** the two line up in time (no growing drift).

If that holds → architecture is a **go**; we build `corti-capture` on it (then narrow the global tap to one
app via `CATapDescription` per-PID).

### If the log still shows `io proc fired 0 times`

- Look at the `aggregate input format:` line in the log. If it shows channels (e.g. `48000 Hz, 3 ch`), the
  aggregate assembled fine and this is purely the **TCC permission** — make sure you approved it and re-ran,
  and check **System Settings → Privacy & Security → Screen & System Audio Recording** for a `corti-spike`
  entry. If it's there and enabled but still 0 callbacks, paste the full log.
- If the aggregate input format is `0 ch` or unreadable, it's a **construction** problem (subdevice/tap UID
  mismatch), not permission — paste the log and I'll adjust the aggregate dictionary.

If after granting it still won't capture, that's the signal to fall back to ScreenCaptureKit + cpal.
