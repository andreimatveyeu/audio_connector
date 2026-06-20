//! audio-connector: a JACK graph watcher.
//!
//! Keeps a declarative TOML connection map applied to a live JACK graph:
//!
//!   * opens a JACK client and listens for port-registration / graph-reorder
//!     events;
//!   * on a settled burst of events, reconciles the live graph against the
//!     declarative map (connect the declared edges that are missing);
//!   * non-destructive — it only *adds* connections (and removes the ones
//!     explicitly marked `disconnect:`), never touching manual wiring.

mod config;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use regex::Regex;
use toml::value::{Table, Value};

const DEFAULT_CLIENT_NAME: &str = "audio-connector";
const DEFAULT_CONFIG: &str = "/etc/audio-connector/connections.toml";
const DEFAULT_DEBOUNCE_MS: u64 = 500; // wait for a burst of rapid events to settle
const DEFAULT_COOLDOWN_MS: u64 = 3000; // absorb graph events caused by our own connections

/// Shared "something changed, please reconcile" flag, signalled from the JACK
/// notification thread and consumed by the watch loop.
#[derive(Default)]
struct Dirty {
    pending: Mutex<bool>,
    cond: Condvar,
}

impl Dirty {
    fn mark(&self) {
        let mut pending = self.pending.lock().unwrap();
        *pending = true;
        self.cond.notify_all();
    }

    /// Block until pending, then clear it.
    fn wait_and_clear(&self) {
        let mut pending = self.pending.lock().unwrap();
        while !*pending {
            pending = self.cond.wait(pending).unwrap();
        }
        *pending = false;
    }

    fn clear(&self) {
        *self.pending.lock().unwrap() = false;
    }
}

/// JACK notification handler: every relevant graph change just flips the flag.
struct Notifier {
    dirty: Arc<Dirty>,
}

impl jack::NotificationHandler for Notifier {
    fn port_registration(&mut self, _: &jack::Client, _port_id: jack::PortId, is_reg: bool) {
        log::info!(
            "port {}",
            if is_reg { "registered" } else { "unregistered" }
        );
        self.dirty.mark();
    }

    fn graph_reorder(&mut self, _: &jack::Client) -> jack::Control {
        self.dirty.mark();
        jack::Control::Continue
    }
}

/// No-op process handler: we register no ports, we just need the client active
/// so its notifications fire.
struct NoopProcess;
impl jack::ProcessHandler for NoopProcess {
    fn process(&mut self, _: &jack::Client, _: &jack::ProcessScope) -> jack::Control {
        jack::Control::Continue
    }
}

struct Settings {
    config_path: PathBuf,
    client_name: String,
    debounce: Duration,
    cooldown: Duration,
}

fn settings_from_env_and_args() -> Settings {
    // Positional arg overrides $AUDIO_CONNECTOR_CONFIG overrides the default.
    let arg_path = std::env::args().nth(1);
    let config_path = arg_path
        .or_else(|| std::env::var("AUDIO_CONNECTOR_CONFIG").ok())
        .unwrap_or_else(|| DEFAULT_CONFIG.to_string())
        .into();

    let client_name =
        std::env::var("AUDIO_CONNECTOR_CLIENT").unwrap_or_else(|_| DEFAULT_CLIENT_NAME.to_string());

    let debounce =
        Duration::from_millis(env_ms("AUDIO_CONNECTOR_DEBOUNCE_MS", DEFAULT_DEBOUNCE_MS));
    let cooldown =
        Duration::from_millis(env_ms("AUDIO_CONNECTOR_COOLDOWN_MS", DEFAULT_COOLDOWN_MS));

    Settings {
        config_path,
        client_name,
        debounce,
        cooldown,
    }
}

fn env_ms(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let settings = settings_from_env_and_args();

    // Open the JACK client; do not start a server (one must already be running).
    let (client, status) =
        match jack::Client::new(&settings.client_name, jack::ClientOptions::NO_START_SERVER) {
            Ok(v) => v,
            Err(e) => {
                log::error!("could not open JACK client '{}': {e}", settings.client_name);
                std::process::exit(1);
            }
        };
    log::info!("'{}' connected (status {status:?})", settings.client_name);

    let dirty = Arc::new(Dirty::default());
    let notifier = Notifier {
        dirty: Arc::clone(&dirty),
    };

    let active = match client.activate_async(notifier, NoopProcess) {
        Ok(a) => a,
        Err(e) => {
            log::error!("could not activate JACK client: {e}");
            std::process::exit(1);
        }
    };
    log::info!("'{}' started, watching graph", settings.client_name);

    // Clean shutdown on SIGINT/SIGTERM so `systemctl stop` deactivates cleanly.
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
    }
    // A signal must also break the condvar wait below.
    {
        let dirty = Arc::clone(&dirty);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            // Cheap nudger: every 250ms, if we've been asked to stop, wake the loop.
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
            }
            dirty.mark();
        });
    }

    // Initial reconcile on startup, before the first graph event. The reconciler
    // owns the parsed-config and compiled-regex caches across passes.
    let mut reconciler = Reconciler::new(settings.config_path);
    reconciler.reconcile(active.as_client());

    // Watch loop: wait -> debounce -> reconcile to a fixpoint.
    loop {
        dirty.wait_and_clear();
        if stop.load(Ordering::Relaxed) {
            break;
        }

        std::thread::sleep(settings.debounce);

        // Reconcile until a pass makes no changes (the graph matches the map).
        //
        // Each pass that *does* change something emits graph events for our own
        // connects/disconnects; we clear those echoes and re-check after a short
        // cooldown. Crucially, the final (no-op) pass is NOT followed by a clear:
        // a genuine external change made during the cooldown -- e.g. the session
        // manager re-establishing a default-sink link we just removed -- surfaces
        // as more work on the next pass, or, if it arrives after we've settled,
        // leaves `dirty` set so the `wait_and_clear` above picks it up. Reconcile
        // reads the live graph fresh every pass, so clearing the flag between
        // passes never loses state. This is what stops the old unconditional
        // post-cooldown `dirty.clear()` from swallowing real events (a re-linked
        // Firefox -> hardware playback) that landed inside the cooldown window.
        while reconciler.reconcile(active.as_client()) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(settings.cooldown);
            // Discard the events our own connections just generated; the next
            // pass re-derives desired state from the live graph regardless.
            dirty.clear();
        }
    }

    log::info!("shutting down");
    if let Err(e) = active.deactivate() {
        log::warn!("deactivate failed: {e}");
    }
}

/// Holds the reconcile state that persists across passes: the parsed config
/// (reloaded only when a source file's mtime changes) and a cache of compiled
/// regexes (rebuilt only when the config reloads).
///
/// Re-parsing the TOML and recompiling every `regex:` spec on each graph event
/// was pure churn -- a watcher can fire many events a second while the config
/// almost never changes -- so both are now memoized.
struct Reconciler {
    config_path: PathBuf,
    config: Option<CachedConfig>,
    /// Keyed by the compiled regex source; `None` marks a pattern that failed
    /// to compile, so we warn once rather than on every pass.
    regex_cache: HashMap<String, Option<Regex>>,
}

/// A parsed config together with the mtime of every file it was built from.
struct CachedConfig {
    table: Table,
    sources: Vec<(PathBuf, Option<SystemTime>)>,
}

impl Reconciler {
    fn new(config_path: PathBuf) -> Self {
        Self {
            config_path,
            config: None,
            regex_cache: HashMap::new(),
        }
    }

    /// Load the config and apply the declared connections to the live graph.
    ///
    /// Returns `true` if it changed the graph (made at least one connect or
    /// disconnect), so the caller can keep reconciling until a pass is a no-op.
    fn reconcile(&mut self, client: &jack::Client) -> bool {
        if !self.ensure_loaded() {
            return false;
        }
        // Disjoint field borrows: the cached table is read while the regex cache
        // is mutated. `ensure_loaded` returning true guarantees `config` is Some.
        let table = match &self.config {
            Some(c) => &c.table,
            None => return false,
        };
        let changed = apply(client, table, &mut self.regex_cache);
        log::info!("connections applied");
        changed
    }

    /// Ensure `self.config` holds an up-to-date parse, reloading only when a
    /// source file changed. Returns `false` only when there is no usable config
    /// at all (the initial load failed); a failed *re*load keeps the last-good
    /// config so a transient bad edit doesn't tear down live connections.
    fn ensure_loaded(&mut self) -> bool {
        let fresh = self
            .config
            .as_ref()
            .is_some_and(|c| sources_unchanged(&c.sources));
        if fresh {
            return true;
        }

        match config::load_config(&self.config_path) {
            Ok(loaded) => {
                let sources = loaded
                    .sources
                    .into_iter()
                    .map(|p| {
                        let m = mtime(&p);
                        (p, m)
                    })
                    .collect();
                self.config = Some(CachedConfig {
                    table: loaded.table,
                    sources,
                });
                // Patterns may have changed; drop any stale compiled regexes.
                self.regex_cache.clear();
                log::info!("loaded config from {}", self.config_path.display());
                true
            }
            Err(e) => {
                log::error!("failed to load config: {e}");
                // Keep serving the last-good config if we have one.
                self.config.is_some()
            }
        }
    }
}

/// True if every cached source still has the mtime we recorded, so the parsed
/// config is still current. A now-missing or unreadable file reads as changed.
fn sources_unchanged(sources: &[(PathBuf, Option<SystemTime>)]) -> bool {
    sources.iter().all(|(path, cached)| mtime(path) == *cached)
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Resolve declared edges against current ports and connect/disconnect as needed.
///
///   * table key  = source (output) port short-name, optionally `disconnect:`-
///     or `regex:`-prefixed;
///   * array value = destination (input) port full-names, optionally `regex:`.
fn apply(
    client: &jack::Client,
    config: &Table,
    regex_cache: &mut HashMap<String, Option<Regex>>,
) -> bool {
    // Snapshot of every current port name; regex specs match against these.
    let all_ports: Vec<String> = client.ports(None, None, jack::PortFlags::empty());
    let port_set: HashSet<&str> = all_ports.iter().map(String::as_str).collect();

    // Lazily-built cache of each source port's existing connections, so repeated
    // declarations and the disconnect check don't re-query JACK each time.
    let mut conns: HashMap<String, HashSet<String>> = HashMap::new();

    // Whether this pass actually mutated the graph.
    let mut changed = false;

    for (client_name, port_map) in config {
        let Value::Table(port_map) = port_map else {
            log::warn!("'{client_name}' is not a table, skipping");
            continue;
        };

        for (output_key, inputs) in port_map {
            let (is_disconnect, output_key) = split_disconnect(output_key);

            let inputs = match inputs {
                Value::Array(a) => a,
                _ => {
                    log::warn!("value for '{client_name}:{output_key}' is not an array, skipping");
                    continue;
                }
            };

            // Resolve the source port(s).
            let output_ports = resolve_ports(
                &all_ports,
                &port_set,
                output_key,
                Some(client_name),
                regex_cache,
            );
            if output_ports.is_empty() {
                // Normal in a declarative model: clients come and go. Demoted to
                // debug so a watcher re-running on every graph event stays quiet.
                log::debug!("no current port for: {client_name}:{output_key}");
                continue;
            }

            for out in &output_ports {
                for inp in inputs {
                    let Some(inp) = inp.as_str() else {
                        log::warn!("non-string destination under '{out}', skipping");
                        continue;
                    };
                    let input_ports = resolve_ports(&all_ports, &port_set, inp, None, regex_cache);
                    if input_ports.is_empty() {
                        log::debug!("no current port for: {inp}");
                        continue;
                    }
                    for dst in &input_ports {
                        changed |= if is_disconnect {
                            try_disconnect(client, out, dst, &mut conns)
                        } else {
                            try_connect(client, out, dst, &mut conns)
                        };
                    }
                }
            }
        }
    }

    changed
}

/// Resolve a port spec to concrete current port names.
///
/// Split a leading `disconnect:` marker off an output key, returning whether it
/// was present and the remaining spec (which may still carry a `regex:` marker).
fn split_disconnect(key: &str) -> (bool, &str) {
    match key.strip_prefix("disconnect:") {
        Some(rest) => (true, rest),
        None => (false, key),
    }
}

/// `client` is `Some` for the output (table key) side, where the client name is
/// prefixed onto the short port name; `None` for the destination side, where the
/// spec is already a full `client:port` name. A `regex:` prefix switches to
/// start-anchored regex matching. The marker is honoured on either side: a key
/// (e.g. `regex:capture_.*`) or the client/table-header (e.g.
/// `["regex:synth(-[0-9]+)?"]`, for clients whose JACK name carries a varying
/// postfix); when it appears on the client, that side is treated as a regex
/// sub-pattern rather than a literal prefix.
fn resolve_ports(
    all_ports: &[String],
    port_set: &HashSet<&str>,
    spec: &str,
    client: Option<&str>,
    regex_cache: &mut HashMap<String, Option<Regex>>,
) -> Vec<String> {
    // A `regex:` marker on the key *or* the client selects regex matching; the
    // literal marker is then stripped from both before compiling the pattern.
    let client_is_regex = client.is_some_and(|c| c.contains("regex:"));
    if spec.contains("regex:") || client_is_regex {
        let body = spec.replacen("regex:", "", 1);
        let pattern = match client {
            Some(c) => format!("{}:{body}", c.replacen("regex:", "", 1)),
            None => body,
        };
        // Compile once per distinct pattern and reuse across passes. A compile
        // failure caches `None` so we warn once, not on every reconcile.
        let source = format!("^(?:{pattern})");
        let re = regex_cache
            .entry(source.clone())
            .or_insert_with(|| match Regex::new(&source) {
                Ok(re) => Some(re),
                Err(e) => {
                    log::warn!("invalid regex '{pattern}': {e}");
                    None
                }
            });
        let Some(re) = re else {
            return Vec::new();
        };
        all_ports
            .iter()
            .filter(|p| re.is_match(p))
            .cloned()
            .collect()
    } else {
        let full = match client {
            Some(c) => format!("{c}:{spec}"),
            None => spec.to_string(),
        };
        if port_set.contains(full.as_str()) {
            vec![full]
        } else {
            Vec::new()
        }
    }
}

/// Existing connections of `source`, cached for the duration of one apply pass.
fn source_conns<'a>(
    client: &jack::Client,
    conns: &'a mut HashMap<String, HashSet<String>>,
    source: &str,
) -> &'a mut HashSet<String> {
    conns.entry(source.to_string()).or_insert_with(|| {
        client
            .port_by_name(source)
            .map(|p| p.get_connections().into_iter().collect())
            .unwrap_or_default()
    })
}

/// Returns `true` if it actually established a new connection.
fn try_connect(
    client: &jack::Client,
    source: &str,
    dest: &str,
    conns: &mut HashMap<String, HashSet<String>>,
) -> bool {
    if source_conns(client, conns, source).contains(dest) {
        return false; // already established
    }
    match client.connect_ports_by_name(source, dest) {
        Ok(()) => {
            log::info!("connected {source} -> {dest}");
            source_conns(client, conns, source).insert(dest.to_string());
            true
        }
        Err(e) => {
            log::warn!("connect {source} -> {dest} failed: {e}");
            false
        }
    }
}

/// Returns `true` if it actually removed an existing connection.
fn try_disconnect(
    client: &jack::Client,
    source: &str,
    dest: &str,
    conns: &mut HashMap<String, HashSet<String>>,
) -> bool {
    if !source_conns(client, conns, source).contains(dest) {
        return false; // nothing to disconnect
    }
    match client.disconnect_ports_by_name(source, dest) {
        Ok(()) => {
            log::info!("disconnected {source} -> {dest}");
            source_conns(client, conns, source).remove(dest);
            true
        }
        Err(e) => {
            log::warn!("disconnect {source} -> {dest} failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn resolve(all: &[String], spec: &str, client: Option<&str>) -> Vec<String> {
        let set: HashSet<&str> = all.iter().map(String::as_str).collect();
        let mut cache = HashMap::new();
        let mut got = resolve_ports(all, &set, spec, client, &mut cache);
        got.sort();
        got
    }

    // ----- split_disconnect -----------------------------------------------

    #[test]
    fn split_disconnect_plain_key() {
        assert_eq!(split_disconnect("out_left"), (false, "out_left"));
    }

    #[test]
    fn split_disconnect_marked_key() {
        assert_eq!(split_disconnect("disconnect:out_0"), (true, "out_0"));
    }

    #[test]
    fn split_disconnect_keeps_regex_marker() {
        // Only the disconnect: prefix is stripped; regex: is left for resolution.
        assert_eq!(
            split_disconnect("disconnect:regex:out_.*"),
            (true, "regex:out_.*")
        );
    }

    // ----- resolve_ports: exact -------------------------------------------

    #[test]
    fn exact_output_prefixes_client() {
        let all = ports(&["synth:out_left", "synth:out_right"]);
        assert_eq!(resolve(&all, "out_left", Some("synth")), ["synth:out_left"]);
    }

    #[test]
    fn exact_output_miss_is_empty() {
        let all = ports(&["synth:out_left"]);
        assert!(resolve(&all, "out_right", Some("synth")).is_empty());
    }

    #[test]
    fn exact_destination_is_full_name() {
        let all = ports(&["mixer:in_1", "mixer:in_2"]);
        // Destination side passes client = None: the spec is already a full name.
        assert_eq!(resolve(&all, "mixer:in_1", None), ["mixer:in_1"]);
    }

    #[test]
    fn exact_key_with_spaces_and_slashes() {
        let all = ports(&["mixer:master_out 1"]);
        assert_eq!(
            resolve(&all, "master_out 1", Some("mixer")),
            ["mixer:master_out 1"]
        );
    }

    // ----- resolve_ports: regex -------------------------------------------

    #[test]
    fn regex_output_matches_client_ports() {
        let all = ports(&["dev:capture_1", "dev:capture_2", "dev:other", "x:capture_9"]);
        assert_eq!(
            resolve(&all, "regex:capture_.*", Some("dev")),
            ["dev:capture_1", "dev:capture_2"]
        );
    }

    #[test]
    fn regex_is_anchored_at_start() {
        // Start-anchored: a leading character before the pattern must not match.
        let all = ports(&["a:in_1", "a:xin_1"]);
        assert_eq!(resolve(&all, "regex:in_1", Some("a")), ["a:in_1"]);
    }

    #[test]
    fn regex_is_not_anchored_at_end() {
        // A prefix pattern matches longer names (re.match-style, no trailing $).
        let all = ports(&["system:playback_1", "system:playback_2"]);
        assert_eq!(
            resolve(&all, "regex:sys", None),
            ["system:playback_1", "system:playback_2"]
        );
    }

    #[test]
    fn regex_client_matches_optional_postfix() {
        // A `regex:` marker on the client/table-header treats the client name as
        // a pattern, so a varying JACK postfix (e.g. "synth-181") still matches.
        let all = ports(&["synth:left", "synth-181:left", "synthx:left"]);
        assert_eq!(
            resolve(&all, "left", Some("regex:synth(-[0-9]+)?")),
            ["synth-181:left", "synth:left"]
        );
    }

    #[test]
    fn regex_client_with_regex_key() {
        // Both sides may carry the marker; each is stripped and combined.
        let all = ports(&["synth-9:left", "synth-9:right", "other:left"]);
        assert_eq!(
            resolve(&all, "regex:.*", Some("regex:synth(-[0-9]+)?")),
            ["synth-9:left", "synth-9:right"]
        );
    }

    #[test]
    fn regex_destination_full_name() {
        let all = ports(&["effects:input_1", "effects:input_2", "effects:bypass"]);
        assert_eq!(
            resolve(&all, "regex:effects:input_.*", None),
            ["effects:input_1", "effects:input_2"]
        );
    }

    #[test]
    fn invalid_regex_yields_no_ports() {
        let all = ports(&["a:b"]);
        // Unbalanced group is an invalid pattern; resolution returns nothing.
        assert!(resolve(&all, "regex:(", Some("a")).is_empty());
    }
}
