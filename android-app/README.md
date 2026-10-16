# slopd for Android

The Android app manages slopd agent panes through an ordinary SSH login. It can
connect to a normal SSH address or use `iroh-ssh` to reach that SSH service by
Iroh Endpoint ID.

The app lists, searches, filters, and sorts managed panes; starts and forks
agents with readiness, environment, and argument controls; tails and paginates
transcripts; sends to one or filtered groups; manages tags; listens or waits for
events; and exposes backup, restore, graveyard, and revive operations. Structured
service information shows the daemon state, selected remote config/socket,
slopctl version, and supported capabilities. The SSH account therefore has the
same effective authority as a local `slopctl` user. Use a normal SSH key in
`authorized_keys`; no forced command or slopd-specific key option is required.

## Receiver

Install `slopd` and `slopctl` for the SSH user and run the slopd user service.
For a direct connection, make the host's SSH service reachable. For Iroh, run an
`iroh-ssh` server compatible with the app's pinned revision. Add the app's public
SSH key to the user's normal `~/.ssh/authorized_keys`, or use password or
keyboard-interactive authentication.

The connection screen accepts:

- **SSH:** `user@host`, `user@host:port`, or `user@[IPv6]:port`
- **Iroh:** `user@<64-character-endpoint-id>`

SSH host keys are verified and pinned separately for each destination. Private
keys, passwords, and connection profiles use Android encrypted storage. Keys can
also remain in an OpenKeychain-compatible provider.

Advanced connection options select the remote `slopctl` executable, config file,
control socket, and optional Iroh relay URLs. Each command argument is quoted
separately before it is passed to the remote shell.

## Architecture

Flutter provides the UI and published `dartssh2` provides normal SSH
authentication and exec channels. For Iroh connections, a small Rust native
adapter calls the pinned `futpib/iroh-ssh` bridge and returns a loopback TCP port;
the same SSH client then connects through that tunnel.
The shared `openkeychain_ssh` Flutter plugin handles Android key providers.

Each app operation invokes the installed `slopctl` over an SSH exec channel.
Long-lived transcript and event subscriptions keep one exec channel open and
consume the command's JSON-lines output. Polling and subscriptions stop while
their page is backgrounded and reconnect explicitly after transport loss.
There is no custom slopd transport, endpoint allowlist, alternate remote CLI, or
additional network protocol. SSH authentication and host-key checks are shared
across those exec channels.

## Build

Requirements: Flutter 3.44.1 or newer, Rust, Android SDK 36, JDK 17, Android NDK
28.2.13676358, and `cargo-ndk`.

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk --version 4.1.2 --locked
cd android-app
flutter pub get
flutter analyze
flutter test
flutter build apk --debug --target-platform android-arm64,android-x64
```

Gradle builds the native adapter automatically. `scripts/build-native.sh` builds
it separately. Release builds require a persistent signing key through
`KEYSTORE_PATH` and `KEYSTORE_PASSWORD`; `scripts/build-release.sh` creates a
universal APK, per-architecture APKs, and `SHA256SUMS`.

The manually triggered **Build Android APK** workflow requires the GitHub
Actions secret `KEYSTORE_BASE64`. It uses the `release` alias and `android`
passwords of the existing `iroh-ssh-android` release keystore. Keep this
keystore for future releases so installed APKs can be updated.
