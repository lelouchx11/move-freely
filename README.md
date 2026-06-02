# move freely

Share **one keyboard + mouse across 2–3 computers** (macOS, Windows, Linux). Push the
pointer to a screen edge and control hands off to the next machine — a software KVM in the
spirit of Synergy / Barrier / Mouse Without Borders.

> **Status:** Milestone 1 complete — the network + protocol layer works end to end.
> OS input capture/injection and automatic edge switching land in later milestones (see
> [the plan](#roadmap)).

## How it works

- **Server** = the machine with the physical keyboard + mouse. It owns *focus* (which screen
  is active) and routes input.
- **Clients** = the other machines. They connect to the server over TCP and act on the input
  it streams.

One binary, two roles: `mf serve` and `mf connect`.

## Build

This repo currently builds with the **nightly** toolchain (the local stable toolchain is
broken). Prefix cargo commands with `+nightly` or `rustup default nightly`.

```sh
cargo +nightly build
cargo +nightly test
```

## Try it on one machine (no permissions needed)

The `--demo` server sends a scripted input stream; the `--echo` client logs what it receives
instead of injecting it. Together they prove the handshake → framing → routing pipe.

```sh
# 1. Make a local config
cp config/layout.example.toml config/layout.toml
#    edit bind = "127.0.0.1:24800" and pick a token

# 2. Start the server (terminal A)
RUST_LOG=mf=info cargo +nightly run -- serve --config config/layout.toml --demo

# 3. Connect a client (terminal B)
RUST_LOG=mf=info cargo +nightly run -- connect 127.0.0.1:24800 \
    --name winbox --token <your-token> --width 1920 --height 1080 --echo
```

You should see the client log `ENTER`, mouse moves, button + key events, `scroll`, then `LEAVE`.

## Configuration

See [`config/layout.example.toml`](config/layout.example.toml). Exactly one screen must be
`position = "center"` (the server's own display); up to two more may be `left` / `right`.

## Roadmap

1. ✅ **Networking + protocol** — handshake, token auth, length-prefixed framing, heartbeat.
2. ⬜ **macOS capture + injection** — `enigo` (inject) + `rdev`/CoreGraphics (capture/grab).
   Requires granting **Accessibility** and **Input Monitoring** in System Settings → Privacy.
3. ⬜ **Edge switching** — automatic handoff when the pointer crosses a screen edge.
4. ⬜ **Windows + Linux/X11 backends**.
5. ⬜ **Polish** — auto-reconnect, optional TLS, mDNS discovery.

Out of scope for v1: clipboard sync, file drag-and-drop, Wayland capture.
