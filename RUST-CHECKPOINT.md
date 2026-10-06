# Rust HTTP checkpoint — 2026-10-06

Rust 0.2.0 の縦断実装を追加。元のNode 0.1.0とvendorは変更していません。

公式 `rmcp=3.5.1`、`jsonwebtoken=11.1.0` を使用。Streamable HTTP `/mcp` は各リクエストでOAuth bearerを検証します。認可サーバーは外部の既存ASで、PKCE S256とresource/audienceの設定が必要です。公開リソースURLはHTTPS、backendはloopback必須。ローカルfixtureはloopback HTTPのみ許可し、認証は有効のままです。

`cargo build --locked --manifest-path rust/Cargo.toml` 成功。
`test/http-smoke.mjs` が実Rustバイナリ、公式Node MCP client、メモリ内のOAuth AS fixture、実tmux、組込みCodex sandboxで成功。詳細は `evidence/http-smoke.log`。

確認: 20ツール、OAuth discovery/challenge/code+PKCE+resource交換、JWT拒否、Origin/Host拒否、sandbox execute、ファイル編集、session/memo、HTTP切断後の通常ジョブ/stdin、出力上限/exit code、停止、close guard、実サーバー再起動後のtmux/memo保持、shell環境分離。

未確認: OCI ARM64、外部OAuthプロバイダー、実ChatGPT/dot接続、公開HTTPS。新規実鍵/client/grant、OCIdeploy、既存mycast/Tailscale変更は未実施。

`rust/build.rs` が未変更vendorからビルド時だけ生成する差分は、tool dispatcherの可視性と、sandbox helperを同じ実行ファイルへルーティングする部分のみ。upstream credit/licenseは元READMEとvendor/LICENSEに保持。

README/配布手順のRust切り替えは後続checkpointで行います。Nodeの手順はdocs/LEGACY-NODE.md、docs/LEGACY-TUNNEL-INSTALL.mdにも退避しました。
