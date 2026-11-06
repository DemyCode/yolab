use std::time::Duration;

use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Str,
    Int,
    Float,
    Bool,
    Osd,
    List,
}

struct Arg {
    name: &'static str,
    ty: Ty,
    required: bool,
}

struct Sig {
    prefix: &'static [&'static str],
    args: &'static [Arg],
}

const fn req(name: &'static str, ty: Ty) -> Arg {
    Arg {
        name,
        ty,
        required: true,
    }
}

const fn opt(name: &'static str, ty: Ty) -> Arg {
    Arg {
        name,
        ty,
        required: false,
    }
}

const fn sig(prefix: &'static [&'static str], args: &'static [Arg]) -> Sig {
    Sig { prefix, args }
}

use Ty::{Bool, Float, Int, List, Osd, Str};

const OSD_REMOVAL: &[Arg] = &[
    req("id", Osd),
    opt("force", Bool),
    opt("yes_i_really_mean_it", Bool),
];

const POOL_DELETE: &[Arg] = &[
    req("pool", Str),
    opt("pool2", Str),
    opt("yes_i_really_really_mean_it", Bool),
    opt("yes_i_really_really_mean_it_not_faking", Bool),
];

static SIGS: &[Sig] = &[
    sig(&["status"], &[]),
    sig(&["health"], &[opt("detail", Str)]),
    sig(&["df"], &[opt("detail", Str)]),
    sig(&["fsid"], &[]),
    sig(&["quorum_status"], &[]),
    sig(&["mon", "dump"], &[opt("epoch", Int)]),
    sig(
        &["mon", "add"],
        &[req("name", Str), req("addr", Str), opt("location", List)],
    ),
    sig(&["osd", "stat"], &[]),
    sig(&["osd", "dump"], &[opt("epoch", Int)]),
    sig(&["osd", "tree"], &[opt("epoch", Int), opt("states", List)]),
    sig(&["osd", "ls"], &[opt("epoch", Int)]),
    sig(
        &["osd", "df"],
        &[
            opt("output_method", Str),
            opt("filter_by", Str),
            opt("filter", Str),
        ],
    ),
    sig(&["osd", "metadata"], &[opt("id", Osd)]),
    sig(
        &["osd", "set"],
        &[req("key", Str), opt("yes_i_really_mean_it", Bool)],
    ),
    sig(&["osd", "unset"], &[req("key", Str)]),
    sig(&["osd", "in"], &[req("ids", List)]),
    sig(&["osd", "out"], &[req("ids", List)]),
    sig(&["osd", "rm"], &[req("ids", List)]),
    sig(&["osd", "ok-to-stop"], &[req("ids", List), opt("max", Int)]),
    sig(&["osd", "safe-to-destroy"], &[req("ids", List)]),
    sig(&["osd", "destroy"], OSD_REMOVAL),
    sig(&["osd", "purge"], OSD_REMOVAL),
    sig(
        &["osd", "purge-actual"],
        &[req("id", Osd), opt("yes_i_really_mean_it", Bool)],
    ),
    sig(
        &["osd", "lost"],
        &[req("id", Osd), opt("yes_i_really_mean_it", Bool)],
    ),
    sig(
        &["osd", "force-create-pg"],
        &[req("pgid", Str), opt("yes_i_really_mean_it", Bool)],
    ),
    sig(
        &["osd", "crush", "reweight"],
        &[req("name", Str), req("weight", Float)],
    ),
    sig(&["osd", "crush", "rule", "ls"], &[]),
    sig(
        &["osd", "primary-affinity"],
        &[req("id", Osd), req("weight", Float)],
    ),
    sig(&["osd", "add-noout"], &[req("ids", List)]),
    sig(&["osd", "rm-noout"], &[req("ids", List)]),
    sig(
        &["osd", "crush", "rm"],
        &[req("name", Str), opt("ancestor", Str)],
    ),
    sig(
        &["osd", "crush", "rule", "create-replicated"],
        &[
            req("name", Str),
            req("root", Str),
            req("type", Str),
            opt("class", Str),
        ],
    ),
    sig(&["osd", "pool", "ls"], &[opt("detail", Str)]),
    sig(
        &["osd", "pool", "get"],
        &[req("pool", Str), req("var", Str)],
    ),
    sig(
        &["osd", "pool", "set"],
        &[
            req("pool", Str),
            req("var", Str),
            req("val", Str),
            opt("yes_i_really_mean_it", Bool),
        ],
    ),
    sig(
        &["osd", "pool", "create"],
        &[
            req("pool", Str),
            opt("pg_num", Int),
            opt("pgp_num", Int),
            opt("pool_type", Str),
            opt("erasure_code_profile", Str),
            opt("rule", Str),
            opt("expected_num_objects", Int),
            opt("size", Int),
            opt("pg_num_min", Int),
            opt("pg_num_max", Int),
            opt("autoscale_mode", Str),
            opt("bulk", Bool),
            opt("target_size_bytes", Int),
            opt("target_size_ratio", Float),
            opt("yes_i_really_mean_it", Bool),
        ],
    ),
    sig(&["osd", "pool", "delete"], POOL_DELETE),
    sig(&["osd", "pool", "rm"], POOL_DELETE),
    sig(&["pg", "stat"], &[]),
    sig(&["pg", "dump"], &[opt("dumpcontents", List)]),
    sig(&["mgr", "stat"], &[]),
    sig(&["mgr", "services"], &[]),
    sig(&["mgr", "module", "ls"], &[]),
    sig(
        &["mgr", "module", "enable"],
        &[req("module", Str), opt("force", Bool)],
    ),
    sig(&["mgr", "module", "disable"], &[req("module", Str)]),
    sig(&["progress", "json"], &[]),
    sig(&["config-key", "get"], &[req("key", Str)]),
    sig(&["config-key", "set"], &[req("key", Str), opt("val", Str)]),
    sig(&["config-key", "rm"], &[req("key", Str)]),
    sig(&["config-key", "dump"], &[opt("key", Str)]),
    sig(&["config", "get"], &[req("who", Str), opt("key", Str)]),
    sig(&["config", "rm"], &[req("who", Str), req("name", Str)]),
    sig(
        &["config", "set"],
        &[
            req("who", Str),
            req("name", Str),
            req("value", Str),
            opt("force", Bool),
        ],
    ),
    sig(&["auth", "get"], &[req("entity", Str)]),
    sig(&["auth", "get-key"], &[req("entity", Str)]),
    sig(&["auth", "rm"], &[req("entity", Str)]),
    sig(&["auth", "caps"], &[req("entity", Str), req("caps", List)]),
    sig(
        &["auth", "get-or-create"],
        &[req("entity", Str), opt("caps", List), opt("key_type", Str)],
    ),
    sig(&["fs", "ls"], &[]),
    sig(&["fs", "status"], &[opt("fs", Str), opt("format", Str)]),
    sig(
        &["fs", "new"],
        &[
            req("fs_name", Str),
            req("metadata", Str),
            req("data", Str),
            opt("force", Bool),
            opt("allow_dangerous_metadata_overlay", Bool),
            opt("fscid", Int),
            opt("recover", Bool),
            opt("yes_i_really_really_mean_it", Bool),
            opt("set", List),
        ],
    ),
    sig(&["fs", "subvolumegroup", "ls"], &[req("vol_name", Str)]),
    sig(
        &["fs", "subvolumegroup", "create"],
        &[
            req("vol_name", Str),
            req("group_name", Str),
            opt("size", Int),
            opt("pool_layout", Str),
            opt("uid", Int),
            opt("gid", Int),
            opt("mode", Str),
            opt("normalization", Str),
            opt("casesensitive", Bool),
        ],
    ),
    sig(
        &["fs", "subvolume", "info"],
        &[
            req("vol_name", Str),
            req("sub_name", Str),
            opt("group_name", Str),
        ],
    ),
    sig(&["dashboard", "ac-user-show"], &[opt("username", Str)]),
    sig(
        &["dashboard", "ac-user-create"],
        &[
            req("username", Str),
            opt("rolename", Str),
            opt("name", Str),
            opt("email", Str),
            opt("enabled", Bool),
            opt("force_password", Bool),
            opt("pwd_expiration_date", Int),
            opt("pwd_update_required", Bool),
        ],
    ),
    sig(
        &["dashboard", "ac-user-set-password"],
        &[req("username", Str), opt("force_password", Bool)],
    ),
];

#[derive(Debug, PartialEq)]
pub(crate) struct MonCommand {
    pub json: Value,
    pub timeout: Option<Duration>,
    pub input: Option<String>,
}

impl MonCommand {
    pub(crate) fn json_format(&self) -> bool {
        self.json["format"]
            .as_str()
            .is_some_and(|f| f.starts_with("json"))
    }
}

struct Globals<'a> {
    words: Vec<&'a str>,
    format: Option<&'a str>,
    timeout: Option<Duration>,
    input: Option<&'a str>,
}

fn is_short_option(a: &str) -> bool {
    a.starts_with('-') && !a.starts_with("--") && !a[1..].starts_with(|c: char| c.is_ascii_digit())
}

fn globals<'a>(args: &[&'a str]) -> Option<Globals<'a>> {
    let mut g = Globals {
        words: Vec::new(),
        format: None,
        timeout: None,
        input: None,
    };
    let mut it = args.iter().copied();
    while let Some(a) = it.next() {
        match a {
            "-f" | "--format" => g.format = Some(it.next()?),
            "-s" | "--status" => g.words.insert(0, "status"),
            "-i" | "--in-file" => g.input = Some(it.next().filter(|p| *p != "-")?),
            "--connect-timeout" => {
                g.timeout = Some(Duration::from_secs(it.next()?.parse().ok()?));
            }
            "--" => return None,
            _ => match a.strip_prefix("--format=") {
                Some(f) => g.format = Some(f),
                None if is_short_option(a) => return None,
                None => g.words.push(a),
            },
        }
    }
    Some(g)
}

fn signature(words: &[&str]) -> Option<&'static Sig> {
    SIGS.iter()
        .filter(|s| words.len() >= s.prefix.len() && words[..s.prefix.len()] == s.prefix[..])
        .max_by_key(|s| s.prefix.len())
}

fn value(ty: Ty, word: &str) -> Option<Value> {
    Some(match ty {
        Str | List => json!(word),
        Int => json!(word.parse::<i64>().ok()?),
        Float => json!(word.parse::<f64>().ok().filter(|f| f.is_finite())?),
        Bool => match word.to_ascii_lowercase().as_str() {
            "true" | "1" => json!(true),
            "false" | "0" => json!(false),
            _ => return None,
        },
        Osd => json!(word
            .strip_prefix("osd.")
            .unwrap_or(word)
            .parse::<u32>()
            .ok()?),
    })
}

fn store(map: &mut Map<String, Value>, arg: &Arg, word: &str) -> Option<()> {
    let v = value(arg.ty, word)?;
    if arg.ty == List {
        map.entry(arg.name)
            .or_insert_with(|| json!([]))
            .as_array_mut()?
            .push(v);
        return Some(());
    }
    if map.contains_key(arg.name) {
        return None;
    }
    map.insert(arg.name.into(), v);
    Some(())
}

fn next_positional(sig: &Sig, map: &Map<String, Value>, from: usize) -> Option<usize> {
    (from..sig.args.len()).find(|&i| sig.args[i].ty == List || !map.contains_key(sig.args[i].name))
}

pub(crate) fn mon_command(args: &[&str]) -> Option<MonCommand> {
    let g = globals(args)?;
    let sig = signature(&g.words)?;
    let mut map = Map::new();
    map.insert("prefix".into(), json!(sig.prefix.join(" ")));
    let mut next = 0;
    let mut rest = g.words[sig.prefix.len()..].iter().copied();
    while let Some(word) = rest.next() {
        if let Some(kw) = word.strip_prefix("--") {
            let (key, inline) = match kw.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (kw, None),
            };
            let key = key.replace('-', "_");
            let arg = sig.args.iter().find(|a| a.name == key)?;
            let given = match (inline, arg.ty) {
                (Some(v), _) => v,
                (None, Bool) => "true",
                (None, _) => rest.next()?,
            };
            store(&mut map, arg, given)?;
            continue;
        }
        next = next_positional(sig, &map, next)?;
        let arg = &sig.args[next];
        store(&mut map, arg, word)?;
        if arg.ty != List {
            next += 1;
        }
    }
    if sig
        .args
        .iter()
        .any(|a| a.required && !map.contains_key(a.name))
    {
        return None;
    }
    if let Some(f) = g.format {
        map.insert("format".into(), json!(f));
    }
    Some(MonCommand {
        json: Value::Object(map),
        timeout: g.timeout,
        input: g.input.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(args: &[&str]) -> Value {
        mon_command(args)
            .unwrap_or_else(|| panic!("{args:?} should go through librados"))
            .json
    }

    #[test]
    fn every_prefix_is_listed_once() {
        let mut seen = std::collections::HashSet::new();
        for s in SIGS {
            assert!(seen.insert(s.prefix), "{:?} is listed twice", s.prefix);
        }
    }

    #[test]
    fn the_storage_page_reads_use_the_names_the_monitor_publishes() {
        assert_eq!(
            sent(&["osd", "df", "tree", "-f", "json"]),
            json!({"prefix": "osd df", "output_method": "tree", "format": "json"})
        );
        assert_eq!(
            sent(&["osd", "pool", "ls", "detail", "-f", "json"]),
            json!({"prefix": "osd pool ls", "detail": "detail", "format": "json"})
        );
        assert_eq!(
            sent(&["df", "-f", "json"]),
            json!({"prefix": "df", "format": "json"})
        );
        assert_eq!(
            sent(&["health", "detail"]),
            json!({"prefix": "health", "detail": "detail"})
        );
        assert_eq!(
            sent(&["pg", "dump", "pgs_brief", "-f", "json"]),
            json!({"prefix": "pg dump", "dumpcontents": ["pgs_brief"], "format": "json"})
        );
    }

    #[test]
    fn positional_words_fill_the_arguments_in_order() {
        assert_eq!(
            sent(&["osd", "pool", "create", "images", "32", "32"]),
            json!({"prefix": "osd pool create", "pool": "images", "pg_num": 32, "pgp_num": 32})
        );
        assert_eq!(
            sent(&["config", "set", "mgr", "mgr/dashboard/ssl", "false"]),
            json!({"prefix": "config set", "who": "mgr", "name": "mgr/dashboard/ssl", "value": "false"})
        );
        assert_eq!(
            sent(&["osd", "crush", "reweight", "osd.2", "0.45000"]),
            json!({"prefix": "osd crush reweight", "name": "osd.2", "weight": 0.45})
        );
    }

    #[test]
    fn a_flag_sets_its_boolean_and_dashes_become_underscores() {
        assert_eq!(
            sent(&[
                "osd",
                "pool",
                "set",
                "images",
                "size",
                "1",
                "--yes-i-really-mean-it"
            ]),
            json!({
                "prefix": "osd pool set", "pool": "images", "var": "size", "val": "1",
                "yes_i_really_mean_it": true
            })
        );
        assert_eq!(
            sent(&["fs", "new", "yolab", "meta", "data", "--force"]),
            json!({"prefix": "fs new", "fs_name": "yolab", "metadata": "meta", "data": "data", "force": true})
        );
    }

    #[test]
    fn an_osd_name_is_sent_as_its_number() {
        assert_eq!(
            sent(&["osd", "purge", "osd.3", "--yes-i-really-mean-it"]),
            json!({"prefix": "osd purge", "id": 3, "yes_i_really_mean_it": true})
        );
        assert_eq!(
            sent(&["osd", "metadata", "7", "-f", "json"]),
            json!({"prefix": "osd metadata", "id": 7, "format": "json"})
        );
        assert_eq!(
            sent(&["osd", "purge-actual", "osd.3", "--yes-i-really-mean-it"]),
            json!({"prefix": "osd purge-actual", "id": 3, "yes_i_really_mean_it": true})
        );
        assert_eq!(
            mon_command(&["osd", "purge", "*", "--yes-i-really-mean-it"]),
            None
        );
        assert_eq!(
            sent(&["osd", "primary-affinity", "osd.5", "0"]),
            json!({"prefix": "osd primary-affinity", "id": 5, "weight": 0.0})
        );
        assert_eq!(
            sent(&["osd", "add-noout", "osd.5"]),
            json!({"prefix": "osd add-noout", "ids": ["osd.5"]})
        );
        assert_eq!(
            sent(&["osd", "crush", "rm", "vdisk-7"]),
            json!({"prefix": "osd crush rm", "name": "vdisk-7"})
        );
        assert_eq!(mon_command(&["osd", "purge", "mon.a"]), None);
    }

    #[test]
    fn list_arguments_take_every_remaining_word() {
        assert_eq!(
            sent(&["osd", "ok-to-stop", "osd.3"]),
            json!({"prefix": "osd ok-to-stop", "ids": ["osd.3"]})
        );
        assert_eq!(
            sent(&[
                "auth",
                "get-or-create",
                "client.yolab-images",
                "mon",
                "profile rbd",
                "osd",
                "profile rbd pool=images",
                "--key_type",
                "aes256k",
            ]),
            json!({
                "prefix": "auth get-or-create",
                "entity": "client.yolab-images",
                "caps": ["mon", "profile rbd", "osd", "profile rbd pool=images"],
                "key_type": "aes256k"
            })
        );
    }

    #[test]
    fn new_caps_for_a_key_take_every_remaining_word() {
        assert_eq!(
            sent(&["auth", "caps", "client.csi-cephfs-node", "mon", "allow r", "mds", "allow rws"]),
            json!({
                "prefix": "auth caps",
                "entity": "client.csi-cephfs-node",
                "caps": ["mon", "allow r", "mds", "allow rws"]
            })
        );
    }

    #[test]
    fn a_named_argument_takes_the_next_word() {
        assert_eq!(
            sent(&[
                "fs",
                "subvolume",
                "info",
                "yolab",
                "vol1",
                "--group_name",
                "csi",
                "-f",
                "json"
            ]),
            json!({
                "prefix": "fs subvolume info", "vol_name": "yolab", "sub_name": "vol1",
                "group_name": "csi", "format": "json"
            })
        );
        assert_eq!(
            sent(&["auth", "get-or-create", "client.x", "--key_type=aes256k"]),
            json!({"prefix": "auth get-or-create", "entity": "client.x", "key_type": "aes256k"})
        );
    }

    #[test]
    fn an_input_file_is_carried_beside_the_command() {
        let c = mon_command(&[
            "dashboard",
            "ac-user-create",
            "admin",
            "-i",
            "/run/pw",
            "administrator",
            "--force-password",
        ])
        .unwrap();
        assert_eq!(
            c.json,
            json!({
                "prefix": "dashboard ac-user-create", "username": "admin",
                "rolename": "administrator", "force_password": true
            })
        );
        assert_eq!(c.input.as_deref(), Some("/run/pw"));
        assert_eq!(
            mon_command(&["dashboard", "ac-user-create", "admin", "-i", "-"]),
            None
        );
    }

    #[test]
    fn dash_s_is_status_as_the_cli_treats_it() {
        assert_eq!(sent(&["-s"]), json!({"prefix": "status"}));
    }

    #[test]
    fn connect_timeout_becomes_the_request_deadline() {
        let c = mon_command(&[
            "--connect-timeout",
            "10",
            "mon",
            "add",
            "node2",
            "[v2:[fd00::2]:3300]",
        ])
        .unwrap();
        assert_eq!(
            c.json,
            json!({"prefix": "mon add", "name": "node2", "addr": "[v2:[fd00::2]:3300]"})
        );
        assert_eq!(c.timeout, Some(Duration::from_secs(10)));
        assert_eq!(mon_command(&["--connect-timeout", "soon", "health"]), None);
    }

    #[test]
    fn negative_numbers_are_values_not_options() {
        assert_eq!(
            sent(&["osd", "pool", "set", "p", "pg_num_min", "-1"]),
            json!({"prefix": "osd pool set", "pool": "p", "var": "pg_num_min", "val": "-1"})
        );
    }

    #[test]
    fn anything_the_table_cannot_place_stays_on_the_cli() {
        for args in [
            vec!["tell", "osd.0", "bench"],
            vec!["daemon", "mon.node1", "mon_status"],
            vec!["fs", "subvolume", "rm", "yolab", "v"],
            vec!["osd", "pool", "create", "p", "lots"],
            vec!["osd", "pool", "get", "p"],
            vec!["osd", "pool", "get", "p", "size", "extra"],
            vec!["config-key", "get", "k", "--no-such-flag"],
            vec!["osd", "unset", "noout", "noout"],
            vec!["status", "-f"],
            vec!["status", "-w"],
            vec!["--", "status"],
            vec![],
        ] {
            assert_eq!(mon_command(&args), None, "{args:?}");
        }
    }

    #[test]
    fn the_json_format_flag_follows_the_format_argument() {
        assert!(mon_command(&["df", "-f", "json-pretty"])
            .unwrap()
            .json_format());
        assert!(!mon_command(&["df", "-f", "plain"]).unwrap().json_format());
        assert!(!mon_command(&["fsid"]).unwrap().json_format());
    }
}
