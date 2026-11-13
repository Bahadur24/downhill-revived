## Android (experimental)

`DownhillRevived-<version>-android-experimental.apk` is the game for Android phones and tablets.
It is experimental: recent phones hold 60 fps at 2x resolution, slower ones (and 4x) may not.
On a slower phone, OPTIONS > SETTINGS > Graphics tab > Performance Mode turns off the riders'
shadows and the sun's glow, the lens flare and the particles, which helps the most. Native VU (the
same tab, on by default) lets the phone's graphics chip run part of the PS2's work, which is
faster; if something in the picture looks wrong, turn it OFF and send us the log.
Play with the touch controls (Classic pad or Thumb zones, tilt steering if you like: OPTIONS >
SETTINGS > Controls tab > Touch Controls) or a controller. The game's cheat codes are typed on the
touch pause menu's **CODES** page.

**What you need**
- Any arm64 phone or tablet with Android 10 or later and Vulkan (most phones since 2018),
  older arm64 cores such as the Snapdragon 680's and Mali GPUs on Arm's newer drivers included.
- Your own disc image of Downhill Domination (USA, SCUS-97177) as an `.iso` file, like the PC
  version. No game data comes with the app.
- A few GB of free storage: the disc image is copied into the app once.

**Install**
1. Download the APK on the phone.
2. Open it. Android asks once to allow installs from your browser or file manager ("Install
   unknown apps"); allow it, then install. The app is not on Google Play.
3. Start Downhill Revived and choose your `.iso` when asked. The copy takes a minute or two.

**Updates.** The game checks once a day whether a newer release is out and says so at the top of
the screen; tap the line to open the release page, then install the new APK over the old one. Your
settings and saves stay. Nothing is downloaded by the game itself. If you installed a test build
before this release, uninstall it first: Android refuses an update signed by another key, and
uninstalling deletes the app's files (the disc copy and your saves).

**If the game closes unexpectedly**, it says so at the next start and offers **Share log**: send
the file to us (a GitHub issue or Discord) and we can see what happened. Long-press the app's icon
for **Share log** at any time, even when the game closes before anything shows. Nothing is sent
unless you pick an app in Android's share sheet.

**Online: P2P races only.** ONLINE in the main menu offers Create A P2P Race and Join With A Code
(a 7-character code from a friend's P2P race, on a phone or a PC). The race data goes directly
between the players' games, so **the other players in a P2P room can see your network (IP)
address**; the game says so. Play them with people you trust. Public races, Quick Race and the
normal private races are not on phones yet.

**Wi-Fi LAN and Tailscale rooms.** These are separate from online P2P races. For Wi-Fi LAN, host
or join while both devices are on the same Wi-Fi network. The join screen searches for open rooms;
if discovery does not work, refresh or enter the host's private IPv4 address and port manually.
For a room over Tailscale, both devices need Tailscale connected to the same tailnet, and its ACLs
must allow the room's TCP connection. The host enters their Tailscale IPv4 address in
`100.64.0.0/10`; the game displays an `IP:port` for the guest to enter. The port is chosen at
runtime and may change for another room. The game does not discover Tailscale peers automatically.
Wi-Fi LAN discovery and two-device Tailscale play have not been verified on physical devices.

**Check the download.** `SHA256SUMS` lists the APK's SHA-256. The APK is signed with the
Downhill Revived release key, certificate SHA-256
`D7:61:3E:E0:F0:93:95:22:D8:3B:06:CE:8A:05:ED:9D:03:E4:A9:96:8E:EA:2F:9C:83:98:58:94:14:56:AE:1B`
(`apksigner verify --print-certs`).
