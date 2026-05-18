# bevy_ticked_lockstep_networking

Lockstep multiplayer networking for `bevy_ticked`. Only player inputs (actions) are sent over the network — all peers compute game state deterministically from the same action stream. Full state is only sent when a player joins.

## Running the examples

### block_placer

A 2D multiplayer demo with WASD movement and mouse-based block placement.

**Native (two terminals):**

```sh
SIGNALLING_SERVER_URL="wss://signal.sigma-dev.eu/ws" cargo run -p bevy_ticked_lockstep_networking --example block_placer
```

**Controls:**
- `H` — Host a lobby
- `J` — Join the first available lobby
- `R` — Refresh lobby list
- `Esc` — Leave lobby
- `WASD` — Move
- `Left click` — Place a block at cursor
- `Right click` — Remove closest own block near cursor
