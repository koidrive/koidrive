# Koidrive for macOS

Koidrive for macOS is the full Koidrive sync client for macOS 26 or later on Apple silicon. Use the setup window to connect to your server, sign in this Mac and link a folder. The menu bar shows sync status and notices, opens folders in Finder and lets you pause or resume sync. **Needs You** asks for decisions before risky changes. **Settings** has **Folders**, **Account**, **Storage**, **Problems** and **General** tabs. See the [Mac app guide](../../docs/guide/macos.md) for setup and everyday use.

While files sync, the menu shows how many are left to upload and download and how many bytes are done. Every synced file is a full copy on disk.

The app carries `koi` in `Contents/Helpers/koi` and shares its device, folders and background service with the CLI. Through `core/ffi` and [UniFFI](https://mozilla.github.io/uniffi-rs/), it runs each change as a `koi --json` command and follows the sync daemon over its socket. While no daemon runs, it reads the state with `koi status`.

## Requirements

- macOS 26 or later on Apple silicon.
- Xcode 26 or later, for the macOS 26 SDK, with its command-line tools selected (`xcode-select -p`).
- Rust through [rustup](https://rustup.rs/). The toolchain version comes from `rust-toolchain.toml` at the repository root and rustup installs it on first use.
- [XcodeGen](https://github.com/yonaskolb/XcodeGen) 2.46.0. You don't install it yourself. The first run of `scripts/generate-project.sh` downloads the release into `build/tools` and checks its SHA-256.

## Build and test

Run these from `apps/macos`:

```sh
scripts/build-ffi.sh

# 2. Test the bindings and the app's models.
swift test --package-path KoidriveFFI

# 3. Generate Koidrive.xcodeproj from project.yml.
scripts/generate-project.sh

xcodebuild -project Koidrive.xcodeproj -scheme Koidrive -configuration Release \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  CODE_SIGNING_ALLOWED=NO build
open build/DerivedData/Build/Products/Release/Koidrive.app
```

The app syncs the profile of the installed launch agent (`~/Library/LaunchAgents/dev.koidrive.koi*.plist`), or else the profile `koi` uses in the app's environment (`~/.local/state/koidrive` by default). Its `koi` commands run with that profile's `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR`. When it starts, it runs `koi _reconcile`. Like `koi folder link`, `koi _reconcile` brings the background service in line with the linked folders, and it leaves a service switched off in Login Items off.

To try it against a local server without touching your own profile, give the app a home of its own and start it from a terminal:

```sh
profile="$(mktemp -d)"
HOME="$profile" XDG_CONFIG_HOME="$profile/config" XDG_STATE_HOME="$profile/state" \
	XDG_RUNTIME_DIR="$profile/run" build/DerivedData/Build/Products/Release/Koidrive.app/Contents/MacOS/Koidrive
```

Then start the Worker in `server/` with `pnpm exec wrangler dev` (after `pnpm install`), enter `http://127.0.0.1:8787` in the setup window, open `http://127.0.0.1:8787/setup` in a browser, and claim the server with the code that `wrangler dev` prints. Koidrive accepts plain `http` only on a loopback address.

To show a given state, such as a question or a problem, start a Debug build on the fake core in `core/ffi` with a scripted scenario. Release builds ignore the `FakeScenario` setting. The fake keeps its state in memory only:

```sh
xcodebuild -project Koidrive.xcodeproj -scheme Koidrive -configuration Debug \
  -derivedDataPath build/DerivedData -destination "platform=macOS,arch=$(uname -m)" \
  CODE_SIGNING_ALLOWED=NO build
open build/DerivedData/Build/Products/Debug/Koidrive.app --args -FakeScenario manyQuestions
```

`launchScenario` in `KoidriveFFI/Sources/KoidriveModel/AppModel.swift` lists the scenario names, such as `firstRun`, `syncing`, `massDelete` and `conflict`.

You can also open `Koidrive.xcodeproj` in Xcode after step 3 and run the Koidrive scheme. The views have SwiftUI previews for these scenarios. Local builds are signed ad hoc, so you don't need an Apple developer account.

`core/ffi/scripts/integration-test.sh`, run from anywhere in the repository, tests the session against the Worker under `wrangler dev` with a debug `koi` and its daemon: claiming, pairing, linking, status events, the pause, questions, problems and signing out. It needs `pnpm install` in `server/` first.

Run `scripts/build-ffi.sh` again after every change to `core/ffi` or `cli`. It rebuilds the Rust library and regenerates the Swift API together. UniFFI checksums stop the app at launch when the library and the bindings disagree on the API, but they do not detect a build that is out of date. An old library with its matching bindings still launches.

## Layout

| Path | Contents |
|---|---|
| `Koidrive/` | The SwiftUI app: the menu bar, setup, Settings, Needs You, Deleted Files, Version History, Share Link and Shared Links windows. User-facing text is in `Localizable.xcstrings`, with English as the base language. |
| `project.yml` | The XcodeGen spec for `Koidrive.xcodeproj`. The generated project is not checked in, so edit `project.yml` and run `scripts/generate-project.sh` again after changing it or adding or removing source files. |
| `KoidriveFFI/` | A Swift package: the `KoidriveFFI` library wraps the Rust core (the generated bindings and the static library as an XCFramework), and `KoidriveModel` holds the app's view models. Both have tests. |
| `scripts/generate-project.sh` | Generates `Koidrive.xcodeproj` with the pinned XcodeGen. |
| `scripts/build-ffi.sh` | Builds the Rust library and `koi`, and fills in `KoidriveFFI/Frameworks`, `KoidriveFFI/Sources` and `build/koi`. These generated files are not checked in. |
| `scripts/build-release.sh`, `sign-app.sh`, `make-dmg.sh`, `notarize.sh`, `verify-release.sh` | Build, sign, package, notarize and verify a release disk image, with `koi` signed inside the app. The macOS release workflow runs them, as [docs/release-macos.md](../../docs/release-macos.md) describes. |
| `Koidrive.entitlements` | The entitlements release builds are signed with, deliberately empty. |

The Rust library and `koi` are built for the architecture of the machine you build on, not as universal binaries.

## License

Apache-2.0, like the rest of `apps/`. See [../LICENSE](../LICENSE).
