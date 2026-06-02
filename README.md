# move freely

Share **one keyboard + mouse across 2–3 computers** (macOS, Windows, Linux). Push the
pointer to a screen edge and control hands off to the next machine — a software KVM in the
spirit of Synergy / Barrier / Mouse Without Borders.

> **Status:** Milestone 2 — real OS input capture + injection works. The server grabs the
> local keyboard/mouse and forwards them to a client you switch to with a **hotkey (F12)**.
> Automatic edge switching (no hotkey) is the next milestone (see [Roadmap](#roadmap)).

## How it works

- **Server** = the machine with the physical keyboard + mouse. It owns *focus* (which screen
  is active), captures input, and routes it.
- **Clients** = the other machines. They connect to the server over TCP and inject the input
  it streams into their own OS.

Focus starts on the server's own screen. Press **F12** to cycle focus to the next connected
client (and again to come back). While a client has focus, the server *grabs* local input —
your physical mouse/keyboard drive the remote machine — and releases it when focus returns.

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

## Run it for real (two machines)

This requires OS input permissions (see below). The server captures; the client injects.

```sh
# Server — the machine with the keyboard + mouse (config lists the client as a [[screen]])
RUST_LOG=mf=info cargo +nightly run -- serve --config config/layout.toml

# Client — on the other machine; --name must match a [[screen]], same --token
RUST_LOG=mf=info cargo +nightly run -- connect <server-ip>:24800 \
    --name winbox --token <your-token> --width 1920 --height 1080
```

Press **F12** on the server to hand control to the client; press it again to take it back.

### Verify injection works first

```sh
cargo +nightly run -- selftest   # nudges the pointer in a square and types "hi"
```

If you see `does not have the permission to simulate input`, grant the permissions below.

## OS permissions

**macOS** — System Settings → Privacy & Security. Grant *both* to the program that runs
`mf` (e.g. your terminal app, or the `mf` binary itself):
- **Accessibility** — required to inject input (client) and to grab input (server).
- **Input Monitoring** — required for the server to capture the keyboard/mouse.

After granting, fully quit and relaunch the terminal. The server logs a clear error and the
`selftest` fails fast if either permission is missing.

**Windows / Linux** — no special grants for v1 on Windows; on Linux use an **X11** session
(Wayland blocks global capture) and your user typically needs access to input devices.

## Configuration

See [`config/layout.example.toml`](config/layout.example.toml). Exactly one screen must be
`position = "center"` (the server's own display); up to two more may be `left` / `right`.

## Roadmap

1. ✅ **Networking + protocol** — handshake, token auth, length-prefixed framing, heartbeat.
2. ✅ **Capture + injection** — `enigo` (inject) + `rdev` (capture/grab), with an F12 hotkey
   to switch the active screen. Cross-platform baseline; macOS validated.
3. ⬜ **Edge switching** — automatic handoff when the pointer crosses a screen edge (the
   geometry already lives in `mf-protocol::layout`).
4. ⬜ **Windows + Linux/X11 backend hardening**.
5. ⬜ **Polish** — auto-reconnect, optional TLS, mDNS discovery.

Out of scope for v1: clipboard sync, file drag-and-drop, Wayland capture.
