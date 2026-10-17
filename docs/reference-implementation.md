# Reference implementation: what an installed copy actually does

This is the evidence we recovered from an installed Letasoft Sound Booster
1.13.0.578 on this machine, by inspecting live system state, by reading the
strings its own control DLL hands the registry, and by asking the installed APO
what it reports to the audio engine (`cargo run -p sb-refprobe`) — no
disassembly. It is recorded because it is the contract our own installer and
APO have to imitate, and because it is cheaper and more reliable than
decompiling.

Nothing here is copied from that product; our APO is an independent
implementation with a different DSP.

## Install directory

`C:\Program Files (x86)\Letasoft Sound Booster`

| File | Description (from version info) |
| --- | --- |
| `SoundBooster.exe` | the GUI / tray process |
| `Sbapo.dll` | "Sound Booster APO (64-bit)" — the audio engine effect |
| `ApoControl.dll` | "APO Control DLL" — channel between GUI and APO |
| `SBH.dll`, `SBH64.dll` | the code-injection method (their fallback path) |
| `SoundBoosterService.exe` | service, `LocalSystem`, start mode Manual |
| `SoundBoosterHelper.exe`, `SoundBoosterTaskHost.exe` | elevated/task helpers |
| `ApoInstaller.exe` | **registers the APO chain** (the component that writes the `FxProperties` entries below) |
| `Filters\gain.dll`, `Filters\limit.dll` | their gain and limiter DSP filters |
| `TurboActivate.exe/.dll`, `UltraActivate.dll` | activation/licensing (TurboActivate SDK and its build) |
| `VmpMgrReg.dll`, `VmpMgrTrial.dll` | licensing (VMProtect-wrapped) |

That file set is taken from the official installer's own manifest
(`unins000.dat`), and it independently confirms the shape we chose: a **gain**
stage and a **limit** stage, plus a dedicated component whose only job is
writing the endpoint effect chain.

Their documentation states they use **two methods, code injection and APO
effects, both enabled by default**. We implement only the APO path.

## How the APO is registered

1. **COM registration** (this is how the engine instantiates it):

   ```
   HKLM\SOFTWARE\Classes\CLSID\{BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}
     (Default)        = "Sbapo Global Effect"
     InprocServer32
       (Default)      = C:\Program Files (x86)\Letasoft Sound Booster\Sbapo.dll
       ThreadingModel = Both
   ```

   `Sbapo.dll` exports exactly four entry points — `DllGetClassObject`,
   `DllCanUnloadNow`, `DllRegisterServer`, `DllUnregisterServer` — so it is a
   self-registering in-proc server and the installer can just call
   `regsvr32`-style registration instead of poking `HKCR\CLSID` itself.

2. **The engine's APO store.** Having a CLSID registered is not enough: the
   engine loads effects listed under the machine-wide APO store, which the
   vendor's installer writes (the string
   `HKEY_CLASSES_ROOT\AudioEngine\AudioProcessingObjects\` appears in
   `ApoControl.dll`, next to the `FxProperties` paths):

   ```
   HKLM\SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\{BABAE75F-...}
     FriendlyName         = "Sbapo"
     Copyright            = "Copyright (c) Letasoft LLC"
     MajorVersion         = 1
     MinorVersion         = 0
     Flags                = 13
     MinInputConnections  = 1
     MaxInputConnections  = 1
     MinOutputConnections = 1
     MaxOutputConnections = 1
     MaxInstances         = -1
     NumAPOInterfaces     = 1
     APOInterface0        = {FD7F2B29-24D0-4B5C-B177-592C39F9CA10}
   ```

   The store is a mirror of what the DLL itself reports (see "What the DLL
   reports to the engine" below); ours is generated from a single shared
   `registration` module so the two cannot drift.

3. **The process-protection key.** `ApoControl.dll` also reads *and writes*
   one more value, and this is the one that decides whether a registry-only
   APO loads at all:

   ```
   HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio
     DisableProtectedAudioDG = 1
   ```

   Microsoft's own APO documentation says an APO that is not deployed as a
   componentized driver package "will run with `DisableProtectedAudioDG=1`, but
   when this test key is removed, your APO will fail to load, **even if it is
   WHQL-signed**." So the key is not a way around signing for us specifically —
   it is part of how this class of install works, it is **already set to 1** on
   this machine, and code signing would not remove the requirement. Our
   installer records the previous value and restores it on uninstall.

4. **Attaching it to an endpoint's effect chain.** For the default render
   endpoint `{f82d7e2b-7fac-42ec-aee8-3740c297ac49}` ("Speakers", Realtek):

   ```
   HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{endpoint}\FxProperties
     {d04e05a6-594b-4fb6-a80d-01af5eed7d1d},15 = {BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}
   ```

   The effect chain is a set of **values on the existing `FxProperties` key**,
   keyed by the FX property set `{d04e05a6-...}` and an index. The original
   list in that slot was two other CLSIDs; Sound Booster replaced it with its
   own single entry.

   `ApoControl.dll` references exactly these indices, which is the full slot
   contract:

   | Index | Slot (`audioenginebaseapo.idl`) | Live contents on this endpoint |
   | --- | --- | --- |
   | 0 | `PKEY_FX_Association` | `REG_SZ {00000000-...}` placeholder |
   | 1 | `PKEY_FX_PreMixEffectClsid` | absent |
   | 2 | `PKEY_FX_PostMixEffectClsid` | absent |
   | 5 | `PKEY_FX_StreamEffectClsid` (SFX) | absent |
   | 6 | `PKEY_FX_ModeEffectClsid` (MFX) | absent |
   | 7 | `PKEY_FX_EndpointEffectClsid` (EFX) | absent |
   | 13 | `PKEY_FX_CompositeFX_StreamEffectClsid` | `{905069CC-...}` (`CRtkAPOSFX2`) |
   | 14 | `PKEY_FX_CompositeFX_ModeEffectClsid` | `{FCDD3010-...}` (DTS MFX) + `{90609662-...}` (`CRtkAPOMFX2`) |
   | 15 | `PKEY_FX_CompositeFX_EndpointEffectClsid` | `{BABAE75F-...}` (Sound Booster) |
   | 19/20 | offloaded composite SFX/MFX | Realtek / DTS offload APOs |

   Three things follow from that reading:

   * These values are **`REG_MULTI_SZ`, i.e. chains**, not single CLSIDs (index
     14 genuinely holds two APOs, and the mode lists under
     `{d3993a3f-...},5/6/7` hold three or four mode GUIDs each). Sound Booster
     replaced the whole endpoint slot with its single CLSID; a less destructive
     installer **appends** its CLSID to the existing chain so the Realtek and
     DTS effects in 13 and 14 keep working, and removes exactly its own entry
     again on uninstall.
   * This endpoint is driven by the **composite** slots (13/14/15). The classic
     slots (5/6/7) and the pre/post-mix slots (1/2) are empty here, so an
     installer that only writes 5/7 would do nothing on this machine even
     though it works on others. Ours targets whichever slot the endpoint
     actually uses, and falls back to creating the composite one.
   * The offload slots (19/20) are for APOs that run inside a hardware-offloaded
     stream; a user-mode DSP cannot run there, which is presumably why Sound
     Booster leaves them alone. So do we.

   Our mapping is therefore: index 13 (composite SFX) gets the **stream** CLSID,
   index 15 (composite EFX) gets the **endpoint** CLSID — one leveler per
   stream, one boost and limiter per device. The MFX slot stays untouched:
   leveling the post-mix bus would let a loud app mask a quiet one, which is
   the entire point of doing it per stream.

   It also references the processing-mode keys
   `{d3993a3f-99c2-4402-b5ec-a92a0367664b},5/6/7` (SFX/MFX/EFX mode GUIDs) and
   `AUDIO_SIGNALPROCESSINGMODE_DEFAULT` (`{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}`,
   per `ksmedia.h`) to read the chain back for its own UI. We leave those
   lists alone: they say which processing modes the effects apply in, and the
   reference product's behaviour on this machine is to keep them as the driver
   package left them.

5. **The "Disable all enhancements" flag.** `ApoControl.dll` also reads
   `PKEY_AudioEndpoint_Disable_SysFx`
   (`{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5`, confirmed in
   `mmdeviceapi.idl`). If the user has that checkbox ticked, no APO is loaded
   for the endpoint no matter how well we register, so our installer checks it
   and says so instead of silently doing nothing.

6. **Backup before modifying**, so uninstall can restore:

   ```
   HKLM\SOFTWARE\Letasoft\Sound Booster\Sbapo\OldAPOs\{endpoint}
     {d04e05a6-...},1  ... ,15   (original values, GUID_NA placeholders included)
   ```

7. **Registry permissions** on `FxProperties`:

   | Principal | Rights |
   | --- | --- |
   | Owner | `NT AUTHORITY\SYSTEM` |
   | `NT SERVICE\TrustedInstaller` | FullControl |
   | `NT SERVICE\Audiosrv`, `AudioEndpointBuilder` | SetValue, CreateSubKey, Delete |
   | `BUILTIN\Administrators` | **SetValue, ReadKey** only |
   | `BUILTIN\Users` | SetValue, ReadKey |

   Two consequences for our installer:

   * it must run **elevated** (or as `LocalSystem`), which explains why the
     vendor ships a service;
   * it may only **set values**, never create or delete subkeys in that key —
     which is consistent with the effect list being stored as values.

8. **User settings** live in `HKCU\SOFTWARE\Letasoft\Sound Booster\Options`
   (`SoundLevel=0x12c` = 300 %, `BoostMethod`, `BoostIsEnabled`).

## What the DLL reports to the engine

The loader is undocumented enough that guessing is expensive, so we ask the
installed product directly. `cargo run -p sb-refprobe` loads a DLL the way
`audiodg.exe` would, calls `DllGetClassObject` → `CreateInstance` →
`GetRegistrationProperties`, and prints the answer together with its
`QueryInterface` behaviour:

```
GetRegistrationProperties -> 0x00000000
  clsid             : {BABAE75F-A7AA-4A13-B1F7-BB2716FD003E}
  flags             : 0x0D (INPLACE | FRAMESPERSECOND_MUST_MATCH | BITSPERSAMPLE_MUST_MATCH)
  friendly name     : "Sbapo"
  copyright         : "Copyright (c) Letasoft LLC"
  version           : 1.0
  connections       : in 1..1 out 1..1
  max instances     : -1
  interfaces        : 1
    [0] {FD7F2B29-24D0-4B5C-B177-592C39F9CA10} = IAudioProcessingObject

QueryInterface:
  IUnknown                             0x00000000
  IAudioProcessingObject               0x00000000
  IAudioProcessingObjectConfiguration  0x00000000
  IAudioProcessingObjectRT             0x00000000
  IAudioSystemEffects                  0x00000000
  IAudioSystemEffects2                 0x80004002
  IAudioMediaType                      0x80004002
```

Three things worth taking from that:

* `flags = 0x0D` is `APO_FLAG_INPLACE | FRAMESPERSECOND_MUST_MATCH |
  BITSPERSAMPLE_MUST_MATCH`. It omits `APO_FLAG_SAMPLESPERFRAME_MUST_MATCH`,
  which is part of the SDK's `APO_FLAG_DEFAULT` — our registration does the
  same, since the chain handles any channel count and the constraint would only
  narrow where the engine may place us.
* It advertises **one** interface, `IAudioProcessingObject`. The engine reaches
  the configuration and real-time interfaces by IID anyway, and the store's
  `NumAPOInterfaces`/`APOInterface0` mirror that single entry.
* It does **not** implement `IAudioSystemEffects2` (only the marker
  `IAudioSystemEffects`), so `GetEffectsList` is not part of the minimum
  contract. Ours implements both anyway, which costs nothing and lets effects
  discovery see the effect.

Our own DLL answers identically for the fields that matter (`flags 0x0D`,
one in / one out, `max instances -1`, one interface), plus a working
`GetEffectsList`:

```console
$ cargo run -p sb-refprobe -- target/release/sb_apo.dll "{A7C3E1D9-4B62-4F38-9E57-1C8D2A6B0F42}"
  flags             : 0x0D (INPLACE | FRAMESPERSECOND_MUST_MATCH | BITSPERSAMPLE_MUST_MATCH)
  version           : 1.0
  connections       : in 1..1 out 1..1
  interfaces        : 1
GetEffectsList -> 0x00000000, 1 effect(s)
```


## Notes and caveats

* The app on this machine was **patched by a leftover crack file** (since
  removed — see the end of this bullet), even though the installer itself is
  genuine (downloaded from `files.letasoft.com`, Authenticode `Valid`,
  `CN=Letasoft LLC`, v1.13.0.578). Everything installed on 2026-10-16 is signed
  by Letasoft; exactly one file was not:

  | | |
  | --- | --- |
  | file | `version.dll`, 2,560 bytes, unsigned, no version info |
  | created | 2023-08-27 12:05 |
  | NTFS origin | `ZoneId=3`, `ReferrerUrl=C:\Users\dhhie\Downloads\LetasoftSoundBooster.1.11.0.514.zip` |
  | strings | `pawel97`, `cracked`, `UltraActivate.dll`, `IsActivated`, `GetFeatureValue`, `CreateThread`, `WriteProcessMemory`, `LoadLibraryA` |

  It is a DLL-hijack licensing patch: `SoundBooster.exe` imports `VERSION.dll`,
  `version.dll` is **not** a KnownDLL on this machine, so this copy is loaded
  ahead of `System32\version.dll` and patches the activation exports while
  forwarding the real version APIs. It arrived in an August 2023 repack (`.zip`
  of an older 1.11 build, not a Letasoft download) and survived the later
  official installs because Inno Setup does not delete foreign files — the
  official uninstall log has **zero** references to it. It has since been
  deleted (SHA-256 before removal
  `0CF1C4CE407BE1CD00CD279C266BA1AD64999C8303F755F18A3600FA337645C1`; the
  directory went from 24 files to 23 and `SoundBooster.exe` is still
  signature-Valid).

  Note the correction: `UltraActivate.dll` is a *genuine* Letasoft file
  (their activation component). The shim patches it; it is not part of the
  crack. What the crack does not change is the registry contract below, which
  is what this document is for.
* The registry contract is safe to rely on, but tuning choices observed at
  runtime (e.g. the `BoostMethod` flag) come from a patched build and are not
  evidence about the retail product's defaults.
* `Sbapo.dll` shows no VMProtect markers in a string scan, but it is a 2 MB
  statically linked C++ binary, so static analysis would mostly confirm the
  registry contract we already have. That is why it was not prioritised.
* All four of their engine-facing binaries (`Sbapo.dll`, `Filters\gain.dll`,
  `Filters\limit.dll`) are Authenticode-signed by `CN=Letasoft LLC` with an
  ordinary code-signing certificate — **not** WHQL/attestation. That is
  consistent with the `DisableProtectedAudioDG` finding above: the signature is
  not what gets their APO into `audiodg.exe`. It also means we can ship
  unsigned for local use, and that signing our DLL would be a trust nicety
  rather than a loading requirement.
* The APO store key is the machine-wide `HKCR\AudioEngine\AudioProcessingObjects`
  (it is populated with the Realtek and DTS APOs from this machine's audio
  driver packages), not an `Audio\APOs` key under `CurrentVersion\Audio`. The
  latter does not exist on this machine, which is why earlier notes saying the
  store was missing were misleading.
* We cannot coexist with it: both implementations want the same
  `{d04e05a6-...},15` slot. Our installer will back up and restore the slot, so
  installing and uninstalling ours is safe, but running both at once is not
  meaningful.
