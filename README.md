# audio-connector

A small, native JACK graph watcher. It keeps a **declarative TOML map** of the
connections you want applied to a running JACK graph: whenever ports appear or
the graph is reordered, it reconciles the graph against the map, making the
connections that are declared but missing.

It is **non-destructive** — it only adds the connections you declare (and removes
the ones you explicitly mark `disconnect:`). Connections you make by hand are
left alone.

This is useful when audio clients come and go (apps launching, devices being
plugged in, effect racks starting): instead of re-wiring by hand each time, you
describe the wiring once and the watcher maintains it.

Works with any JACK server, including PipeWire's JACK implementation. libjack is
loaded dynamically at runtime, so the binary is not hard-linked against a
specific JACK library.

## Build

```sh
cargo build --release
```

The `jack` crate's build step probes for `jack.pc` via `pkg-config`, so JACK
development files must be present **at build time** (e.g. `libjack-jackd2-dev`,
`jack2-devel`, or `pipewire-jack`'s dev package, depending on your distro) along
with `pkg-config`. At **run** time, libjack is `dlopen`'d, so only the runtime
JACK library needs to be reachable on the library path.

## Tests

```sh
cargo test
```

The suite covers the pure logic: config loading, `include` merging (relative
paths, array concat/dedup ordering, cycle detection, error cases) and port
resolution (client prefixing, exact lookup, start-anchored regex matching,
`disconnect:` parsing). The JACK I/O paths require a running server and are
exercised by running the binary, not by unit tests. Like the build, `cargo test`
needs `jack.pc` reachable via `pkg-config`.

## Usage

```sh
audio-connector [CONFIG_PATH]
```

The config path is taken from, in order of precedence:

1. the first positional argument,
2. the `AUDIO_CONNECTOR_CONFIG` environment variable,
3. the default `/etc/audio-connector/connections.toml`.

### Environment variables

| Variable                      | Default                                  | Meaning                                             |
| ----------------------------- | ---------------------------------------- | --------------------------------------------------- |
| `AUDIO_CONNECTOR_CONFIG`      | `/etc/audio-connector/connections.toml`  | Path to the connection map.                         |
| `AUDIO_CONNECTOR_CLIENT`      | `audio-connector`                        | JACK client name to register as.                    |
| `AUDIO_CONNECTOR_DEBOUNCE_MS` | `500`                                    | Wait for a burst of graph events to settle.         |
| `AUDIO_CONNECTOR_COOLDOWN_MS` | `3000`                                   | After applying, ignore the events we just caused.   |
| `RUST_LOG`                    | `info`                                   | Log verbosity (`error`, `warn`, `info`, `debug`).   |

Set `RUST_LOG=debug` to see which declared ports are currently absent.

The process logs to stdout/stderr and runs until it receives `SIGINT` or
`SIGTERM`, at which point it deactivates its JACK client and exits cleanly —
suitable to run directly or under any process supervisor.

## Releases

CI builds and tests every push and pull request. The package version is **not**
hardcoded — `Cargo.toml` carries a `0.0.0` placeholder, and pushing a semver tag
stamps that tag as the version and publishes a GitHub release with the built
binary:

```sh
git tag 1.2.0
git push origin 1.2.0
```

## Configuration format

The map is a TOML file. Each table is a **source** client; each key is one of
that client's **output** (or capture) ports; the value is an array of
**destination** input ports, written as full `client:port` names.

```toml
[synth]
out_left  = ["mixer:in_1", "recorder:in_1"]
out_right = ["mixer:in_2", "recorder:in_2"]
```

Quote keys that contain spaces or slashes:

```toml
[mixer]
"master_out 1" = ["system:playback_1"]
"master_out 2" = ["system:playback_2"]
```

### Regex matching

A `regex:` marker selects start-anchored regular-expression matching. On a key
it matches against the source client's port names; on a value it matches against
full destination port names:

```toml
[capture-device]
"regex:capture_.*" = ["regex:effects:input_.*"]
```

### Disconnect

A `disconnect:` prefix on a key removes the listed edges instead of adding them.
It can be combined with `regex:`:

```toml
[player]
"disconnect:out_0"        = ["system:playback_1"]
"disconnect:regex:out_.*" = ["regex:monitor:.*"]
```

### Includes

A top-level `include` array splits a large map across files. Paths resolve
relative to the including file, and same-key lists from different files are
merged (concatenated, de-duplicated):

```toml
include = ["midi.toml", "instruments.toml"]
```

See [`examples/connections.toml`](examples/connections.toml) for a complete
annotated example.

## Running as a service

The binary is a well-behaved foreground daemon: it logs to stdout/stderr and
shuts down cleanly on `SIGTERM`. Point your init/supervisor at it and give it
the config path via argument or `AUDIO_CONNECTOR_CONFIG`. Because it registers
as a JACK client, it must run in the same session as the JACK server it should
watch.

## License

MIT
