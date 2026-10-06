# Rust migration validation

2026-10-06 UTC、独立した `/workspace/oci-dev-mcp`、Debian 13 x86_64、Rust/Cargo 1.98.1、tmux 3.5a（task-local抽出）、bubblewrap 0.12.0で検証しました。今回のtest client・OAuth fixtureはRustで、Nodeプロセスは使いません。

## 実行結果

```sh
cargo test --locked --manifest-path rust/Cargo.toml --test auth_focused -- --nocapture
cargo test --locked --manifest-path rust/Cargo.toml --test http_smoke -- --nocapture
```

両方とも終了code 0、各1 integration testが成功しました。auth suiteは8.08秒、HTTP suiteは8.38秒。最初に両testの `cargo test --no-run` も成功しています。実行ログは [rust-auth-focused.log](../evidence/rust-auth-focused.log) と [rust-http-smoke.log](../evidence/rust-http-smoke.log)。

実環境では次の環境変数を使いました。Cargo cache以外は他プロジェクトの設定を再利用しません。

```sh
export CARGO_HOME=/workspace/scratch/oci-dev-cargo
export RUSTUP_HOME=/workspace/toolchains/rustup
export CARGO_TARGET_DIR=/workspace/oci-dev-mcp/vendor/local-mcp/target
export PATH=/workspace/toolchains/cargo/bin:$PATH
export OCI_DEV_TMUX_BIN=/workspace/scratch/oci-dev-reference/tmux/usr/bin/tmux
```

各testはCargoでbuildした実サーバーを別processで起動します。HTTP/stdio clientは公式Rust SDK rmcp 3.5.1。セッション/ファイル/ジョブのmockを作らず、実tmux・実Codex sandboxを通します。

## 確認した動作

- 起動前の本人設定制約：空・複数・重複・空文字subをdiscovery前に拒否。
- AS discovery：OAuth path-insertion、OIDC path-insertion、OIDC path-appending、末尾slash付きissuerの完全一致。本人は20ツール取得、他subは403。opaque Bearerやproxy assertion単独は401。
- OAuth resource metadata、401 challenge、手書きfixture clientでのcode/PKCE S256/resource交換。JWTの署名・issuer・audience・exp/nbf・本人sub・scope・必須claim、none/HS256、query tokenを拒否。悪意あるOrigin/Hostは403。
- 現行2026-07-28 stateless HTTPのdiscovery/list/call、session headerなし、resultType complete。公式Rust SDKでも20ツールを取得。
- session作成/一覧/接続、memo、sandbox executeとファイルの読取/書込/上書き編集。
- HTTP clientを実終了して再接続。通常job/tmux jobを再発見し、継続中のstdinを送信。
- tmux停止と停止後unavailable、1024-byte出力上限・truncated・exit 7、通常toolの64 KiB応答上限、stop_job、active upstream job中のclose拒否。
- Rust server processの実終了/再起動後にもtmux processとmemoを維持。
- shellへのtransport秘密環境の非継承、混入時の起動拒否、Bearerをstderrへ出さないこと、明示close、公式Rust SDKのstdioで20ツール取得。

最初のHTTP実行はHost拒否の期待値で失敗しました。旧Node fixtureは400を期待していましたが、reqwestから単一Hostを送った場合、rmcpの許可外Host拒否は403と `Host header is not allowed` を返します。そのstatus/bodyを検証するよう新Rust testだけを修正し、同じ縦断testで再確認しました。本番サーバーの認証処理を変更していません。

外側sandboxでは内側bubblewrapのsynthetic-mount lockがread-onlyになるため、HTTP suiteは承認された制約外のローカルexecで実行しました。最初の承認要求は自動レビューのモデル容量不足で実行されず、同じ承認経路の再試行は通過しました。通常executeのsandboxは有効のままです。

## 検証の範囲と保存

fixture ASのRSA秘密鍵は実行時メモリ内だけで、新規実鍵/client/grantを作成・保存していません。OAuth交換は手書きのfixture clientが実行し、取得済みBearerをSDKへ渡しています。公式SDKの自動OAuth登録/linking、本人の実IdPログイン、ChatGPT consentの検証ではありません。

このcheckpointでは本番Rust source、vendor原本、旧JS、既存mycastを変更していません。追加のtest source/docs/evidenceとdev依存/lockfileのみです。旧Node削除、GitHub push、Libraryへの新しい書込みは行っていません。

未確認：OCI ARM64、release build、公開HTTPS、systemd実配置、外部AS、自動OAuth linking、実ChatGPT/dot/Tunnel接続。旧Nodeコードと配布参照の片付けは承認待ちです。棚卸しと再開点は [RUST-MIGRATION.md](RUST-MIGRATION.md)。
