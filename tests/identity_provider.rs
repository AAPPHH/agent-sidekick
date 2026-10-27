use std::collections::BTreeMap;
use std::path::PathBuf;

use agent_sidekick::backends::{Backend, BackendKind, Cost, OpencodeProvider};
use agent_sidekick::dispatch::Identity;

fn opencode(provider: OpencodeProvider, base_url: Option<&str>) -> Backend {
    Backend {
        name: "b".into(),
        description: "t".into(),
        cost: Cost::Free,
        is_default: false,
        max_concurrent: None,
        model: None,
        models: None,
        env: BTreeMap::new(),
        pass_env: Vec::new(),
        implicit: false,
        kind: BackendKind::OpenCode {
            provider,
            base_url: base_url.map(Into::into),
            api_key_env: None,
            harness_path: None,
            read_only: false,
            request_timeout_s: None,
            model_options: Default::default(),
            limits: Default::default(),
            base_dir: PathBuf::from("/x"),
        },
        foreign_secret_env: Vec::new(),
    }
}

fn provider_of(b: &Backend) -> String {
    Identity::default().key(b, Some("m")).provider
}

#[test]
fn openai_compatible_opencode_uses_base_url_host() {
    let kic = opencode(OpencodeProvider::OpenaiCompatible, Some("https://chat.kiconnect.nrw/api/v1"));
    let groq = opencode(OpencodeProvider::OpenaiCompatible, Some("https://api.groq.com/openai/v1"));
    let dgx = opencode(OpencodeProvider::OpenaiCompatible, Some("http://dgx.mv.intern.hs-duesseldorf.de:18080/v1"));
    assert_eq!(provider_of(&kic), "chat.kiconnect.nrw");
    assert_eq!(provider_of(&groq), "api.groq.com");
    assert_eq!(provider_of(&dgx), "dgx.mv.intern.hs-duesseldorf.de");
    assert_eq!(provider_of(&opencode(OpencodeProvider::Ollama, Some("http://localhost:11434"))), "ollama");
    assert_eq!(provider_of(&opencode(OpencodeProvider::Openrouter, None)), "openrouter");
}

#[test]
fn codex_with_base_url_uses_host() {
    let mut b = opencode(OpencodeProvider::Ollama, None);
    b.kind = BackendKind::Codex {
        auth: agent_sidekick::backends::CodexAuth::ApiKey,
        base_url: Some("https://chat.kiconnect.nrw/api/v1".into()),
        api_key_env: None,
        harness_path: None,
        read_only: false,
        windows_sandbox: Default::default(),
        base_dir: PathBuf::from("/x"),
    };
    assert_eq!(provider_of(&b), "chat.kiconnect.nrw");
}
