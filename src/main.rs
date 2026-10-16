use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// Sign SSH keys with OpenBao using named profiles, like `aws --profile`.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Path to the config file [default: ~/.config/baosign/config.toml]
    #[arg(short, long, global = true, env = "BAOSIGN_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Sign your public key using a profile
    Sign {
        /// Profile to use
        #[arg(env = "BAOSIGN_PROFILE", default_value = "default")]
        profile: String,

        #[command(flatten)]
        opts: SignOpts,
    },
    /// Sign your public key, then connect with ssh
    Ssh {
        /// Profile to use
        profile: String,

        #[command(flatten)]
        opts: SignOpts,

        /// Target, e.g. user@server.com
        target: String,

        /// Extra arguments passed through to ssh (after `--`)
        #[arg(last = true)]
        ssh_args: Vec<String>,
    },
    /// List the profiles defined in the config file
    List,
}

#[derive(Args, Debug)]
struct SignOpts {
    /// Print the commands instead of running them
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Override the role from the profile
    #[arg(long)]
    role: Option<String>,

    /// Override the valid principals from the profile
    #[arg(long)]
    principals: Option<String>,

    /// Override the certificate TTL from the profile
    #[arg(long)]
    ttl: Option<String>,
}

impl SignOpts {
    /// The selected profile with command-line overrides applied.
    fn profile(&self, config: &Config, name: &str) -> Result<Profile> {
        let mut p = config.resolve(name)?;
        if self.role.is_some() {
            p.role = self.role.clone();
        }
        if self.principals.is_some() {
            p.valid_principals = self.principals.clone();
        }
        if self.ttl.is_some() {
            p.ttl = self.ttl.clone();
        }
        Ok(p)
    }
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields)]
struct Profile {
    /// Path to the bao binary (default: `bao` on PATH)
    bao_bin: Option<String>,
    /// Sets BAO_ADDR for bao
    address: Option<String>,
    /// Sets BAO_NAMESPACE for bao
    namespace: Option<String>,
    /// Auth method for `bao login` when not authenticated, e.g. "oidc"
    login_method: Option<String>,
    /// Additional args for `bao login`, e.g. ["-path=oidc", "role=admin"]
    login_args: Option<Vec<String>>,
    /// Mount of the SSH secrets engine (default: "ssh")
    mount_point: Option<String>,
    /// Role to sign the key with
    role: Option<String>,
    private_key_path: Option<String>,
    /// Default: `<private_key_path>.pub`
    public_key_path: Option<String>,
    /// Where to write the signed certificate (default: `<private_key_path>-cert.pub`)
    cert_path: Option<String>,
    valid_principals: Option<String>,
    ttl: Option<String>,
    /// Certificate extensions to request; when unset, the role's default_extensions apply
    extensions: Option<Vec<String>>,
}

impl Profile {
    /// Fill unset fields of `self` from `base`.
    fn merged_over(self, base: &Profile) -> Profile {
        let base = base.clone();
        Profile {
            bao_bin: self.bao_bin.or(base.bao_bin),
            address: self.address.or(base.address),
            namespace: self.namespace.or(base.namespace),
            login_method: self.login_method.or(base.login_method),
            login_args: self.login_args.or(base.login_args),
            mount_point: self.mount_point.or(base.mount_point),
            role: self.role.or(base.role),
            private_key_path: self.private_key_path.or(base.private_key_path),
            public_key_path: self.public_key_path.or(base.public_key_path),
            cert_path: self.cert_path.or(base.cert_path),
            valid_principals: self.valid_principals.or(base.valid_principals),
            ttl: self.ttl.or(base.ttl),
            extensions: self.extensions.or(base.extensions),
        }
    }
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Settings shared by every profile; also used when `--profile default` has no own section.
    #[serde(default)]
    default: Profile,
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

impl Config {
    fn resolve(&self, name: &str) -> Result<Profile> {
        match self.profiles.get(name) {
            Some(p) => Ok(p.clone().merged_over(&self.default)),
            None if name == "default" => Ok(self.default.clone()),
            None => {
                let known: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
                bail!(
                    "profile '{name}' not found (available: {})",
                    if known.is_empty() { "none".into() } else { known.join(", ") }
                )
            }
        }
    }
}

/// Key and certificate locations with defaults applied and `~`/`$VARS` expanded.
#[derive(Debug, PartialEq)]
struct KeyPaths {
    private: String,
    public: String,
    cert: String,
}

impl KeyPaths {
    fn from_profile(p: &Profile) -> Result<KeyPaths> {
        let private = expand(
            p.private_key_path
                .as_deref()
                .context("profile sets no `private_key_path`")?,
        )?;
        let public = match &p.public_key_path {
            Some(path) => expand(path)?,
            None => format!("{private}.pub"),
        };
        let cert = match &p.cert_path {
            Some(path) => expand(path)?,
            None => format!("{private}-cert.pub"),
        };
        Ok(KeyPaths { private, public, cert })
    }
}

fn default_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".config/baosign/config.toml"))
}

fn expand(s: &str) -> Result<String> {
    Ok(shellexpand::full(s)
        .with_context(|| format!("failed to expand '{s}'"))?
        .into_owned())
}

/// API path for signing, e.g. `ssh/sign/my-role`.
fn sign_path(p: &Profile) -> Result<String> {
    let mount = p.mount_point.as_deref().unwrap_or("ssh").trim_matches('/');
    let role = p.role.as_deref().context("profile sets no `role`")?;
    Ok(format!("{mount}/sign/{role}"))
}

/// JSON body for the sign request. Unset fields are left out so the role's defaults apply.
fn sign_body(p: &Profile, public_key: &str) -> Value {
    let mut body = Map::new();
    body.insert("public_key".into(), json!(public_key.trim()));
    body.insert("cert_type".into(), json!("user"));
    if let Some(v) = &p.valid_principals {
        body.insert("valid_principals".into(), json!(v));
    }
    if let Some(v) = &p.ttl {
        body.insert("ttl".into(), json!(v));
    }
    if let Some(exts) = &p.extensions {
        let map: Map<String, Value> = exts.iter().map(|e| (e.clone(), json!(""))).collect();
        body.insert("extensions".into(), Value::Object(map));
    }
    Value::Object(body)
}

/// Arguments for `bao login`, or None if the profile has no login configured.
fn login_args(p: &Profile) -> Option<Vec<String>> {
    let method = p.login_method.as_ref()?;
    let mut args = vec!["login".to_string(), format!("-method={method}")];
    if let Some(extra) = &p.login_args {
        args.extend(extra.iter().cloned());
    }
    Some(args)
}

/// A `bao` command with the profile's environment applied.
fn bao_command(bin: &str, p: &Profile) -> Command {
    let mut cmd = Command::new(bin);
    if let Some(a) = &p.address {
        cmd.env("BAO_ADDR", a);
    }
    if let Some(ns) = &p.namespace {
        cmd.env("BAO_NAMESPACE", ns);
    }
    cmd
}

/// True if the current token is valid (`bao token lookup` succeeds).
fn is_authenticated(bin: &str, p: &Profile) -> Result<bool> {
    let status = bao_command(bin, p)
        .args(["token", "lookup"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to execute '{bin}'"))?;
    Ok(status.success())
}

/// Sign the public key and return the certificate.
fn sign(bin: &str, p: &Profile, body: &Value) -> Result<String> {
    let mut child = bao_command(bin, p)
        .args(["write", "-field=signed_key", &sign_path(p)?, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to execute '{bin}'"))?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(body.to_string().as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("signing the key failed");
    }
    let cert = String::from_utf8(out.stdout).context("certificate is not valid UTF-8")?;
    Ok(format!("{}\n", cert.trim()))
}

fn write_cert(path: &str, cert: &str) -> Result<()> {
    if let Some(dir) = Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, cert).with_context(|| format!("failed to write certificate {path}"))
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_=./@:,+%".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn render(env: &[(&str, &Option<String>)], bin: &str, args: &[String]) -> String {
    let mut parts: Vec<String> = env
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={}", shell_quote(v))))
        .collect();
    parts.push(shell_quote(bin));
    parts.extend(args.iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

fn load_config(path: &PathBuf) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = match cli.config {
        Some(p) => p,
        None => default_config_path()?,
    };
    let config = load_config(&path)?;

    match cli.command {
        Cmd::List => {
            for name in config.profiles.keys() {
                println!("{name}");
            }
            Ok(())
        }
        Cmd::Sign { profile, opts } => {
            run_sign(&opts.profile(&config, &profile)?, opts.dry_run)?;
            Ok(())
        }
        Cmd::Ssh {
            profile,
            opts,
            target,
            ssh_args: extra,
        } => {
            let p = opts.profile(&config, &profile)?;
            let keys = run_sign(&p, opts.dry_run)?;
            let argv = ssh_args(&keys, &target, &extra);
            if opts.dry_run {
                println!("{}", render(&[], "ssh", &argv));
                return Ok(());
            }
            exec_ssh(&argv)
        }
    }
}

/// Arguments for ssh (excluding the binary itself).
fn ssh_args(keys: &KeyPaths, target: &str, extra: &[String]) -> Vec<String> {
    let mut args = vec![
        "-i".to_string(),
        keys.private.clone(),
        "-o".to_string(),
        format!("CertificateFile={}", keys.cert),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        target.to_string(),
    ];
    args.extend(extra.iter().cloned());
    args
}

fn exec_ssh(argv: &[String]) -> Result<()> {
    let mut cmd = Command::new("ssh");
    cmd.args(argv);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Replace this process so ssh owns the terminal and signals directly.
        let err = cmd.exec();
        Err(err).context("failed to execute 'ssh'")
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().context("failed to execute 'ssh'")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Log in if needed and sign the key. Returns the key paths used.
fn run_sign(p: &Profile, dry_run: bool) -> Result<KeyPaths> {
    let bao = expand(p.bao_bin.as_deref().unwrap_or("bao"))?;
    let keys = KeyPaths::from_profile(p)?;
    let login_argv = login_args(p);

    if dry_run {
        let env = [("BAO_ADDR", &p.address), ("BAO_NAMESPACE", &p.namespace)];
        if let Some(argv) = &login_argv {
            println!("# if not authenticated:\n{}", render(&env, &bao, argv));
        }
        let body = sign_body(p, &format!("<contents of {}>", keys.public));
        let sign_argv = ["write", "-field=signed_key", &sign_path(p)?, "-"].map(String::from);
        println!(
            "{} > {} <<'EOF'\n{}\nEOF",
            render(&env, &bao, &sign_argv),
            shell_quote(&keys.cert),
            serde_json::to_string_pretty(&body)?
        );
        return Ok(keys);
    }

    let public_key = std::fs::read_to_string(&keys.public)
        .with_context(|| format!("failed to read public key {}", keys.public))?;

    if let Some(argv) = &login_argv
        && !is_authenticated(&bao, p)?
    {
        eprintln!("baosign: not authenticated, running bao login");
        let status = bao_command(&bao, p)
            .args(argv)
            .status()
            .with_context(|| format!("failed to execute '{bao}'"))?;
        if !status.success() {
            bail!("bao login failed");
        }
    }

    let cert = sign(&bao, p, &sign_body(p, &public_key))?;
    write_cert(&keys.cert, &cert)?;
    eprintln!("baosign: certificate written to {}", keys.cert);
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [default]
        login_method = "oidc"
        login_args = ["role=dev"]
        private_key_path = "/keys/id"
        ttl = "10m"

        [profiles.web-dev]
        role = "web-dev"
        valid_principals = "deploy"

        [profiles.prod]
        role = "prod"
        mount_point = "ssh-prod"
        ttl = "1m"
        extensions = ["permit-pty", "permit-port-forwarding"]
        cert_path = "/certs/prod-cert.pub"
        login_args = ["-path=oidc-admin", "role=admin"]
    "#;

    fn cfg() -> Config {
        toml::from_str(SAMPLE).unwrap()
    }

    #[test]
    fn sign_request_uses_role_defaults_when_unset() {
        let p = cfg().resolve("web-dev").unwrap();
        assert_eq!(sign_path(&p).unwrap(), "ssh/sign/web-dev");
        assert_eq!(
            sign_body(&p, "ssh-ed25519 AAAA\n"),
            json!({
                "public_key": "ssh-ed25519 AAAA",
                "cert_type": "user",
                "valid_principals": "deploy",
                "ttl": "10m",
            })
        );
    }

    #[test]
    fn sign_request_with_overrides_and_extensions() {
        let p = cfg().resolve("prod").unwrap();
        assert_eq!(sign_path(&p).unwrap(), "ssh-prod/sign/prod");
        let body = sign_body(&p, "k");
        assert_eq!(body["ttl"], "1m");
        assert_eq!(
            body["extensions"],
            json!({"permit-pty": "", "permit-port-forwarding": ""})
        );
        assert!(body.get("valid_principals").is_none());
    }

    #[test]
    fn key_paths_default_from_private_key() {
        let p = cfg().resolve("web-dev").unwrap();
        assert_eq!(
            KeyPaths::from_profile(&p).unwrap(),
            KeyPaths {
                private: "/keys/id".into(),
                public: "/keys/id.pub".into(),
                cert: "/keys/id-cert.pub".into(),
            }
        );
        let p = cfg().resolve("prod").unwrap();
        assert_eq!(KeyPaths::from_profile(&p).unwrap().cert, "/certs/prod-cert.pub");
    }

    #[test]
    fn cli_parses_subcommands() {
        let cli = Cli::try_parse_from(["baosign", "sign", "dev", "--ttl", "1m"]).unwrap();
        match cli.command {
            Cmd::Sign { profile, opts } => {
                assert_eq!(profile, "dev");
                assert_eq!(opts.ttl.as_deref(), Some("1m"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::try_parse_from(["baosign", "ssh", "dev", "u@h", "--", "-L", "80:x:80"]).unwrap();
        match cli.command {
            Cmd::Ssh {
                profile,
                target,
                ssh_args,
                ..
            } => {
                assert_eq!(profile, "dev");
                assert_eq!(target, "u@h");
                assert_eq!(ssh_args, ["-L", "80:x:80"]);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(Cli::try_parse_from(["baosign", "ssh", "u@h"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["baosign", "list"]).unwrap().command,
            Cmd::List
        ));
    }

    #[test]
    fn ssh_command_line() {
        let p = cfg().resolve("prod").unwrap();
        let keys = KeyPaths::from_profile(&p).unwrap();
        assert_eq!(
            ssh_args(&keys, "deploy@host", &["-L".into(), "8080:localhost:80".into()]),
            [
                "-i",
                "/keys/id",
                "-o",
                "CertificateFile=/certs/prod-cert.pub",
                "-o",
                "IdentitiesOnly=yes",
                "deploy@host",
                "-L",
                "8080:localhost:80",
            ]
        );
    }

    #[test]
    fn login_args_inherit_and_override() {
        let c = cfg();
        assert_eq!(
            login_args(&c.resolve("web-dev").unwrap()).unwrap(),
            ["login", "-method=oidc", "role=dev"]
        );
        assert_eq!(
            login_args(&c.resolve("prod").unwrap()).unwrap(),
            ["login", "-method=oidc", "-path=oidc-admin", "role=admin"]
        );
    }

    #[test]
    fn no_login_without_method() {
        let c: Config = toml::from_str("[default]\nrole = \"x\"").unwrap();
        assert!(login_args(&c.resolve("default").unwrap()).is_none());
    }

    #[test]
    fn missing_role_errors() {
        let c: Config = toml::from_str("[default]\nprivate_key_path = \"/k\"").unwrap();
        assert!(sign_path(&c.resolve("default").unwrap()).is_err());
    }

    #[test]
    fn unknown_profile_errors() {
        let err = cfg().resolve("nope").unwrap_err().to_string();
        assert!(err.contains("prod, web-dev"), "{err}");
    }

    #[test]
    fn old_keys_are_rejected() {
        assert!(toml::from_str::<Config>("[default]\nmode = \"ca\"").is_err());
    }
}
