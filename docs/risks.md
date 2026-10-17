# Open risks and things to verify by experiment

These are the parts of an APO-based booster that are not settled by code alone.
Each one needs a measurement on this machine before we call it done. Items that
were settled by reading the SDK and this machine's registry say so and name the
evidence.

## 1. Loading an APO that is not a driver package

**Settled enough to build on.** The gate is not code signing. Microsoft's APO
documentation says an APO that is not deployed as a componentized driver
package runs with `DisableProtectedAudioDG=1`, and that without that key it
fails to load *even if it is WHQL-signed*. Independent confirmation from the
reference product: `ApoControl.dll` (Letasoft's own control DLL) contains the
string `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio` next to
`DisableProtectedAudioDG`, and its APO DLLs are signed with an ordinary
`CN=Letasoft LLC` certificate rather than WHQL. So the signature is not what
gets it in, and the key is not a workaround we invented.

On this machine the key is **already `1`**, so nothing had to change. Our
installer records the original value and restores it on uninstall.

**Still to verify:** that `audiodg.exe` really loads `sb_apo.dll` once we
register it. `sb-setup status` reports whether the DLL is in `audiodg.exe`'s
module list, which is the measurement.

## 2. Which `FxProperties` index is which slot

**Settled.** `audioenginebaseapo.idl` names them, and the live values on this
machine agree:

| Index | Slot | On this endpoint |
| --- | --- | --- |
| 13 | `PKEY_CompositeFX_StreamEffectClsid` | `{905069CC-...}` (`CRtkAPOSFX2`) |
| 14 | `PKEY_CompositeFX_ModeEffectClsid` | DTS + `CRtkAPOMFX2` |
| 15 | `PKEY_CompositeFX_EndpointEffectClsid` | `{BABAE75F-...}` (Sound Booster) |
| 19/20 | offloaded composite SFX/MFX | Realtek / DTS offload APOs |

The values are `REG_MULTI_SZ` chains, so we **append** to them and remove only
our own entry on uninstall. The classic slots (5/6/7) and the pre/post-mix
slots (1/2) are empty on this endpoint, so the installer targets whichever slot
the endpoint actually uses.

**Still to verify:** that one APO instance appears per playing stream from the
stream slot (the whole reason the leveler goes there rather than on the mix).
A counter in the APO, or `sb-setup status` plus two players, answers it.

## 3. When does the chain reload?

Registry changes to the effect chain usually need the endpoint rebuilt:
restart the audio service, disable/enable the device, or replug. Our installer
restarts `Audiosrv`, which tears down its child `audiodg.exe`.

**Still to verify:** that this is enough on this machine, and how long it takes
to come back.

## 4. Does the stream slot raise the risk of clipping the mix?

Per-stream gain is applied before mixing, so several loud streams can sum past
0 dBFS even though each one is limited. That is why the endpoint effect keeps
the final look-ahead limiter.

**Still to verify:** play several loud streams at 500 % and confirm the endpoint
limiter is doing the work.

## 5. Latency and exclusivity

* The limiter adds `lookahead_ms` of latency (5 ms by default). Fine for
  playback; worth knowing for anything interactive.
* Effects are bypassed for *raw* streams (WASAPI exclusive mode, some pro-audio
  paths). This is the same limitation every APO has.

## 6. Control block across sessions — **the APO cannot use a `Local\` section**

Measured on this machine: `audiodg.exe` runs in **session 0**, while the
interactive user is in session 16. A `Local\` section is per-session, so the
tray app in the user's session and the APO in session 0 would each get their
own private copy and the slider would do nothing. The shared block must be
`Global\SoundBoosterControl_v1`.

Creating a `Global\` object normally needs `SeCreateGlobalPrivilege`, which the
interactive user does not have — but the rule is narrower than that. Microsoft's
"Kernel object namespaces" page: *"The creation of a file-mapping object or
symbolic link object in the global namespace ... **from a session other than
session zero** is a privileged operation ... The privilege check is limited to
the creation of these objects, and does not apply to opening existing ones."*

So the split falls out of the sessions we measured: `audiodg.exe` is in session
0, so the **APO creates the section**, and the tray app — in the user's session,
with no privilege — only **opens** it, which needs nothing but the access rights
in the DACL. `sb-ipc` therefore takes a security descriptor from SDDL
(`D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;AU)`) when creating, and the tray app
retries its `OpenFileMappingW` until the audio engine has started.

Two consequences worth remembering:

* Because the APO holds the section open, a brand-new section has no UI behind
  it. It reads as *not compatible*, so the APO is a true bypass until somebody
  publishes — and it keeps the handle anyway, so it notices the moment the UI
  writes, without a second `OpenFileMappingW` on the audio thread.
* If the tray app holds a handle, the section survives an `audiodg.exe`
  restart, so the next APO instance finds the existing block instead of a fresh
  empty one.

**Still to verify:** that the tray app's `OpenFileMappingW` succeeds against a
section created from session 0 under our SDDL. This needs a real install,
because a single-session test process cannot create the machine-wide section at
all (it exercises the `Local\` fallback instead).

## 7. Levels stored as millidB

Parameters cross the control block as integers (millidB, milliseconds) to keep
the layout POD and the seqlock honest. The rounding is 0.001 dB, so this is a
formality.

## 8. Running alongside the reference product

The reference product is installed on this machine and its CLSID is still in
the endpoint slot we are about to append to, so both effects would run and the
result would be two boosts in series. Before testing, either uninstall Letasoft
Sound Booster (its uninstall log restores the original chain itself, and our
installer would otherwise restore values it never wrote) or leave it disabled.

**To verify:** uninstall it, then confirm `crates/setup`'s dry run shows only
`CRtkAPOSFX2` in slot 13 and an empty slot 15, and that `uninstall` puts the
chain back exactly as found.

## 9. Anti-cheat and antivirus

An APO is not injection, so it should be friendlier than the code-injection
method, but the DLL is loaded into `audiodg.exe` and the installer writes
protected registry keys. **Verify:** Defender does not quarantine the DLL, and
a game with anti-cheat still starts with the effect registered.
