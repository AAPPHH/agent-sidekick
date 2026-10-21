//! npm without a shell: how to run `npm install -g <package>` through the located node, and
//! where a globally installed npm package (the Gemini CLI) lives.
//!
//! On Windows `npm` is `npm.cmd`, a batch script that would need cmd.exe and its quoting
//! rules. The wizard never runs it: it runs `node <node dir>\node_modules\npm\bin\npm-cli.js`
//! with fixed arguments instead (that file ships with every Node.js installer for Windows).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::locate::Os;

/// npm package of OpenCode.
pub const OPENCODE_PACKAGE: &str = "opencode-ai";
/// npm package of the Gemini CLI.
pub const GEMINI_PACKAGE: &str = "@google/gemini-cli";
/// Name of the Gemini CLI's executable in its package.json `bin`.
const GEMINI_BIN: &str = "gemini";

/// A program and its arguments, run without a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl Invocation {
    /// For display only (`node C:\...\npm-cli.js install -g opencode-ai`).
    pub fn display(&self) -> String {
        let mut parts = vec![quote(self.program.as_os_str())];
        parts.extend(self.args.iter().map(|a| quote(a)));
        parts.join(" ")
    }

    /// The same program with other arguments after the fixed ones (`npm-cli.js`).
    fn with(&self, extra: &[&str]) -> Invocation {
        let mut args = self.args.clone();
        args.extend(extra.iter().map(OsString::from));
        Invocation {
            program: self.program.clone(),
            args,
        }
    }
}

fn quote(s: &OsStr) -> String {
    let s = s.to_string_lossy();
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s.into_owned()
    }
}

/// How to run npm itself: `node <npm-cli.js>` next to `node`, or (Unix only) an `npm` found
/// in PATH (`npm_in_path`; a shebang script, executable without a shell). `None`: no npm
/// that can run without a shell.
pub fn npm_base(
    node: &Path,
    os: Os,
    is_file: &dyn Fn(&Path) -> bool,
    npm_in_path: Option<&Path>,
) -> Option<Invocation> {
    let node_dir = node.parent()?;
    let cli = |root: PathBuf| root.join("npm").join("bin").join("npm-cli.js");
    let candidates = match os {
        // Windows installer, nvm-windows, winget: <node dir>\node_modules\npm.
        Os::Windows => vec![cli(node_dir.join("node_modules"))],
        // Official tarballs, nvm, distro packages: <prefix>/bin/node, <prefix>/lib/node_modules/npm.
        Os::Unix => {
            let mut c = Vec::new();
            if let Some(prefix) = node_dir.parent() {
                c.push(cli(prefix.join("lib").join("node_modules")));
            }
            c.push(cli(PathBuf::from("/usr/share/nodejs")));
            c.push(cli(PathBuf::from("/usr/lib/node_modules")));
            c
        }
    };
    if let Some(js) = candidates.into_iter().find(|c| is_file(c)) {
        return Some(Invocation {
            program: node.to_path_buf(),
            args: vec![js.into_os_string()],
        });
    }
    match (os, npm_in_path) {
        (Os::Unix, Some(npm)) if npm.is_absolute() => Some(Invocation {
            program: npm.to_path_buf(),
            args: Vec::new(),
        }),
        _ => None,
    }
}

/// `npm install -g <package>` (package: one of the constants above, never user input).
pub fn install(base: &Invocation, package: &str) -> Invocation {
    base.with(&["install", "-g", package])
}

/// `npm root -g` (prints the global node_modules directory).
pub fn root_global(base: &Invocation) -> Invocation {
    base.with(&["root", "-g"])
}

/// Global node_modules directories to look in, in order: `npm_root` (output of
/// `npm root -g`), then the usual locations: Windows the directory of a `gemini.cmd` /
/// `opencode.cmd` shim in PATH, `%APPDATA%\npm` and the node directory; Unix
/// `<prefix>/lib/node_modules` for every PATH entry `<prefix>/bin` and for node's prefix.
pub fn global_roots(
    npm_root: Option<&Path>,
    node: Option<&Path>,
    os: Os,
    env: &dyn Fn(&str) -> Option<OsString>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_absolute() && !roots.contains(&p) {
            roots.push(p);
        }
    };
    if let Some(r) = npm_root {
        push(r.to_path_buf());
    }
    let path_dirs: Vec<PathBuf> = env("PATH")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|d| !d.as_os_str().is_empty() && d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    match os {
        Os::Windows => {
            for d in &path_dirs {
                if is_file(&d.join("gemini.cmd")) || is_file(&d.join("opencode.cmd")) {
                    push(d.join("node_modules"));
                }
            }
            if let Some(appdata) = env("APPDATA").filter(|v| !v.is_empty()) {
                push(PathBuf::from(appdata).join("npm").join("node_modules"));
            }
            if let Some(dir) = node.and_then(Path::parent) {
                push(dir.join("node_modules"));
            }
        }
        Os::Unix => {
            if let Some(prefix) = node.and_then(Path::parent).and_then(Path::parent) {
                push(prefix.join("lib").join("node_modules"));
            }
            for d in &path_dirs {
                if d.file_name() == Some(OsStr::new("bin")) {
                    if let Some(prefix) = d.parent() {
                        push(prefix.join("lib").join("node_modules"));
                    }
                }
            }
        }
    }
    roots
}

/// The Gemini CLI's JavaScript entry point in the first root that has the package: the
/// `bin.gemini` entry of its package.json (`bundle/gemini.js` in current versions), or
/// `bundle/gemini.js` if package.json cannot be read. Run it with node, never via a shim.
pub fn gemini_entry(
    roots: &[PathBuf],
    is_file: &dyn Fn(&Path) -> bool,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Option<PathBuf> {
    roots.iter().find_map(|root| {
        let pkg = root.join("@google").join("gemini-cli");
        let rel = read(&pkg.join("package.json"))
            .and_then(|text| bin_entry(&text, GEMINI_BIN))
            .unwrap_or_else(|| "bundle/gemini.js".to_owned());
        // `bin` paths are relative to the package and use `/`; never leave the package.
        let rel = rel.trim_start_matches("./");
        if rel.split(['/', '\\']).any(|c| c == ".." || c.is_empty()) {
            return None;
        }
        let path = rel.split('/').fold(pkg.clone(), |p, c| p.join(c));
        is_file(&path).then_some(path)
    })
}

/// The `bin` entry `name` of a package.json (`"bin": "x.js"` or `"bin": {"name": "x.js"}`).
fn bin_entry(package_json: &str, name: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(package_json).ok()?;
    match v.get("bin")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(map) => map.get(name)?.as_str().map(str::to_owned),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn files(list: &[&Path]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = list.iter().map(|p| p.to_path_buf()).collect();
        move |p: &Path| set.contains(p)
    }

    fn abs(p: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\{}", p.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/{p}"))
        }
    }

    #[test]
    fn windows_uses_node_and_npm_cli_js() {
        let node = abs("nodejs/node.exe");
        let js = abs("nodejs/node_modules/npm/bin/npm-cli.js");
        let base = npm_base(&node, Os::Windows, &files(&[&js]), None).unwrap();
        let inv = install(&base, OPENCODE_PACKAGE);
        assert_eq!(inv.program, node);
        assert_eq!(
            inv.args,
            [
                js.as_os_str(),
                OsStr::new("install"),
                OsStr::new("-g"),
                OsStr::new("opencode-ai")
            ]
        );
        // No npm-cli.js: no shell fallback on Windows.
        assert_eq!(
            npm_base(
                &node,
                Os::Windows,
                &files(&[]),
                Some(&abs("nodejs/npm.cmd"))
            ),
            None
        );
    }

    #[test]
    fn unix_prefers_npm_cli_js_then_npm_in_path() {
        let node = abs("home/u/.nvm/versions/node/v22/bin/node");
        let js = abs("home/u/.nvm/versions/node/v22/lib/node_modules/npm/bin/npm-cli.js");
        let base = npm_base(&node, Os::Unix, &files(&[&js]), None).unwrap();
        assert_eq!(base.program, node);
        assert_eq!(base.args, [js.as_os_str()]);

        let npm = abs("usr/bin/npm");
        let base = npm_base(&node, Os::Unix, &files(&[]), Some(&npm)).unwrap();
        let inv = install(&base, GEMINI_PACKAGE);
        assert_eq!(inv.program, npm);
        assert_eq!(inv.args, ["install", "-g", "@google/gemini-cli"]);
        assert_eq!(root_global(&base).args, ["root", "-g"]);
        assert_eq!(npm_base(&node, Os::Unix, &files(&[]), None), None);
    }

    #[test]
    fn display_quotes_spaces() {
        let inv = Invocation {
            program: PathBuf::from("C:/Program Files/nodejs/node.exe"),
            args: vec!["x.js".into(), "install".into()],
        };
        assert_eq!(
            inv.display(),
            "\"C:/Program Files/nodejs/node.exe\" x.js install"
        );
    }

    #[test]
    fn gemini_entry_follows_package_json_bin() {
        let root = abs("npm/node_modules");
        let pkg = root.join("@google").join("gemini-cli");
        let entry = pkg.join("dist").join("cli.js");
        let read = |p: &Path| {
            (p == pkg.join("package.json")).then(|| {
                r#"{"name":"@google/gemini-cli","bin":{"gemini":"dist/cli.js"}}"#.to_owned()
            })
        };
        let found = gemini_entry(&[abs("elsewhere"), root.clone()], &files(&[&entry]), &read);
        assert_eq!(found, Some(entry));

        // String form and the fallback without a package.json.
        assert_eq!(
            bin_entry(r#"{"bin":"a.js"}"#, "gemini").as_deref(),
            Some("a.js")
        );
        let bundle = pkg.join("bundle").join("gemini.js");
        assert_eq!(
            gemini_entry(std::slice::from_ref(&root), &files(&[&bundle]), &|_| None),
            Some(bundle)
        );
        // A bin path that leaves the package is ignored.
        let read_evil = |_: &Path| Some(r#"{"bin":{"gemini":"../../evil.js"}}"#.to_owned());
        assert_eq!(
            gemini_entry(&[root], &files(&[&abs("npm/evil.js")]), &read_evil),
            None
        );
    }

    #[test]
    fn global_roots_per_os() {
        let appdata = abs("Users/u/AppData/Roaming");
        let shim_dir = abs("tools/npm");
        let path = std::env::join_paths([shim_dir.clone(), PathBuf::from("relative")]).unwrap();
        let env = |k: &str| match k {
            "PATH" => Some(path.clone()),
            "APPDATA" => Some(appdata.clone().into_os_string()),
            _ => None,
        };
        let shim = shim_dir.join("gemini.cmd");
        let node = abs("nodejs/node.exe");
        let npm_root = abs("custom/node_modules");
        let roots = global_roots(
            Some(&npm_root),
            Some(&node),
            Os::Windows,
            &env,
            &files(&[&shim]),
        );
        assert_eq!(
            roots,
            [
                npm_root,
                shim_dir.join("node_modules"),
                appdata.join("npm").join("node_modules"),
                abs("nodejs/node_modules"),
            ]
        );

        let bin = abs("opt/node/bin");
        let path = std::env::join_paths([bin.clone()]).unwrap();
        let env = |k: &str| (k == "PATH").then(|| path.clone());
        let node = abs("home/u/.nvm/v22/bin/node");
        let roots = global_roots(None, Some(&node), Os::Unix, &env, &files(&[]));
        assert_eq!(
            roots,
            [
                abs("home/u/.nvm/v22/lib/node_modules"),
                abs("opt/node/lib/node_modules"),
            ]
        );
    }
}
