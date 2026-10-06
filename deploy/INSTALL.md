# OCI: Rust + OAuth HTTP の起動手順

このファイルは配布手順です。この作業ではOCI実機、実OAuthプロバイダー、public HTTPSへ接続していません。以下のOSユーザー/サービス/認証/client/鍵/DNS/firewall/TLSの実変更はまだ行っていません。本人が既存設定を確認し、実機のパス/既存権限へ合わせてから実行します。既存Tailscale/mycastは変更不要です。

## 1. 実機で確認してネイティブビルド

SSH/Tailscale等の既存経路でOCIへ入り、このprivate repoまたはLibrary source archiveを**独立ディレクトリ**へ配置します。

```sh
uname -sm
cat /etc/os-release
sh scripts/preflight.sh --build
cargo build --release --locked --manifest-path rust/Cargo.toml
./rust/target/release/oci-dev-mcp --version
```

CPU aarch64/arm64はARM64、x86_64/amd64はAMD64として検出します。Mac/GHAは使いません。必要なbuild依存はRust 1.96+、cc、make、perl、pkg-config。runtime依存はtmux、bubblewrap、CA証明書とネイティブbinaryの動的ライブラリです。古いbubblewrapの場合は上流Codex sandboxの要件に合わせて更新します。Codex依存のため初回buildは大きめです。

ローカル実fixture（Node 22+は検証用だけ）:

```sh
npm ci --ignore-scripts
OCI_DEV_RUST_BIN="$PWD/rust/target/release/oci-dev-mcp" node test/http-smoke.mjs
```

テストが環境のnamespace/security制約でsandboxに失敗した場合、理由を報告して解決します。通常executeのsandboxを黙って無効化しません。localの制約外fixtureでは組込みsandboxが成功しています。

## 2. 既存OAuth認可サーバーの設定

OpenAIが案内する [Auth0 MCP設定ガイド](https://github.com/openai/openai-mcpkit/blob/main/python-authenticated-mcp-server-scaffold/README.md) を例に、既存のAuth0等を使います。本プロジェクトはRust resource serverなので、ガイドのPythonサーバーは必要ありません。以下の設定をASの管理画面で行います。

1. API/resource identifierを **`https://実ドメイン/mcp`** にする。access tokenの`aud`とRustのresourceが完全一致すること。resource indicatorをauthorization/tokenの両リクエストで受け取れること。
2. token署名はRS256、scopeは `mcp:tools`、access-token有効期間は短め（例5–15分）。正規ユーザーだけがこのscopeを得られるようAS側でも制限する。
3. authorization-code + PKCE S256を有効にし、discoveryに `code_challenge_methods_supported: ["S256"]`、issuer、authorization/token endpoints、JWKSを公開する。issuerは末尾slashも含め正確にコピーする。
4. ChatGPTのCIMD対応があればそれを使う。対応しない場合はDCRまたはChatGPT管理画面の事前登録clientを使う。**管理画面に表示される正確なclient metadata/redirect URIをコピー**し、推測しない。CIMDでは `none` または `private_key_jwt` とAS policyを合わせる。秘密はAS/ChatGPTの適切な管理画面へ直接入力し、chat/README/shell envへ貼らない。
5. 許可する本人のユーザーID（JWTの`sub`）をAS管理画面で確認する。表示名や未検証emailで代用しない。

Rust設定に必要なのはresource/issuer/subで、AS側のprivate keyやclient secretは置きません。新しいtenant/client/grant/実鍵作成はこの実装で自動実行していません。すでに適合するASがなければ、上記の管理画面設定が利用開始前の作業です。OAuth code/token endpointはASが提供し、Rustにログインサーバーを重複実装しません。

[OpenAIの現行OAuth説明](https://developers.openai.com/plugins/build/auth) と [MCP Authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization) に沿います。

## 3. 専用の作業ユーザー・HTTP設定

既存の低権限開発ユーザーがあればそれを使います。例の`devmcp`を実ユーザーへ合わせ、sudo一般許可やroot実行を与えません。binary/sourceはroot所有等で固定し、作業プロジェクト/stateは作業ユーザー所有にします。全プロジェクトを同じUIDから扱える強い権限であることに同意してから使います。

`deploy/http-config.json.example`を `/etc/oci-dev-mcp/http.json` に配置し、domain/issuer/subを実値へ置換します。設定ファイルはroot所有・serviceユーザー読取可（例0640）、stateは0700、生成metadata/memoは0600です。

```json
{
  "listen": "127.0.0.1:8765",
  "resource": "https://dev.example.com/mcp",
  "issuer": "https://YOUR-TENANT.auth0.com/",
  "allowed_subjects": ["auth0|ACTUAL-USER-ID"],
  "scope": "mcp:tools",
  "allowed_origins": ["https://chatgpt.com:443"],
  "local_fixture": false
}
```

dot等がOriginを送る場合は、その正確なoriginを追加します。Originなしの非ブラウザMCP clientも使えます。すべてのHTTP methodをOAuth middlewareが保護し、無認証で取得できるのはprotected-resource metadataだけです。

Node workerとRust serverは同じstateで併用しません。旧Node版へ戻す場合も、使うbackendを1つにします。stateは既定HOME/.local/state/oci-dev-mcp、上流session metadataはXDG_STATE_HOME/local-mcp/sessionsです。変更する際は両方の保存先を引き継いでください。

## 4. HTTPSと常駐

DNS/TLS/公開ネットワークの実変更は本人の確認後に行います。`deploy/Caddyfile.example`を実ドメインに合わせ、Caddy自身のユーザーでTLS秘密鍵を保管してください。serviceユーザーからTLS鍵を読める構成にしません。OAuth AS秘密も作業UIDへ置きません。backendをpublic HTTPへforwardする設定は禁止です。

`deploy/oci-dev-http.service`を実ユーザーと実パスへ合わせます。binary配置例 `/opt/oci-dev-mcp/bin/oci-dev-mcp`、作業例 `/home/devmcp/projects`。承認後にunitを配置し、`systemctl daemon-reload`、`systemctl enable --now oci-dev-http` を行います。unitのKillMode=processはtmuxをserver再起動から独立させるためです。停止後もtmuxが残るので、全作業終了にはMCPのmux_stop/close_sessionまたは専用socketへのtmux kill-serverを明示的に使います。

Caddy例は `/mcp` とprotected-resource metadataだけをproxyし、SSEをflushします。raw HTTP headers/bodyのログは有効にしません。TLS公開前もMCP backendはloopback + OAuth必須で、認証を外す起動フラグはありません。

## 5. 利用者側で接続して確認

ChatGPT/dotのMCP接続URLへ `https://実ドメイン/mcp` を指定し、OAuth linkingで本人としてログインします。AS側で登録方式/client redirectが正しいことを確認します。

最小確認はmetadata取得 → 無認証MCPが401 → OAuth linking → list_sessions → create_session → execute/read/write → mux_open/send/poll → 切断/再接続 → mux_stop/close_session。実機・provider・clientでここまで通って初めて接続済みと報告します。

## 任意のSecure MCP Tunnel

HTTPにはTunnelは不要です。Tunnelを選ぶ場合は `oci-dev-mcp stdio` が使えます。`deploy/oci-dev-rust-stdio`は別UIDへの固定clean-env launcher例です。root所有で変更不可にし、sudoersはこの固定wrapperだけを許可します。runtime keyは別Tunnelユーザーのsystemd LoadCredential等へ置き、作業shellへ渡しません。

旧Nodeの常駐daemon方式、公式Tunnelのlinux-arm64/amd64取得・設定例は [../docs/LEGACY-TUNNEL-INSTALL.md](../docs/LEGACY-TUNNEL-INSTALL.md)。Rust stdio adapter自体は常駐daemonを介さないため、そのstdioプロセス終了では通常upstream jobが終了し、tmuxだけが残ります。
