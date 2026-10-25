//! `connect.sh connect|disconnect --client codex|gemini` against a throwaway HOME and the fake server.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::env::{Out, TestEnv};

const TOKEN: &str = "mem_abcd1234_secret";

/// A copy of the plugin scripts (so `bin/mem-hook` lands in the temp dir) and a way to run connect.sh.
struct Connect {
    env: TestEnv,
    fake: crate::fake_server::Fake,
    script: PathBuf,
}

impl Connect {
    fn new() -> Connect {
        let (fake, env) = TestEnv::with_fake();
        let plugin = env.dir.path().join("clients/claude-code-plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../clients/claude-code-plugin");
        for f in ["connect.sh", "install.sh"] {
            std::fs::copy(src.join(f), plugin.join(f)).unwrap();
        }
        fake.state().context = Some("## Memory\n".into()); // what connect.sh probes the token with
        Connect { script: plugin.join("connect.sh"), env, fake }
    }

    fn home(&self) -> &Path {
        self.env.dir.path()
    }

    fn codex_config(&self) -> PathBuf {
        self.home().join(".codex/config.toml")
    }

    fn gemini_settings(&self) -> PathBuf {
        self.home().join(".gemini/settings.json")
    }

    fn run(&self, args: &[&str]) -> Out {
        let out = Command::new("bash")
            .arg(&self.script)
            .args(args)
            .env("HOME", self.home())
            .env("MEMORY_HOME", &self.env.home)
            .env("MEM_HOOK_BIN", env!("CARGO_BIN_EXE_mem-hook"))
            .env("no_proxy", "127.0.0.1")
            .env_remove("CODEX_HOME")
            .env_remove("GEMINI_CLI_HOME")
            .output()
            .unwrap();
        Out {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn connect(&self, client: &str) -> Out {
        self.run(&["connect", "--client", client, "--url", &self.fake.url, "--token", TOKEN])
    }

    fn disconnect(&self, client: &str) -> Out {
        self.run(&["disconnect", "--client", client])
    }
}

fn ok(out: Out) -> Out {
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    out
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

const CODEX_ORIGINAL: &str = "model = \"o4\"\n\n# my own hook\n[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"notify-send done\"\n\n[tui]\ntheme = \"dark\"\n";

#[test]
fn codex_connect_is_idempotent_and_disconnect_restores_the_file_exactly() {
    let c = Connect::new();
    write(&c.codex_config(), CODEX_ORIGINAL);

    let out = ok(c.connect("codex"));
    assert!(out.stdout.contains("Codex CLI"), "{}", out.stdout);
    let first = std::fs::read_to_string(c.codex_config()).unwrap();
    assert!(first.starts_with(CODEX_ORIGINAL), "user content untouched:\n{first}");
    let url = &c.fake.url;
    assert!(first.contains(&format!("[mcp_servers.memory]\nurl = \"{url}/mcp\"\n")), "{first}");
    assert!(first.contains(&format!("Authorization = \"Bearer {TOKEN}\"")));
    for event in ["SessionStart", "UserPromptSubmit", "PostToolUse", "Stop", "SessionEnd"] {
        assert!(first.contains(&format!("[[hooks.{event}.hooks]]")), "{event}");
    }
    assert_eq!(first.matches("event --client codex'").count(), 5);
    assert_eq!(first.matches("# >>> memoryvault").count(), 1);
    assert!(ok(c.run(&["status"])).stdout.contains("codex    registered"));
    // The memory config the hooks read is written too.
    let cfg = std::fs::read_to_string(c.env.home.join("config")).unwrap();
    assert!(cfg.contains(&format!("MEMORY_URL={url}")) && cfg.contains(&format!("MEMORY_TOKEN={TOKEN}")));
    assert_eq!(
        std::fs::read_to_string(format!("{}.memoryvault.bak", c.codex_config().display())).unwrap(),
        CODEX_ORIGINAL
    );

    ok(c.connect("codex"));
    assert_eq!(std::fs::read_to_string(c.codex_config()).unwrap(), first, "a second connect changes nothing");

    ok(c.disconnect("codex"));
    assert_eq!(std::fs::read_to_string(c.codex_config()).unwrap(), CODEX_ORIGINAL);
    assert!(!c.codex_config().with_extension("toml.memoryvault.bak").exists());
    assert!(c.env.home.join("config").exists(), "the shared memory config stays");
}

#[test]
fn codex_config_created_by_connect_is_removed_by_disconnect() {
    let c = Connect::new();
    ok(c.connect("codex"));
    assert!(c.codex_config().exists());
    ok(c.disconnect("codex"));
    assert!(!c.codex_config().exists());
    ok(c.disconnect("codex"));
}

#[test]
fn codex_keeps_a_memory_server_the_user_defined() {
    let c = Connect::new();
    let original = "[mcp_servers.memory]\ncommand = \"my-memory\"\n";
    write(&c.codex_config(), original);
    let out = ok(c.connect("codex"));
    assert!(out.stderr.contains("already defines an MCP server 'memory'"), "{}", out.stderr);
    let after = std::fs::read_to_string(c.codex_config()).unwrap();
    assert!(after.starts_with(original) && !after.contains("url = "), "{after}");
    assert!(after.contains("[[hooks.SessionStart]]"));
    ok(c.disconnect("codex"));
    assert_eq!(std::fs::read_to_string(c.codex_config()).unwrap(), original);
}

#[test]
fn codex_hooks_in_a_form_that_cannot_be_extended_are_skipped() {
    let c = Connect::new();
    let original = "hooks = { Stop = [] }\n";
    write(&c.codex_config(), original);
    let out = ok(c.connect("codex"));
    assert!(out.stderr.contains("hooks were not registered"), "{}", out.stderr);
    let after = std::fs::read_to_string(c.codex_config()).unwrap();
    assert!(after.contains("[mcp_servers.memory]") && !after.contains("[[hooks."), "{after}");
}

#[test]
fn codex_config_that_is_not_toml_is_left_alone() {
    let c = Connect::new();
    write(&c.codex_config(), "model = \n[[[\n");
    let out = c.connect("codex");
    assert_eq!(out.code, Some(1), "{}{}", out.stdout, out.stderr);
    assert!(out.stderr.contains("not valid TOML"), "{}", out.stderr);
    assert_eq!(std::fs::read_to_string(c.codex_config()).unwrap(), "model = \n[[[\n");
}

const GEMINI_ORIGINAL: &str = r#"{
  "theme": "dark",
  "hooks": {
    "AfterTool": [ { "matcher": "write_.*", "hooks": [ { "type": "command", "command": "echo hi" } ] } ]
  },
  "mcpServers": { "other": { "command": "other-server" } }
}
"#;

#[test]
fn gemini_connect_is_idempotent_and_disconnect_restores_the_file_exactly() {
    let c = Connect::new();
    write(&c.gemini_settings(), GEMINI_ORIGINAL);

    let out = ok(c.connect("gemini"));
    assert!(out.stdout.contains("Gemini CLI"), "{}", out.stdout);
    let first = read_json(&c.gemini_settings());
    let original: Value = serde_json::from_str(GEMINI_ORIGINAL).unwrap();
    assert_eq!(first["theme"], "dark");
    assert_eq!(first["mcpServers"]["other"], original["mcpServers"]["other"]);
    assert_eq!(
        first["mcpServers"]["memory"],
        json!({"httpUrl": format!("{}/mcp", c.fake.url), "headers": {"Authorization": format!("Bearer {TOKEN}")}})
    );
    // The user's AfterTool hook is still first and untouched; ours are tagged by name.
    assert_eq!(first["hooks"]["AfterTool"][0], original["hooks"]["AfterTool"][0]);
    for event in ["SessionStart", "BeforeAgent", "AfterTool", "AfterAgent", "SessionEnd"] {
        let ours: Vec<&Value> = first["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .filter(|h| h["name"] == "memoryvault")
            .collect();
        assert_eq!(ours.len(), 1, "{event}");
        assert!(ours[0]["command"].as_str().unwrap().ends_with("mem-hook\" event --client gemini"));
        assert!(ours[0]["timeout"].as_u64().unwrap() >= 2000, "milliseconds");
    }
    assert!(ok(c.run(&["status"])).stdout.contains("gemini   registered"));

    ok(c.connect("gemini"));
    assert_eq!(read_json(&c.gemini_settings()), first, "a second connect changes nothing");

    ok(c.disconnect("gemini"));
    assert_eq!(std::fs::read_to_string(c.gemini_settings()).unwrap(), GEMINI_ORIGINAL);
    assert!(!c.home().join(".gemini/settings.json.memoryvault.bak").exists());
}

#[test]
fn gemini_disconnect_after_the_user_edited_the_file_keeps_their_edits() {
    let c = Connect::new();
    write(&c.gemini_settings(), GEMINI_ORIGINAL);
    ok(c.connect("gemini"));
    let mut edited = read_json(&c.gemini_settings());
    edited["theme"] = json!("light");
    write(&c.gemini_settings(), &edited.to_string());
    ok(c.disconnect("gemini"));
    let mut want: Value = serde_json::from_str(GEMINI_ORIGINAL).unwrap();
    want["theme"] = json!("light");
    assert_eq!(read_json(&c.gemini_settings()), want);
}

#[test]
fn gemini_settings_created_by_connect_are_removed_by_disconnect() {
    let c = Connect::new();
    ok(c.connect("gemini"));
    assert!(c.gemini_settings().exists());
    ok(c.disconnect("gemini"));
    assert!(!c.gemini_settings().exists());
}

#[test]
fn gemini_keeps_a_memory_server_the_user_defined() {
    let c = Connect::new();
    let original = r#"{"mcpServers":{"memory":{"command":"mine"}}}"#;
    write(&c.gemini_settings(), original);
    let out = ok(c.connect("gemini"));
    assert!(out.stderr.contains("not ours"), "{}", out.stderr);
    assert_eq!(read_json(&c.gemini_settings())["mcpServers"]["memory"], json!({"command": "mine"}));
    ok(c.disconnect("gemini"));
    assert_eq!(read_json(&c.gemini_settings()), serde_json::from_str::<Value>(original).unwrap());
}

#[test]
fn gemini_settings_that_are_not_plain_json_are_left_alone() {
    let c = Connect::new();
    let original = "{\n  // my settings\n  \"theme\": \"dark\"\n}\n";
    write(&c.gemini_settings(), original);
    let out = c.connect("gemini");
    assert_eq!(out.code, Some(1), "{}{}", out.stdout, out.stderr);
    assert!(out.stderr.contains("nothing was changed"), "{}", out.stderr);
    assert_eq!(std::fs::read_to_string(c.gemini_settings()).unwrap(), original);
}

#[test]
fn the_registered_gemini_hook_command_delivers_an_event() {
    let c = Connect::new();
    ok(c.connect("gemini"));
    let settings = read_json(&c.gemini_settings());
    let command = settings["hooks"]["BeforeAgent"][0]["hooks"][0]["command"].as_str().unwrap();
    let out = run_hook(
        &c,
        command,
        &json!({"session_id": "g-7", "cwd": c.env.cwd, "hook_event_name": "BeforeAgent", "prompt": "hello"}),
    );
    assert_eq!((out.code, out.stdout.as_str()), (Some(0), ""), "{}", out.stderr);
    let create = c.fake.matching("POST", "/v1/sessions").into_iter().find(|r| r.path == "/v1/sessions").unwrap();
    assert_eq!(create.body["harness"], "gemini");
    assert_eq!(c.fake.events()[0]["payload"], json!({"prompt": "hello"}));
}

#[test]
fn the_registered_codex_hook_command_delivers_an_event() {
    let c = Connect::new();
    ok(c.connect("codex"));
    let text = std::fs::read_to_string(c.codex_config()).unwrap();
    let command = text.lines().find_map(|l| l.strip_prefix("command = '")).and_then(|l| l.strip_suffix('\'')).unwrap();
    let out = run_hook(
        &c,
        command,
        &json!({"session_id": "c-7", "cwd": c.env.cwd, "hook_event_name": "UserPromptSubmit", "prompt": "hello"}),
    );
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    let create = c.fake.matching("POST", "/v1/sessions").into_iter().find(|r| r.path == "/v1/sessions").unwrap();
    assert_eq!(create.body["harness"], "codex");
}

fn run_hook(c: &Connect, command: &str, input: &Value) -> Out {
    use std::io::Write;
    let mut child = Command::new("sh")
        .args(["-c", command])
        .env("HOME", c.home())
        .env("MEMORY_HOME", &c.env.home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    Out {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn an_unknown_client_is_refused() {
    let c = Connect::new();
    let out = c.connect("vim");
    assert_eq!(out.code, Some(1));
    assert!(out.stderr.contains("--client must be claude, codex or gemini"), "{}", out.stderr);
}
