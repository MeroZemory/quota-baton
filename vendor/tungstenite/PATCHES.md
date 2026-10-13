# quota-baton buffer patch

Source: crates.io `tungstenite` 0.28.0, upstream https://github.com/snapview/tungstenite-rs.
Original MIT and Apache licenses are included. This is not a deflate fork.

Only `src/protocol/frame/mod.rs` differs from the published source:

- After reading a frame larger than 1 MiB, copy only unread tail bytes into a
  small buffer so the socket no longer owns the large frame allocation.
- After a write buffer has drained completely, replace buffers larger than
  1 MiB with a 128 KiB buffer. Partial writes retain their pending bytes.
- A regression verifies both buffers shrink and the next frame remains intact.

No protocol, TLS or compression behavior is changed. Per-message deflate remains
unsupported. The local patch is needed because 0.28.0 exposes no buffer-shrink API.
