# OCI development MCP — Rust + OAuth HTTP

OCIを複数プロジェクトの開発環境として使うための独立したMCPサーバーです。標準構成は **Rust単一実行ファイル + OAuthで保護するStreamable HTTP**。Tunnelは任意です。既存mycast/Tailscaleは変更しません。

```
ChatGPT / dot → HTTPS reverse proxy → 127.0.0.1:8765/mcp
                   OAuth bearerをRust側で毎回検証
                                      ↓
                         local-mcp tools / tmux
```

公式Rust MCP SDK `rmcp 3.5.1` が現行2026-07-28と旧版の互換処理を担当します。認可サーバーは既存IdP（Auth0等）を利用し、独自のログイン/暗号実装を持ちません。Rust側がRFC 9728 protected-resource metadata、401 challenge、RS256 JWTの署名・issuer・audience・exp/nbf・scope・本人subの検証を担当します。`jsonwebtoken 11.1.0` を使用し、未知のissuer/key URL、HS256、unsigned token、URL中のtokenを受け付けません。

## すぐ使うための前提

必要な設定は公開HTTPS `/mcp` URL、ASの正確なissuer、許可する本人の`sub`、作業ユーザー/ディレクトリです。scopeは既定 `mcp:tools`。MCPサーバー側にOAuth client secretや署名秘密鍵は不要です。

ASはauthorization-code + **PKCE S256**、discovery/JWKS、**RS256で署名したJWT access tokenをAuthorization: Bearerで渡すこと**、`resource`パラメータをauthorization/token両方で受けて同じ値をaccess tokenの`aud`へ入れる設定、scope、ChatGPTのCIMD・DCR・事前登録のいずれかに対応する必要があります。AS側でログイン・同意・クライアント/redirect登録を行います。対応するASを既に持たない場合、この設定が残る運用作業です。fixtureのASは本番用ではありません。

**Cloudflare Access Managed OAuthは現版では非対応です。** 公式仕様ではopaque access tokenを発行し、originには`Cf-Access-Jwt-Assertion`を渡します。本サーバーはopaque tokenの照会やそのheaderによる認証を実装していないため、issuerだけをCloudflareへ変更しても動きません。[Cloudflareのtoken形式](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/managed-oauth/#token-format)

詳細とAuth0の公式手順、systemd/Caddy例は [deploy/INSTALL.md](deploy/INSTALL.md)。バックエンドはloopback以外へのbindを拒否します。公開URLとissuerはHTTPS必須。公開HTTPSのTLS終端はreverse proxyが行います。

## ビルド・起動

Linux、Rust 1.96以降、Cコンパイラ、make/perl/pkg-config、tmux、bubblewrap、CA証明書が必要です。Nodeは本番Rustサーバーには不要です。まず実機でCPU/OSを確認します。

```sh
sh scripts/preflight.sh --build
cargo build --release --locked --manifest-path rust/Cargo.toml
# 実機の値に変更した設定を用意（例には秘密情報なし）
cp deploy/http-config.json.example /absolute/private/http.json
./rust/target/release/oci-dev-mcp serve --config /absolute/private/http.json
```

HTTP endpointは `/mcp`。無認証アクセスは401とdiscoveryへのchallengeを返します。設定例のdomain/issuer/subを本物へ変更しないと起動できません。`local_fixture: true` はloopback IPのHTTP URLだけを許可するテスト設定で、認証は常に有効です。

Codex Linux sandbox helperも同じ実行ファイルへ組み込みます。tmuxとbubblewrapはOS側依存として残ります。単一ファイルはLinuxネイティブの動的リンク実行ファイルで、別CPU/OSへの万能バイナリではありません。ARM64実機でネイティブビルドしてください。

## ツール

| 再利用するlocal-mcpの道具 | 追加するセッション/端末の道具 |
| --- | --- |
| `session_info`, `read_file`, `get_image`, `list_directory`, `write_file` | `list_sessions`, `create_session`, `connect_session`, `get_memo`, `set_memo` |
| `execute`, `start_command`, `poll_job`, `stop_job`, `without_sandbox` | `mux_open`, `mux_poll`, `mux_send`, `mux_stop`, `close_session` |

`create_session({"session_id":"project-a","cwd":"/home/devmcp/projects/a"})` で開始。同じIDで`connect_session`すればジョブを再発見できます。`execute`にはargv配列を渡します。通常execute/write_fileは上流のCodex sandboxを使い、network禁止。`without_sandbox`は上流の承認端末へ要求します。

`mux_open`のcommand省略でinteractive bash。返されたjob_idへ`mux_send`のtextと`keys:["Enter"]`を送信し、`mux_poll`で読む構成です。stdout/stderrは端末で統合されます。停止は`mux_stop`。`set_memo`で目的/現状/次の作業を任意に保存できます。会話は自動記録しません。同時編集ロック・強制worktree・固定workflowはありません。

HTTP切断で通常ジョブを終了しません。サーバー再起動でもtmuxとメモは残ります。通常のupstream jobはRustサーバープロセスに依存し、再起動で失われます。ホスト再起動/tmux終了で端末プロセスは終了します。tmuxは耐障害性のためのプロセス管理で、永続ジョブキューではありません。

出力は消費しないsnapshot、最大64 KiB（既定16 KiB）、tmux履歴10000行。上流の返却テキストも全体64 KiBへ制限します。上流の巨大ファイル/出力の内部メモリ消費は元実装に従います。全出力は作業側でファイル保存してください。daemon化した子孫は別途停止が必要です。`close_session`は追跡中のupstream jobがある間拒否します。

## 権限・認証・秘密の分離

**mux_open/mux_sendはserviceユーザーの全ファイル・network権限で任意コマンドを実行します。sandboxや別承認はありません。** 本サーバーは本人1人専用です。`allowed_subjects`は本人の`sub`を1件だけ指定し、空・複数件（重複も含む）は起動時に拒否します。全MCP接続がこの唯一の本人として同じOSユーザー/セッションを使います。複数ユーザー間のセッション分離は提供しません。

ASの署名秘密鍵/client secretはAS側に置きます。HTTPS proxyのTLS秘密鍵や任意Tunnelcredentialは、作業shellと別OSユーザー/別権限へ置いてください。作業shellへbearer/transport環境を渡しません。transport関連envの混入は起動を拒否します。ログにHTTP Authorization/body、コマンド、環境変数を出すdebug設定は使わないでください。上流の承認画面はargvやdiffを表示するため、秘密をargv/file内容に入れないでください。

署名鍵をJWKSから取得し通常5分cache。未知kidによる取得は30秒間隔へ制限します。JWKS取得/検証失敗は拒否します。JWT時計ずれの許容30秒。失効照会/introspectionは未実装なのでASで短いaccess-token期限を使い、緊急時はsubをallowlistから外してサーバーを再起動してください。受信bearerはASや他サービスへ転送しません。

## Tunnel・旧Node版

任意のlocal stdioは `oci-dev-mcp stdio`。公式Secure MCP Tunnelから起動する場合、以前と同じく別UIDとclean-env wrapperを使います。stdio接続の終了でそのRustプロセスの通常jobは終了しますがtmuxは残ります。常駐HTTPが推奨構成です。

動作済みNode 0.1.0は `src/` に保持しています。旧daemon方式の起動/配布は [docs/LEGACY-NODE.md](docs/LEGACY-NODE.md)、[docs/LEGACY-TUNNEL-INSTALL.md](docs/LEGACY-TUNNEL-INSTALL.md)。新Rust版と旧Node workerを同じstateで同時運用しないでください。Nodeへ戻す場合はRustサービスを停止し、旧サービスだけを使用します。

## 検証・保存・出典

実Linux x86_64でビルドし、実Rust HTTP、公式MCP client、メモリ内OAuth fixture、実tmux、組込みsandboxによる縦断疎通が成功。詳細は [VALIDATION.md](VALIDATION.md) と [evidence/http-smoke.log](evidence/http-smoke.log)。**OCI ARM64、外部AS、公開HTTPS、実ChatGPT/dot接続は未確認です。** 新規実鍵/client/grant、OCIdeploy、OSネットワーク変更は行っていません。

本体の再利用元は [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp)、revision `21025d048f54cc9f948c26ac42fa36183dc453c2`。vendorの元ソースは変更せず、Rust build.rsでdispatcherの可視性と同一バイナリsandbox helper呼出だけをビルド時に調整します。上流LICENSEはMIT、Cargo欄はApache-2.0で不一致があるため両方を保持・記録します。Codex等の推移的依存は各依存のライセンスに従います。追加コードはMITです。

現行仕様確認: [公式Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)、[MCP Authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)、[Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)、[OpenAI OAuth案内](https://developers.openai.com/plugins/build/auth)。任意Tunnelは [公式案内](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)、[tunnel-client](https://github.com/openai/tunnel-client) を参照。
