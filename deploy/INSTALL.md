# Linux: dev-session-mcp + Secure MCP Tunnel

これは実機導入の手順書です。実ホストでのTunnel認証・dot接続・OSサービス変更はまだ実施していません。既存SSH/TailscaleでLinuxホストへ入り、独立ディレクトリで進めます。OCI VPS、他のVPS、自宅Linuxなど、クラウド固有のAPIは使いません。公開MCP port/HTTPSや外部OAuthの設定は不要です。

## 1. 実OS/CPUを検出してbuild

```sh
uname -sm
cat /etc/os-release
sh scripts/preflight.sh --build
cargo build --release --locked --manifest-path rust/Cargo.toml
./rust/target/release/dev-session-mcp --version
```

preflightはaarch64/arm64をARM64、x86_64/amd64をAMD64として検出します。Rust 1.96+、cc/make/perl/pkg-config、bubblewrap、CA証明書と動的ライブラリが必要です。不足は実OSのパッケージマネージャで導入します。Nodeは不要。低メモリならbuildに `CARGO_BUILD_JOBS=1` を付けます。

任意のローカルstdio確認は、Node不要の `cargo test --locked --manifest-path rust/Cargo.toml --test stdio_smoke -- --nocapture`。このtestは一時HOME/state/projectで実サーバーを起動し、作業プロジェクトを変更しません。namespace制約でsandboxが失敗したら原因を報告し、通常executeのsandboxを無効化しません。

## 2. 公式tunnel-clientを用意

[Platform Tunnels](https://platform.openai.com/settings/organization/tunnels) のdownload、または [公式latest release](https://github.com/openai/tunnel-client/releases/latest) から、検出したCPUに合うfull clientの `linux-arm64` / `linux-amd64` ZIPを選びます。配布のSHA256SUMSと照合し、実行ファイルを `/usr/local/bin/tunnel-client` へ配置します。再現用に導入したversionを記録し、更新時にもchecksumを確認します。

```sh
tunnel-client --version
tunnel-client help quickstart
```

既存Tunnel IDとruntime API keyを使います。runtimeの本人にはTunnels Read + Use、対象ChatGPT workspaceにはprivate MCPを利用できるassociation/権限が必要です。未作成・未許可なら本人または管理者の操作が残ります。ここでは新規key/grantを自動作成しません。runtime keyをchat/argv/履歴/exportへ貼らず、常駐clientにはadmin keyを使いません。[公式権限案内](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels#permissions-and-access)

## 3. 作業UIDとTunnel UIDを分けて配置

以下はroot権限を使う**未実行の配置例**です。`devmcp`と`mcp-tunnel`が既に存在するならuseraddを省略し、home/パスを全ファイルで合わせます。互いのグループに入れず、devmcpに一般sudo権限を与えません。

```sh
sudo useradd --create-home --user-group --shell /bin/bash devmcp
sudo useradd --system --create-home --user-group --home-dir /var/lib/mcp-tunnel --shell /usr/sbin/nologin mcp-tunnel
sudo chmod 0700 /home/devmcp /var/lib/mcp-tunnel
sudo install -d -o root -g root -m 0755 /opt/dev-session-mcp/bin /usr/local/libexec
sudo install -o root -g root -m 0755 rust/target/release/dev-session-mcp /opt/dev-session-mcp/bin/dev-session-mcp
sudo install -d -o devmcp -g devmcp -m 0700 /home/devmcp/projects /home/devmcp/.local /home/devmcp/.local/state /home/devmcp/.local/state/local-mcp /home/devmcp/.local/state/dev-session-mcp
sudo install -o root -g root -m 0755 deploy/dev-session-mcp-stdio /usr/local/libexec/dev-session-mcp-stdio
sudo install -o root -g root -m 0440 deploy/sudoers.example /etc/sudoers.d/dev-session-mcp
sudo visudo -cf /etc/sudoers.d/dev-session-mcp
sudo install -d -o root -g mcp-tunnel -m 0750 /etc/dev-session-mcp-tunnel
sudo install -o root -g mcp-tunnel -m 0640 deploy/tunnel-client.yaml.example /etc/dev-session-mcp-tunnel/config.yaml
sudo install -o root -g root -m 0644 deploy/dev-session-mcp-tunnel.service /etc/systemd/system/dev-session-mcp-tunnel.service
```

config.yamlのTunnel IDを実値へ置換します。MCP commandは `sudo -n -u devmcp -- /usr/local/libexec/dev-session-mcp-stdio`。sudoersはこの固定wrapperの**引数なし**だけを許可します。wrapperはsudoを再実行せず `env -i` でRust stdioを起動します。binary/wrapperと親ディレクトリはroot所有・作業UIDから変更不可にします。

Rust stdioはTunnelのchildとして常駐し、同一binaryのprivate PTY brokerを作業UIDで別processとして自動起動します。broker専用unitは不要です。上流session metadataは `/home/devmcp/.local/state/local-mcp/sessions`、拡張session/job metadataとbroker.sock/broker.pidは `/home/devmcp/.local/state/dev-session-mcp`。broker.sockは0600、stateは0700で同一UIDを確認します。パス変更時はwrapperのHOME/XDG_STATE_HOME/DEV_SESSION_MCP_STATE_DIRを揃えます。

## 4. 既存runtime keyを安全に配置して起動

本人が安全な端末経路で既存runtime keyを `/etc/dev-session-mcp-tunnel/runtime-key` に保存し、root:root 0600にします。鍵本文をコマンドに埋め込む例は提供しません。systemd `LoadCredential` がTunnel UID専用のprivate copyを作り、clientは `--control-plane.api-key=file:%d/runtime-key` で読みます。作業UIDに鍵・credential directoryを読める権限を与えません。

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now dev-session-mcp-tunnel.service
sudo systemctl status dev-session-mcp-tunnel.service
curl -fsS http://127.0.0.1:8080/healthz
curl -fsS http://127.0.0.1:8080/readyz
```

同じTunnel IDのclientは1個だけにします。既存client/別Rust backendが動いていれば、重複起動せず現状を確認して停止対象を決めます。必要な外向き接続は `api.openai.com:443`、既存control-plane mTLSを使う場合は `mtls.api.openai.com:443`。inbound portは開けません。health/UIはloopbackのみ。既存Tailscale/firewallを変更しません。

このunitは `KillMode=process` でPTY brokerをTunnelの停止/restartから残します。broker自体の終了/ホスト再起動ではPTYと保持出力を失います。Rust stdioが終了すれば通常の上流jobの追跡handleは失われます。生存が必要な作業は通常の `run_command` / `send_stdin` で開始します。Tunnel停止を全作業の終了と取り違えないでください。

## 5. ChatGPT/dotで既存Tunnelを接続

clientの健康状態を確認したうえで、ChatGPTのAdd custom MCP serverのConnection=Tunnelから既存Tunnel IDを選び、作成したprivate app/pluginをdotへ接続します。組織/ワークスペースassociationと本人の利用権限を確認します。stdio側はこの認可を信頼し、任意コマンドを作業UID権限で実行できるため本人専用にします。[公式接続案内](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)

最小確認: `list_sessions` → `create_session` → execute/read/write → `run_command/send_stdin/read_output` → 接続を切って同じsessionへ再接続 → `stop_command/close_session`。実ホスト・認証・dotでここまで通って初めて「接続済み」と報告します。セッション型MCP clientはchildが置き換わったときinitialize/initializedを再送します。現行2026-07-28の自己完結requestもRust SDK/Tunnelで扱います。

終了は `stop_command` / `close_session`。作業ファイルは保持し、daemon化した子孫はプロジェクト側で管理します。without_sandboxの承認は既存SSH経路で作業UIDになり、同じHOME/XDG_STATE_HOME/DEV_SESSION_MCP_STATE_DIRで `/opt/dev-session-mcp/bin/dev-session-mcp approval-console SESSION_ID` を実行します。Ctrl-C/stdin EOFはconsoleのdetachだけです。人が内容を読んでy/nを入力します。この手順はyoloや永続許可を新設しません。

全作業を終了/upgradeする場合、まず全sessionをcloseし、管理者が当該stateのbroker.pidとprocessのUID/argvを照合して、そのbrokerへSIGTERMを送ります。brokerは保持jobを停止してsocket/pid fileを除去します。PID fileだけを盲信してkillしません。Tunnel restartだけで古いbroker/binaryが更新されるとは仮定しません。これらは実機で対象を確認して行う操作です。

添付入力はimport_fileを使います。固定wrapperのDEV_SESSION_MCP_FILE_ORIGINSは既定で空です。正式clientが渡す添付URLの配信originを管理者が検証した後、root所有wrapper内へ完全一致のHTTPS originをカンマ区切りで設定します。ワイルドカード、モデルが指定したorigin、未確認の配信hostを許可しません。値は作業shell/brokerへ渡しません。署名URL全体をログ/chatへ貼らず、originだけを確認します。外部HTTPS取得と実ChatGPT添付入力はまだ未確認です。

成果物の出力交換は [ファイル転送設計](../docs/FILE-TRANSFER.md) を参照します。正式なdot/Library出力連携の公開契約は未確認・未実装で、現在は既存SSH/SFTPまたは正式なclient側転送連携が必要です。file URIを返すだけでdotからdownloadできるとは報告しません。

現在、ARM64実機（OCIを含む）・release build・実Tunnel認証・dot接続・systemd実配置は未確認です。ソースは非公開GitHubへ保存します。詳細は [stdio検証](../VALIDATION.md)。
