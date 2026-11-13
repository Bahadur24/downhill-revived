# 199: Wi-Fi LAN and Tailscale remote rooms

Status: in progress

Branch `feature/android-lan`, based on `main` f097107. The existing Wi-Fi LAN implementation stays separate from the online room service and from Tailscale mode. In Wi-Fi mode, the host runs a local WebSocket room service on its private Wi-Fi IPv4 interface, guests can discover it over bounded UDP broadcast or enter a private IPv4:port manually, and the host relays lobby messages and race STATE/EVENT frames. The Wi-Fi interface picker and UDP discovery remain specific to this mode.

## Finding and latest improvements

The attached 2026-11-12 Android log contains two relevant runs. In the earlier run, `Invalid LAN room setting` appears immediately before a native SIGSEGV. The Rust source emits that validation error only when a room-settings message has an invalid `track`, `laps`, or non-null `seed`; the log does not include the field values, so it cannot establish which one was wrong. In the later run, the game successfully hosted a LAN room, entered its lobby, and left it. The log does not show a room-discovery attempt, QR scan, or two-phone connection. Android reports a later native crash with status 6, but this log has no tombstone, stack trace, or faulting thread; the precise cause is unknown, and the evidence does not tie it to LAN or QR.

A host can start a one-player race without a redundant link check; multiplayer LAN races still require the existing current link check. A regression test covers the server's solo-start message. The LAN join screen automatically searches the same Wi-Fi network and lists rooms that are open, not racing, and have at least one free player slot. Users can refresh or enter a private IPv4:port manually if discovery is unavailable. The bounded UDP scan returns only the room details needed by the list and does not publish a PIN.

QR display, camera scanning, camera permission, scanner activity, JNI/API plumbing, and QR libraries were removed. In a LAN lobby, the room-circle button copies the host Wi-Fi address. Hosting uses the assigned private IPv4 interface, preferring Wi-Fi, and refreshes the advertised address when interfaces change.

## Tailscale remote rooms

Tailnet play is a separate connection choice; it does not replace or configure Wi-Fi LAN. Both Android devices need Tailscale installed, connected to a tailnet, and allowed to reach one another. A host manually supplies their Tailscale IPv4 address in `100.64.0.0/10`; the game starts a local TCP/WebSocket room listener and displays the host's `IP:port`. A guest manually enters that exact `IP:port`. The listener port is selected at runtime and may change for another room.

There is no automatic enumeration or discovery of Tailscale peers. Being on the same Tailscale account or tailnet does not make the game discover devices. The host must share the address and port, or use another out-of-band method to communicate them. The Tailscale VPN must remain active and the game must be included in its routing; custom tailnet ACLs must allow the room's TCP port. Tailscale chooses its own direct or relay path as needed; the game does not add an internet relay server.

Tailnet mode binds the local listener for VPN-routed connections and validates peers against the Tailscale address range. The Wi-Fi interface picker stays Wi-Fi-only. Entering Tailnet mode cancels and suppresses local UDP LAN-discovery scans, while room-service messages—including the multiplayer pre-race connectivity check—remain active. This prevents Tailnet traffic from being confused with a Wi-Fi broadcast scan.

Host additions include a player-eject action with an explicit confirmation dialog (Cancel selected by default); the request carries the selected member ID and the Rust room service verifies that the requester is the host. The host can also start solo, edit room settings, set a room PIN, and copy the Tailnet address. The number of available game tracks remains 25.

## Checks and limits

- The `dhd_net` Rust library suite passed serially and offline: 59 passed, 0 failed, 10 ignored. It covers room behavior including Tailnet address/peer filtering and cancellation or suppression of UDP discovery in Tailnet mode.
- `cargo fmt` and the crate-scoped formatting check passed. The ABI layout check passed for four targets using temporary headers under `/tmp`.
- Documentation and tree checks passed; the WASM core passed its strict build/link step using LLVM/LLD 23.1. `python3 port/tools/test_runtime.py` passed the ABI/signature, guest, and semantic tests, then stopped at the native host tests because their low-level programs require Windows (`NotImplementedError: low host test programs need Windows`). The full port check is therefore not green on this Linux machine.
- The fresh debug APK built successfully from that core with Android NDK 27.2.12479018. `aapt` and `apksigner` verified package `app.downhillrevived`, version 0.7.16 (versionCode 716), minSdk 29, arm64-v8a only, debuggable, and a valid v2 debug signature. APK: `build/android/downhill-revived-0.7.16-debug.apk` (SHA-256 `17d9b32765634a922124dad725785894c989628d6abf0f3394306beffd6700ce`). Its native library contains the Tailnet UI and host-eject strings.
- `git diff --check` and the documentation check passed after the final documentation edits. `src/` is unchanged and `TRACK_COUNT` remains 25.
- These are code-level, unit, ABI, or build checks—not a live cross-house connection test. No two-device test across two homes was performed. A real acceptance test still needs two Android devices connected through Tailscale, a host and guest using the displayed manual `IP:port`, and a check that the tailnet ACL and Android VPN routing permit the selected TCP port. Wi-Fi LAN discovery and a full race also need device testing.

## Android guidance

The Wi-Fi implementation uses bounded UDP broadcast discovery rather than Android NSD. See [Android Network Service Discovery](https://developer.android.com/develop/connectivity/wifi/use-nsd) for the platform alternative and [Android local network permission](https://developer.android.com/privacy-and-security/local-network-permission) for current platform guidance. Tailscale mode uses the Android Tailscale VPN with regular app sockets; see [Tailscale Android](https://tailscale.com/kb/1017/install-android) and [Tailscale IP addresses](https://tailscale.com/kb/1033/ip-and-dns-addresses). The app targets SDK 35 and requires Android 10 (API 29) or later.

## Result

Changes remain uncommitted on `feature/android-lan`; pre-existing working-tree changes have been preserved. The implementation does not change the available tracks or race count. The debug APK at `build/android/downhill-revived-0.7.16-debug.apk` is freshly built from the updated core. Physical Wi-Fi and two-home Tailscale behavior remain unverified, and the available log does not establish the precise cause of the status-6 crash.
