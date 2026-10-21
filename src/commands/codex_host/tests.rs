use super::*;

fn reg(dir: &Path) -> Registration {
    Registration {
        command: dir.join("bin").join("agent-sidekick.exe"),
        env: vec![(
            ENV_CLAUDE.to_owned(),
            dir.join("claude")
                .join("claude.exe")
                .to_string_lossy()
                .into_owned(),
        )],
        env_vars: vec!["HTTPS_PROXY".to_owned(), "MY_KEY".to_owned()],
    }
}

/// A user config with comments, other servers (one with an inline comment), profiles and no
/// final line break: everything outside our table must survive byte for byte.
const USER_CONFIG: &str = "# my codex config\nmodel = \"gpt-6.1-sol\" # favourite\n\n\
[projects.'C:\\work']\ntrust_level = \"trusted\"\n\n\
[mcp_servers.other]\ncommand = \"foo\" # inline comment\nargs = [\"a\",   \"b\"]\n\
tool_timeout_sec = 1900.0\n\n[mcp_servers.other.env]\nX = \"1\"\n\n\
[profiles.fast]\nmodel = \"gpt-6-luna\"";

#[test]
fn config_path_prefers_codex_home() {
    let home = Path::new("/home/u");
    let env = |k: &str| (k == "CODEX_HOME").then(|| OsString::from("/x/codex"));
    assert_eq!(
        config_path(&env, Some(home)),
        Some(PathBuf::from("/x/codex").join("config.toml"))
    );
    let empty = |k: &str| (k == "CODEX_HOME").then(|| OsString::from(""));
    assert_eq!(
        config_path(&empty, Some(home)),
        Some(home.join(".codex").join("config.toml"))
    );
    assert_eq!(config_path(&|_| None, None), None);
}

#[test]
fn register_into_an_empty_config() {
    let tmp = tempfile::tempdir().unwrap();
    let r = reg(tmp.path());
    std::fs::create_dir_all(tmp.path().join("claude")).unwrap();
    std::fs::write(&r.env[0].1, "").unwrap();
    let text = register_in("", &r).unwrap();
    assert!(text.starts_with("[mcp_servers.agent-sidekick]\n"), "{text}");
    let e = read_entry(&text).unwrap().unwrap();
    assert_eq!(
        e.command.as_deref(),
        Some(r.command.to_string_lossy().as_ref())
    );
    assert_eq!(e.args, ["serve"]);
    assert!(e.enabled);
    assert_eq!(e.tool_timeout_sec, Some(TOOL_TIMEOUT_SEC as f64));
    assert_eq!(TOOL_TIMEOUT_SEC, 15_000);
    assert_eq!(e.env, r.env);
    assert_eq!(e.env_vars, ["HTTPS_PROXY", "MY_KEY"]);
    assert!(entry_problems(&e, None, &["MY_KEY".to_owned()]).is_empty());
    // The read-only tools run without an approval prompt; sidekick_run keeps Codex's default.
    let v: toml::Value = toml::from_str(&text).unwrap();
    let tools = &v["mcp_servers"]["agent-sidekick"]["tools"];
    for t in ["sidekick_backends", "sidekick_status"] {
        assert_eq!(tools[t]["approval_mode"].as_str(), Some("approve"), "{t}");
    }
    assert!(tools.get("sidekick_run").is_none());
    // Registering again changes nothing.
    assert_eq!(register_in(&text, &r).unwrap(), text);
}

#[test]
fn register_appends_and_keeps_everything_else_byte_for_byte() {
    let r = reg(Path::new("/opt/as"));
    let text = register_in(USER_CONFIG, &r).unwrap();
    assert!(text.starts_with(USER_CONFIG), "{text}");
    assert!(text.contains("command = \"foo\" # inline comment"));
    assert!(text.contains("tool_timeout_sec = 1900.0"));
    let e = read_entry(&text).unwrap().unwrap();
    assert_eq!(e.args, ["serve"]);
    // Idempotent, and removing it restores the original (plus the separating line break).
    assert_eq!(register_in(&text, &r).unwrap(), text);
    let back = unregister_in(&text).unwrap().unwrap();
    assert_eq!(back.trim_end(), USER_CONFIG);
}

#[test]
fn reregistering_updates_in_place_and_keeps_user_settings() {
    let old = "a = 1\n\n[mcp_servers.agent-sidekick]\ncommand = 'C:\\old\\agent-sidekick.exe'\n\
               args = [\"serve\", \"--old\"]\nstartup_timeout_ms = 5000\ntool_timeout_sec = 60\n\
               enabled = false\nenv_vars = [\"KEEP_ME\"]\n\n\
               [mcp_servers.agent-sidekick.env]\nOTHER = \"x\"\nAGENT_SIDEKICK_CLAUDE = \"C:\\\\old\\\\claude.exe\"\n\n\
               [mcp_servers.agent-sidekick.tools.sidekick_status]\napproval_mode = \"approve\"\n\n\
               [mcp_servers.other]\ncommand = \"foo\" # keep\n";
    let r = reg(Path::new("/new"));
    let text = register_in(old, &r).unwrap();
    let e = read_entry(&text).unwrap().unwrap();
    assert_eq!(
        e.command.as_deref(),
        Some(r.command.to_string_lossy().as_ref())
    );
    assert_eq!(e.args, ["serve"]);
    assert_eq!(e.tool_timeout_sec, Some(15_000.0));
    assert!(!e.enabled, "the user's enabled = false is kept");
    assert_eq!(e.env_vars, ["HTTPS_PROXY", "KEEP_ME", "MY_KEY"]);
    assert!(e.env.contains(&("OTHER".to_owned(), "x".to_owned())));
    assert_eq!(e.claude(), Some(r.env[0].1.as_str()));
    assert!(!text.contains("startup_timeout_ms"), "{text}");
    assert!(text.contains("startup_timeout_sec = 30"), "{text}");
    let v: toml::Value = toml::from_str(&text).unwrap();
    let tools = &v["mcp_servers"]["agent-sidekick"]["tools"];
    assert_eq!(
        tools["sidekick_backends"]["approval_mode"].as_str(),
        Some("approve")
    );
    assert_eq!(
        tools["sidekick_status"]["approval_mode"].as_str(),
        Some("approve")
    );
    // A user's choice for a tool is kept.
    let custom = old.replace(
        "[mcp_servers.agent-sidekick.tools.sidekick_status]\napproval_mode = \"approve\"",
        "[mcp_servers.agent-sidekick.tools.sidekick_status]\napproval_mode = \"prompt\"",
    );
    let v: toml::Value = toml::from_str(&register_in(&custom, &r).unwrap()).unwrap();
    assert_eq!(
        v["mcp_servers"]["agent-sidekick"]["tools"]["sidekick_status"]["approval_mode"].as_str(),
        Some("prompt")
    );
    assert!(
        text.starts_with("a = 1\n\n[mcp_servers.agent-sidekick]\n"),
        "{text}"
    );
    assert!(
        text.ends_with("[mcp_servers.other]\ncommand = \"foo\" # keep\n"),
        "{text}"
    );
    let problems = entry_problems(&e, None, &[]);
    assert_eq!(problems[0], "it is disabled (`enabled = false`)");
}

#[test]
fn layouts_that_are_not_edited() {
    let r = reg(Path::new("/x"));
    let e = register_in("mcp_servers = { other = { command = \"foo\" } }\n", &r).unwrap_err();
    assert!(e.contains("not a [table]"), "{e}");
    let e = register_in("[mcp_servers]\nagent-sidekick = \"x\"\n", &r).unwrap_err();
    assert!(e.contains("is not a [table]"), "{e}");
    let e = register_in("this is = = not toml", &r).unwrap_err();
    assert!(e.contains("not valid TOML"), "{e}");
    // A plain [mcp_servers] table gets our subtable appended.
    let text = register_in("[mcp_servers]\n", &r).unwrap();
    assert!(read_entry(&text).unwrap().is_some());
}

#[test]
fn unregister_removes_only_our_tables() {
    let r = reg(Path::new("/x"));
    let with = register_in(USER_CONFIG, &r).unwrap();
    let without = unregister_in(&with).unwrap().unwrap();
    assert_eq!(read_entry(&without).unwrap(), None);
    assert!(without.contains("command = \"foo\" # inline comment"));
    assert!(without.contains("[profiles.fast]"));
    assert_eq!(unregister_in(USER_CONFIG).unwrap(), None);
    assert_eq!(unregister_in("").unwrap(), None);
}

#[test]
fn files_are_backed_up_before_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("codex-home").join("config.toml");
    let r = reg(tmp.path());

    // No file yet: created (with its directory), no backup.
    assert_eq!(
        register(&path, &r, "20261021-120000").unwrap(),
        Written::Changed { backup: None }
    );
    let first = std::fs::read_to_string(&path).unwrap();
    // Same again: nothing written.
    assert_eq!(
        register(&path, &r, "20261021-120001").unwrap(),
        Written::Unchanged
    );
    // A change: the old file is backed up.
    std::fs::write(&path, format!("{USER_CONFIG}\n")).unwrap();
    let Written::Changed {
        backup: Some(backup),
    } = register(&path, &r, "20261021-120002").unwrap()
    else {
        panic!("expected a backup")
    };
    assert!(backup
        .to_string_lossy()
        .ends_with("config.toml.bak-agent-sidekick-20261021-120002"));
    assert_eq!(
        std::fs::read_to_string(&backup).unwrap(),
        format!("{USER_CONFIG}\n")
    );
    assert!(read_entry_at(&path).unwrap().is_some());
    assert_ne!(std::fs::read_to_string(&path).unwrap(), first);

    // Unregister: backed up again, entry gone; a second time is a no-op.
    let w = unregister(&path, "20261021-120003").unwrap().unwrap();
    assert!(matches!(w, Written::Changed { backup: Some(_) }));
    assert_eq!(read_entry_at(&path).unwrap(), None);
    assert_eq!(unregister(&path, "20261021-120004").unwrap(), None);
    assert_eq!(
        unregister(&tmp.path().join("none.toml"), "x").unwrap(),
        None
    );
    assert_eq!(read_entry_at(&tmp.path().join("none.toml")).unwrap(), None);
}

#[test]
fn needed_vars_come_from_the_config() {
    let cfg: Config = toml::from_str(
        "pass_env = [\"GLOBAL_PASS\"]\n\
         [backends.a]\nharness = \"codex\"\nauth = \"api-key\"\napi_key_env = \"OAI_KEY\"\n\
         description = \"d\"\ndefault = true\npass_env = [\"B_PASS\"]\n",
    )
    .unwrap();
    assert_eq!(config_vars(&cfg), ["B_PASS", "GLOBAL_PASS", "OAI_KEY"]);
    let all = needed_vars(Some(&cfg));
    for v in [
        "B_PASS",
        "GLOBAL_PASS",
        "OAI_KEY",
        "HTTPS_PROXY",
        "SSL_CERT_FILE",
        "CLAUDE_CODE_GIT_BASH_PATH",
    ] {
        assert!(all.iter().any(|x| x == v), "{v}: {all:?}");
    }
    if cfg!(unix) {
        assert!(all.iter().any(|x| x == "DBUS_SESSION_BUS_ADDRESS"));
    }
    let r = Registration::new(
        Path::new("/bin/as"),
        Some(Path::new("/bin/claude")),
        Some(&cfg),
        &|k| (k == "AGENT_SIDEKICK_CONFIG_DIR").then(|| OsString::from("/cfg")),
    );
    assert_eq!(
        r.env,
        [
            (ENV_CLAUDE.to_owned(), "/bin/claude".to_owned()),
            ("AGENT_SIDEKICK_CONFIG_DIR".to_owned(), "/cfg".to_owned())
        ]
    );
    assert_eq!(r.env_vars, all);
}

#[test]
fn problems_of_a_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = tmp.path().join("agent-sidekick.exe");
    std::fs::write(&exe, "").unwrap();
    let base = Entry {
        command: Some(exe.to_string_lossy().into_owned()),
        args: vec!["serve".into()],
        enabled: true,
        tool_timeout_sec: Some(15_000.0),
        env: vec![],
        env_vars: vec!["K".into()],
    };
    assert!(entry_problems(&base, Some(&exe), &["K".into()]).is_empty());

    let no_timeout = Entry {
        tool_timeout_sec: None,
        ..base.clone()
    };
    let p = entry_problems(&no_timeout, Some(&exe), &[]);
    assert!(p[0].contains("after 60 s"), "{p:?}");
    let low = Entry {
        tool_timeout_sec: Some(1800.0),
        ..base.clone()
    };
    assert!(entry_problems(&low, Some(&exe), &[])[0].contains("below 15000"));
    let other = tmp.path().join("other.exe");
    let p = entry_problems(&base, Some(&other), &["K".into(), "MISSING".into()]);
    assert!(p.iter().any(|x| x.contains("this binary is")), "{p:?}");
    assert!(
        p.iter().any(|x| x.contains("does not pass MISSING")),
        "{p:?}"
    );
    let gone = Entry {
        env: vec![(
            ENV_CLAUDE.into(),
            tmp.path().join("nope").to_string_lossy().into(),
        )],
        ..base
    };
    assert!(entry_problems(&gone, Some(&exe), &[])[0].contains("does not exist"));
}
