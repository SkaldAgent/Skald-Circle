# E2E Plaintext Framing

> Defines the structure of the **bytes that are encrypted** in the `ciphertext` field of the
> `Message` frame ([relay-protocol.md §6](relay-protocol.md)). **The cryptography does not
> change** ([crypto.md](crypto.md)): a blob of bytes is always encrypted with AES-256-GCM. What
> changes is *what* those bytes are: a **versioned frame** wrapping the JSON payload.
>
> The relay remains **blind**: it sees only ciphertext, nothing about versions or compression.

---

## 1. Structure

The **plaintext** (what is encrypted) is:

```
plaintext = version (1 byte)  ‖  comp (1 byte)  ‖  payload
```

| Field | Byte | Values | Meaning |
|-------|------|--------|---------|
| `version` | 1 | `0x01` \| `0x02` | Framing version. `0x01` = JSON app payload; `0x02` = **pipe signaling** (MsgPack, see below). Unknown value → receiver discards with log. |
| `comp` | 1 | `0x00` \| `0x01` | Compression algorithm applied to `payload` (§2). |
| `payload` | N | — | The content: **JSON UTF-8** ([payloads.md](payloads.md)) for `0x01`, **MsgPack `PipeSignal`** ([pipe.md §2](pipe.md)) for `0x02`; optionally compressed. |

> **`version 0x02` (pipe signaling).** Reserved for the pipe control plane ([pipe.md](pipe.md)):
> `0x02 ‖ 0x00 ‖ <MsgPack PipeSignal>` (uncompressed). It rides this same E2E channel; a receiver
> peeks the first byte to route `0x02` to its pipe layer and `0x01` to the JSON app path. The
> existing `decompress_payload` still only accepts `0x01` — the pipe layer handles `0x02` itself.

`version` and `comp` are **in plaintext inside the plaintext** (readable only after decryption):
they cannot go in the AAD or outside the ciphertext, or the relay would see them. They are
integrity-protected by the GCM tag along with the rest.

> **Two versioning planes, do not confuse.** `version` (this byte, `0x01`) versions the
> **framing** (the binary envelope). The JSON field `v` inside the `payload`
> ([payloads.md §1](payloads.md)) versions the **payload schema**. They are independent: framing
> can evolve while a `kind`'s schema stays fixed, and vice versa. In these documents "version" =
> framing byte; "`v`" = payload schema. (The name `v` is unchanged from the original design for
> consistency with existing payloads.)

## 2. Compression

| `comp` | Algorithm | Notes |
|--------|-----------|-------|
| `0x00` | none | `payload` = JSON UTF-8 as-is. |
| `0x01` | **zlib / DEFLATE** (RFC 1950/1951) | Default for large payloads. Safe interop: Rust `flate2` ↔ iOS `Compression` framework (`COMPRESSION_ZLIB`). |
| `0x02…` | _reserved_ | E.g. `lz4` in the future. Addable without breakage: a receiver that does not know a `comp` value discards with log. |

Rules:

1. **Compress-then-encrypt, always in this order.** The ciphertext is not compressible; compressing
   after would give no gain.
2. Compression is **optional on the sender side**, **mandatory on the receiver side**: anyone
   receiving MUST handle both `0x00` and `0x01`.
3. **Threshold**: compress only if `len(payload)` exceeds ~1 KiB. Below that, the zlib header
   overhead wipes out any gain → use `0x00`.
4. Compression operates on `payload` **only**, not on the two header bytes.

## 3. Decoding (receiver side)

For each decrypted `Message` envelope:

1. AES-GCM → obtain the `plaintext` blob (AAD/anti-replay identical to the crypto contract,
   [crypto.md §6](crypto.md)).
2. Read `version = plaintext[0]`. If `!= 0x01` → discard with log.
3. Read `comp = plaintext[1]`. If unknown → discard with log.
4. `body = plaintext[2:]`; if `comp == 0x01` → decompress (zlib).
5. Parse `body` as JSON; validate `v`/`kind` ([payloads.md §6](payloads.md)); apply action
   idempotently.

## 4. No Version Disambiguation

There is no v1/v2 transport coexistence in production (clean break, no distributed v1 clients).
Therefore **no disambiguation trick is needed**: every payload is a versioned frame (`version = 0x01`).
A receiver reading a `version` different from `0x01` discards with log (§3 step 2).

## 5. Sizes & Limits

The `ciphertext` travels as **raw bytes** in the `Message` protobuf
([relay-protocol.md §10](relay-protocol.md)): **no base64**, so the frame limit applies almost
entirely to the ciphertext. Full chain:

```
payload  →(zlib?)→  body  →(GCM: +16B tag)→  raw ciphertext  →(protobuf: +~tens of bytes)→  frame
```

**Normative constants** (frame limit differs per channel):

```
# Standard frame 64 KiB (control + Message live=false store-and-forward)
MAX_CIPHERTEXT_BYTES       = 65000     # raw ciphertext (GCM tag included)

# Live frame 512 KiB (Message live=true, authenticated connection)
MAX_LIVE_CIPHERTEXT_BYTES  = 524000    # raw ciphertext (GCM tag included)
```

Values leave a few hundred bytes of margin for the protobuf envelope (`peer` 32B, `nonce` 12B,
field tags, `live`) under the respective `MAX_*_FRAME_BYTES`. Anyone composing a large payload
**MUST** close the packet before exceeding `MAX_LIVE_CIPHERTEXT_BYTES`, estimating the size
**after** compression and tag.

Compression helps fit more data per frame: health-type data (JSON numeric and repetitive)
typically compresses 5–10×.
