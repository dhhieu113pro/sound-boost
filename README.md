# Sound Booster (Rust)

A Windows system-wide volume booster with **automatic loudness leveling**, in the
spirit of Letasoft Sound Booster but built around a different DSP goal: instead
of only making everything louder, it pulls quiet sources up and leaves loud ones
alone, so a quiet video and a loud trailer end up at a similar perceived level.

Implemented as an **Audio Processing Object (APO)** loaded by the Windows audio
engine, so there is no virtual cable, no driver, and no per-app injection.

> Status: the DSP core, the APO, the control channel, the elevated installer
> and the tray app are implemented and tested; the APO's DLL is registered and
> verified as a PE but has **not yet been loaded by the audio engine** (that
> needs an elevated install).

## Why an APO

Windows caps session and endpoint volume at `1.0` (100 %); real boost means
touching the samples. The audio engine already runs effects for the endpoint in
user mode, and `audiodg.exe` loads them from the endpoint's registry effect
chain. That is exactly what the commercial product does — see
[`docs/reference-implementation.md`](docs/reference-implementation.md) for the
registry evidence recovered from an installed copy.

Two placements matter, and we use both:

| Placement | Sees | Used for |
| --- | --- | --- |
| Stream effect (SFX) | one instance per stream, before mixing | auto leveling per app/video |
| Endpoint effect (EFX) | the final mix | master boost + look-ahead limiter |

Leveling the *mix* would let a loud app mask a quiet one, which is the problem
we are trying to solve, so per-stream is where the AGC lives.

## Signal chain

```
input -> [RMS level detector] -> [noise gate] -> [auto leveling AGC]
      -> [manual boost slider] -> [look-ahead brick-wall limiter] -> output
```

* **Noise gate** — below `gate_db` the gain is *frozen* rather than raised, so
  silence and low hiss are never amplified.
* **AGC** — `gain = target - measured`, clamped to `[min_gain_db, max_gain_db]`,
  smoothed in dB with fast attack (protect) and slow release (stay musical), and
  scaled by a user "leveling strength". A stereo pair is detected as one linked
  signal so the image does not wander.
* **Manual boost** — applied after detection, so moving the slider never changes
  how strongly the leveler works. 100 % = 0 dB, 500 % = +13.98 dB.
* **Look-ahead limiter** — the gain for the delayed sample is derived from a
  window that provably contains it, so the output cannot overshoot the ceiling.
  This is what keeps 300–500 % boost from crackling.

Presets: `Off`, `Speech` (fast, for dialogue), `Music` (slow, keeps dynamics),
`Night` (heavy leveling, lower ceiling).

## Layout

| Crate | Role | Status |
| --- | --- | --- |
| `crates/core` | DSP: envelope, gate, AGC, limiter, presets. No allocation on the audio path. | done, 30 tests |
| `crates/ipc` | Named shared-memory control block (seqlock) between tray app and APO. | done, 5 tests |
| `crates/apo` | `cdylib` APO: `DllGetClassObject` + `IAudioProcessingObject*`, one CLSID per placement. | done, 36 tests, DLL verified (`sb-refprobe`) |
| `crates/setup` | Elevated install: CLSID + APO-store registration, `FxProperties` chain append, backup/restore, `DisableProtectedAudioDG`, audio-service restart, `status`/`--dry-run`. | done, 14 tests |
| `crates/refprobe` | Dev tool: asks an installed APO what it reports to the engine, so our registration can be diffed against a known-good one. | done, 2 tests |
| `crates/app` | `sb-tray`: tray icon, flyout panel (on/off, boost slider, preset, leveling strength), right-click menu, start with Windows. `sb-gain`: the same control from a command line. | done, 31 tests; level meters not yet (needs APO telemetry) |

The APO runs inside `audiodg.exe` — which is in **session 0**, while the UI is in
the user's session — so a per-session `Local\` section would silently give each
side its own copy. The control block is therefore
`Global\SoundBoosterControl_v1`, created by the APO — which runs in session 0,
where creating a global mapping needs no privilege — with a DACL that lets the
interactive user open it; the tray app only opens it. See
[`docs/risks.md`](docs/risks.md#6-control-block-across-sessions--the-apo-cannot-use-a-local-section).

## Build and test

```powershell
cargo test --workspace
cargo build --release
```

The APO must be built for `x86_64-pc-windows-msvc` (the default host toolchain).

## Tray app

`sb-tray.exe` sits in the notification area. Left-click opens the panel,
right-click opens a menu with presets, quick boost levels, **Start with
Windows** and **Exit**. Settings are saved per user under
`HKCU\Software\Sound Booster\Tray`.

* The control block only exists once the audio engine has loaded the APO, so
  the tray starts without it, retries once a second, and publishes the saved
  settings as soon as it attaches ("Waiting for the audio engine" until then).
* It follows changes made by `sb-gain` while it runs, so the two can be mixed.
* **Exit leaves the last settings in force** — the APO keeps reading the
  block, exactly as after `sb-gain`. Untick **Enabled** to stop processing.
* Launching it a second time opens the running instance's panel; the autostart
  entry passes `--background` so a login starts it quietly.

## Next steps

1. First real install: `tools\first-install.ps1` from an elevated prompt. It
   removes the reference product, registers the APO and prints the status
   report.
2. Then the measurements listed in [`docs/risks.md`](docs/risks.md): is
   `sb_apo.dll` in `audiodg.exe`'s module list, does `sb-gain 500` change what
   you hear, does the stream slot give one instance per player, and does the
   limiter hold at 500 %.
3. Run `target\release\sb-tray.exe`, play something, and check the panel says
   *Connected to the audio engine* and the slider changes what you hear.
4. Level meters in the tray: the APO would need to publish its current AGC
   gain and limiter reduction into the control block's reserved words.

Open risks are tracked in [`docs/risks.md`](docs/risks.md).
