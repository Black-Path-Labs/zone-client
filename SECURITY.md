# Security

## Key files

`zone` keeps everything in its state dir: `$XDG_DATA_HOME/zone`, else `~/.local/share/zone`, or
`%LOCALAPPDATA%\zone` on Windows, or the dir `--state DIR` names.

| File               | What it holds                                                                      |
| ------------------ | ---------------------------------------------------------------------------------- |
| `vault.key`        | 32 random bytes made on first use; they wrap the store key with AES-256-GCM        |
| `vault.bin`        | The SDK store key, wrapped by `vault.key`                                          |
| `index.bin`        | The SDK's item index, encrypted under the store key                                |
| `items/*.bin`      | The SDK's items (person secret, device and zone keys, zone state), each encrypted  |
| `client-sync.json` | What `zone status` reads: last and next sync times, the last failure's reason code |
| `mcp.json`         | `zone mcp`'s OAuth tokens for the hosted zone MCP server, and its endpoints        |

On Unix the state dir is mode 0700 and every file in it 0600, from the first write (the client
sets its umask to 077 at start). A dir or file that others can read or enter is refused, never
narrowed, because someone else may already have read it. Windows has no mode bits; there the
state dir relies on the user profile's ACL, which limits it to the user, SYSTEM and
Administrators.

Anyone who can read `mcp.json` can act through the hosted zone MCP server as the agent it
connected, until the zone owner revokes the grant or the refresh token runs out (30 days).

Anyone who can read `vault.key` and the rest of the state dir together can act as this device in its zone. Back
up neither to a place others can read. A lost `vault.key` cannot be recovered: join again as a
new device.

The kit key (`--key-file FILE`, or `--key-file -` for stdin) is read from a file of mode 0600 and
never from the command line or the environment, so it does not show up in `ps`, shell history or
a process's environment. The client keeps it in memory only while it runs and never writes it.

## What never leaves the device

- `vault.key`, the person secret, the store key and the device's private keys.
- The kit key. Core and the zone's admitter see only a request it signed and its fingerprint.
- Zone content in the clear. Messages, files and receipts reach Core encrypted to the zone's
  members (HPKE), and Core cannot read them.

`zone mcp` sends the agent's MCP messages, in the clear inside TLS, to the hosted zone MCP server,
which acts for the agent in the zone. That server and the zone's Core read them.

What does leave: the device's public bundle (its public keys and the device name `zone-cli`), its
per-zone pseudonym, which the zone's members see, and the encrypted envelopes it sends. No
command prints key material; `zone whoami` prints fingerprints only, and `zone status` prints a
reason code, never Core's text.

The cryptography in zone-sdk has not had an independent review yet.

## Reporting a vulnerability

Report it privately through GitHub's private vulnerability reporting: the repository's Security
tab, then Report a vulnerability. Do not open a public issue. Say what you found, how to
reproduce it, and the output of `zone --version`, which names the zone-sdk revision.
