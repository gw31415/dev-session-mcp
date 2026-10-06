use std::{env, fs, path::PathBuf};

// Keep the vendored upstream intact, including the working Node release.
// Only expose its tool dispatcher and route its helper into this executable.
fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for file in ["mcp.rs", "sandbox.rs"] {
        let source = PathBuf::from("../vendor/local-mcp/src").join(file);
        println!("cargo:rerun-if-changed={}", source.display());
        let mut text = fs::read_to_string(source).unwrap();
        if file == "mcp.rs" {
            for old in ["fn tools()", "async fn call_tool("] {
                assert_eq!(text.matches(old).count(), 1);
                text = text.replace(old, &format!("pub(crate) {old}"));
            }
        } else {
            let old = "let executable = std::env::current_exe()?\n            .parent()\n            .context(\"local-mcp executable has no parent directory\")?\n            .join(\"codex-linux-sandbox\");";
            assert_eq!(text.matches(old).count(), 1);
            text = text.replace(old, "let executable = std::env::current_exe()?;");
            let old = "let mut process = Command::new(executable);\n        process.args(args);";
            assert_eq!(text.matches(old).count(), 1);
            text = text.replace(old, "let mut process = Command::new(executable);\n        process.arg0(codex_sandboxing::landlock::CODEX_LINUX_SANDBOX_ARG0);\n        process.args(args);");
        }
        fs::write(out.join(file), text).unwrap();
    }
}
