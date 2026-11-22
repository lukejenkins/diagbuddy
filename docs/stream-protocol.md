# Stream protocol

The stream is DIAG over HDLC on a byte stream, the same framing a DIAG serial port uses.
A client that can read a DIAG serial port can read diagbuddy with only the transport
changed. This page has enough to write one from scratch.

## Transport

`stream.listen` sets the address: `tcp:<ip>:<port>` (loopback addresses only; default
`127.0.0.1:43555`) or `unix:<path>` (created mode 0600). The stream has no authentication;
the loopback-only and 0600 rules are its access control.

Up to `stream.max_clients` clients (default 8) are attached at once. A connection that
arrives while that many are attached is accepted and closed at once.

## Framing

Both directions use the same frames:

```
<packet bytes, escaped> <FCS lo> <FCS hi, escaped> 0x7E
```

- **FCS**: CRC-16/X-25 (poly 0x8408 reflected, init 0xFFFF, final complement) over the
  unescaped packet, appended little-endian.
- **Escaping**: `0x7E` and `0x7D` in the packet or FCS are sent as `0x7D` followed by the
  byte XOR `0x20`.
- **Terminator**: one `0x7E` ends each frame. diagbuddy does not send a leading `0x7E`.
  It ignores empty frames from a client (two flags in a row).

A request with a bad FCS or a bad escape, or of 8192 bytes or more on the wire (escaped,
before its closing flag), is dropped and counted in `bad_requests`; nothing is sent back.

## Server to client: log records

Each record the client asked for arrives as one frame whose packet is:

```
[0x98 multi-radio header, 8 bytes]   only if diag.multi_radio_header is true
0x10                                 DIAG log command
0x00                                 "more" byte
u16 len                              length of the log item that follows
<log item>                           as the modem logged it: u16 len, u16 log code,
                                     u64 timestamp, then the body
```

All integers are little-endian. The multi-radio header is off by default because some
readers refuse it. When it is on and the DIAG stack supplied a header for a record,
that header is sent; otherwise `98 01 00 00 01 00 00 00`.

A client receives only the codes in its own masks, never one because another client or
`default_codes` subscribed it. Replies to its requests (below) are interleaved with log
frames on the same connection.

## Client to server: LOG_CONFIG

diagbuddy answers exactly one command, `LOG_CONFIG` (`0x73`), which is how a DIAG reader
sets its log mask. Requests begin `73 00 00 00`, then `u32 op`. Every reply begins
`73 00 00 00 <u32 op> <u32 status = 0>`.

A log code is `(equipment_id << 12) | item`, equipment id 0 to 15, item 0 to 0xFFF.

| Op | Request after `op` | Reply after `status` | Effect |
|---|---|---|---|
| 0 disable | nothing | nothing (12-byte reply) | Clears all of this client's masks. |
| 1 id ranges | nothing | 16 × `u32`: last item per equipment id | Reports `0x0FFF` for equipment ids 1 and 11, 0 for the rest. |
| 3 set mask | `u32 equip`, `u32 last_item`, mask bytes | the request's own fields, echoed | Replaces this client's mask for `equip`. Bit `n` of the mask (byte `n/8`, bit `n%8`) wants item `n`; bits at or past `last_item` are ignored. The mask must be at least `ceil(last_item/8)` bytes. |
| 4 get mask | `u32 equip` | `u32 equip`, mask bytes | This client's mask for `equip`, `ceil(L/8)` bytes, `L` being the `last_item` of its last op 3 for that id, or 0x0FFF if none. |

Masks are per client: op 0 and op 4 see and change only the asking client's state.

Anything else (another command byte, another op, an equipment id over 15, a mask shorter
than `last_item` needs) gets the bad-command reply: `0x13` followed by the first 16 bytes
(or fewer) of the request. It is counted in `refused_commands` under the request's first byte.

### The subscription cap

diagbuddy holds one DCI subscription: the union of every client's codes and
`default_codes`, at most **4096** codes. An op 3 that would push the union past the cap
still gets its normal reply, but nothing changes: the client's masks stay as they were, the
other clients are untouched, and `over_cap` counts it. A client that needs to know whether
it got its codes should read them back with op 4 or check its row in the status file.

### Example

Set equipment id 0xB, items up to 0xC0 inclusive, asking for item 0xC0 (code 0xB0C0):

```
73 00 00 00  03 00 00 00  0B 00 00 00  C1 00 00 00  <25 mask bytes, byte 24 = 0x01>
```

The reply is the same bytes with `00 00 00 00` inserted after the op, and from then on the
client receives every 0xB0C0 record.

## Flow control and lifetime

- diagbuddy never blocks on a client. Each client has its own queue of 4096 frames; when it
  is full, the oldest frame is dropped and counted in `dropped`. A write that makes no
  progress for 5 s detaches the client.
- When a client disconnects, only the codes no other client (or `default_codes`) still
  wants are unsubscribed.
- When diagbuddy restarts, every client is disconnected and its masks are gone. A client
  should reconnect, with a delay, and send its masks again.
