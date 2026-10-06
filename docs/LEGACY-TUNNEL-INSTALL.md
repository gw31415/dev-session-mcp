> Node 0.1.0 + Tunnel rollback reference. Current Rust HTTP instructions are [deploy/INSTALL.md](../deploy/INSTALL.md). Paths below are repository-root-relative.

# OCI Linux install (未実行)

以下はroot権限のインストール例です。今回の作業では実行していません。新しい認証鍵/永続grant、OCI実deploy、Tailscaleやfirewallの変更は含めません。任意コマンドを実行できる接続を有効にする操作はユーザーが行います。

1. source archiveをOCIへ転送・展開し、`sh scripts/preflight.sh` で**そのOCIの**OS/CPUを検出。OCIがUbuntu/Debianならnode/npm/tmux/bubblewrap/build-essential/pkg-config/curl/unzip、Oracle Linuxなら相当するパッケージを導入します。Nodeは22以降、Rustはlocal-mcp要件の1.96以降。実OSを確認してパッケージマネージャを選び、既存Tailscaleは変更しません。
2. 展開した独立プロジェクトで `npm ci --omit=dev --ignore-scripts` と `cargo build --locked --release --manifest-path vendor/local-mcp/Cargo.toml`。低メモリなら `CARGO_BUILD_JOBS=1`。release buildは初回に時間/ディスクを使います。GHA/Macは不要です。本体をすでに持っているなら同じ確認済み版のlocal-mcpとcodex-linux-sandboxを使えます。
3. [Platform tunnels](https://platform.openai.com/settings/organization/tunnels) の公式download、または [latest release](https://github.com/openai/tunnel-client/releases/latest) からpreflightが示す `linux-arm64` / `linux-amd64` のfull `tunnel-client` ZIPを選ぶ。公式SHA256SUMSと照合してから `/usr/local/bin/tunnel-client` に配置。arm64版をx86_64へ流用しません。`tunnel-client --version` と `tunnel-client help quickstart` で現行仕様を確認。
4. workerユーザー `devmcp` と、秘密を持つ専用ユーザー `mcp-tunnel` を作る。どちらも互いのグループに入れず、devmcpにsudo一般権限を与えない。専用homeは0700。既存projectを使う場合はdevmcpへ必要なアクセスだけ付与。

配置例（パス・ユーザー名を変える場合はunit/wrapper/YAMLを一緒に合わせる）：

```sh
sudo useradd --create-home --user-group --shell /bin/bash devmcp
sudo useradd --system --create-home --user-group --home-dir /var/lib/mcp-tunnel --shell /usr/sbin/nologin mcp-tunnel
sudo chmod 0700 /home/devmcp /var/lib/mcp-tunnel
sudo install -d -o root -g root -m 0755 /opt/oci-dev-mcp /opt/oci-dev-mcp/bin /usr/local/libexec
sudo cp -a src package.json package-lock.json node_modules /opt/oci-dev-mcp/
sudo chown -R root:root /opt/oci-dev-mcp
sudo install -m 0755 vendor/local-mcp/target/release/local-mcp vendor/local-mcp/target/release/codex-linux-sandbox /opt/oci-dev-mcp/bin/
sudo install -d -o devmcp -g devmcp -m 0700 /home/devmcp/projects
sudo install -m 0755 deploy/oci-dev-mcp-stdio /usr/local/libexec/oci-dev-mcp-stdio
sudo install -m 0440 deploy/sudoers.example /etc/sudoers.d/oci-dev-mcp
sudo visudo -cf /etc/sudoers.d/oci-dev-mcp
sudo install -d -o root -g mcp-tunnel -m 0750 /etc/oci-dev-tunnel
sudo install -o root -g mcp-tunnel -m 0640 deploy/tunnel-client.yaml.example /etc/oci-dev-tunnel/config.yaml
sudo install -m 0644 deploy/oci-dev-worker.service deploy/oci-dev-tunnel.service /etc/systemd/system/
```

既存ユーザー名がある場合はuseraddを再実行せず設定を合わせます。`/usr/bin/node` が実際のNodeパスと異なる場合はunit/wrapperを修正。設置したコード・wrapperはroot所有を保ちます。

既存Tunnel IDをconfig.yamlに設定し、runtime keyを `/etc/oci-dev-tunnel/runtime-key` にroot:root 0600で安全に保存します（秘密をcommand line、履歴、一般shell環境にexportしない）。keyの本文をここへ書かないでください。systemdはLoadCredentialのprivate copyをmcp-tunnelだけへ渡します。永続ファイルはrootだけが読み、devmcpはruntime credential directoryやmcp-tunnelの/procへアクセスできません。

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now oci-dev-worker.service
sudo systemctl enable --now oci-dev-tunnel.service
sudo systemctl status oci-dev-worker.service oci-dev-tunnel.service
curl -fsS http://127.0.0.1:8080/healthz
curl -fsS http://127.0.0.1:8080/readyz
```

必要な外向き接続はapi.openai.com:443（mTLS構成時はmtls.api.openai.com:443）。stdio MCPにはinboundポート不要、health/UIはloopbackのみ。**同じtunnel IDのactive tunnel-clientは1個だけ**にし、手動runとsystemdを重複させない。設定診断は対応するfull clientのdoctorを使い、runtime keyを一般シェルへ渡さない。

ChatGPTのAdd custom MCP serverでConnection=Tunnel、既存tunnel IDを選択。組織/ワークスペースassociationとTunnels Read+Useを確認し、作成されたprivate app/pluginをdotに接続。任意shell権限の警告を確認してください。新規grant/鍵が必要な場合は別途ユーザーがその権限を設定します。

Tunnelの停止/再起動はworkerやtmuxを止めません。worker停止は通常local-mcpジョブを終了しますがtmux端末は残します。全tmux作業の終了は明示的なmux_stop/close_session、またはdevmcpとして `tmux -S /home/devmcp/.local/state/oci-dev-mcp/tmux.sock kill-server`。close_sessionはメモも削除します。任意にdaemon化した子孫はtmux外へ残ることがあるため、そのプロジェクトのプロセス管理で停止します。

local-mcpのwithout_sandbox承認を行う場合は、Tailscale/SSHからdevmcpとして対応するapprovals端末へattachする。connect_sessionでkind=approvalsのjob_idを調べ、`tmux -S /home/devmcp/.local/state/oci-dev-mcp/tmux.sock attach -t odm_JOB_ID`。本体の既定askを維持し、ラッパーが自動でyoloへ変更することはありません。
