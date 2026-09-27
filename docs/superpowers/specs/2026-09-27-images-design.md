# Images

Show received images inline and send images, without leaving the keyboard
more than necessary.

## Decisions

- Received images are downloaded as soon as they arrive, so they are ready
  when the chat is opened and outlive Signal's CDN (~45 days). Images of
  imported history, and any whose download failed or was cut short, are
  fetched when their chat is opened.
- Thumbnails are shown in the bubble; clicking one opens the file in the
  system's default image viewer.
- Images are sent by pasting (`Ctrl+V`), dropping files on the chat, or
  picking files (`Ctrl+O`). They are staged above the compose box; the text
  becomes the caption and `Enter` sends both. `Backspace` in an empty compose
  box unstages the last image.
- Only `image/*` attachments are images. Other attachments stay `📎 name`,
  stickers `[sticker]`, and view-once media is shown as `[view-once media]`
  and never downloaded.
- Messages stored before this change keep their `📎 image/jpeg` text; there
  is no pointer left to download them from.

## Storage

```
accounts/<id>/attachments/<attachment id>.<ext>
```

A new table in `messages.sqlite`, one row per image, inserted in the same
transaction as its message and only if the message itself is new:

```
attachments(id, message_id, idx, content_type, file_name, width, height,
            pointer BLOB, file)
```

`pointer` is Signal's protobuf-encoded `AttachmentPointer` (CDN location and
keys). `file` is `NULL` until the image is downloaded. Sent images store the
pointer returned by the upload and are written from memory, so they can be
downloaded again if the local copy is lost.

Deleting a message (remote delete, disappearing-message purge) deletes its
rows and returns their files, which the controller removes. A download that
completes after its message was deleted removes its file too.

The chat-list preview and notifications mark images: `🖼 Photo`,
`🖼 3 photos`, or `🖼 caption`.

## Flow

- **Receive:** `convert` turns image attachments into `NewAttachment`s on the
  message (pointer bytes, type, name, dimensions). A message with images but
  no text is kept. After storing a live message, the controller requests
  downloads of the chat's missing images.
- **Download:** `Backend::download(pointer, dest)` decrypts via presage and
  writes `dest` (through a `.part` file), at most 3 at a time. The
  controller tracks in-flight and failed downloads per account in memory;
  failures are shown as "Image not available" and retried when the chat is
  opened again. Offline counts as a failure.
- **Send:** the UI loads and checks the file (must be `image/*`, at most
  100 MiB), decodes it off the main thread for its dimensions and a
  thumbnail, and stages it. `Backend::send` uploads the images, sends one
  `DataMessage` with the text and attachments, and returns the pointers. The
  controller then stores the message and writes the images to disk. On
  failure, text and images go back into the compose area.

## UI

- Thumbnails fit 220×220 px and are sized from the known dimensions before
  they load, so the list doesn't jump. They're decoded in a thread and
  cached for the open chat.
- While an image downloads, a placeholder of its size with a spinner is
  shown. An image GTK can't decode (e.g. HEIC) is shown as a clickable
  `🖼 name` link.
- Staged images are a row of small thumbnails with ✕ above the compose box;
  its placeholder then reads "Add a caption".
- While the file chooser is open, the window doesn't hide on focus loss
  and drops to the bottom layer, out of the chooser's way. Dialogs and the
  image viewer get no parent window when the window is a layer surface:
  compositors disconnect a client that parents a dialog to one.

## Testing

- `convert`: image attachments become `NewAttachment`s; non-images stay `📎`;
  image-only messages are kept; view-once media is not downloaded.
- `store`: attachments are stored with their message and not duplicated;
  missing images are listed and marked downloaded; deletes and purges return
  files; previews mark images.
- Demo mode includes an image, to try the UI.
