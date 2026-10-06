# OCI: Rust stdio + Secure MCP Tunnel

これは実機導入の手順書です。OCI・Tunnel認証・dot接続・OSサービス変更はまだ実施していません。既存SSH/TailscaleでOCIへ入り、独立ディレクトリで進めます。公開MCP port/HTTPSや外部OAuthの設定は不要です。

## 1. 実OS/CPUを検出してbuild

```sh
uname -sm
cat /etc/os-release
sh scripts/preflight.sh --build
cargo build --release --locked --manifest-path rust/Cargo.toml
./rust/target/release/oci-dev-mcp --version
```

preflightはaarch64/arm64をARM64、x86_64/amd64をAMD64として検出します。Rust 1.96+、cc/make/perl/pkg-config、tmux、bubblewrap、CA証明書と動的ライブラリが必要です。不足は実OSのパッケージマネージャで導入します。Nodeは不要。低メモリならbuildに `CARGO_BUILD_JOBS=1` を付けます。

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
sudo install -d -o root -g root -m 0755 /opt/oci-dev-mcp/bin /usr/local/libexec
sudo install -o root -g root -m 0755 rust/target/release/oci-dev-mcp /opt/oci-dev-mcp/bin/oci-dev-mcp
sudo install -d -o devmcp -g devmcp -m 0700 /home/devmcp/projects /home/devmcp/.local /home/devmcp/.local/state /home/devmcp/.local/state/local-mcp /home/devmcp/.local/state/oci-dev-mcp
sudo install -o root -g root -m 0755 deploy/oci-dev-rust-stdio /usr/local/libexec/oci-dev-rust-stdio
sudo install -o root -g root -m 0440 deploy/sudoers.example /etc/sudoers.d/oci-dev-mcp
sudo visudo -cf /etc/sudoers.d/oci-dev-mcp
sudo install -d -o root -g mcp-tunnel -m 0750 /etc/oci-dev-tunnel
sudo install -o root -g mcp-tunnel -m 0640 deploy/tunnel-client.yaml.example /etc/oci-dev-tunnel/config.yaml
sudo install -o root -g root -m 0644 deploy/oci-dev-tunnel.service /etc/systemd/system/oci-dev-tunnel.service
```

config.yamlのTunnel IDを実値へ置換します。MCP commandは `sudo -n -u devmcp -- /usr/local/libexec/oci-dev-rust-stdio`。sudoersはこの固定wrapperの**引数なし**だけを許可します。wrapperはsudoを再実行せず `env -i` でRust stdioを起動します。binary/wrapperと親ディレクトリはroot所有・作業UIDから変更不可にします。

Node worker、HTTP service、Caddyはこの構成では配置/起動しません。Rust stdioはTunnelのchildとして常駐するため別workerサービスやUnix RPC brokerは不要です。上流session metadataは `/home/devmcp/.local/state/local-mcp/sessions`、拡張session/memo/tmuxは `/home/devmcp/.local/state/oci-dev-mcp`。パス変更時はwrapperのHOME/XDG_STATE_HOME/OCI_DEV_STATE_DIRを揃えます。

## 4. 既存runtime keyを安全に配置して起動

本人が安全な端末経路で既存runtime keyを `/etc/oci-dev-tunnel/runtime-key` に保存し、root:root 0600にします。鍵本文をコマンドに埋め込む例は提供しません。systemd `LoadCredential` がTunnel UID専用のprivate copyを作り、clientは `--control-plane.api-key=file:%d/runtime-key` で読みます。作業UIDに鍵・credential directoryを読める権限を与えません。

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now oci-dev-tunnel.service
sudo systemctl status oci-dev-tunnel.service
curl -fsS http://127.0.0.1:8080/healthz
curl -fsS http://127.0.0.1:8080/readyz
```

同じTunnel IDのclientは1個だけにします。手動runや旧Node worker/別Rust backendが同じstateで動いていれば、重複起動せず現状を確認して停止対象を決めます。必要な外向き接続は `api.openai.com:443`、既存control-plane mTLSを使う場合は `mtls.api.openai.com:443`。inbound portは開けません。health/UIはloopbackのみ。既存Tailscale/firewallを変更しません。

このunitは `KillMode=process` でtmuxをTunnelの停止/restartから残します。Rust stdioが終了すれば通常jobの追跡handleは失われます。生存が必要な作業はmux toolsで開始します。Tunnel停止を全作業の終了と取り違えないでください。

## 5. ChatGPT/dotで既存Tunnelを接続

clientの健康状態を確認したうえで、ChatGPTのAdd custom MCP serverのConnection=Tunnelから既存Tunnel IDを選び、作成したprivate app/pluginをdotへ接続します。組織/ワークスペースassociationと本人の利用権限を確認します。stdio側はこの認可を信頼し、任意コマンドを作業UID権限で実行できるため本人専用にします。[公式接続案内](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)

最小確認: `list_sessions` → `create_session` → execute/read/write → `mux_open/send/poll` → 接続を切って同じsessionへ再接続 → `mux_stop/close_session`。実OCI・認証・dotでここまで通って初めて「接続済み」と報告します。legacy MCP clientはchildが置き換わったときinitialize/initializedを再送します。現行2026-07-28の自己完結requestもRust SDK/Tunnelで扱います。

終了は `mux_stop` / `close_session`。全tmux作業を明示的に終了するならdevmcpとして `tmux -S /home/devmcp/.local/state/oci-dev-mcp/tmux.sock kill-server`。closeはmemoも削除し、daemon化した子孫はプロジェクト側のプロセス管理で停止します。without_sandboxの承認端末はconnect_sessionのkind=approvalsのjob_idから、devmcpで同じsocketの `odm_JOB_ID` sessionへattachします。

現在、OCI ARM64・release build・実Tunnel認証・dot接続・systemd実配置は未確認です。旧JS削除とGitHub main保存は本人確認待ち。詳細は [stdio検証](../docs/TUNNEL-STDIO-VALIDATION.md)。
