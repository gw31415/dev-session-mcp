# OCI development MCP

`nakasyou/local-mcp` をそのまま使い、セッション管理・明示的なメモ・tmux端末だけを追加する独立プロジェクトです。Node.js 22以降、Linux、tmux、bubblewrap、ビルド済みlocal-mcpが必要です。既存mycast・Tailscaleの設定を変更しません。

## 構成

dot → OpenAI Secure MCP Tunnel → tunnel-client → stdio adapter → 0600 Unix socket → worker daemon → local-mcp / tmux。

workerとTunnelは別OSユーザー・別systemdサービスです。Tunnelの再起動やstdio adapterの切断でlocal-mcp本体を終了させません。local-mcpの通常ジョブはworkerが生きている間継続し、tmux端末はworker再起動にも耐える構成です。ホスト再起動・tmux終了を越えて実行プロセスは継続しません。セッション情報とメモはディスクに残ります。

## ツール

local-mcpから再利用: `session_info`, `read_file`, `get_image`, `list_directory`, `write_file`, `execute`, `start_command`, `poll_job`, `stop_job`, `without_sandbox`。argvを渡し、シェル構文には明示的に `bash -lc` を使います。通常のexecuteはsandbox内・ネットワーク禁止、without_sandboxはlocal-mcpの承認画面を使います。

追加: `list_sessions`, `create_session`, `connect_session`, `get_memo`, `set_memo`, `mux_open`, `mux_poll`, `mux_send`, `mux_stop`, `close_session`。

例: create_sessionに `{ "session_id": "project-a", "cwd": "/home/devmcp/projects/a" }` → executeに `{ "session_id": "project-a", "command": ["pwd"] }`。再接続は同じIDでconnect_session。メモはset_memoの `text` を任意に保存します。自動で会話を保存しません。

mux_openはcommandを省略するとbashを開きます。返されたjob_idでmux_sendの `text` と `keys: ["Enter"]` を送信し、mux_pollで出力を取得します。stdout/stderrは端末上で統合されます。出力は消費しないsnapshotで、最大64 KiB、tmux履歴は最大10000行。全出力が必要なら作業側がファイルへ保存します。停止はmux_stop。daemon化した子孫は別途プロセス管理が必要です。

## 権限と秘密

**mux_open/mux_sendはserviceユーザーの全ファイル権限・ネットワーク権限で任意コマンドを実行します。sandboxも別承認画面もありません。** この権限を与えてよい利用者だけにTunnel/appアクセスを許可してください。worktree・同時編集ロック・固定workflowは強制しません。

MCPはstdioとユーザー専用Unix socketのみ。認証なしshellのTCP/HTTP listenerは提供しません。Tunnelの認証は公式のruntime key、組織/ワークスペース権限に委ねます。root権限のworker、devmcpのsudo一般許可、Tunnelcredentialをdevmcpと同じUIDに置く構成は禁止です。envの除去だけでは同じUIDの/proc・ファイルアクセスを隔離できません。

配布の設定例はTunnelcredentialをsystemd LoadCredentialでTunnelユーザーだけへ渡し、root所有の固定wrapperをsudoでdevmcpへ切り替えてからenv -iでstdio adapterを起動します。workerは別サービスなのでTunnelcredentialを受け取りません。サーバーもtransport関連envを検出すると起動を拒否します。ソース・設定・メモに鍵を書かず、デバッグのraw HTTP/bodyログを有効にしないでください。local-mcpの承認端末はコマンドや差分を表示するため、秘密をargvやファイル内容として渡さないでください。

## ビルドとローカル起動

```sh
uname -sm
node --version
tmux -V
bwrap --version
npm ci --ignore-scripts
cargo build --locked --manifest-path vendor/local-mcp/Cargo.toml
export OCI_DEV_LOCAL_MCP_BIN="$PWD/vendor/local-mcp/target/debug/local-mcp"
node src/daemon.mjs
# 別ターミナルで、MCPクライアントのstdio commandに指定:
node /absolute/path/oci-dev-mcp/src/server.mjs
```

Linuxではlocal-mcpと同じディレクトリにcodex-linux-sandboxも必要です。Rust要件・依存はvendor/local-mcp/Cargo.toml/Cargo.lockで固定します。本体のRustビルドは小さくなく、Nodeラッパーの依存はpackage-lock.jsonに固定します。単一Rustバイナリ化は今回の範囲に含めません。

## OCI起動

詳細は [deploy/INSTALL.md](deploy/INSTALL.md) と同梱systemd/unit・sudoers・clean-env wrapperです。`scripts/preflight.sh` が実OS/CPUを検出し、公式Tunnelのlinux-arm64/amd64配布を選べます。今回、新しい鍵・grant・OSネットワーク変更・OCI deployは実施していません。OCI ARM64で動作確認済みではありません。

## 検証

`npm test` は実ビルドのlocal-mcpと実tmuxを使う小さいstdio MCP疎通テストです。結果・環境制約は [VALIDATION.md](VALIDATION.md) に記録します。tmuxがtask-localの場合は `OCI_DEV_TMUX_BIN=/absolute/path/tmux npm test`。

本体のread_file/execは上流実装を維持しており、巨大ファイル/出力の内部メモリ消費は上流仕様に従います。ラッパーの返却テキストは全体64 KiBへ制限します。作業コマンドは全体をfileへ保存し、必要な範囲を抽出して読んでください。画像も本体のget_imageのままです。メモはsession directory内のmemo.md（0600）、追加metadataは0700 state directory、停止済みterminalの出力はtmuxが生きている間だけ取得できます。

## 参照とライセンス

本体は [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) の実ソースをvendor/local-mcpへ同梱し、ツールの再実装を避けています。取得時HEAD: `21025d048f54cc9f948c26ac42fa36183dc453c2`。同梱LICENSEはMIT、Cargoのlicense欄はApache-2.0で不一致があるため両方を記録し、上流LICENSEをそのまま保持します。Codex sandboxの推移的依存のライセンスもその依存に従います。追加コードはMITです。

公式確認: [Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels), [tunnel-client](https://github.com/openai/tunnel-client), [configuration](https://github.com/openai/tunnel-client/blob/main/docs/configuration.md)。stdioは1 tunnel IDあたり1 active client。outbound HTTPS/443のみで、health/UIはloopbackへ限定します。
