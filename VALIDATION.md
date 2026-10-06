# Validation — Rust stdio + Secure MCP Tunnel

2026-10-06 UTC、独立した `/workspace/dev-session-mcp`、Debian 13 x86_64、Rust/Cargo 1.98.1、実tmux 3.5a、bubblewrap 0.12.0で検証しています。推奨導入はRust stdio + 公式Secure MCP Tunnelのみです。現backendはtmux。軽量Rust backendへの置換・resize APIは未実装です。

```sh
cargo test --locked --manifest-path rust/Cargo.toml --test stdio_smoke -- --nocapture
```

名称変更・session端末の自動対応後に再buildし、実stdio testはexit 0、1 passed、0 failed（1.50秒）でした。公式Rust MCP SDK clientで実Rust binaryを別processとして起動します。HTTP/OAuth fixtureやNodeを起動せず、一時HOME/state/projectと専用tmux socketを使います。実行ログは [evidence/rust-stdio-smoke.log](evidence/rust-stdio-smoke.log)。

確認済み動作:

- 新MCP製品名と20ツール取得、session作成/一覧/接続、明示memo保存。
- 自動端末の作成/再利用、session_idだけでstdin/output/stop、2つのsession間でjob/output/inputを混同しないこと。
- 複数in-flight command、stdio再起動後の選択job復帰、個別stopで他commandを維持、closeで他sessionを維持、close後/再作成後の古いjob ID拒否。
- 組込みCodex sandboxでコマンドとファイル読取/書込/上書き編集。
- stdio processの実終了、別PIDで再起動、session/memo/tmux jobの再発見、継続中のstdin送信、停止。
- 1024-byte出力上限・truncated・exit 7、shellへのtransport秘密環境非継承、明示close、秘密env混入時の起動拒否。
- fixture pathに替えた実clean-env wrapper本体で、fake Tunnel envを除去しRust stdioの20ツール取得。実UID切替の成功証拠ではありません。

この環境では外側sandboxが内側bubblewrapのsynthetic-mount lockをread-onlyにするため、実testは承認された制約外ローカルexecで実行します。通常executeのsandboxは有効のままです。

wrapper/preflightのshell構文と実OS/CPU/build依存検出は確認済み。wrapperの引数拒否は終了code 64。systemd unitの配置先tunnel-clientはこの環境に無く、sudo/visudoも未導入なので、実service起動・sudoers/UID移行は未確認です。これを通すためのOS変更は実施していません。

旧Node実装・JS test・npm依存/cache・旧worker/wrapper・重複手順と古いログは整理しました。旧版の復元元と保全するcheckpointは [docs/HISTORY.md](docs/HISTORY.md)。元vendor source/ライセンスと既存mycastは変更しません。

Rust HTTP/OAuth実装と対応するRust test/過去ログは保留機能として残しています。過去HTTPログには当時の旧名が含まれます。Tunnel導入・stdio testでは使用せず、機能追加もしていません。本番ASや認証/ログイン成功の証拠として扱いません。

未確認: ARM64実機（OCIを含む）、release build、実Tunnel認証、実ChatGPT/dot接続、systemd/sudoers/UIDの実配置。新規実鍵/grant、実ホストdeploy、OS/network/Tailscale変更は今回の承認範囲に含めず実施していません。導入手順は [INSTALL](deploy/INSTALL.md)、Linuxホスト側の準備依頼は [BOOTSTRAP](deploy/BOOTSTRAP.md)。
