# Validation — Rust HTTP 0.2.0

2026-10-06 UTC、独立した /workspace/oci-dev-mcp、Debian 13 x86_64。Rust/Cargo 1.98.1、Node 24.19.0（検証clientだけ）、実tmux 3.5a（task-local抽出）、bubblewrap 0.12.0。

`cargo build --locked --manifest-path rust/Cargo.toml` 成功。debug executableは1ファイル、Codex Linux sandbox helperを組込み。localではCargo target cacheを共有したため実ファイルはvendor/local-mcp/target/debug/oci-dev-mcpに生成。通常buildではrust/target/debug/oci-dev-mcpに生成されます。release profile/OCI ARM64のbuildは未実施です。

実fixtureのコマンド:

```sh
OCI_DEV_RUST_BIN=/workspace/oci-dev-mcp/vendor/local-mcp/target/debug/oci-dev-mcp \
OCI_DEV_TMUX_BIN=/workspace/scratch/oci-dev-reference/tmux/usr/bin/tmux \
node test/http-smoke.mjs
```

終了code 0。証拠は [evidence/http-smoke.log](evidence/http-smoke.log)。本番サーバー、公式MCP client、実tmux、実sandboxを使います。OAuth認可サーバーだけはloopbackのfixtureで、RS256鍵は実行時メモリ内に生成し、保存/外部登録しません。authorization/token交換はテストコードが手書きで行い、取得済みBearerを公式MCP clientへ直接渡しています。公式clientの自動OAuth discovery・client登録・外部AS linking、実IdPの本人ログイン、ChatGPT consentの成功証拠ではありません。

通過項目:

- サーバーのOAuth resource metadata・401 WWW-Authenticate・AS discovery/JWKSと、手書きfixture clientによるauthorization-code + PKCE S256 + resource交換。違うverifier/resourceはfixture ASが拒否。
- JWT署名、issuer、audience、exp、nbf、kid、本人sub、scopeを検証。none/HS256、query token、悪意あるOrigin/Hostを拒否。
- 現行2026-07-28 stateless HTTPのserver/discover、tools/list、tools/call。protocol metadata、Mcp-Method/Mcp-Name、session headerなし、resultType completeを確認。
- 公式Node client 2.3.1の旧版互換HTTP initialize/tools/listで20ツール（本体10+追加10）。
- 実local-mcp session作成/一覧/接続、明示メモ、組込みCodex sandbox execute、ファイル読取/書込/編集。現行HTTPでも上流read_fileを実行。
- HTTPクライアント実切断/再作成後のupstream job継続、job再発見、live stdin送信。
- tmux停止、停止後unavailable、1024-byte出力上限/truncated/exit code 7、upstream返却65536-byte上限、stop_job、通常jobがあるとclose拒否。
- Rust server実終了/再起動後にもtmux processとmemoが維持。
- shell環境にtransport/token/credentialが含まれないこと、明示close、transport envの起動拒否、server stderrにbearerが出ないこと。
- 任意Rust stdioでも実MCP initialize/tools/list。

独立監査後のfocused実プロセス検証は `test/auth-focused.mjs`、証拠は [evidence/auth-focused.log](evidence/auth-focused.log)、終了code 0。空・複数・重複・空文字subの設定をdiscovery/HTTP bind前に拒否し、本人1件の設定で20ツール取得、他subの拒否を確認しました。path付きissuerのOAuth path-insertion → OIDC path-insertion → OIDC path-appendingの優先順と、末尾slash付きissuerの完全一致も実fixtureで確認。opaque Bearerと`Cf-Access-Jwt-Assertion`単独では認証できないことも確認しました。これはRustサーバー自身のdiscovery/JWT受入テストで、MCP clientの自動OAuth登録/linkingテストではありません。

設定名/CLIは維持し、`allowed_subjects`の要素数を1件へ制約しました。複数ユーザー間のMCPセッション分離を増築していません。Cloudflare Access Managed OAuthのopaque-token方式は現版非対応で、README/INSTALLに明記しています。

最初の停止後判定の不具合（tmux display-messageの曖昧なtarget）はexact has-session確認とcapture時の終了race処理で修正し、同じ実fixtureで再確認しました。現行discovery応答はsupportedVersionsで、旧initializeのprotocolVersionと区別して検証します。

この環境では外側sandboxが内側bubblewrapのsynthetic-mount lockをread-onlyにします。fixtureを許可された制約外execで実行しました。通常executeをunsandboxedへ差し替えたり、Codex sandboxを無効化してはいません。

preflightは実OS/CPU/runtime/build依存を検出して通過。shell構文確認済み。systemd/Caddyは設定例のみで、実配置/起動/TLS成功を検証したものではありません。Node版の過去証拠は [docs/LEGACY-VALIDATION.md](docs/LEGACY-VALIDATION.md)。元Nodeコードとvendor Rust原本は未変更です。

未実施: OCI ARM64、公開HTTPS、外部OAuth AS、実ChatGPT/dot/Tunnel接続。新規実OAuth client/key/grant、OSユーザー/サービス/ネットワーク変更、OCIdeployは実施していません。既存mycastのtracked変更なし。private専用GitHubのmainへのソース保存だけを実施しています。

通常upstream jobsはサーバーprocess終了で失われ、tmuxもホスト再起動を越えて継続しません。巨大file/outputの内部メモリ消費は上流のままです。JWT失効照会は未実装で、期限とallowlistを使います。
