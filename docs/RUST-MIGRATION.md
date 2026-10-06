# JavaScript → Rust 移行 checkpoint

このcheckpointは本番Rustサーバーの検証clientとOAuth fixtureをRustへ移す追加実装です。旧Nodeファイルの削除、既存README/配布設定の置換、GitHub mainへの保存、Library書込みは行いません。親タスクで削除とmain保存の承認確認が残っているためです。

## 棚卸し

| 旧JavaScript | Rust側の対応 | このcheckpointでの扱い |
| --- | --- | --- |
| `src/core.mjs` | `rust/src/workspace.rs` のsession/memo/tmux | 本番機能は移行済み。旧ファイル維持 |
| `src/worker.mjs` | 同ファイルのworkerと `rust/src/main.rs` のhidden CLI | 本番機能は移行済み。旧ファイル維持 |
| `src/daemon.mjs` | `rust/src/server.rs` の上流tool組込み・job追跡・出力制限、Workspace | 本番機能は移行済み。旧Unix socket brokerを増築しない |
| `src/server.mjs`, `src/ipc.mjs` | 常駐Rust Streamable HTTP、任意Rust stdio | 旧Node daemon向けの0600 Unix socket接続方式は旧版専用として維持 |
| `test/http-smoke.mjs` | `rust/tests/http_smoke.rs` と `rust/tests/common/mod.rs` | Rust公式SDK client、実サーバー、実tmux/sandboxを使う検証へ移行 |
| `test/auth-focused.mjs` | `rust/tests/auth_focused.rs` と共通fixture | 本人制約・AS discovery・認証拒否を実Rustプロセスで検証 |
| `test/smoke.mjs` | 旧Node Unix socket専用 | 新HTTP suiteが現行の道具を検証。旧方式の複製はしない |

20ツールの本番実行経路は既にRustです。本番Rust build/起動も新しいRust検証もNodeを呼び出しません。旧JSコード、package.json/package-lock.json、旧Nodeサービス/説明書は残っています。「全JS削除完了」ではありません。

## Rustだけで再検証する

Linux、Rust 1.96以上、tmux、bubblewrap、通常のネイティブbuild依存が必要です。実OS/CPUを `sh scripts/preflight.sh --build` で確認します。

```sh
cargo test --locked --manifest-path rust/Cargo.toml --test auth_focused -- --nocapture
cargo test --locked --manifest-path rust/Cargo.toml --test http_smoke -- --nocapture
```

実行するサーバーはCargoがbuildした `oci-dev-mcp` です。必要なら `OCI_DEV_RUST_BIN` で既存binaryを指定でき、tmuxは `OCI_DEV_TMUX_BIN` で指定できます。testごとに一時HOME/state/projectと専用tmux socketを作り、正常終了/失敗時にfixtureを片付けます。既存プロジェクトや本番stateを使いません。

公式Rust SDK `rmcp 3.5.1` のHTTP/stdio clientを使います。認可サーバーはloopbackのAxum fixtureです。fixtureのRSA鍵は実行時メモリにだけ置き、実サービスの鍵/client/grantを新設しません。認可code・PKCE S256・resource交換は手書きのfixture clientで行い、取得済みBearerをMCP SDKへ渡します。SDKの自動OAuth linking、外部AS、本物のChatGPT/dot接続の成功証拠ではありません。

新しい依存は `dev-dependencies` に限定し、本番Rust source/vendor原本を変更していません。共通fixtureと各testは標準 `cargo test` の統合テストです。JSON出力を手で比較する模擬MCPサーバーではなく、本番の20ツールを実際に呼びます。

この実行環境は外側sandboxが内側bubblewrapのsynthetic-mount lockをread-onlyにするため、HTTP縦断testを承認された制約外のローカルexecで実行する必要があります。サーバーの通常executeのsandboxを無効化する回避策は使いません。

## 残る片付け

削除と保存が承認された後に、旧 `src/*.mjs` / `test/*.mjs`、Node package metadata/依存、旧Node service/wrapperを取り除き、README/VALIDATIONと旧説明書の参照をRust suiteへ置換します。Rust stdio/Tunnel wrapper、HTTP service、preflight shellはNodeコードではなく、維持する配布要素です。HTTPの標準接続方式は変えません。

OCI ARM64、release profile、systemd/公開HTTPS、外部AS、自動OAuth linking、実ChatGPT/dot/Tunnel接続はこの移行checkpointでは未検証です。OCIへのdeployやOSサービス/ネットワーク変更も行っていません。
