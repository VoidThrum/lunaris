# Third-party components in Lunaris packages

Lunaris is free software, licensed under the GNU General Public License v3.0 or
later (`GPL-3.0-or-later`, see `LICENSE`). Its corresponding source code is the
Lunaris repository at the tag of the release: https://github.com/collyn/lunaris
(plus https://github.com/collyn/lunaris-media at the revision pinned in
`Cargo.lock`).

The prebuilt packages also contain the third-party components below. All are
GPL-3.0-compatible; nothing "nonfree" is included. The LGPL libraries are
dynamically linked shared objects that you may replace. Licence texts are
shipped next to this file (AppImage: `usr/share/licenses/`).

## Linux AppImages (agent and client) and distro packages

| Component | Licence | Source |
|-----------|---------|--------|
| Qt 6.8 (Core, Gui, Qml, Quick, QuickControls2, Widgets, WaylandClient, DBus, plugins) | LGPL-3.0 | https://download.qt.io/official_releases/qt/6.8/ |
| FFmpeg 8.1 (libavcodec, libavutil, libswscale, …), built with `--enable-gpl`; linked statically into the AppImage binaries | GPL-2.0-or-later (as built) | https://github.com/FFmpeg/FFmpeg/tree/n8.1 |
| x264 (stable branch; commit in `x264/VERSION`), statically linked | GPL-2.0-or-later | https://code.videolan.org/videolan/x264 |
| SVT-AV1 2.3, statically linked | BSD-3-Clause-Clear + AOM Patent License 1.0 (royalty-free) | https://gitlab.com/AOMediaCodec/SVT-AV1 |
| dav1d 1.5 (statically linked) | BSD-2-Clause | https://code.videolan.org/videolan/dav1d |
| libva 2.22 | MIT | https://github.com/intel/libva |
| oneVPL dispatcher (libvpl) 2.14 | MIT | https://github.com/intel/libvpl |
| nv-codec-headers 12.2 (headers only) | MIT | https://github.com/FFmpeg/nv-codec-headers |
| FFmpeg 7.1 without codecs (libavcodec, libavformat, libavutil, libswscale, libswresample), for Qt Multimedia'"'"'s FFmpeg plugin (client AppImage only) | LGPL-2.1-or-later | https://github.com/FFmpeg/FFmpeg/tree/n7.1 |
| ICU 73.2 data library, reduced to the locales Qt Core needs (`scripts/packaging/icu-data-filter.json`); the code libraries are Qt'"'"'s own | Unicode License (ICU) | https://github.com/unicode-org/icu/releases/tag/release-73-2 |
| Other libraries copied from Ubuntu 22.04 (e.g. libopus, OpenSSL, ICU, xkbcommon, fontconfig) | BSD / Apache-2.0 / Unicode / MIT | https://packages.ubuntu.com/jammy/ |

The exact build recipe (versions and FFmpeg `configure` flags) is
`scripts/packaging/Containerfile.appimage` in the Lunaris repository.

## Windows packages

| Component | Licence | Source |
|-----------|---------|--------|
| Qt 6 (deployed with `windeployqt`) | LGPL-3.0 | https://download.qt.io/official_releases/qt/ |
| FFmpeg 8.1, trimmed, cross-built with mingw-w64 (`scripts/packaging/windows/build-ffmpeg-mingw.sh`) | GPL-2.0-or-later (as built) | https://github.com/FFmpeg/FFmpeg/tree/n8.1 |
| x264, SVT-AV1 2.3, dav1d 1.5 (linked into the FFmpeg DLLs) | GPL-2.0-or-later / BSD-3-Clause-Clear + AOM patents / BSD-2-Clause | see the Linux table |
| AMF and nv-codec-headers (headers only) | MIT | https://github.com/GPUOpen-LibrariesAndSDKs/AMF, https://github.com/FFmpeg/nv-codec-headers |
| opus, OpenSSL (from vcpkg) | BSD-3-Clause / Apache-2.0 | https://github.com/microsoft/vcpkg |

## Cursor images

The remote-cursor images of the web and desktop clients are the Bibata
Modern Classic cursors (GPL-3.0, https://github.com/ful1e5/Bibata_Cursor);
see `assets/cursors/README.md`.

## Logos of other programs

The web and desktop clients show the logo of the program a computer of
another type runs, beside that type's name ("Add computer", the ways to
connect on a computer's card), only to say which kind of computer is meant.
The files are copied unchanged from those programs' sources:

| File | Origin | Licence |
|------|--------|---------|
| `web/src/assets/logos/rustdesk.svg`, `client-qml/qml/logos/rustdesk.svg` | RustDesk (https://github.com/rustdesk/rustdesk), `res/logo.svg`, Copyright (c) Purslane Tech Pte. Ltd. and contributors | AGPL-3.0 |
| `web/src/assets/logos/sunshine.svg`, `client-qml/qml/logos/sunshine.svg` | Sunshine (https://github.com/LizardByte/Sunshine), `sunshine.svg`, Copyright (c) LizardByte and contributors | GPL-3.0 |

"RustDesk" and "Sunshine" and their logos are trademarks of their respective
owners. Lunaris is not affiliated with or endorsed by them; the names and
logos are used only to identify the kind of computer being connected to.

## RustDesk protocol definitions and ported code

The agent's RustDesk-compatible listener (`agent/src/rustdesk.rs` and
`agent/src/rustdesk/`, off unless `rustdesk_port` is set) contains material
from RustDesk (https://github.com/rustdesk/rustdesk, commit
3e953e886a874956d8351d6c25e340ea82dd2343, version 1.5.3), which is licensed
under the GNU Affero General Public License v3.0 (AGPL-3.0):

- `agent/proto/rustdesk/message.proto`, copied from
  `libs/base/protos/message.proto`; the Rust types generated from it are
  compiled into the agent.
- `agent/src/rustdesk/qos.rs` and `agent/src/rustdesk/qos/tests/`, copied from
  `src/server/video_qos.rs` and `src/server/video_qos/tests/` (with
  `base_bitrate`, `Quality` and `calc_bitrate` from `libs/scrap`).
- The other files of the listener follow RustDesk's sources function by
  function (each step names the function it was ported from): the wire
  protocol, login, input handling, cursor, clipboard, file transfer and
  codec negotiation.
- `agent/src/rustdesk/files.rs` (the host's side of RustDesk's file
  transfer, for a RustDesk client's file manager) follows
  `src/server/connection.rs` (the file-transfer login, `FileAction`,
  `FileResponse`), `src/ui_cm_interface.rs` (`handle_fs`) and
  `libs/base/src/fs.rs` (`read_dir`, `TransferJob`,
  `is_write_need_confirmation`, `handle_read_jobs`).
- `agent/src/connector/rustdesk.rs` (the agent as a RustDesk client, for
  showing a RustDesk computer) follows the client's side the same way:
  `src/client.rs`, `src/client/io_loop.rs`, `src/keyboard.rs` and
  `src/ui_session_interface.rs`.

GPL-3.0 section 13 permits combining the two; the AGPL-3.0 continues to apply
to that part.

## Sunshine / GameStream connector (ported code)

`agent/src/connector/sunshine/` is the client's side of the GameStream
protocol, so that a computer running Sunshine can be shown and controlled.
Lunaris does not contain or install Sunshine. The code is ported to Rust from
the projects below; each ported piece names its file and function in a
comment.

| Source | Licence | What was ported |
|--------|---------|-----------------|
| moonlight-common-c (https://github.com/moonlight-stream/moonlight-common-c), Copyright (c) Moonlight Game Streaming Project | GPL-3.0 | RTSP exchange and SDP (`RtspConnection.c`, `SdpGenerator.c`), control stream (`ControlStream.c`), input packets (`InputStream.c`, `Input.h`), video and audio receiving (`RtpVideoQueue.c`, `VideoDepacketizer.c`, `VideoStream.c`, `RtpAudioQueue.c`, `AudioStream.c`), order of connection stages (`Connection.c`) |
| moonlight-embedded, libgamestream (https://github.com/moonlight-stream/moonlight-embedded), Copyright (c) Iwan Timmer and contributors | GPL-3.0-or-later | pairing, serverinfo, application list, launch/resume/quit, client certificate (`client.c`, `http.c`, `mkcert.c`, `xml.c`) |
| moonlight-qt (https://github.com/moonlight-stream/moonlight-qt), Copyright (c) Moonlight Game Streaming Project | GPL-3.0 | checks and time-outs of pairing and requests (`app/backend/nvpairingmanager.cpp`, `nvhttp.cpp`) |
| nanors (https://github.com/sleepybishop/nanors), Copyright (c) 2021 Joseph Calderon | MIT | Reed-Solomon erasure code of the video and audio streams (`rs.c`) |
| Sunshine (https://github.com/LizardByte/Sunshine), Copyright (c) LizardByte and contributors, and its library libvirtualhid | GPL-3.0 | read as the host's side of the protocol (what the host expects and sends; key tables); the host used in the connector's tests is written from `nvhttp.cpp`, `rtsp.cpp`, `stream.cpp`, `crypto.cpp`, `input.cpp` and is not part of any package |

Libraries the connector links into the agent (all packages):

| Component | Licence | Source |
|-----------|---------|--------|
| rusty_enet (Rust port of ENet, Copyright (c) Lee Salzman and contributors) | MIT | https://github.com/jabuwu/rusty_enet |
| rsa, aes, aes-gcm, cbc (RustCrypto) | MIT OR Apache-2.0 | https://github.com/RustCrypto |

## Other ported code

- `agent/src/connector/keys.rs`: the key table is generated from rdev, the
  keyboard library of RustDesk's client (https://github.com/rustdesk-org/rdev,
  MIT License, Copyright (c) 2020 Nicolas Patry): `src/keycodes/windows.rs`
  joined with `src/keycodes/linux.rs`.
- `agent/src/video/vp9.rs`: the VP9 RTP payloader is ported from pion/rtp
  (https://github.com/pion/rtp, MIT License, Copyright (c) The Pion
  community): `codecs/vp9_packet.go` and `codecs/vp9/header.go`.

Libraries the listener links into the agent (all packages):

| Component | Licence | Source |
|-----------|---------|--------|
| Zstandard (libzstd, built from the `zstd-sys` crate, statically linked) with the `zstd` and `zstd-safe` crates | BSD-3-Clause (libzstd is dual BSD-3-Clause / GPL-2.0; used under BSD) and MIT (crates) | https://github.com/facebook/zstd, https://github.com/gyscos/zstd-rs |
| xxhash-rust | BSL-1.0 | https://github.com/DoumanAsh/xxhash-rust |
| prost | Apache-2.0 | https://github.com/tokio-rs/prost |
| regex, socket2 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex, https://github.com/rust-lang/socket2 |

## Hardware encoders and drivers

Hardware encoders (VAAPI, NVENC, AMF, Quick Sync) and NvFBC run in the GPU
vendor's driver, which is not bundled; Lunaris loads those libraries at
runtime.
