# Google Play Protect: expected warning & appeal kit

Sideloaded APKs with no Play Protect verdict history are frequently shown a
"Harmful app blocked / This app may be harmful" dialog on first install. This is
a **server-side heuristic verdict**, not a detection we can change from code.
It keys on:

- a signing certificate with no install-base reputation (PaheFlow's current key
  was created 2026-11-11 during the security rotation), and
- the install context (APK transferred via chat/cloud, installed from a file
  manager).

The app itself is deliberately low-risk on the usual heuristics: since v1.1.4 it
does **not** hold `REQUEST_INSTALL_PACKAGES` — it only downloads the update APK
to `Downloads/PaheFlow` and hands it to your file manager or package installer
to install (see "Update flow" below).

None of these indicate actual harm; the current release is reproducible and its
signing certificate is fixed (see checksums below).

## Installing anyway (per-device)

1. On the Play Protect dialog, tap **Install anyway** (shown under the warning).
2. If Play Protect later sends a "Harmful app found" notification for the
   installed app, open it and choose **Not harmful / Keep** — do not just
   dismiss it, so the verdict data reflects your choice.
3. Optionally verify the APK before installing (see checksums).

## Update flow (no self-install)

The in-app update option checks for newer releases and downloads the latest
APK, but never installs anything itself:

1. The APK is saved to `Downloads/PaheFlow/paheflow-update-v<version>.apk`
   (via MediaStore on Android 10+, the shared Downloads folder on older
   devices).
2. A dialog pops up with an **Open with…** button that hands the file to your
   file manager or package installer; you confirm the install there.
3. The download file is signed with the same certificate as the installed app —
   if it somehow isn't (e.g. an APK from another source), the dialog shows a
   warning so you can uninstall the old version first.

Because the app never installs packages, it holds no
`REQUEST_INSTALL_PACKAGES` permission and never needs the
"install unknown apps" settings flow.

## Verify the APK you received

```bash
sha256sum paheflow-v1.1.4.apk
# expected: 9aaa6d1a13e36f7274e26213393ef1c0e5fd68857f9d928c36be4d222af8f530
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
| SHA256 hash of APK on VirusTotal | `9aaa6d1a13e36f7274e26213393ef1c0e5fd68857f9d928c36be4d222af8f530` |

3. Suggested "Additional information" text:

```text
PaheFlow (package com.animepahe.app, versionCode 11, versionName 1.1.4) is an
open-source AnimePahe client whose source and build instructions are published
under GPL-3.0-or-later. It is not malware. The "may be harmful" verdict is a
false positive driven by heuristics: the app is a new sideloaded APK and its
signing key was recently rotated (the previous key was exposed in a public git
history and had to be revoked).

The app does not request the REQUEST_INSTALL_PACKAGES permission and does not
install packages itself: its update feature only downloads the latest release
APK into the shared Downloads/PaheFlow folder and hands the file to the user's
own file manager or package installer. Downloaded update APKs are verified to
be signed with the same certificate as the installed app, and release artifacts
ship with SHA-256 checksums. Please re-evaluate the verdict.
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
  certificate and shows a warning in the hand-off dialog on mismatch.
- Manifest: `allowBackup=false`, cleartext traffic disabled except loopback via
  `network_security_config.xml`, no broad storage permissions, no
  `MANAGE_EXTERNAL_STORAGE`, and **no `REQUEST_INSTALL_PACKAGES`** (the app
  never installs APKs itself).
- CI runs a Betterleaks secret scan on every push/PR so no credentials can
  re-enter the repo.
