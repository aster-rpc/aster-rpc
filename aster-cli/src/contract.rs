//! `aster contract …`: contract manifests and typed clients. Both hand off to
//! a language binding, which alone can read its classes and knows its own
//! runtime API, through a versioned JSON envelope on stdout:
//!
//! ```text
//! gen     {"aster_manifest_handoff": 1, "manifests": [<ContractManifest>, ...]}
//! client  {"aster_codegen_handoff": 1, "files": [<path>, ...], "usage": "..."}
//! ```
//!
//! Python: `python -m aster.contract gen …` and `python -m aster.codegen
//! client …`. TypeScript: `aster-gen client …`. For `gen` the CLI adds
//! version-control details and writes the manifest file, keeping the
//! binding's key order.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use aster_cli_kit::out::{DIM, HEADER, Styler, table};
use aster_cli_kit::{Ctx, Error, ErrorKind, Report, Result, Tool};
use aster_directory_client::pyjson::{DumpOptions, PyJson};
use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::json;

const DEFAULT_MANIFEST: &str = ".aster/manifest.json";
const HANDOFF_VERSION: i128 = 1;

#[derive(Debug, Subcommand)]
pub enum ContractCommand {
    /// Generate the manifest of services in your code, via their language binding
    Gen {
        /// Service to include, repeatable (Python: `module.path:ClassName`)
        #[arg(long = "service", value_name = "SPEC", required = true)]
        services: Vec<String>,
        /// Language of the services
        #[arg(long, value_enum, default_value_t = Lang::Python)]
        lang: Lang,
        /// Semantic version to record in the manifest
        #[arg(long, value_name = "VERSION")]
        semver: Option<String>,
        /// Manifest file to write
        #[arg(long, value_name = "FILE", default_value = DEFAULT_MANIFEST)]
        out: PathBuf,
        /// Python interpreter [default: $ASTER_PYTHON, ./.venv, then python3 on PATH]
        #[arg(long, value_name = "PATH")]
        python: Option<PathBuf>,
    },
    /// Generate a typed client library from a manifest file, via the target language's binding
    Client {
        /// Manifest file
        #[arg(value_name = "FILE", default_value = DEFAULT_MANIFEST)]
        manifest: PathBuf,
        /// Language to generate
        #[arg(long, value_enum)]
        lang: ClientLang,
        /// Directory to write the client package into
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// Package name [default: from the manifest file or its first service]
        #[arg(long, value_name = "NAME")]
        package: Option<String>,
        /// Service address to show in the usage example
        #[arg(long, value_name = "TICKET")]
        address: Option<String>,
        /// Python interpreter for `--lang python` [default: as for `gen`]
        #[arg(long, value_name = "PATH")]
        python: Option<PathBuf>,
    },
    /// Show the services and methods in a manifest file
    Show {
        /// Manifest file
        #[arg(value_name = "FILE", default_value = DEFAULT_MANIFEST)]
        file: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Lang {
    Python,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ClientLang {
    Python,
    Typescript,
}

pub async fn run(ctx: &Ctx, cmd: ContractCommand) -> Result<()> {
    match cmd {
        ContractCommand::Gen {
            services,
            lang: Lang::Python,
            semver,
            out,
            python,
        } => {
            let manifests = python_handoff(python.as_deref(), &services, semver.as_deref())?;
            ctx.emit(&write_manifests(manifests, &out)?)
        }
        ContractCommand::Client {
            manifest,
            lang,
            out,
            package,
            address,
            python,
        } => ctx.emit(&client(
            &manifest,
            lang,
            &out,
            package,
            address.as_deref(),
            python.as_deref(),
        )?),
        ContractCommand::Show { file } => ctx.emit(&show(&file)?),
    }
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "contract_show",
            "List the services and methods in an Aster contract manifest file.",
            json!({
                "type": "object",
                "properties": {"file": {"type": "string", "description": format!("Manifest path (default {DEFAULT_MANIFEST})")}},
                "additionalProperties": false,
            }),
            |_: Arc<Ctx>, args: ShowArgs| async move {
                show(Path::new(args.file.as_deref().unwrap_or(DEFAULT_MANIFEST)))
            },
        )
        .read_only(),
    ]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowArgs {
    file: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ServiceSummary {
    service: String,
    version: i128,
    contract_id: String,
    scoped: String,
    methods: Vec<MethodSummary>,
}

#[derive(Debug, Serialize)]
pub struct MethodSummary {
    name: String,
    pattern: String,
    request: String,
    response: String,
}

#[derive(Debug, Serialize)]
pub struct Manifests {
    file: PathBuf,
    services: Vec<ServiceSummary>,
}

impl Report for Manifests {
    fn human(&self, style: Styler) -> String {
        let mut out = Vec::new();
        for s in &self.services {
            out.push(format!(
                "{}  {}",
                style.paint(HEADER, &format!("{} v{}", s.service, s.version)),
                style.paint(
                    DIM,
                    &format!(
                        "{} method{} · {} · {}",
                        s.methods.len(),
                        if s.methods.len() == 1 { "" } else { "s" },
                        s.scoped,
                        self.file.display()
                    )
                )
            ));
            out.push(format!("  contract {}", s.contract_id));
            let rows: Vec<Vec<String>> = s
                .methods
                .iter()
                .map(|m| {
                    vec![
                        m.name.clone(),
                        m.pattern.clone(),
                        format!("{} → {}", m.request, m.response),
                    ]
                })
                .collect();
            for line in table(style, &["METHOD", "PATTERN", "REQUEST → RESPONSE"], &rows).lines()
            {
                out.push(format!("  {line}"));
            }
            out.push(String::new());
        }
        out.join("\n").trim_end().to_string()
    }
}

fn text(v: Option<&PyJson>) -> String {
    match v {
        Some(PyJson::Str(s)) => s.clone(),
        Some(PyJson::Int(i)) => i.to_string(),
        _ => String::new(),
    }
}

fn summarise(m: &PyJson) -> ServiceSummary {
    let methods = match m.get("methods") {
        Some(PyJson::Array(ms)) => ms
            .iter()
            .map(|mm| MethodSummary {
                name: text(mm.get("name")),
                pattern: text(mm.get("pattern")),
                request: text(mm.get("request_wire_tag").or(mm.get("request_type"))),
                response: text(mm.get("response_wire_tag").or(mm.get("response_type"))),
            })
            .collect(),
        _ => Vec::new(),
    };
    ServiceSummary {
        service: text(m.get("service")),
        version: match m.get("version") {
            Some(PyJson::Int(i)) => *i,
            _ => 1,
        },
        contract_id: text(m.get("contract_id")),
        scoped: match text(m.get("scoped")) {
            s if s.is_empty() => "shared".into(),
            s => s,
        },
        methods,
    }
}

fn show(file: &Path) -> Result<Manifests> {
    let raw = std::fs::read_to_string(file).map_err(|e| {
        Error::from(e)
            .with_kind(ErrorKind::NotFound)
            .with_hint("generate one with `aster contract gen --service MODULE:CLASS`")
    })?;
    let manifests =
        match PyJson::parse(&raw).map_err(|e| Error::usage(format!("{}: {e}", file.display())))? {
            PyJson::Array(items) => items,
            single => vec![single],
        };
    Ok(Manifests {
        file: file.to_path_buf(),
        services: manifests.iter().map(summarise).collect(),
    })
}

/// Fill version-control details the binding leaves out, then write one
/// manifest as an object or several as an array.
fn write_manifests(mut manifests: Vec<PyJson>, out: &Path) -> Result<Manifests> {
    let vcs = [
        ("vcs_revision", git(&["rev-parse", "HEAD"])),
        ("vcs_tag", git(&["describe", "--tags", "--exact-match"])),
        ("vcs_url", git(&["remote", "get-url", "origin"])),
    ];
    for m in &mut manifests {
        if let PyJson::Object(fields) = m {
            for (key, value) in &vcs {
                if let (Some(slot), Some(v)) = (fields.iter_mut().find(|(k, _)| k == key), value)
                    && slot.1 == PyJson::Null
                {
                    slot.1 = PyJson::Str(v.clone());
                }
            }
        }
    }
    let doc = match manifests.len() {
        1 => manifests[0].clone(),
        _ => PyJson::Array(manifests.clone()),
    };
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = doc.dumps(DumpOptions::PRETTY)?;
    text.push('\n');
    std::fs::write(out, text)?;
    Ok(Manifests {
        file: out.to_path_buf(),
        services: manifests.iter().map(summarise).collect(),
    })
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// `--python`, `$ASTER_PYTHON`, the project's `.venv`, then `python3`.
fn python_interpreter(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(p) = std::env::var_os("ASTER_PYTHON").filter(|p| !p.is_empty()) {
        return p.into();
    }
    let venv = if cfg!(windows) {
        ".venv/Scripts/python.exe"
    } else {
        ".venv/bin/python"
    };
    if Path::new(venv).exists() {
        return venv.into();
    }
    if cfg!(windows) {
        "python".into()
    } else {
        "python3".into()
    }
}

/// A language binding's command: what to run and how to describe it.
struct Binding {
    program: PathBuf,
    args: Vec<String>,
    /// For messages, e.g. "the Python binding".
    name: &'static str,
    /// Shown when the program or package is missing.
    install_hint: String,
}

impl Binding {
    fn python(explicit: Option<&Path>, module: &str) -> Self {
        let program = python_interpreter(explicit);
        Self {
            install_hint: format!(
                "{} has no current `aster` package; install it there, or pass --python / set ASTER_PYTHON",
                program.display()
            ),
            args: vec!["-m".into(), module.into()],
            program,
            name: "the Python binding",
        }
    }

    /// `$ASTER_TS_GEN` (the `aster-gen` command to run), else the project's own
    /// `node_modules/.bin/aster-gen`, found from the working directory up.
    /// Never `npx` or `bunx`: they would look the name up in the public registry.
    fn typescript() -> Result<Self> {
        let install_hint =
            "install @aster-rpc/aster in this project, or set ASTER_TS_GEN to the `aster-gen` command to run".to_string();
        if let Ok(configured) = std::env::var("ASTER_TS_GEN")
            && !configured.trim().is_empty()
        {
            let mut words = split_command(&configured).map_err(|e| {
                Error::usage(format!("ASTER_TS_GEN: {e}")).with_hint(
                    "quote paths containing spaces, e.g. ASTER_TS_GEN='bun \"/my dir/gen.js\"'",
                )
            })?;
            if words.is_empty() {
                return Err(Error::usage("ASTER_TS_GEN names no command"));
            }
            let program = words.remove(0);
            return Ok(Self {
                program: program.into(),
                args: words,
                name: "the TypeScript binding",
                install_hint,
            });
        }
        let bin = if cfg!(windows) {
            "aster-gen.cmd"
        } else {
            "aster-gen"
        };
        let cwd = std::env::current_dir()?;
        let found = cwd
            .ancestors()
            .map(|d| d.join("node_modules/.bin").join(bin))
            .find(|p| p.is_file());
        match found {
            Some(program) => Ok(Self {
                program,
                args: Vec::new(),
                name: "the TypeScript binding",
                install_hint,
            }),
            None => Err(Error::new(
                ErrorKind::Unavailable,
                "no `aster-gen` in node_modules/.bin here or in any parent directory",
            )
            .with_hint(install_hint)),
        }
    }

    /// Run with `args`; return the envelope after checking `key` is `HANDOFF_VERSION`.
    fn handoff(&self, args: &[String], key: &str, what: &str) -> Result<PyJson> {
        let out = std::process::Command::new(&self.program)
            .args(&self.args)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| {
                Error::new(
                    ErrorKind::Unavailable,
                    format!("cannot run {}: {e}", self.program.display()),
                )
                .with_hint(self.install_hint.clone())
            })?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            let detail = stderr
                .trim()
                .lines()
                .last()
                .unwrap_or("no error output")
                .to_string();
            let err = Error::new(
                ErrorKind::Failure,
                format!("{} could not {what}: {detail}", self.name),
            );
            let missing = [
                "No module named aster",
                "No module named 'aster",
                "__main__",
                "could not determine executable",
                "command not found",
            ]
            .iter()
            .any(|m| stderr.contains(m));
            return Err(if missing {
                err.with_kind(ErrorKind::Unavailable)
                    .with_hint(self.install_hint.clone())
            } else {
                err
            });
        }
        let envelope = PyJson::parse(&String::from_utf8_lossy(&out.stdout)).map_err(|e| {
            Error::new(
                ErrorKind::Failure,
                format!("unreadable output from {}: {e}", self.name),
            )
        })?;
        match envelope.get(key) {
            Some(PyJson::Int(HANDOFF_VERSION)) => Ok(envelope),
            other => Err(Error::new(
                ErrorKind::Failure,
                format!("unsupported {key} version {other:?} from {}; this CLI speaks {HANDOFF_VERSION}", self.name),
            )
            .with_hint("use matching versions of the aster CLI and language package")),
        }
    }
}

fn python_handoff(
    python: Option<&Path>,
    services: &[String],
    semver: Option<&str>,
) -> Result<Vec<PyJson>> {
    let mut args = vec!["gen".to_string()];
    for s in services {
        args.extend(["--service".into(), s.clone()]);
    }
    if let Some(v) = semver {
        args.extend(["--semver".into(), v.into()]);
    }
    let envelope = Binding::python(python, "aster.contract").handoff(
        &args,
        "aster_manifest_handoff",
        "generate the manifest",
    )?;
    match envelope.get("manifests") {
        Some(PyJson::Array(items)) if !items.is_empty() => Ok(items.clone()),
        _ => Err(Error::new(
            ErrorKind::Failure,
            "the manifest handoff returned no manifests",
        )),
    }
}

#[derive(Debug, Serialize)]
pub struct Generated {
    lang: &'static str,
    package: String,
    files: Vec<String>,
    usage: String,
}

impl Report for Generated {
    fn human(&self, style: Styler) -> String {
        format!(
            "{} {} {} files for package `{}`.{}",
            style.paint(aster_cli_kit::out::OK, "Generated"),
            self.files.len(),
            self.lang,
            self.package,
            self.usage.trim_end()
        )
    }
}

/// `--package`, else the manifest file's stem (`X.aster.json` → `X`), else,
/// for the default `manifest.json`, its first service; sanitised to an
/// identifier as the Python CLI did.
fn package_name(explicit: Option<String>, manifest: &Path, first_service: &str) -> String {
    let raw = explicit.unwrap_or_else(|| {
        let stem = manifest
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                n.trim_end_matches(".json")
                    .trim_end_matches(".aster")
                    .replace('.', "_")
            })
            .unwrap_or_default();
        if stem.is_empty() || stem == "manifest" {
            first_service.to_string()
        } else {
            stem
        }
    });
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    match cleaned.trim_matches('_') {
        "" => "aster_client".into(),
        s => s.into(),
    }
}

fn client(
    manifest: &Path,
    lang: ClientLang,
    out: &Path,
    package: Option<String>,
    address: Option<&str>,
    python: Option<&Path>,
) -> Result<Generated> {
    let first = show(manifest)?
        .services
        .into_iter()
        .next()
        .map(|s| s.service)
        .unwrap_or_default();
    let package = package_name(package, manifest, &first);
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let mut args = vec![
        "client".into(),
        "--manifest".into(),
        path(manifest),
        "--out".into(),
        path(out),
        "--namespace".into(),
        package.clone(),
    ];
    if let Some(a) = address {
        args.extend(["--address".into(), a.into()]);
    }
    let (binding, lang_name) = match lang {
        ClientLang::Python => (Binding::python(python, "aster.codegen"), "Python"),
        ClientLang::Typescript => (Binding::typescript()?, "TypeScript"),
    };
    let envelope = binding.handoff(&args, "aster_codegen_handoff", "generate the client")?;
    let files = match envelope.get("files") {
        Some(PyJson::Array(items)) => items.iter().map(|f| text(Some(f))).collect(),
        _ => Vec::new(),
    };
    Ok(Generated {
        lang: lang_name,
        package,
        files,
        usage: text(envelope.get("usage")),
    })
}

/// Split a command line as a POSIX shell would for plain words: whitespace
/// separates, `'…'` is literal, `"…"` honours `\"` and `\\`, and a
/// backslash outside quotes escapes the next character.
fn split_command(line: &str) -> std::result::Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if let Some(w) = word.take() {
                    words.push(w);
                }
            }
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => w.push(c),
                        None => return Err("unterminated single quote"),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => w.push(c),
                            Some(c) => {
                                w.push('\\');
                                w.push(c);
                            }
                            None => return Err("unterminated double quote"),
                        },
                        Some(c) => w.push(c),
                        None => return Err("unterminated double quote"),
                    }
                }
            }
            '\\' => match chars.next() {
                Some(c) => word.get_or_insert_with(String::new).push(c),
                None => return Err("trailing backslash"),
            },
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    if let Some(w) = word {
        words.push(w);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::split_command;

    #[test]
    fn splits_commands_like_a_shell() {
        assert_eq!(
            split_command("bunx aster-gen").unwrap(),
            ["bunx", "aster-gen"]
        );
        assert_eq!(
            split_command(r#"bun "/my dir/gen.js" '/a b' c\ d """#).unwrap(),
            ["bun", "/my dir/gen.js", "/a b", "c d", ""]
        );
        assert_eq!(split_command(r#""a\"b\\c""#).unwrap(), [r#"a"b\c"#]);
        assert!(split_command("'open").is_err());
    }
}
