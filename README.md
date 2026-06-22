# clockstop

`clockstop` is a small Rust tray app for focus sessions with compositor-level drift enforcement.

It snapshots the open Wayland toplevel app-id population when a session starts, watches for population drift, emits visible `notify-send` state changes, and runs a lock command after sustained unreverted drift.

## Run

Smoke-test without locking the real session:

```bash
CLOCKSTOP_T_SECONDS=10 CLOCKSTOP_LOCK_CMD='notify-send clockstop WOULD_LOCK' nix develop -c cargo run
```

Normal dev run:

```bash
cargo run
```

Or without the dev shell if your environment already has the required build deps:

```bash
cargo run
```

## Tray Actions

- `Start <default> min`: start a focus session using the current default duration
- `Start custom...`: open a small egui duration picker, start that duration, and make it the new default for this process
- `Stop`: stop the active session
- `Status notification`: send the current phase/drift status through `notify-send`
- `Quit`: stop the tray process

## Matching

`clockstop` v0.1 uses population drift:

- on start, snapshot `{app_id: count}`
- drift means a new app id appears or an existing app id count increases
- closing the drift window drains the drift bucket
```

## Notifications

Notifications are part of the human-smoke contract. The app sends notifications when it starts, starts/stops sessions, detects drift, fires the lock command, enters cooldown, completes, or reports status.

Useful environment variables:

- `CLOCKSTOP_T_SECONDS`: drift threshold and cooldown window, default `20`
- `CLOCKSTOP_TICK_SECONDS`: sampler tick interval, default `2`
- `CLOCKSTOP_LOCK_CMD`: lock command, default `swaylock-effects --screenshots --effect-pixelate 10`
- `CLOCKSTOP_LOG`: repo-local smoke log path, default `clockstop-smoke.log`
