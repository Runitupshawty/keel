//! The OpenSSH client configuration (`~/.ssh/config`), read without ever running anything.
//!
//! Supported: `Host` blocks (`*`, `?`, several patterns, `!` negation), `Include` (relative
//! to `~/.ssh`, `*`/`?` in the file name, at most eight levels, missing files ignored),
//! `keyword value` and `keyword=value`, quoted values, comments, and first-obtained-value
//! wins as in OpenSSH (`IdentityFile` accumulates). Honoured keywords: `HostName`, `User`,
//! `Port`, `IdentityFile`, `IdentitiesOnly`, `ProxyJump`, `ServerAliveInterval`. `Match`
//! blocks are skipped with a note, `ProxyCommand` is refused, the rest of OpenSSH's keywords
//! are ignored. As in OpenSSH, a keyword it does not know (a typo) is an error unless
//! `IgnoreUnknown` lists it, so a misspelt `Host` never turns its block into a default. A
//! leading byte-order mark is skipped; files over 1 MiB are refused.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Include files nest at most this deep; a ProxyJump route has at most this many hops.
pub const MAX_DEPTH: usize = 8;
/// Aliases resolved for one connection at most (each hop's own route is followed for the
/// first hop only, so a valid route needs far fewer).
const MAX_RESOLVES: usize = 64;
/// Configuration and key files are refused above this size.
pub const MAX_FILE: u64 = 1 << 20;

/// Every keyword OpenSSH's client accepts (including deprecated ones it still parses, and
/// the common vendor additions), lower case. Anything else is a "Bad configuration option".
#[rustfmt::skip]
const KNOWN: &[&str] = &[
    "addkeystoagent", "addressfamily", "afstokenpassing", "batchmode", "bindaddress",
    "bindinterface", "canonicaldomains", "canonicalizefallbacklocal", "canonicalizehostname",
    "canonicalizemaxdots", "canonicalizepermittedcnames", "casignaturealgorithms",
    "certificatefile", "challengeresponseauthentication", "channeltimeout", "checkhostip",
    "cipher", "ciphers", "clearallforwardings", "compression", "compressionlevel",
    "connectionattempts", "connecttimeout", "controlmaster", "controlpath", "controlpersist",
    "dsaauthentication", "dynamicforward", "enableescapecommandline", "enablesshkeysign",
    "escapechar", "exitonforwardfailure", "fallbacktorsh", "fingerprinthash",
    "forkafterauthentication", "forwardagent", "forwardx11", "forwardx11timeout",
    "forwardx11trusted", "gatewayports", "globalknownhostsfile", "gssapiauthentication",
    "gssapiclientidentity", "gssapidelegatecredentials", "gssapikexalgorithms",
    "gssapikeyexchange", "gssapirenewalforcesrekey", "gssapiserveridentity", "gssapitrustdns",
    "hashknownhosts", "host", "hostbasedacceptedalgorithms", "hostbasedauthentication",
    "hostbasedkeytypes", "hostkeyalgorithms", "hostkeyalias", "hostname", "identitiesonly",
    "identityagent", "identityfile", "ignoreunknown", "include", "ipqos",
    "kbdinteractiveauthentication", "kbdinteractivedevices", "keepalive", "kerberosauthentication",
    "kerberostgtpassing", "kexalgorithms", "knownhostscommand", "localcommand", "localforward",
    "loglevel", "logverbose", "macs", "match", "nohostauthenticationforlocalhost",
    "numberofpasswordprompts", "obscurekeystroketiming", "passwordauthentication",
    "permitlocalcommand", "permitremoteopen", "pkcs11provider", "port",
    "preferredauthentications", "protocol", "proxycommand", "proxyjump", "proxyusefdpass",
    "pubkeyacceptedalgorithms", "pubkeyacceptedkeytypes", "pubkeyauthentication",
    "refuseconnection", "rekeylimit", "remotecommand", "remoteforward", "requesttty",
    "requiredrsasize", "revokedhostkeys", "rhostsauthentication", "rhostsrsaauthentication",
    "rsaauthentication", "securitykeyprovider", "sendenv", "serveralivecountmax",
    "serveraliveinterval", "sessiontype", "setenv", "skeyauthentication", "smartcarddevice",
    "stdinnull", "streamlocalbindmask", "streamlocalbindunlink", "stricthostkeychecking",
    "syslogfacility", "tag", "tcpkeepalive", "tisauthentication", "tunnel", "tunneldevice",
    "updatehostkeys", "usekeychain", "useprivilegedport", "user", "userknownhostsfile",
    "useroaming", "usersh", "verifyhostkeydns", "versionaddendum", "visualhostkey",
    "warnweakcrypto", "xauthlocation",
];

/// What one alias resolves to. `jumps` is the full ordered ProxyJump route (first hop
/// first); each hop's own `jumps` is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    pub host_name: String,
    pub port: u16,
    pub user: String,
    pub identity_files: Vec<PathBuf>,
    pub identities_only: bool,
    pub jumps: Vec<Resolved>,
    /// Seconds; `Some(0)` turns keepalives off.
    pub server_alive_interval: Option<u64>,
    /// Whether any value came from the configuration.
    pub matched: bool,
    /// Things the user should know (skipped `Match` blocks), with their config lines.
    pub notes: Vec<String>,
}

pub struct Resolver {
    path: PathBuf,
    home: PathBuf,
    local_user: String,
}

/// A value and the `file:line` it came from.
type Located = (String, String);

const SCALARS: [&str; 6] = [
    "hostname",
    "user",
    "port",
    "identitiesonly",
    "proxyjump",
    "serveraliveinterval",
];

#[derive(Default)]
struct Found {
    values: HashMap<&'static str, Located>,
    identity_files: Vec<String>,
    notes: Vec<String>,
    /// `IgnoreUnknown` patterns (first obtained value).
    ignore_unknown: Option<String>,
}

/// The text of a configuration or key file of at most [`MAX_FILE`] bytes; None when it does
/// not exist. Anything but a regular file (a folder, a pipe, a device) is refused before it
/// is opened, so a read can neither block nor run out of memory.
pub fn read_small(path: &Path) -> Result<Option<String>> {
    use std::io::Read;
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    anyhow::ensure!(meta.is_file(), "{} is not a regular file", path.display());
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut bytes))
        .with_context(|| format!("could not read {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_FILE,
        "{} is larger than {} KiB",
        path.display(),
        MAX_FILE >> 10
    );
    let text =
        String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
    Ok(Some(text))
}

impl Resolver {
    /// `path` is the config file to read (it need not exist); `home` expands `~` and `%d`;
    /// `local_user` is `%u` and the user when the config names none.
    pub fn new(path: impl Into<PathBuf>, home: impl Into<PathBuf>, local_user: String) -> Self {
        Self {
            path: path.into(),
            home: home.into(),
            local_user,
        }
    }

    /// `path` (default `<home>/.ssh/config`) for the current user; None without a home.
    pub fn for_current_user(path: Option<&Path>) -> Option<Self> {
        let home = dirs::home_dir()?;
        let path = path.map_or_else(|| home.join(".ssh").join("config"), Path::to_owned);
        Some(Self::new(path, home, local_user()))
    }

    pub fn resolve(&self, alias: &str) -> Result<Resolved> {
        self.resolve_depth(alias, 0, true, &mut 0)
    }

    /// `route`: follow the alias's own ProxyJump (only the target and first hops need it).
    /// `resolves` counts the aliases resolved so far for this connection.
    fn resolve_depth(
        &self,
        alias: &str,
        depth: usize,
        route: bool,
        resolves: &mut usize,
    ) -> Result<Resolved> {
        *resolves += 1;
        anyhow::ensure!(
            *resolves <= MAX_RESOLVES,
            "{alias}: the ProxyJump routes name more than {MAX_RESOLVES} hosts"
        );
        let mut found = Found::default();
        self.read(&self.path, alias, true, 0, &mut found)?;
        let value = |key| found.values.get(key);
        let host_name = match value("hostname") {
            Some((v, _)) => expand(v, &[('h', alias)]),
            None => alias.to_owned(),
        };
        let user = value("user").map_or_else(|| self.local_user.clone(), |(v, _)| v.clone());
        let port = match value("port") {
            Some((v, at)) => v
                .parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .with_context(|| format!("{alias}: {at}: Port must be 1-65535"))?,
            None => 22,
        };
        let server_alive_interval = match value("serveraliveinterval") {
            Some((v, at)) => Some(v.parse::<u64>().with_context(|| {
                format!("{alias}: {at}: ServerAliveInterval must be a number of seconds")
            })?),
            None => None,
        };
        let home = self.home.to_string_lossy();
        let identity_files = found
            .identity_files
            .iter()
            .map(|f| {
                let f = match f.strip_prefix('~') {
                    Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
                        format!("{home}{rest}")
                    }
                    _ => f.clone(),
                };
                let tokens = [
                    ('d', &*home),
                    ('u', &self.local_user),
                    ('h', &host_name),
                    ('r', &user),
                ];
                PathBuf::from(expand(&f, &tokens))
            })
            .collect();
        let identities_only =
            value("identitiesonly").is_some_and(|(v, _)| v.eq_ignore_ascii_case("yes"));
        let mut resolved = Resolved {
            matched: !found.values.is_empty() || !found.identity_files.is_empty(),
            identities_only,
            host_name,
            port,
            user,
            identity_files,
            jumps: Vec::new(),
            server_alive_interval,
            notes: found.notes.clone(),
        };
        let Some((jumps, at)) = value("proxyjump").filter(|_| route) else {
            return Ok(resolved);
        };
        if jumps.eq_ignore_ascii_case("none") {
            return Ok(resolved);
        }
        anyhow::ensure!(
            depth < MAX_DEPTH && jumps.split(',').count() <= MAX_DEPTH,
            "{alias}: {at}: ProxyJump route is cyclic or longer than {MAX_DEPTH} hops"
        );
        for (i, spec) in jumps.split(',').enumerate() {
            let (user, host, port) = parse_hop(spec)
                .with_context(|| format!("{alias}: {at}: bad ProxyJump hop {spec:?}"))?;
            // Later hops are reached through the chain: their own routes are never followed.
            let mut hop = self.resolve_depth(host, depth + 1, i == 0, resolves)?;
            // As OpenSSH: only the first hop's own route applies (`ssh -J a,b` reaches b
            // through a); later hops are reached through the chain.
            if i == 0 {
                resolved.jumps.append(&mut hop.jumps);
            }
            hop.jumps.clear();
            if let Some(user) = user {
                hop.user = user.to_owned();
            }
            if let Some(port) = port {
                hop.port = port;
            }
            resolved.notes.append(&mut hop.notes);
            resolved.jumps.push(hop);
        }
        anyhow::ensure!(
            resolved.jumps.len() <= MAX_DEPTH,
            "{alias}: {at}: ProxyJump route is longer than {MAX_DEPTH} hops"
        );
        resolved.notes.dedup();
        Ok(resolved)
    }

    /// Applies one file's lines for `alias`. Only called for an active Include, so the Host
    /// lines of a file included from an inactive block can never apply.
    fn read(
        &self,
        path: &Path,
        alias: &str,
        mut active: bool,
        depth: usize,
        found: &mut Found,
    ) -> Result<()> {
        let Some(text) = read_small(path).with_context(|| alias.to_owned())? else {
            return Ok(());
        };
        // Windows PowerShell and older Notepad start UTF-8 files with a byte-order mark.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        for (no, line) in text.lines().enumerate() {
            let at = format!("{}:{}", path.display(), no + 1);
            let Some((key, args)) = split_line(line).with_context(|| format!("{alias}: {at}"))?
            else {
                continue;
            };
            let first = args.first().cloned();
            // As OpenSSH, in every block: a typo must not pass for an ignored keyword.
            let ignored = |list: &str| {
                list.split(',')
                    .any(|p| super::hostkeys::glob(&p.trim().to_ascii_lowercase(), &key))
            };
            if !KNOWN.contains(&key.as_str())
                && !found.ignore_unknown.as_deref().is_some_and(ignored)
            {
                bail!(
                    "{alias}: {at}: bad configuration option {key:?} (check the spelling, or \
                     list it under IgnoreUnknown)"
                );
            }
            match key.as_str() {
                "host" => active = host_matches(&args, alias),
                "match" => {
                    active = false;
                    found.notes.push(format!(
                        "{at}: Match blocks are not supported and were skipped"
                    ));
                }
                _ if !active => {}
                "include" => {
                    anyhow::ensure!(
                        depth < MAX_DEPTH,
                        "{alias}: {at}: Include files nest deeper than {MAX_DEPTH} levels"
                    );
                    for pattern in &args {
                        for file in self.include_files(pattern) {
                            self.read(&file, alias, true, depth + 1, found)?;
                        }
                    }
                }
                "proxycommand" => {
                    let set = first
                        .as_deref()
                        .is_some_and(|v| !v.eq_ignore_ascii_case("none"));
                    // A ProxyJump obtained first wins, as in OpenSSH; otherwise fail closed.
                    if set && !found.values.contains_key("proxyjump") {
                        bail!(
                            "{alias}: {at}: ProxyCommand is not supported (Keel never runs \
                             programs from the SSH configuration); use ProxyJump instead"
                        );
                    }
                }
                "identityfile" => {
                    let file = first
                        .with_context(|| format!("{alias}: {at}: IdentityFile needs a value"))?;
                    found.identity_files.push(file);
                }
                "ignoreunknown" => {
                    if found.ignore_unknown.is_none() {
                        found.ignore_unknown = Some(args.join(","));
                    }
                }
                k => {
                    // Everything else (UserKnownHostsFile included) is ignored.
                    let Some(key) = SCALARS.into_iter().find(|known| *known == k) else {
                        continue;
                    };
                    let value =
                        first.with_context(|| format!("{alias}: {at}: {k} needs a value"))?;
                    found.values.entry(key).or_insert((value, at));
                }
            }
        }
        Ok(())
    }

    /// The files an `Include` argument names, in lexical order.
    fn include_files(&self, pattern: &str) -> Vec<PathBuf> {
        let pattern = match pattern.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None => PathBuf::from(pattern),
        };
        let pattern = if pattern.is_absolute() {
            pattern
        } else {
            self.home.join(".ssh").join(pattern)
        };
        let name = pattern
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !name.contains(['*', '?']) {
            return vec![pattern];
        }
        // ponytail: wildcards in the file name only, not in folder names.
        let Some(dir) = pattern.parent() else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| super::hostkeys::glob(&name, &e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        files
    }
}

/// The local account name (`%u`, and the user when neither Keel nor the config names one).
pub fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

/// `Host` patterns: some positive pattern matches and no `!` pattern does.
fn host_matches(patterns: &[String], alias: &str) -> bool {
    let mut hit = false;
    for p in patterns {
        match p.strip_prefix('!') {
            Some(neg) if super::hostkeys::glob(neg, alias) => return false,
            Some(_) => {}
            None => hit |= super::hostkeys::glob(p, alias),
        }
    }
    hit
}

/// `%x` tokens from `tokens`, `%%` for a literal `%`; unknown tokens are kept as written.
fn expand(text: &str, tokens: &[(char, &str)]) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some(t) => match tokens.iter().find(|(k, _)| *k == t) {
                Some((_, v)) => out.push_str(v),
                None => {
                    out.push('%');
                    out.push(t);
                }
            },
            None => out.push('%'),
        }
    }
    out
}

/// `keyword args…` or `keyword=args…`: the lower-case keyword and its arguments with the
/// quotes removed; None for blank and comment lines. An unquoted `#` starts a comment.
fn split_line(line: &str) -> Result<Option<(String, Vec<String>)>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (key, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    let mut args = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none_or(|c| *c == '#') {
            break;
        }
        let (mut arg, mut quoted) = (String::new(), false);
        for c in chars.by_ref() {
            match c {
                '"' => quoted = !quoted,
                c if c.is_whitespace() && !quoted => break,
                c => arg.push(c),
            }
        }
        anyhow::ensure!(!quoted, "unterminated quote");
        args.push(arg);
    }
    Ok(Some((key.to_ascii_lowercase(), args)))
}

/// `[user@]host[:port]`, IPv6 as `[addr]:port`.
fn parse_hop(spec: &str) -> Result<(Option<&str>, &str, Option<u16>)> {
    let spec = spec.trim();
    let (user, rest) = match spec.rsplit_once('@') {
        Some((u, r)) => (Some(u), r),
        None => (None, spec),
    };
    let (host, port) = match rest.strip_prefix('[') {
        Some(inner) => {
            let (host, after) = inner.split_once(']').context("missing ]")?;
            (host, after.strip_prefix(':'))
        }
        None => match rest.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (rest, None),
        },
    };
    let port = port
        .map(|p| p.parse::<u16>().ok().filter(|p| *p > 0).context("bad port"))
        .transpose()?;
    anyhow::ensure!(!host.is_empty(), "empty host");
    anyhow::ensure!(user.is_none_or(|u| !u.is_empty()), "empty user");
    Ok((user, host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(text: &str) -> (tempfile::TempDir, Resolver) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".ssh")).unwrap();
        let path = dir.path().join(".ssh/config");
        std::fs::write(&path, text).unwrap();
        let resolver = Resolver::new(path, dir.path(), "local".into());
        (dir, resolver)
    }

    #[test]
    fn patterns_negation_equals_quotes_and_first_wins() {
        let (_dir, r) = fixture("Host a? other !ax\n HOSTNAME=\"%h.internal\" # note\n User = remote\n Port 2200\nHost *\n Port 2222\n User fallback\n");
        let got = r.resolve("ab").unwrap();
        assert_eq!(got.host_name, "ab.internal");
        assert_eq!(got.user, "remote");
        assert_eq!(got.port, 2200);
        assert_eq!(r.resolve("ax").unwrap().port, 2222);
        assert_eq!(r.resolve("other").unwrap().port, 2200);
    }

    #[test]
    fn tokens_and_multiple_identity_files() {
        let (dir, r) = fixture("Host *\n HostName %h.internal\n User remote\n IdentityFile \"~/.ssh/key #one\"\n IdentityFile %d/%u/%h/%r\n IdentitiesOnly yes\n ServerAliveInterval 0\n");
        let got = r.resolve("alias").unwrap();
        assert_eq!(
            got.identity_files,
            [
                dir.path().join(".ssh/key #one"),
                dir.path().join("local/alias.internal/remote")
            ]
        );
        assert!(got.identities_only);
        assert_eq!(got.server_alive_interval, Some(0));
    }

    #[test]
    fn includes_globs_and_block_context() {
        let (dir, r) = fixture(
            "Include missing *.conf\nHost ignored\n Include inactive\nHost *\n Port 2299\n",
        );
        let ssh = dir.path().join(".ssh");
        std::fs::write(ssh.join("01.conf"), "Host alias\n Port 2201\n").unwrap();
        std::fs::write(ssh.join("02.conf"), "Host *\n Port 2202\n").unwrap();
        std::fs::write(ssh.join("inactive"), "Host *\n ProxyCommand forbidden\n").unwrap();
        assert_eq!(r.resolve("alias").unwrap().port, 2201);
        assert_eq!(r.resolve("another").unwrap().port, 2202);
    }

    #[test]
    fn match_is_skipped_without_running_exec() {
        let (_dir, r) =
            fixture("Match exec forbidden\n ProxyCommand forbidden\n Port 1\nHost *\n Port 2222\n");
        let got = r.resolve("alias").unwrap();
        assert_eq!(got.port, 2222);
        assert!(got.notes.iter().any(|n| n.contains("Match")));
    }

    #[test]
    fn refuses_proxycommand_and_reports_alias_and_line() {
        let (_dir, r) = fixture("Host alias\n ProxyCommand forbidden\n");
        let err = r.resolve("alias").unwrap_err().to_string();
        assert!(
            err.contains("alias") && err.contains(":2") && err.contains("ProxyCommand"),
            "{err}"
        );
        let (_dir, r) = fixture("Host *\n IdentityFile \"unfinished\n");
        let err = r.resolve("alias").unwrap_err().to_string();
        assert!(err.contains("alias") && err.contains(":2"), "{err}");
    }

    #[test]
    fn proxycommand_in_a_jump_is_refused_and_an_earlier_proxyjump_wins() {
        let (_dir, r) = fixture(
            "Host target
 ProxyJump hop
Host hop
 ProxyCommand forbidden
",
        );
        let err = format!("{:#}", r.resolve("target").unwrap_err());
        assert!(err.contains("hop") && err.contains("ProxyCommand"), "{err}");
        let (_dir, r) = fixture(
            "Host target
 ProxyJump hop
 ProxyCommand ignored
 UserKnownHostsFile /x
",
        );
        assert_eq!(r.resolve("target").unwrap().jumps[0].host_name, "hop");
        let (_dir, r) = fixture(
            "host TARGET
 proxycommand none
",
        );
        assert!(r.resolve("target").unwrap().jumps.is_empty());
    }

    #[test]
    fn jump_chains_resolve_each_alias_and_hop_overrides() {
        let (_dir, r) = fixture("Host target\n ProxyJump hop1,override@hop2:2202\nHost hop1\n Port 2201\nHost hop2\n User config\n Port 2200\nHost *\n HostName %h.internal\n");
        let got = r.resolve("target").unwrap();
        assert_eq!(got.jumps.len(), 2);
        assert_eq!(got.jumps[0].host_name, "hop1.internal");
        assert_eq!(got.jumps[0].port, 2201);
        assert_eq!(got.jumps[1].user, "override");
        assert_eq!(got.jumps[1].port, 2202);
    }

    #[test]
    fn cycles_and_include_depth_fail_boundedly() {
        let (_dir, r) = fixture("Host *\n ProxyJump loop\n");
        assert!(r.resolve("alias").is_err());
        let (_dir, r) = fixture("Include config\n");
        assert!(r.resolve("alias").is_err());
    }

    /// Review M1: OpenSSH refuses both, so neither may make the first block a default.
    #[test]
    fn a_byte_order_mark_is_skipped_and_a_misspelt_keyword_is_refused() {
        let blocks =
            "Host bastion\n HostName bastion.example.invalid\n User admin\nHost nas\n HostName nas.example.invalid\n";
        let (_dir, r) = fixture(&format!("\u{feff}{blocks}"));
        let got = r.resolve("nas").unwrap();
        assert_eq!(
            (got.host_name.as_str(), got.user.as_str()),
            ("nas.example.invalid", "local")
        );
        let (_dir, r) = fixture(&blocks.replacen("Host", "Hots", 1));
        let err = r.resolve("nas").unwrap_err().to_string();
        assert!(
            err.contains("nas") && err.contains(":1") && err.contains("hots"),
            "{err}"
        );
        // Unknown keywords are refused in inactive blocks too, unless IgnoreUnknown lists them.
        let (_dir, r) = fixture("Host other\n Bogus 1\n");
        assert!(r.resolve("alias").is_err());
        let (_dir, r) = fixture("IgnoreUnknown bog*,x\nHost other\n Bogus 1\nHost *\n Port 2200\n");
        assert_eq!(r.resolve("alias").unwrap().port, 2200);
        let (_dir, r) = fixture("UseKeychain yes\nhostname=%h.internal\n");
        assert_eq!(r.resolve("a").unwrap().host_name, "a.internal");
    }

    /// Review minor 2: later hops' own routes are not followed (no exponential resolve), and
    /// files are read through a size limit.
    #[test]
    fn nested_routes_resolve_fast_and_huge_files_are_refused() {
        let mut text = String::new();
        for level in 0..8 {
            let next = level + 1;
            text += &format!("Host l{level}\n ProxyJump z,l{next},l{next},l{next}\n");
        }
        let (dir, r) = fixture(&text);
        let started = std::time::Instant::now();
        let got = r.resolve("l0").unwrap();
        let took = started.elapsed();
        assert_eq!(got.jumps.len(), 4);
        if std::env::var_os("CI").is_none() {
            assert!(took < std::time::Duration::from_millis(500), "{took:?}");
        }
        let huge = dir.path().join(".ssh/huge");
        std::fs::write(&huge, "#".repeat(MAX_FILE as usize + 1)).unwrap();
        std::fs::write(dir.path().join(".ssh/config"), "Include huge\n").unwrap();
        let err = format!("{:#}", r.resolve("alias").unwrap_err());
        assert!(err.contains("larger than"), "{err}");
        std::fs::write(dir.path().join(".ssh/config"), "Include .\n").unwrap();
        let err = format!("{:#}", r.resolve("alias").unwrap_err());
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[test]
    fn missing_config_and_proxyjump_none() {
        let (dir, r) = fixture("Host *\n ProxyJump none\nHost *\n ProxyJump unreachable\n");
        assert!(r.resolve("alias").unwrap().jumps.is_empty());
        std::fs::remove_file(dir.path().join(".ssh/config")).unwrap();
        let got = r.resolve("alias").unwrap();
        assert!(!got.matched);
        assert_eq!(got.host_name, "alias");
        assert_eq!(got.user, "local");
    }
}
