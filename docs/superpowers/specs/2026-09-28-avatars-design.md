# Profile pictures

Show contacts' and groups' pictures in the chat list, the chat header, next
to senders in group chats, and in notifications.

## Decisions

- Chat list: a 28px round picture left of each chat; rows keep their
  height. Chat header: a 22px one before the title.
- Group chats: the first message of a sender's run gets their 24px picture
  and name; the rest of the run is indented to line up. (Until now the
  name was repeated on every message.)
- Without a picture: the name's initials on a colour derived from the
  contact or group id, like Signal. Phone numbers get `#`.
- Notifications: a direct chat shows the contact's picture; a group
  message shows the sender's, else the group's. Without a picture the
  notification has no icon, as before.
- Pictures are refreshed at most once a day per chat: Signal serves the
  profile to learn whether the picture changed, so every check is a
  request.

## Fetching

The backend gets `fetch_avatar(conversation, dest)`: for a contact, the
profile picture via presage (needs the contact's ACI and profile key; none
means no picture); for a group, the group avatar. It shares the three
download slots with attachments and writes through a `.part` file.

The controller decides what to fetch, for ready accounts only: every chat
with activity (checked on each refresh against an in-memory copy of the
table below), and the senders of a group chat when it is opened.

## Storage

`accounts/<id>/avatars/<hash>-<fetched_at>.img`, recorded in
`messages.sqlite`:

```
avatars(subject TEXT PRIMARY KEY,   -- ConversationId string, d:… or g:…
        file TEXT,                  -- NULL: has no picture
        fetched_at INTEGER NOT NULL)
```

A new file name per fetch makes stale cached textures impossible; the
previous file is deleted. Failed fetches aren't recorded; they're
remembered in memory and retried on the next start.

## UI

`ui/avatar.rs`: one widget for a picture or initials at a given size, with
decoded textures cached by path.

## Testing

- Initials and colour choice, as pure functions.
- `store`: avatars round-trip and replacing one returns the old file.
- Demo: Alice and Mum have generated pictures, everyone else initials.
