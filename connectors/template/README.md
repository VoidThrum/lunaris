# Lunaris connector template

A connector lets the Lunaris agent show a computer that speaks another
protocol: the connector logs in to that computer and hands the agent its
picture and sound as that computer encodes them. The agent does not decode
or encode anything; it packs the frames into its normal session with the
viewer (browser or desktop client).

This folder is a minimal **external** connector, to copy and start from. It
connects to nothing: it plays `pattern.ivf` (a 320 x 180 VP8 test pattern)
in a loop, sends silence, moves a cursor, and answers every command, so
every message of the protocol below is exercised once. It uses only the
Rust standard library, `serde` and `serde_json`; a connector can be written
in any language that can read and write its standard input and output.

This file is the reference of the protocol, **version 1**.

## Installing a connector

A connector is a program the user chose to run, with the user's own rights.
The agent never downloads or installs one. It exists when the user created
its folder:

```
<agent data dir>/connectors/<id>/
    connector.json      the manifest
    <the program>       and whatever else it needs
```

`<agent data dir>` is the folder of the agent's config file
(`~/.config/lunaris-agent` on Linux, `%APPDATA%\lunaris-agent` on Windows).
`<id>` is letters, digits, `-` and `_`.

The agent starts the program only for a target that names this connector,
once per session, with the connector's folder as the working directory, and
stops it when the session ends.

To try this template:

```
cargo build --release -p connector-template
mkdir -p ~/.config/lunaris-agent/connectors/template
cp target/release/connector-template connectors/template/connector.json \
   connectors/template/pattern.ivf ~/.config/lunaris-agent/connectors/template/
```

## The manifest: `connector.json`

```json
{
  "id": "template",
  "name": "Test pattern (template connector)",
  "command": "connector-template",
  "args": [],
  "fields": [
    { "key": "file", "label": "IVF file", "kind": "text" },
    { "key": "fps", "label": "Frames a second", "kind": "text", "default": "30" }
  ],
  "capabilities": { "video": ["vp8", "vp9", "av1"], "audio": true, "input": true, "clipboard": true, "cursor": true, "setup": true }
}
```

| Field | Meaning |
|---|---|
| `id` | The same as the folder's name. Targets name the connector by it. |
| `name` | What a person sees. |
| `note` | Optional: one or two plain sentences shown under the Type choice of "Add computer", for what the person must know before adding one ("X must already be installed and running on that computer."). In English. |
| `command` | The program: a file **inside the connector's folder** (a path that leads out of it is refused). |
| `args` | Arguments for it, optional. |
| `fields` | What a target is described by. `kind` is `text`, `port`, `secret` or `choice`; `default` and `required` are optional. A `secret` is never shown or written to a log. |
| (a `choice` field) | Has `"options": [{"value": "picture", "label": "In the picture"}, ...]`: the person presses one of them instead of typing, and its `value` is what you get in `target`. A client from before choices shows a text box in its place, so take the same words typed by hand, and say what is wrong with any other in your error. The agent does not check the value against the options. |
| (a field's `key`) | Not `id`, `name`, `connector` or `group`: those are the agent's own words for a target, and a field called so is left out of the form's values. A field with the key `host` or `address` is taken for where the computer is: when a computer is added at an address that another of the agent's computers (of any kind) has, the two are shown as one computer with a way to connect for each (the agent gives them a common `group`, an opaque word; the address itself still never leaves the agent). |
| `capabilities.video` | The codecs the connector can deliver: `h264`, `h265`, `vp8`, `vp9`, `av1`. |
| `capabilities.audio`, `input`, `clipboard`, `cursor` | What else it does. |
| `capabilities.setup` | Adding a computer has a set-up of its own, such as a pairing (see "Set-up" below). Left out or `false`: the connector is never started for one. |
| `capabilities.files` | The computer's files can be browsed and copied (see "Files" below). Left out or `false`: the connector is never started for files, and the viewer is told "This kind of computer has no file transfer". |

A target is the connector's id and a value for each field:
`{"connector": "template", "fps": "30"}`.

A computer to show is added in the agent's settings ("Other computers"),
which writes it to the agent's config file as a source: the target with an
id and a name.

```json
"sources": [
  { "id": "pattern", "name": "Test pattern", "connector": "template", "fps": "30" }
]
```

Viewers then choose it in the stream's toolbar. Only the id and the name
leave the computer; the fields (addresses, passwords) stay in the config
file. Whoever may use the computer may use its sources.

## Messages

Both directions use the same framing, on the program's standard input
(from the agent) and standard output (to the agent):

```
u32, little endian   length of the header in bytes   (at most 65 536)
u32, little endian   length of the payload in bytes  (at most 67 108 864)
header               a JSON object with a "type"
payload              bytes; what they are depends on the type; may be empty
```

Standard output carries nothing else. Standard error is free: each line
goes to the agent's log (at debug level). **Never write there what a person
typed, the clipboard, or a password.**

A message whose type the receiver does not know is skipped, so a later
agent can add messages without breaking a connector.

### The start

| # | Direction | Header | |
|---|---|---|---|
| 1 | agent → connector | `{"type":"hello","version":1}` | |
| 2 | connector → agent | `{"type":"hello","version":1}` | Within 10 s. The agent goes on only when the versions are the same. |
| 3 | agent → connector | `{"type":"connect","target":{...},"viewer":{"codecs":["vp8","h264"],"fps":60}}` | `target` is the connector's id and the values of its fields (and what its set-up returned, if it has one). `viewer.codecs` is what the viewer can decode, the preferred first. `viewer.width`, `viewer.height` (pixels) and `viewer.bitrate_kbps` may be there too: the size and the bitrate the viewer asked for, 0 or absent = no wish (for a computer that sends in the size and at the rate a client names; ignore them otherwise). |

After `connect` the connector logs in to the computer and sends events. If
it cannot (wrong password, not reachable, no codec in common), it sends
`closed` with a reason for the person and ends.

### Events (connector → agent)

| Header | Payload | |
|---|---|---|
| `{"type":"video","codec":"vp8","key":true,"capture_time_ms":1234}` | one encoded frame | One whole frame (access unit). H.264 and H.265 as Annex-B, with the parameter sets in key frames. `codec` must be one of `viewer.codecs` and the same for the whole session. `capture_time_ms` is optional: the other computer's own time for the frame, on any clock that moves steadily forward; the agent uses it for the frame's RTP time while it stays in step with the agent's clock, else the time of arrival. The first frame must be a key frame. |
| `{"type":"audio","channels":2}` | one Opus packet | 48 kHz. |
| `{"type":"cursor_shape","width":16,"height":16,"hot_x":0,"hot_y":0}` | `width * height * 4` bytes, RGBA | |
| `{"type":"cursor_position","x":10,"y":20,"visible":true}` | – | In pixels of the display shown. |
| `{"type":"cursor_embedded","embedded":true}` | – | The pointer is part of the picture now (`true`), so the viewer must not draw its own; or no longer (`false`). For a moment such as a window being moved: viewers honour it for a `windows` computer (`platform`) while a mouse button is held. A pointer that is in the picture all the time: `cursor_always_embedded`. |
| `{"type":"cursor_maybe_embedded","maybe":true}` | – | The pointer *may* be part of the picture now (the connector cannot know, only that it is a moment in which it would be, such as a window being moved). The viewer keeps its own pointer, looks for the computer's in the picture and hides its own only when it finds it. `false`: the moment is over. Use `cursor_embedded` when you know. |
| `{"type":"cursor_always_embedded"}` | – | The pointer is part of the picture for the whole session and you send no `cursor_shape` or `cursor_position` (the other computer always draws its pointer into what it sends). Once, at the start. The viewer then shows no pointer at all over the picture, neither one it draws nor its own system's: the only pointer is the one in the picture. Unlike `cursor_embedded` it holds whatever the kind of computer and whether or not a button is held. An agent from before this message logs "a message that is not understood" and goes on; its viewers keep their own pointer. |
| `{"type":"platform","os":"windows"}` | – | What kind of computer it is: `windows`, `linux`, `macos`, `android`. Once, at the start. |
| `{"type":"latency","ms":12}` | – | The delay between your connector and the other computer, there and back (a round trip), in milliseconds, as your protocol measures it; if it only knows one way, send twice that. A JSON number, whole (`12`) or with a fraction (`0.4`) when you know it that finely; not negative. When it is known and again when it changes, about once a second at most. The viewer adds it to its own round trip to the agent and shows the sum as "Ping". Optional: never sent = the viewer shows only its round trip to the agent. An agent from before this message logs "a message that is not understood" and goes on. |
| `{"type":"clipboard"}` | the text, UTF-8 | The other computer's clipboard changed. Text of at most 1 MiB; the agent does not pass on the same text twice in a row, nor text it has just sent itself. |
| `{"type":"clipboard_error"}` | the reason, UTF-8 | The clipboard cannot be shared (the other computer does not allow it, the text is too large). Shown to the viewer; say it once, not for every attempt. |
| `{"type":"displays","displays":[{"name":"1","width":1920,"height":1080}],"current":0}` | – | At the start, and when the list, a size or the display shown changes. |
| `{"type":"closed","reason":"Wrong password"}` | – | The end. Nothing follows; the program exits. |

The agent reads events as fast as the viewer's connection takes them. When
it is slow the connector's writes block: do not queue pictures without
bound behind a blocked write; pass the slowness on to the other computer
(most protocols then lower their rate) or drop frames and send a key frame
next.

### Commands (agent → connector)

| Header | Payload | |
|---|---|---|
| `{"type":"pointer_move","x":10,"y":20}` | – | In pixels of the display shown. |
| `{"type":"pointer_move_relative","dx":3,"dy":-4}` | – | The pointer moved by this much (a viewer that holds the pointer, as games want it). |
| `{"type":"pointer_button","button":"left","down":true}` | – | `left`, `middle`, `right`, `back`, `forward`. |
| `{"type":"wheel","dx":0,"dy":-1}` | – | Notches; `dy` > 0 is up, `dx` > 0 right. |
| `{"type":"key","position":30,"down":true}` | – | A key by where it is on the keyboard: the Linux input key code (`KEY_A` is 30). The other computer's layout decides what it types. |
| `{"type":"text"}` | the text, UTF-8 | Text to type as it is. |
| `{"type":"clipboard"}` | the text, UTF-8 | The viewer's clipboard changed. |
| `{"type":"request_key_frame"}` | – | The viewer cannot go on without a whole picture. Answer with a key frame soon. |
| `{"type":"quality","fps":30,"bitrate_kbps":0}` | – | What the viewer would like; 0 is "no wish". |
| `{"type":"switch_display","index":1}` | – | An index into the last `displays`. |
| `{"type":"close"}` | – | Send `closed` and exit. The agent also closes the program's input and stops the program 2 s later if it is still there. |

## Files

The Lunaris file manager works on a computer of another type when its
connector says `"files": true` in its capabilities. A file session is a
session of its own, beside any picture session: the agent starts the
program once more, says `hello` as above, and then sends `open_files` in
the place of `connect`:

| # | Direction | Header | |
|---|---|---|---|
| 3 | agent → connector | `{"type":"open_files","target":{...}}` | The target as in `connect`. |
| 4 | connector → agent | `{"type":"files_ready","os":"windows","sep":"\\","home":"C:\\Users\\An","roots":[{"name":"C:","path":"C:\\"}]}` | Within 30 s, after logging in. `sep` is the path separator there; `home` the folder to start in (leave it out when it cannot be told: `""` as a path then means "where you start"); `roots` the places to offer (drives, the home folder). When the files cannot be opened: `{"type":"closed","reason":"..."}`, a text for the person (wrong password, file transfer switched off there, ...). |

Then requests and answers. Every request has an `id` (the requester's
text) and every answer names it. Paths are the other computer's own, as
text. Any failure is `{"type":"file_error","id":"...","message":"..."}`
with a text for the person; the session goes on.

| Request (agent → connector) | Answer (connector → agent) | |
|---|---|---|
| `{"type":"file_list","id":"1","path":"C:\\Users","hidden":false}` | `{"type":"file_dir","id":"1","path":"C:\\Users","parent":"C:\\","entries":[{"name":"An","dir":true,"size":0,"mtime":1700000000,"hidden":false}]}` | `parent` is left out or `null` where nothing is above. `mtime` is seconds since 1970 (0: not known). `hidden` entries only when asked for. |
| `{"type":"file_stat","id":"2","path":"..."}` | `{"type":"file_stat","id":"2","entry":{...}}` | `entry` is `null` when nothing is there by that name. |
| `{"type":"file_walk","id":"3","path":"..."}` | `{"type":"file_walk","id":"3","path":"...","files":[{"rel":"a/b.txt","size":2,"mtime":0}],"dirs":["a"]}` | Every file under a folder, for copying it: `rel` is its path from the folder, written with `/`. |
| `{"type":"file_mkdir","id":"4","path":"..."}` | `{"type":"file_ok","id":"4"}` | The folders above it too. |
| `{"type":"file_remove","id":"5","path":"..."}` | `{"type":"file_ok","id":"5"}` | A file, or a folder with what is in it. A link is removed itself, not what it leads to. |
| `{"type":"file_rename","id":"6","path":"...","name":"new name"}` | `{"type":"file_ok","id":"6"}` | In the same folder. Not onto a name that is taken. |
| `{"type":"file_read","id":"7","path":"...","offset":0}` | `{"type":"file_read_start","id":"7","size":10,"mtime":1700000000,"offset":0}`, then `{"type":"file_read_data","id":"7"}` with the next bytes as payload, as often as needed, then `{"type":"file_read_end","id":"7"}` | A download. `offset` > 0: the viewer has that much already; send from there. The sizes of the payloads are the progress. Keep a payload under a few megabytes. One download at a time. |
| `{"type":"file_write","id":"8","path":"...","size":6,"mtime":1700000000,"on_exists":"rename"}` | `{"type":"file_write_ready","id":"8","offset":0,"path":"..."}` or `{"type":"file_write_skip","id":"8","reason":"identical","path":"..."}` | An upload. `on_exists` says what to do when another file has that name: `rename` (keep both: write under another name and say which in `path`), `replace`, `skip` (answer `file_write_skip` with reason `exists`). The same file there already (size and time): `file_write_skip` with reason `identical`. `offset` > 0: this much of this same file is there from an upload that broke off; the data continues from there. |
| `{"type":"file_write_data","id":"8"}` with the next bytes as payload, then `{"type":"file_write_end","id":"8"}` | `{"type":"file_write_done","id":"8","path":"..."}` | The file is whole only now: until then keep it under another name, so that a broken upload leaves no half file under the real one. |
| `{"type":"file_cancel","id":"7"}` | for a download: `{"type":"file_read_end","id":"7","cancelled":true}`, and nothing more of it; for an upload: nothing | Stops a transfer. |
| `{"type":"close"}` | `{"type":"closed","reason":"Closed"}` | The end of the session. |

The built-in RustDesk connector (`agent/src/connector/rustdesk_files.rs`)
is an implementation of the same interface (`agent/src/connector/files.rs`).

This template has no files: its manifest does not say `files`, so the agent
never asks; if it is sent `open_files` all the same it answers `closed`
with "This kind of computer has no file transfer".

## Set-up (pairing)

Some computers must be paired with once before they can be shown: the
connector makes a PIN, the person types it at the other computer, and the
connector gets a certificate or a key to keep. A connector that says
`"setup": true` in its capabilities is asked to do this once, when a
computer of its kind is added (never for a connector that does not say so,
and never again for the same computer). It is a run of its own: the agent
starts the program, says `hello` as above, and then sends `setup` in the
place of `connect`:

| # | Direction | Header | |
|---|---|---|---|
| 3 | agent → connector | `{"type":"setup","target":{...}}` | The target as the person filled it in. |
| 4 | connector → agent | `{"type":"setup_step","message":"Enter this PIN at the other computer.","code":"1234"}` | Zero or more times: something to show the person now. `message` is shown as it is (write it in plain English); `code` is optional and shown large. Each step replaces the one before. |
| 5 | connector → agent | `{"type":"setup_done","values":{"client_key":"...","server_cert":"..."}}` | The end: the computer is added. `values` (text by name; may be empty or left out) is kept with the target in the agent's config file and is part of `target` in every later `connect` and `open_files`. It is never shown to anyone, never sent to a server or a viewer, and stays when the person edits the computer. The names `id`, `name`, `connector` and `group` are not kept. A value under the name of one of the form's fields replaces what was typed. |
| 5 | connector → agent | `{"type":"closed","reason":"Wrong PIN"}` | The end: nothing is added, and the person is shown the reason. |

The person sees the first step when it comes within 3 seconds of adding
(otherwise "Getting ready..." until it comes). A set-up that is not over
after 5 minutes is given up on ("Pairing was not completed in time"), and
so is one whose window the person closes: the agent stops the program, so
do not rely on a goodbye. A target that needs no set-up answers
`setup_done` at once.

**Never write the code, a key or a certificate to standard error.**

This template has a pretend one, to try the screens with: add a computer
with the field "Demo pairing" set to `1`. It shows the PIN `0000` and waits
until a file named `paired` exists in the connector's folder (`touch
~/.config/lunaris-agent/connectors/template/paired`), then returns two
values; a file named `refused` makes it fail with "The other computer
refused the pairing". With the environment variable
`LUNARIS_TEMPLATE_PAIR_SECONDS=5` in the agent's environment it succeeds by
itself after that many seconds. Remove the file again before the next try.

## What this template does with them

- `video`: the frames of the IVF file named by the target's `file`
  (`pattern.ivf` when empty) at the target's `fps`, in a loop.
  `request_key_frame` starts the loop again at its first frame (a recording
  has no other key frame to give); `quality` changes the rate.
  With the target's `then_file` and `then_after`, it changes to a second
  file after that many seconds: with another codec this shows what the
  agent does when a computer changes its codec in the middle of a session
  (it tells the viewer to start the session again).
- `audio`: 20 ms of Opus silence every 20 ms.
- `cursor_shape` once, `cursor_position` twice a second on a circle.
- `clipboard` and `pointer_move` are echoed back as `clipboard` and
  `cursor_position`, to show they arrived. The other commands are written
  to standard error by kind (never which key or what text).

- `setup`: the pretend pairing described under "Set-up" above.

`agent/tests/connector_template.rs` runs the program through the agent's
adapter (`agent/src/connector/process.rs`) and checks each of these. This
folder needs nothing of Lunaris to build: copy it out and `cargo build`.

To make another test pattern (VP8, VP9 or AV1 in an IVF file):

```
ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -t 2 -c:v libvpx -b:v 150k -g 600 -f ivf pattern.ivf
```
