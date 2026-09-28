# Voice memos

Play voice memos (and other audio attachments) inside the app.

## Decisions

- Audio attachments are downloaded like images: on arrival, or when their
  chat is opened for history and failed downloads.
- Each memo gets a player in its bubble: play/pause, a seekable progress
  bar, elapsed and total time, and, while it is the one playing, a speed
  button cycling 1× → 1.5× → 2×. The speed holds for the session.
- One memo plays at a time. Playing keeps going when switching chats.
- When a memo ends, the next one plays if it belongs to the very next
  message (or the same message), like Signal.
- `Ctrl+Space` in a chat pauses or resumes the memo playing in it;
  otherwise it plays the chat's first unplayed memo, or the newest one if
  all were played. (Input methods that use `Ctrl+Space` take it first.)
- A dot marks memos not played yet. Received memos start unplayed; our own
  and imported history start played. Playing a memo marks it played.
- Only receiving; recording is out of scope.

## Playback

GStreamer's `GstPlay` (via `gstreamer-play`), not GTK's media stream, which
has no rate control. A `scaletempo` filter keeps the pitch at higher speeds.
Signal's memos are AAC, in MP4 or ADTS; the package gains the GStreamer
plugins for that (base, good, bad for the player library, libav for AAC).

Durations aren't in the attachment pointer. A memo's duration is probed
with `gstreamer-pbutils`' Discoverer in a thread the first time it is shown,
then stored.

## Storage

`attachments` gains three columns, added to existing databases on open:

```
kind         'image' | 'voice' | 'audio'   (voice: Signal's voice-message flag)
played       0 | 1
duration_ms  NULL until known
```

`NewAttachment` gets a `kind`; `played` follows the message's read state
when stored. Previews and notifications: `🎤 Voice message`, `🎵 Audio`,
or `🎤 caption`.

## UI

`ui/voice.rs` holds the player and the bubble controls. The chat view
registers each rendered memo's controls with the player by
(account, attachment); only the current memo's controls get position
updates. Re-rendering the chat re-registers them, so playback survives
refreshes. The player reports "played" and "duration known" as UI events;
the controller stores them.

## Testing

- `convert`: audio attachments become `voice`/`audio` by the flag.
- `store`: kinds, played and durations round-trip; old databases gain the
  columns; previews name voice messages.
- The rule for which memo plays next and for `Ctrl+Space`, as pure
  functions.
- Demo mode has two unplayed memos in a row in Bob's chat (generated
  WAVs).
