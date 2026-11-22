// SPDX-License-Identifier: Apache-2.0
//! Profile model: JSON files describing a modem and a host, merged and validated.
//!
//! Layout under each profile directory: `host/<stem>.json`, `modem/<stem>.json`,
//! and optionally `combo/<host>--<modem>.json`.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const BOARD_NAME_PATH: &str = "/tmp/sysinfo/board_name";

/// Profile directories used when none is given: the packaged set, then local additions
/// (a later directory wins a name clash).
pub const DEFAULT_DIRS: [&str; 2] = ["/usr/share/rayhunter/profiles", "/etc/rayhunter/profiles.d"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Transport {
    #[serde(rename = "diag-router")]
    DiagRouter,
    #[serde(rename = "kernel-char")]
    KernelChar,
    #[serde(rename = "serial")]
    Serial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SessionMode {
    #[serde(rename = "dci")]
    Dci,
    #[serde(rename = "exclusive")]
    Exclusive,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StockConsumer {
    pub name: String,
    pub pidof: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diag {
    pub transport: Transport,
    pub session: SessionMode,
    #[serde(default)]
    pub default_codes: Vec<String>,
    #[serde(default)]
    pub multi_radio_header: bool,
    #[serde(default)]
    pub stock_consumers: Vec<StockConsumer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub recordings_path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stream {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Stream clients attached at once; one more is refused and logged.
    #[serde(default = "default_max_clients")]
    pub max_clients: usize,
}

impl Default for Stream {
    fn default() -> Self {
        Stream { listen: default_listen(), max_clients: default_max_clients() }
    }
}

fn default_max_clients() -> usize {
    crate::stream::DEFAULT_MAX_CLIENTS
}

fn default_listen() -> String {
    "tcp:127.0.0.1:43555".to_string()
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub spec_version: u32,
    pub diag: Diag,
    pub storage: Storage,
    #[serde(default)]
    pub stream: Stream,
    #[serde(default = "empty_object")]
    pub evidence: Value,
}

/// JSON Merge Patch (RFC 7386): objects merge by key, `null` deletes, all else replaces.
pub fn merge(base: &mut Value, patch: &Value) {
    match patch {
        Value::Object(p) => {
            if !base.is_object() {
                *base = empty_object();
            }
            let b = base.as_object_mut().expect("just made an object");
            for (k, v) in p {
                if v.is_null() {
                    b.remove(k);
                } else {
                    merge(b.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        other => *base = other.clone(),
    }
}

fn valid_code(c: &str) -> bool {
    match c.strip_prefix("0x") {
        Some(h) => (1..=4).contains(&h.len()) && h.chars().all(|ch| ch.is_ascii_hexdigit()),
        None => false,
    }
}

impl Profile {
    pub fn check(&self) -> Result<(), String> {
        if self.spec_version != 1 {
            return Err(format!("spec_version must be 1, got {}", self.spec_version));
        }
        let d = &self.diag;
        if d.session == SessionMode::Exclusive && !d.stock_consumers.is_empty() {
            return Err(format!(
                "diag.session is \"exclusive\" but diag.stock_consumers lists {}: \
                 an exclusive session would fight a stock DIAG consumer",
                d.stock_consumers.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if d.session == SessionMode::Dci && d.transport == Transport::Serial {
            return Err("diag.session \"dci\" needs diag.transport \"diag-router\" or \
                        \"kernel-char\", not \"serial\""
                .to_string());
        }
        if d.transport == Transport::Serial && d.session != SessionMode::Exclusive {
            return Err("diag.transport \"serial\" requires diag.session \"exclusive\"".to_string());
        }
        let mut distinct = std::collections::BTreeSet::new();
        for c in &d.default_codes {
            match c.strip_prefix("0x").filter(|_| valid_code(c)) {
                Some(h) => distinct.insert(u16::from_str_radix(h, 16).expect("checked hex")),
                None => {
                    return Err(format!(
                        "diag.default_codes: \"{c}\" is not 0x followed by 1-4 hex digits"
                    ))
                }
            };
        }
        let cap = crate::daemon::MAX_SUBSCRIBED_CODES;
        if distinct.len() > cap {
            return Err(format!(
                "diag.default_codes lists {} codes, over the subscription cap of {cap}",
                distinct.len()
            ));
        }
        let l = &self.stream.listen;
        if !(l.starts_with("tcp:") || l.starts_with("unix:")) {
            return Err(format!("stream.listen \"{l}\" must start with tcp: or unix:"));
        }
        let (n, most) = (self.stream.max_clients, crate::stream::MAX_CLIENTS_LIMIT);
        if !(1..=most).contains(&n) {
            return Err(format!("stream.max_clients {n} must be between 1 and {most}"));
        }
        Ok(())
    }
}

/// Validate the host-only keys, then remove them so the rest can be typed.
fn take_host_keys(v: &mut Value) -> Result<(Option<String>, Option<String>), String> {
    let Some(obj) = v.as_object_mut() else {
        return Err("profile must be a JSON object".to_string());
    };
    let mut board = None;
    if let Some(m) = obj.remove("match") {
        let mo = m.as_object().ok_or("\"match\" must be an object")?;
        if let Some(k) = mo.keys().find(|k| k.as_str() != "board") {
            return Err(format!("unknown key \"{k}\" in \"match\""));
        }
        board = Some(
            mo.get("board")
                .and_then(Value::as_str)
                .ok_or("\"match.board\" must be a string")?
                .to_string(),
        );
    }
    let mut modem = None;
    if let Some(m) = obj.remove("modem") {
        modem = Some(m.as_str().ok_or("\"modem\" must be a string")?.to_string());
    }
    Ok((board, modem))
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Which merge layer a profile file is, from the directory it sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Host,
    Modem,
    Combo,
}

impl Layer {
    fn of(path: &Path) -> Option<Layer> {
        match path.parent()?.file_name()?.to_str()? {
            "host" => Some(Layer::Host),
            "modem" => Some(Layer::Modem),
            "combo" => Some(Layer::Combo),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Layer::Host => "host",
            Layer::Modem => "modem",
            Layer::Combo => "combo",
        }
    }
}

/// Check a partial profile: unknown keys anywhere are rejected. The file is merged
/// onto a minimal complete skeleton so the typed model does the key checking.
/// `match` and `modem` are host keys; any layer accepts them here (see
/// [`validate_layer`] for the layer-aware check).
pub fn validate_value(partial: &Value) -> Result<(), String> {
    validate_layer(partial, None)
}

/// [`validate_value`], refusing the host-only keys in a modem or combo file. `None`
/// (a file outside a host/modem/combo directory) has no layer to judge by.
pub fn validate_layer(partial: &Value, layer: Option<Layer>) -> Result<(), String> {
    if let (Some(l @ (Layer::Modem | Layer::Combo)), Some(obj)) = (layer, partial.as_object()) {
        if let Some(k) = ["match", "modem"].into_iter().find(|k| obj.contains_key(*k)) {
            return Err(format!(
                "\"{k}\" is only allowed in a host profile, not in a {} profile",
                l.name()
            ));
        }
    }
    let mut p = partial.clone();
    take_host_keys(&mut p)?;
    let mut full = serde_json::json!({
        "spec_version": 1,
        "diag": {"transport": "diag-router", "session": "dci"},
        "storage": {"recordings_path": "/skeleton"}
    });
    merge(&mut full, &p);
    serde_json::from_value::<Profile>(full).map(|_| ()).map_err(|e| e.to_string())
}

pub fn validate_file(path: &Path) -> Result<(), String> {
    validate_layer(&read_json(path)?, Layer::of(path))
}

/// Read one layer file and validate it on its own, so an error names the file that
/// holds the bad key rather than the merged profile.
fn read_layer(path: &Path, layer: Layer) -> Result<Value, String> {
    let v = read_json(path)?;
    validate_layer(&v, Some(layer)).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(v)
}

/// Files `<dir>/<sub>/<stem>.json` across `dirs`; a later directory wins a name clash.
fn collect(dirs: &[PathBuf], sub: &str) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d.join(sub)) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    out.insert(stem.to_string(), p.clone());
                }
            }
        }
    }
    out
}

/// Resolve a profile; also returns the host and modem file stems that were used.
pub fn resolve_named(
    dirs: &[PathBuf],
    host: &str,
    modem: &str,
    overrides: &Value,
) -> Result<(Profile, String, String), String> {
    resolve_named_with_board(dirs, host, modem, overrides, Path::new(BOARD_NAME_PATH))
}

pub(crate) fn resolve_named_with_board(
    dirs: &[PathBuf],
    host: &str,
    modem: &str,
    overrides: &Value,
    board_file: &Path,
) -> Result<(Profile, String, String), String> {
    let hosts = collect(dirs, "host");
    let host_stem = if host == "auto" {
        let text = std::fs::read_to_string(board_file)
            .map_err(|e| format!("host \"auto\": cannot read {}: {e}", board_file.display()))?;
        let board = text.lines().next().unwrap_or("").trim().to_string();
        let mut found = Vec::new();
        for (stem, path) in &hosts {
            let mut v = read_json(path)?;
            let keys = take_host_keys(&mut v).map_err(|e| format!("{}: {e}", path.display()))?;
            if keys.0.as_deref() == Some(board.as_str()) {
                found.push(stem.clone());
            }
        }
        match found.len() {
            0 => return Err(format!("no host profile matches board \"{board}\"")),
            1 => found.remove(0),
            _ => {
                return Err(format!(
                    "board \"{board}\" is matched by more than one host profile: {}; \
                     name the host or remove one",
                    found.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")
                ))
            }
        }
    } else {
        host.to_string()
    };
    let host_path = hosts
        .get(&host_stem)
        .ok_or_else(|| format!("no host profile named \"{host_stem}\""))?;
    let mut host_json = read_layer(host_path, Layer::Host)?;
    let (_, host_modem) =
        take_host_keys(&mut host_json).map_err(|e| format!("{}: {e}", host_path.display()))?;

    let modem_stem = if modem == "auto" {
        host_modem.ok_or_else(|| {
            format!("modem \"auto\": host profile \"{host_stem}\" names no \"modem\"")
        })?
    } else {
        modem.to_string()
    };
    let modems = collect(dirs, "modem");
    let modem_path = modems
        .get(&modem_stem)
        .ok_or_else(|| format!("no modem profile named \"{modem_stem}\""))?;

    let mut merged = empty_object();
    merge(&mut merged, &read_layer(modem_path, Layer::Modem)?);
    merge(&mut merged, &host_json);
    if let Some(combo) = collect(dirs, "combo").get(&format!("{host_stem}--{modem_stem}")) {
        merge(&mut merged, &read_layer(combo, Layer::Combo)?);
    }
    merge(&mut merged, overrides);

    let profile: Profile = serde_json::from_value(merged)
        .map_err(|e| format!("profile {host_stem}/{modem_stem}: {e}"))?;
    profile.check().map_err(|e| format!("profile {host_stem}/{modem_stem}: {e}"))?;
    Ok((profile, host_stem, modem_stem))
}

pub fn resolve(
    dirs: &[PathBuf],
    host: &str,
    modem: &str,
    overrides: &Value,
) -> Result<Profile, String> {
    resolve_named(dirs, host, modem, overrides).map(|(p, _, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn base(diag: serde_json::Value) -> serde_json::Value {
        json!({"spec_version": 1, "diag": diag, "storage": {"recordings_path": "/data/x"}})
    }
    fn profile_from(v: serde_json::Value) -> Profile {
        serde_json::from_value(v).unwrap()
    }
    fn repo_profiles_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles")
    }

    #[test]
    fn merge_objects_by_key_and_replaces_arrays() {
        let mut a = json!({"diag": {"session": "dci", "default_codes": ["0xB0C0"]}, "x": 1});
        merge(&mut a, &json!({"diag": {"default_codes": []}, "x": null}));
        assert_eq!(a, json!({"diag": {"session": "dci", "default_codes": []}}));
    }
    #[test]
    fn exclusive_with_stock_consumers_is_refused() {
        let p = profile_from(base(json!({"transport": "serial", "session": "exclusive",
            "stock_consumers": [{"name": "diag_read", "pidof": "diag_read"}]})));
        let e = p.check().unwrap_err();
        assert!(e.contains("exclusive") && e.contains("stock_consumers"), "{e}");
    }
    #[test]
    fn dci_on_serial_is_refused() {
        let p = profile_from(base(json!({"transport": "serial", "session": "dci"})));
        let e = p.check().unwrap_err();
        assert!(e.contains("serial") && e.contains("dci"), "{e}");
    }
    #[test]
    fn unknown_key_is_refused() {
        let e = serde_json::from_value::<Profile>(base(json!({"transport": "serial", "sesion": "dci"})))
            .unwrap_err()
            .to_string();
        assert!(e.contains("sesion"), "{e}");
    }
    #[test]
    fn bad_code_is_refused() {
        let p = profile_from(base(json!({"transport": "diag-router", "session": "dci",
            "default_codes": ["B0C0x"]})));
        assert!(p.check().unwrap_err().contains("B0C0x"));
    }
    #[test]
    fn default_codes_over_the_subscription_cap_are_refused() {
        let codes = |n: u32| (0..n).map(|i| format!("0x{:04X}", 0x1000 + i)).collect::<Vec<_>>();
        let at_cap = profile_from(base(json!({"transport": "diag-router", "session": "dci",
            "default_codes": codes(4096)})));
        assert_eq!(at_cap.check(), Ok(()));
        let over = profile_from(base(json!({"transport": "diag-router", "session": "dci",
            "default_codes": codes(4097)})));
        let e = over.check().unwrap_err();
        assert!(e.contains("4097") && e.contains("4096"), "{e}");
        // The same code spelt twice counts once.
        let mut dup = codes(4096);
        dup.push("0x1abc".into());
        let dup = profile_from(base(json!({"transport": "diag-router", "session": "dci",
            "default_codes": dup})));
        assert_eq!(dup.check(), Ok(()));
    }
    #[test]
    fn bad_spec_version_and_listen_are_refused() {
        let mut v = base(json!({"transport": "diag-router", "session": "dci"}));
        v["spec_version"] = json!(2);
        assert!(profile_from(v).check().unwrap_err().contains("spec_version"));
        let mut v = base(json!({"transport": "diag-router", "session": "dci"}));
        v["stream"] = json!({"listen": "udp:1.2.3.4:5"});
        assert!(profile_from(v).check().unwrap_err().contains("stream.listen"));
    }
    #[test]
    fn max_clients_defaults_to_8_and_must_be_1_to_64() {
        let v = base(json!({"transport": "diag-router", "session": "dci"}));
        assert_eq!(profile_from(v).stream.max_clients, 8);
        for (n, ok) in [(0, false), (1, true), (64, true), (65, false)] {
            let mut v = base(json!({"transport": "diag-router", "session": "dci"}));
            v["stream"] = json!({"max_clients": n});
            let r = profile_from(v).check();
            assert_eq!(r.is_ok(), ok, "{n}: {r:?}");
            if !ok {
                assert!(r.unwrap_err().contains("stream.max_clients"));
            }
        }
    }
    #[test]
    fn shipped_fx4200_profiles_resolve() {
        let p = resolve(&[repo_profiles_dir()], "inseego-fx4200", "auto", &json!({})).unwrap();
        assert_eq!(p.diag.stock_consumers[0].pidof, "diag_read");
        assert_eq!(p.stream.listen, "tcp:127.0.0.1:43555");
        assert_eq!(p.stream.max_clients, 8);
        assert_eq!(p.storage.recordings_path, "/data/rayhunter");
        assert_eq!(p.evidence["diag_transport"]["verdict"], "🟡");
        p.check().unwrap();
    }
    #[test]
    fn resolve_named_reports_stems_and_overrides_win() {
        let o = json!({"stream": {"listen": "unix:/run/d.sock"}});
        let (p, h, m) = resolve_named(&[repo_profiles_dir()], "inseego-fx4200", "auto", &o).unwrap();
        assert_eq!((h.as_str(), m.as_str()), ("inseego-fx4200", "inseego-fx4200-sdx75"));
        assert_eq!(p.stream.listen, "unix:/run/d.sock");
    }
    #[test]
    fn resolve_runs_check() {
        let o = json!({"diag": {"session": "exclusive"}});
        let e = resolve(&[repo_profiles_dir()], "inseego-fx4200", "auto", &o).unwrap_err();
        assert!(e.contains("exclusive") && e.contains("stock_consumers"), "{e}");
    }
    #[test]
    fn auto_host_uses_board_file_and_unknown_board_names_it() {
        let dir = std::env::temp_dir().join(format!("diagbuddy-board-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("board_name");
        std::fs::write(&f, "ap-sdxpinn-qcn9224-V2\nextra\n").unwrap();
        let (_, h, _) =
            resolve_named_with_board(&[repo_profiles_dir()], "auto", "auto", &json!({}), &f).unwrap();
        assert_eq!(h, "inseego-fx4200");
        std::fs::write(&f, "no-such-board\n").unwrap();
        let e = resolve_named_with_board(&[repo_profiles_dir()], "auto", "auto", &json!({}), &f)
            .unwrap_err();
        assert!(e.contains("no-such-board"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A fresh profile tree under the temp dir; pid + counter so concurrent runs and
    /// tests in one run never share a directory.
    fn tree(files: &[(&str, serde_json::Value)]) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "diagbuddy-tree-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, v) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, v.to_string()).unwrap();
        }
        dir
    }
    fn modem_ok() -> serde_json::Value {
        json!({"spec_version": 1, "diag": {"transport": "diag-router", "session": "dci"},
               "storage": {"recordings_path": "/data/x"}})
    }
    #[test]
    fn host_only_keys_are_refused_in_modem_and_combo_files() {
        let d = tree(&[
            ("host/h.json", json!({"match": {"board": "b"}, "modem": "m"})),
            ("modem/m.json", json!({"match": {"board": "b"}})),
            ("combo/h--m.json", json!({"modem": "m"})),
            ("loose.json", json!({"modem": "m"})),
        ]);
        validate_file(&d.join("host/h.json")).unwrap();
        let e = validate_file(&d.join("modem/m.json")).unwrap_err();
        assert!(e.contains("\"match\"") && e.contains("modem profile"), "{e}");
        let e = validate_file(&d.join("combo/h--m.json")).unwrap_err();
        assert!(e.contains("\"modem\"") && e.contains("combo profile"), "{e}");
        // A file outside a host/modem/combo directory has no layer to judge by.
        validate_file(&d.join("loose.json")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn two_host_files_matching_one_board_is_an_error_naming_both() {
        let d = tree(&[
            ("host/a.json", json!({"match": {"board": "b1"}, "modem": "m"})),
            ("host/z.json", json!({"match": {"board": "b1"}, "modem": "m"})),
            ("modem/m.json", modem_ok()),
        ]);
        std::fs::write(d.join("board_name"), "b1\n").unwrap();
        let e = resolve_named_with_board(&[d.clone()], "auto", "auto", &json!({}), &d.join("board_name"))
            .unwrap_err();
        assert!(e.contains("\"a\"") && e.contains("\"z\"") && e.contains("b1"), "{e}");
        // Named explicitly, the same tree resolves: the clash only matters to "auto".
        resolve_named_with_board(&[d.clone()], "a", "auto", &json!({}), &d.join("board_name")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn a_bad_key_is_reported_against_the_file_that_holds_it() {
        let mut bad_modem = modem_ok();
        bad_modem["diag"]["sesion"] = json!("dci");
        let d = tree(&[
            ("host/h.json", json!({"modem": "m"})),
            ("modem/m.json", bad_modem),
        ]);
        let e = resolve(&[d.clone()], "h", "auto", &json!({})).unwrap_err();
        assert!(e.contains("sesion") && e.contains("modem/m.json"), "{e}");
        let d2 = tree(&[
            ("host/h.json", json!({"modem": "m"})),
            ("modem/m.json", modem_ok()),
            ("combo/h--m.json", json!({"match": {"board": "b"}})),
        ]);
        let e = resolve(&[d2.clone()], "h", "auto", &json!({})).unwrap_err();
        assert!(e.contains("combo/h--m.json") && e.contains("\"match\""), "{e}");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&d2);
    }
    #[test]
    fn validate_accepts_partial_and_rejects_unknown() {
        validate_value(&json!({"diag": {"session": "dci"}})).unwrap();
        validate_value(&json!({"match": {"board": "b"}, "modem": "m"})).unwrap();
        let e = validate_value(&json!({"diag": {"sesion": "dci"}})).unwrap_err();
        assert!(e.contains("sesion"), "{e}");
        assert!(validate_value(&json!({"match": {"bord": "b"}})).is_err());
    }
}
