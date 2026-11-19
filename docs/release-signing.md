# Release signing (in-app updates)

The Lunaris app and the standalone agent update themselves from GitHub
Releases. They install an update only when:

1. `SHA256SUMS.sig` is a valid Ed25519 signature over the exact bytes of
   `SHA256SUMS`, checked with the public key built into the program
   (`RELEASE_SIGNING_PUBLIC_KEY_B64` in `updater/src/verify.rs`),
2. the first line of `SHA256SUMS`, `# lunaris <version>`, names the version
   of the release being installed (the signature covers it, so an old signed
   list re-published under a newer tag cannot force a downgrade), and
3. the downloaded file's SHA-256 is the one its line in `SHA256SUMS` gives.

The release workflow (`.github/workflows/release.yml`, step "Checksums and
signature") writes both files on every `v*` tag and uploads them with the
release. It fails, and nothing is published, when the secret
`LUNARIS_RELEASE_SIGNING_KEY` is missing or when its public key is not the
one in `updater/src/verify.rs`.

While `RELEASE_SIGNING_PUBLIC_KEY_B64` is the placeholder
(`UNSET-RELEASE-SIGNING-PUBLIC-KEY`), a build still says when a new version
exists but installs nothing ("Updates are not signed in this build").

## Create the key (once)

On a trusted computer, in an empty folder outside the repository:

```sh
umask 077
openssl genpkey -algorithm ed25519 -out lunaris-release-key.pem
chmod 600 lunaris-release-key.pem

# The private key goes to GitHub as a secret (repository collyn/lunaris).
gh secret set LUNARIS_RELEASE_SIGNING_KEY --repo collyn/lunaris < lunaris-release-key.pem

# The raw 32-byte public key, base64: this is what the programs carry.
openssl pkey -in lunaris-release-key.pem -pubout -outform DER | tail -c 32 | base64
```

Paste the printed public key (44 characters, ending in `=`) into
`updater/src/verify.rs`:

```rust
pub const RELEASE_SIGNING_PUBLIC_KEY_B64: &str = "<the printed key>";
```

Keep the line exactly in that form: the workflow reads it with `sed` to
check it against the secret. Leave `PLACEHOLDER_PUBLIC_KEY` as it is.

Then keep `lunaris-release-key.pem` offline (a password manager or an
encrypted backup) and delete the working copy. Never commit it.

Note: only builds made after the key is in the source can install updates.
Copies of 0.6.3 and older have no key; they keep showing the notice with a
link to the release page.

## Check a release by hand

```sh
gh release download v0.6.4 --repo collyn/lunaris -p SHA256SUMS -p SHA256SUMS.sig -p 'lunaris-client_*_amd64.deb'
# Public key in PEM form, from the base64 one in verify.rs:
{ printf '\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00'; echo '<key>' | base64 -d; } \
  | openssl pkey -pubin -inform DER -out release-pub.pem
openssl pkeyutl -verify -pubin -inkey release-pub.pem -rawin -in SHA256SUMS -sigfile SHA256SUMS.sig
sha256sum --ignore-missing -c SHA256SUMS
```

## Replacing the key

If the key is lost or leaks: create a new one as above, set the secret,
change the constant and publish a release. Programs that carry the old key
cannot check releases signed with the new one; their users update once by
hand from the release page.

## How each install updates and restarts

| Install | Detected by | Update | Restart |
|---|---|---|---|
| `.deb` / `.rpm` / Arch package | `/usr/bin/lunaris-{client,agent}` owned by dpkg / rpm / pacman | `pkexec apt-get install -y` (or `dpkg -i`), `pkexec dnf install -y`, `pkexec pacman -U --noconfirm`; the other Lunaris package is updated in the same transaction when it is installed | The window quits and starts the new program; the package scripts run `systemctl try-restart lunaris-capture.service`; a headless agent (`--cli`, started at login) notices its program file was replaced and re-executes itself |
| AppImage | `$APPIMAGE` / `$APPDIR` | The new AppImage is copied next to the old one and renamed over it (mode kept, +x) | The window quits and starts the new AppImage |
| Windows installer | `uninstall.exe` and `install.log` beside the program | A hidden helper waits for the program to exit, runs the NSIS installer with UAC (`/S /D=<folder>`) and starts the program again; the installers stop and start the `LunarisAgentService` service | By the helper |
| Development build, zip / tar.gz, Docker, macOS | anything else | Notice and a link to the release page only | — |
| Standalone server | — | A log line when a newer release exists (`LUNARIS_NO_UPDATE_CHECK=1` turns it off) | — |

The app and the agent check `https://api.github.com/repos/collyn/lunaris/releases/latest`
(stable releases only) about 20 seconds after they start and then daily,
with the User-Agent `lunaris-updater` and nothing else; "Check for updates
automatically" in their settings turns that off. They never install an older
version.
