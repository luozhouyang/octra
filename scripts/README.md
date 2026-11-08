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

   Bundles are written to `apps/desktop/src-tauri/target/release/bundle/`.

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

Install Xcode command-line tools and the Rust target for the machine:

```bash
xcode-select --install
```

Universal bundles require both `aarch64-apple-darwin` and
`x86_64-apple-darwin` targets plus one sidecar per triple.

### Windows

Install the Microsoft C++ build tools, WebView2, and the
`x86_64-pc-windows-msvc` Rust target. Run the sidecar script from a shell where
`cargo` and `rustc` resolve the MSVC toolchain.

## Signing placeholders

- macOS: set `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
  `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, and
  `APPLE_TEAM_ID` before `tauri:build` for signing and notarization.
- Windows: configure a code-signing certificate through
  `WINDOWS_CERTIFICATE` and `WINDOWS_CERTIFICATE_PASSWORD`, or use a hardware
  token with `signtool`.
- Linux: sign the `.deb`/`.rpm` packages with the distribution's packaging key
  after the bundle step.

The GitHub Actions workflow at `.github/workflows/desktop.yml` builds unsigned
artifacts. Add signing secrets and the corresponding Tauri environment
variables before publishing release installers.
