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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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

    // Initial reconcile on startup, before the first graph event.
    reconcile(active.as_client(), &settings.config_path);

    // Watch loop: wait -> clear -> debounce -> apply -> cooldown -> clear.
    loop {
        dirty.wait_and_clear();
        if stop.load(Ordering::Relaxed) {
            break;
        }

        std::thread::sleep(settings.debounce);
        reconcile(active.as_client(), &settings.config_path);
        std::thread::sleep(settings.cooldown);
        // Discard the events our own connections just generated.
        dirty.clear();
    }

    log::info!("shutting down");
    if let Err(e) = active.deactivate() {
        log::warn!("deactivate failed: {e}");
    }
}

/// Load the config and apply the declared connections to the live graph.
fn reconcile(client: &jack::Client, config_path: &std::path::Path) {
    log::info!("applying connections from {}", config_path.display());
    let config = match config::load_config(config_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("failed to load config: {e}");
            return;
        }
    };
    apply(client, &config);
    log::info!("connections applied");
}

/// Resolve declared edges against current ports and connect/disconnect as needed.
///
///   * table key  = source (output) port short-name, optionally `disconnect:`-
///     or `regex:`-prefixed;
///   * array value = destination (input) port full-names, optionally `regex:`.
fn apply(client: &jack::Client, config: &Table) {
    // Snapshot of every current port name; regex specs match against these.
    let all_ports: Vec<String> = client.ports(None, None, jack::PortFlags::empty());
    let port_set: HashSet<&str> = all_ports.iter().map(String::as_str).collect();

    // Lazily-built cache of each source port's existing connections, so repeated
    // declarations and the disconnect check don't re-query JACK each time.
    let mut conns: HashMap<String, HashSet<String>> = HashMap::new();

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
            let output_ports = resolve_ports(&all_ports, &port_set, output_key, Some(client_name));
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
                    let input_ports = resolve_ports(&all_ports, &port_set, inp, None);
                    if input_ports.is_empty() {
                        log::debug!("no current port for: {inp}");
                        continue;
                    }
                    for dst in &input_ports {
                        if is_disconnect {
                            try_disconnect(client, out, dst, &mut conns);
                        } else {
                            try_connect(client, out, dst, &mut conns);
                        }
                    }
                }
            }
        }
    }
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
/// start-anchored regex matching.
fn resolve_ports(
    all_ports: &[String],
    port_set: &HashSet<&str>,
    spec: &str,
    client: Option<&str>,
) -> Vec<String> {
    // A `regex:` marker anywhere in the spec selects regex matching; the literal
    // marker is then stripped before compiling the pattern.
    if spec.contains("regex:") {
        let body = spec.replacen("regex:", "", 1);
        let pattern = match client {
            Some(c) => format!("{c}:{body}"),
            None => body,
        };
        let re = match Regex::new(&format!("^(?:{pattern})")) {
            Ok(re) => re,
            Err(e) => {
                log::warn!("invalid regex '{pattern}': {e}");
                return Vec::new();
            }
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

fn try_connect(
    client: &jack::Client,
    source: &str,
    dest: &str,
    conns: &mut HashMap<String, HashSet<String>>,
) {
    if source_conns(client, conns, source).contains(dest) {
        return; // already established
    }
    match client.connect_ports_by_name(source, dest) {
        Ok(()) => {
            log::info!("connected {source} -> {dest}");
            source_conns(client, conns, source).insert(dest.to_string());
        }
        Err(e) => log::warn!("connect {source} -> {dest} failed: {e}"),
    }
}

fn try_disconnect(
    client: &jack::Client,
    source: &str,
    dest: &str,
    conns: &mut HashMap<String, HashSet<String>>,
) {
    if !source_conns(client, conns, source).contains(dest) {
        return; // nothing to disconnect
    }
    match client.disconnect_ports_by_name(source, dest) {
        Ok(()) => {
            log::info!("disconnected {source} -> {dest}");
            source_conns(client, conns, source).remove(dest);
        }
        Err(e) => log::warn!("disconnect {source} -> {dest} failed: {e}"),
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
        let mut got = resolve_ports(all, &set, spec, client);
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
