# Desktop packaging

The desktop bundle ships the daemon as a Tauri sidecar. The daemon binary is a
build artifact and is never committed.

## Local build

1. Install the platform prerequisites listed below.
2. Install JavaScript dependencies and build the frontend once:

   ```bash
   pnpm install --frozen-lockfile
   pnpm --filter @octra/desktop build
   ```

3. Build and stage the sidecar for the host target:

   ```bash
   bash scripts/build-desktop-sidecar.sh
   ```

   To cross-compile for another installed Rust target, pass the triple:

   ```bash
   bash scripts/build-desktop-sidecar.sh --target aarch64-apple-darwin
   ```

   The script copies `octra-daemon` to
   `apps/desktop/src-tauri/binaries/octra-daemon-<triple>` (`.exe` on Windows).
   Tauri reads that name from the `bundle.externalBin` entry in
   `apps/desktop/src-tauri/tauri.conf.json`.

   Tauri validates the sidecar from its build script, including for
   `pnpm dev`, so run this step once per target before starting the desktop
   app.

4. Build the installer:

   ```bash
   pnpm --filter @octra/desktop tauri:build
   ```

   Bundles are written to `target/release/bundle/` (Tauri uses the Cargo
   workspace target directory).

## Universal macOS bundle

A universal bundle contains both Apple Silicon and Intel code. Install both
Rust targets, stage the sidecars, and build with the synthetic
`universal-apple-darwin` target:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
bash scripts/build-desktop-sidecar.sh --target universal-apple-darwin
pnpm --filter @octra/desktop tauri:build --target universal-apple-darwin
```

The sidecar script builds each architecture, stages
`octra-daemon-aarch64-apple-darwin` and `octra-daemon-x86_64-apple-darwin`,
and combines them with `lipo` into
`octra-daemon-universal-apple-darwin`. All three names are staged because Tauri
may look up either the universal name or the per-architecture name while
building the bundle. The script fails with a clear error when `lipo` is not
available, so it must run on macOS with the Xcode command-line tools installed.

pnpm 10 forwards a literal `--` to the script, which the Tauri CLI treats as an
argument separator. Use `tauri:build --target <triple>` as shown above. With
npm, `npm run tauri:build -- --target universal-apple-darwin` is equivalent.

Universal bundles are written to
`target/universal-apple-darwin/release/bundle/`.

## Linux compile check with Docker

The host may not have WebKitGTK or permission to install system packages, so
the desktop crate can be compiled inside Ubuntu 24.04:

```bash
bash scripts/check-desktop-linux.sh
```

When the host already has the Linux prerequisites, run the same commands
directly with the pinned toolchain:

```bash
cargo check -p octra-desktop
cargo clippy -p octra-desktop -- -D warnings
```

The Docker script is the reproducible fallback and CI helper.

The script:

- starts `ubuntu:24.04` and installs `libwebkit2gtk-4.1-dev`,
  `libappindicator3-dev`, `librsvg2-dev`, `patchelf`, `libgtk-3-dev`,
  `libdbus-1-dev`, `pkg-config`, `curl`, and `build-essential`;
- installs Rust 1.97.1 with rustup, including clippy, rustfmt, and the host
  target;
- mounts the repository read-only at `/work` and compiles a copy at `/build`, so
  the check never modifies the working tree;
- keeps Cargo and rustup state in the named volume
  `octra-desktop-linux-cargo` so repeated runs are fast;
- runs `cargo check -p octra-desktop` and
  `cargo clippy -p octra-desktop -- -D warnings`.

`tauri-build` validates `bundle.externalBin` during `cargo check`. If no Linux
sidecar has been staged, the script creates a placeholder executable inside the
container copy only; the real sidecar is built by the packaging commands above.

Override the defaults when needed:

```bash
OCTRA_DESKTOP_LINUX_IMAGE=ubuntu:24.04 \
OCTRA_DESKTOP_LINUX_CARGO_VOLUME=octra-desktop-linux-cargo \
OCTRA_DESKTOP_LINUX_RUST_VERSION=1.97.1 \
bash scripts/check-desktop-linux.sh
```

If Docker Hub is unreachable, point `OCTRA_DESKTOP_LINUX_IMAGE` at a mirror of
Ubuntu 24.04. If `archive.ubuntu.com` is slow, set
`OCTRA_DESKTOP_LINUX_APT_MIRROR` to an Ubuntu mirror, for example
`http://mirrors.aliyun.com/ubuntu`; the package set is unchanged.

## Platform prerequisites

### Linux

```bash
sudo apt-get update
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev \
  libappindicator3-dev \
  librsvg2-dev \
  patchelf \
  libgtk-3-dev \
  libdbus-1-dev
```

### macOS

Install Xcode command-line tools and the Rust targets:

```bash
xcode-select --install
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

### Windows

Install the Microsoft C++ build tools, WebView2, and the
`x86_64-pc-windows-msvc` Rust target. Run the sidecar script from a shell where
`cargo` and `rustc` resolve the MSVC toolchain.

## macOS signing and notarization

Tauri signs and notarizes macOS bundles when the standard Apple environment
variables are present. Configure them as GitHub Actions secrets; nothing is
stored in the repository. When the secrets are absent the workflow still
builds unsigned artifacts, which is the normal pull-request path.

Configure these repository secrets:

| Secret                       | Value                                                                                                                                       |
| ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `APPLE_CERTIFICATE`          | Base64-encoded `Developer ID Application` certificate exported as `.p12`.                                                                   |
| `APPLE_CERTIFICATE_PASSWORD` | Password chosen when exporting the `.p12`.                                                                                                  |
| `APPLE_SIGNING_IDENTITY`     | Identity name, for example `Developer ID Application: Example Corp (TEAMID1234)`. Optional when the certificate contains a single identity. |

Then choose one notarization method.

Apple ID:

| Secret           | Value                                                                |
| ---------------- | -------------------------------------------------------------------- |
| `APPLE_ID`       | Apple Developer account email.                                       |
| `APPLE_PASSWORD` | App-specific password for that Apple ID. Never the account password. |
| `APPLE_TEAM_ID`  | Apple Developer Team ID.                                             |

App Store Connect API key:

| Secret             | Value                                                        |
| ------------------ | ------------------------------------------------------------ |
| `APPLE_API_KEY`    | Contents of the downloaded `AuthKey_<KEYID>.p8` private key. |
| `APPLE_API_KEY_ID` | Key ID shown in App Store Connect.                           |
| `APPLE_API_ISSUER` | Issuer ID shown above the keys table in App Store Connect.   |

The workflow writes the `.p8` to a temporary file and exports the variable
names Tauri expects: `APPLE_API_KEY` becomes the key ID and
`APPLE_API_KEY_PATH` points at the temporary `.p8` file. For a local build
using an API key, set them directly:

```bash
export APPLE_API_KEY="<key id>"
export APPLE_API_KEY_PATH="/path/to/AuthKey_<KEYID>.p8"
export APPLE_API_ISSUER="<issuer id>"
```

### Generate the signing material

These steps assume an active Apple Developer Program membership and a Mac with
Xcode Command Line Tools (`xcode-select --install`).

#### 1. Find the Team ID

Sign in at <https://developer.apple.com/account>, open **Membership details**,
and copy the 10-character **Team ID**. This is the value for `APPLE_TEAM_ID`.

#### 2. Create a Certificate Signing Request (CSR)

1. Open **Keychain Access** on the Mac.
2. Menu **Keychain Access → Certificate Assistant → Request a Certificate From
   a Certificate Authority…**.
3. Enter the Apple Developer email, a Common Name such as `Octra Developer ID`,
   leave the CA email blank, choose **Saved to disk**, and continue.
4. Keep the generated `.certSigningRequest` file and leave the private key in
   the login keychain.

#### 3. Create a Developer ID Application certificate

1. Open <https://developer.apple.com/account/resources/certificates/list>.
2. Click **+**, choose **Developer ID Application** (under Software, for
   distribution outside the Mac App Store), and continue.
3. Upload the CSR, download the `.cer` file, and double-click it to install it
   in the login keychain.
4. Verify the identity is visible:

   ```bash
   security find-identity -v -p codesigning
   ```

   The line should look like:

   ```text
   Developer ID Application: Example Corp (TEAMID1234)
   ```

   Use that exact string for `APPLE_SIGNING_IDENTITY`.

#### 4. Export the certificate as .p12

1. In **Keychain Access → login → My Certificates**, expand the new
   `Developer ID Application` entry so both the certificate and its private key
   are selected.
2. Right-click the selection, choose **Export 2 items…**, select **Personal
   Information Exchange (.p12)**, and set a strong export password.
3. Verify the archive:

   ```bash
   openssl pkcs12 -in DeveloperID.p12 -info -noout
   ```

4. Base64-encode it for CI:

   ```bash
   base64 -i DeveloperID.p12 | tr -d '\n' > apple-certificate.b64
   ```

   Put the contents of `apple-certificate.b64` in the `APPLE_CERTIFICATE`
   secret and the export password in `APPLE_CERTIFICATE_PASSWORD`. Never commit
   the `.p12`, the `.b64`, or the password.

#### 5. Choose a notarization credential

**Apple ID + app-specific password (simpler):**

1. Sign in at <https://appleid.apple.com>.
2. Open **Sign-In and Security → App-Specific Passwords**.
3. Generate a password named `Octra notarization` and copy it once.
4. Configure `APPLE_ID`, `APPLE_PASSWORD` (the app-specific password), and
   `APPLE_TEAM_ID`. Never use the Apple account password.

**App Store Connect API key (better for CI):**

1. Open <https://appstoreconnect.apple.com> → **Users and Access** →
   **Integrations** (or **Keys**) → **App Store Connect API** → **Team Keys**.
2. Click **+**, name the key `Octra notarization`, choose a role with
   notarization access (Developer or Admin), and generate it.
3. Download `AuthKey_<KEYID>.p8` once; Apple cannot show it again.
4. Record the **Key ID** and the **Issuer ID** shown above the key table.
5. Configure:
   - `APPLE_API_KEY` = the full `.p8` file content;
   - `APPLE_API_KEY_ID` = the Key ID;
   - `APPLE_API_ISSUER` = the Issuer ID.

For a local build, write the `.p8` to a private path and export:

```bash
export APPLE_API_KEY="<KEYID>"
export APPLE_API_KEY_PATH="$HOME/.appstoreconnect/AuthKey_<KEYID>.p8"
export APPLE_API_ISSUER="<ISSUER_UUID>"
```

#### 6. Build and verify locally

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
bash scripts/build-desktop-sidecar.sh --target universal-apple-darwin
pnpm --filter @octra/desktop tauri:build --target universal-apple-darwin
```

With the Apple environment variables exported, Tauri signs the app and sidecar
and submits the bundle for notarization. Verify the result:

```bash
APP="target/universal-apple-darwin/release/bundle/macos/Octra.app"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -dv --verbose=4 "$APP"
spctl --assess --type execute --verbose=4 "$APP"
xcrun stapler validate "$APP"
```

`spctl` should report `accepted` and `stapler validate` should report that the
ticket is valid. If notarization is asynchronous, `xcrun stapler staple "$APP"`
after the notary submission completes.

#### 7. Security notes

- Store the `.p12`, `.p8`, and passwords only in a password manager and GitHub
  Actions secrets. Do not commit them.
- The `.p8` key can be downloaded only once; keep a backup in a secure vault.
- Revoke and replace the certificate or API key immediately if either leaks.
- The workflow creates a temporary keychain and deletes it, the `.p12`, and the
  temporary `.p8` in an `always()` cleanup step.
- The app does not need a custom entitlements file today. Add
  `bundle.macOS.entitlements` only if a future feature requires a specific
  entitlement.

The workflow imports the certificate into a temporary keychain, enables the
hardened runtime through `bundle.macOS.hardenedRuntime`, and removes the
keychain and temporary API key in an `always()` cleanup step. The current app
does not need a custom entitlements file: it is not sandboxed, WKWebView runs
its web content out of process, and Tauri signs the sidecar separately. Add
`bundle.macOS.entitlements` only if a future capability requires a specific
entitlement.

## GitHub Actions workflow

`.github/workflows/desktop.yml` keeps the Linux, macOS, and Windows package
matrix. The macOS jobs import the certificate and notarize only when the Apple
secrets above are configured. A separate `universal-macos` job builds
`universal-apple-darwin` on pushes to `main` and manual dispatch; it is skipped
for pull requests. Artifacts are uploaded as `octra-desktop-<os>` and
`octra-desktop-macos-universal`.

## Verification

```bash
bash -n scripts/build-desktop-sidecar.sh scripts/check-desktop-linux.sh
bash scripts/check-desktop-linux.sh
pnpm --filter @octra/desktop typecheck
pnpm test
pnpm build
```
