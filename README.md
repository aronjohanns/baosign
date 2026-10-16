# baosign

Sign SSH keys with OpenBao, using profiles like `aws --profile`.

`baosign sign <profile>`:

1. Checks your token with `bao token lookup`. If it is not valid and `login_method` is set, runs `bao login`.
2. Signs your public key with `bao write <mount_point>/sign/<role>`.
3. Saves the certificate to `cert_path`.

Then connect with plain `ssh`. It picks up `<key>-cert.pub` next to your key on its own.

`baosign ssh <profile> user@host` does the same, then connects for you.

Requires the [OpenBao CLI](https://openbao.org/docs/install/) (`bao`) and OpenSSH.

## Install

### macOS

```
brew install aronjohanns/tap/baosign
```

### Linux / macOS

```
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/aronjohanns/baosign/releases/latest/download/baosign-installer.sh | sh
```

### Windows

```
powershell -ExecutionPolicy Bypass -c "irm https://github.com/aronjohanns/baosign/releases/latest/download/baosign-installer.ps1 | iex"
```

### Cargo

```
cargo install baosign
```

Binaries are on the [releases page](https://github.com/aronjohanns/baosign/releases).

Then create a config from [config.example.toml](config.example.toml):

```sh
mkdir -p ~/.config/baosign
curl -LsSf https://raw.githubusercontent.com/aronjohanns/baosign/main/config.example.toml -o ~/.config/baosign/config.toml
```

## Usage

```sh
baosign sign web-dev                  # sign using the web-dev profile
ssh -i ~/.ssh/id_ed25519 deploy@web01.example.com

baosign ssh web-dev deploy@web01.example.com                         # sign, then connect
baosign ssh web-dev deploy@web01.example.com -- -L 8080:localhost:80 # args after -- go to ssh
baosign ssh web-dev deploy@web01.example.com -n                      # dry run

baosign sign web-dev --ttl 2m         # override per run (works with ssh too)
baosign sign web-dev -n               # dry run: print the commands (works with ssh too)
baosign sign                          # uses $BAOSIGN_PROFILE, then `default`
baosign list                          # list profiles
```

| Command / flag | What it does |
|---|---|
| `sign [profile]` | Sign your key with this profile. Default: `$BAOSIGN_PROFILE`, then `default`. |
| `ssh <profile> <user@host> [-- args]` | Sign, then run `ssh`. The profile is required. Args after `--` go to `ssh`. |
| `list` | List profiles. |
| `-n`, `--dry-run` | `sign`/`ssh`: print the commands instead of running them. |
| `--role` | `sign`/`ssh`: use this role instead of the profile's. |
| `--principals` | `sign`/`ssh`: use these principals instead of the profile's. |
| `--ttl` | `sign`/`ssh`: use this TTL instead of the profile's. |
| `-c`, `--config` | Config file. Default: `$BAOSIGN_CONFIG`, then `~/.config/baosign/config.toml`. |

## Config

See `config.example.toml`. Keys in `[default]` apply to every profile. A profile can override any of them.
List values in a profile replace the list in `[default]`; they are not combined.
Paths can use `~` and `$VARS`.

| Key | What it does |
|---|---|
| `address` | OpenBao server URL. Sets `BAO_ADDR`. |
| `namespace` | OpenBao namespace. Sets `BAO_NAMESPACE`. |
| `bao_bin` | Path to `bao`. Default: `bao`. |
| `login_method` | Auth method for `bao login`, e.g. `oidc`. If set, baosign logs in when you are not logged in. |
| `login_args` | Other `bao login` arguments, as a list, e.g. `["-path=oidc", "role=dev"]`. |
| `mount_point` | Where the SSH secrets engine is mounted. Default: `ssh`. |
| `role` | Role to sign the key with. **Required.** |
| `private_key_path` | Your private SSH key. **Required.** |
| `public_key_path` | The public key to sign. Default: `<private_key_path>.pub`. |
| `cert_path` | Where to save the signed certificate. Default: `<private_key_path>-cert.pub`. |
| `valid_principals` | Names the certificate is valid for. Must match the user you log in as. Default: the role's `default_user`. |
| `ttl` | How long the certificate is valid, e.g. `10m`. Default: the role's `ttl`. Capped by the role's `max_ttl`. |
| `extensions` | Certificate permissions to ask for, as a list, e.g. `["permit-pty", "permit-port-forwarding"]`. Default: the role's `default_extensions`. The role must allow them. |

### Extensions

| Extension | Allows |
|---|---|
| `permit-pty` | An interactive terminal (normal shell). |
| `permit-port-forwarding` | `ssh -L` / `-R` tunnels. |
| `permit-agent-forwarding` | `ssh -A`. |
| `permit-X11-forwarding` | `ssh -X`. |
| `permit-user-rc` | Running `~/.ssh/rc` on the server. |

Check what a certificate got with `ssh-keygen -L -f <cert_path>`.

### The ssh command

`baosign ssh` runs:

```sh
ssh -i <private_key_path> -o CertificateFile=<cert_path> -o IdentitiesOnly=yes user@host [args after --]
```

`IdentitiesOnly=yes` stops ssh from trying other keys first, which can hit the server's limit on failed attempts.

### Certificate file

With the default `cert_path`, `ssh -i <key>` finds the certificate on its own.
With a different `cert_path`, tell ssh where it is: `ssh -i <key> -o CertificateFile=<cert_path> user@host`.

Profiles with the same key and `cert_path` write the same file, so the last `sign` wins. Sign again when you switch profiles.
