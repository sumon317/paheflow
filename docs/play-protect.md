# Google Play Protect: expected warning & appeal kit

Sideloaded APKs with no Play Protect verdict history are frequently shown a
"Harmful app blocked / This app may be harmful" dialog on first install. This is
a **server-side heuristic verdict**, not a detection we can change from code.
It keys on:

- a signing certificate with no install-base reputation (PaheFlow's current key
  was created 2026-11-11 during the security rotation),
- the app holding `REQUEST_INSTALL_PACKAGES` and downloading its own updates
  (the same heuristics that catch dropper malware), and
- the install context (APK transferred via chat/cloud, installed from a file
  manager).

None of these indicate actual harm; the current release is reproducible and its
signing certificate is fixed (see checksums below).

## Installing anyway (per-device)

1. On the Play Protect dialog, tap **Install anyway** (shown under the warning).
2. If Play Protect later sends a "Harmful app found" notification for the
   installed app, open it and choose **Not harmful / Keep** — do not just
   dismiss it, so the verdict data reflects your choice.
3. Optionally verify the APK before installing (see checksums).

## Verify the APK you received

```bash
sha256sum paheflow-v1.1.3.apk
# expected: 27b3675ec46927b939f6ae1fbe2152789c3966b9037c7ff7c9b175b25a976e1d
```

Signing certificate (must match — update installers rely on this):

```
SHA-256: D0:E7:E7:79:B8:C4:AF:D0:C3:FF:3D:6E:AC:5A:E4:58:42:76:89:7C:93:2B:DE:E1:AE:78:CE:72:6B:6D:8B:F3
CN:      PaheFlow Developer
```

## Developer appeal (false positive)

Submit via the **Play Protect Appeals Submission Form**:
https://support.google.com/googleplay/android-developer/contact/protectappeals

Steps:

1. Upload the APK to <https://www.virustotal.com> first (the form asks for the
   SHA-256 of the file as uploaded there).
2. Fill in the form fields:

| Field | Value |
| --- | --- |
| Email address | your developer email |
| Developer name | PaheFlow Developer |
| Application package name | `com.animepahe.app` |
| SHA256 hash of APK on VirusTotal | `27b3675ec46927b939f6ae1fbe2152789c3966b9037c7ff7c9b175b25a976e1d` |

3. Suggested "Additional information" text:

```text
PaheFlow (package com.animepahe.app, versionCode 10, versionName 1.1.3) is an
open-source AnimePahe client distributed via GitHub Releases
(https://github.com/sumon317/paheflow). It is not malware. The "may be harmful"
verdict is a false positive driven by heuristics: the app is a new sideloaded
APK, it holds REQUEST_INSTALL_PACKAGES to perform in-app self-updates, and its
signing key was recently rotated (the previous key was exposed in a public git
history and had to be revoked).

The in-app updater verifies that any downloaded update APK is signed with the
same certificate as the installed app before launching the installer, and
release artifacts are published with SHA-256 checksums. Source code and build
instructions are public under GPL-3.0-or-later. Please re-evaluate the verdict.
```

Notes:

- Appeals are reviewed once; per the form, "all appeal decisions are final and
  you will not receive a response". If the warning persists, it typically fades
  as install-base reputation builds — expect days to weeks.
- Every new release build needs its hash re-checked here (the APK SHA-256
  changes on each build; the signing certificate does not).

## Hardening already applied in this repo

- `android/app/src/main/java/com/animepahe/app/MainActivity.kt`: the updater
  compares the downloaded APK's certificate with the installed app's
  certificate and refuses mismatches (with user guidance).
- Manifest: `allowBackup=false`, cleartext traffic disabled except loopback via
  `network_security_config.xml`, no broad storage permissions, no
  `MANAGE_EXTERNAL_STORAGE`.
- CI runs a Betterleaks secret scan on every push/PR so no credentials can
  re-enter the repo.
