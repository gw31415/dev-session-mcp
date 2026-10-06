# dev-session-mcp — Rust stdio + Secure MCP Tunnel

Linuxホストを複数プロジェクトの開発環境として使うための独立したMCPサーバーです。特定のクラウドには依存せず、OCI VPSも利用例の一つです。導入経路は **公式Secure MCP Tunnel → Rust stdio → 開発ツール/tmux**。Nodeは不要です。既存プロジェクトやSSH/Tailscaleの設定を変更せず、独立して導入できます。

```text
dot / ChatGPT → OpenAI Secure MCP Tunnel
                         ↑ outbound HTTPS 443
               tunnel-client (mcp-tunnel UID / runtime key)
                         ↓ 固定wrapperでUID切替・envを空にする
               dev-session-mcp stdio (devmcp UID / keyなし)
                         ↓
               local-mcpの道具 + セッション/memo/tmux
```

MCPの公開port、公開HTTPS、外部OAuth認可サーバーの用意はこの構成に不要です。認証・利用許可はTunnel側で行います。Rust stdio自体に独立したログイン認証はないため、Tunnelとprivate app/pluginの利用権限は本人に限定してください。[公式Tunnel案内](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)

## 最短の導入

1. 既存SSH/TailscaleでLinuxホストへ入り、sourceを独立ディレクトリへ配置。実機のOS/CPUを検出してRustをネイティブbuildします。
2. 公式tunnel-client、作業UIDとTunnel専用UID、固定wrapper、既存Tunnel ID/runtime keyを配置します。秘密はTunnel専用UIDだけに渡します。
3. systemdでTunnelを1個だけ常駐させ、ChatGPT/dotのprivate接続へ既存Tunnelを指定します。

実コマンドは [deploy/INSTALL.md](deploy/INSTALL.md)。Linuxホスト側のAIへ渡す最小準備依頼は [deploy/BOOTSTRAP.md](deploy/BOOTSTRAP.md)。新規key/grant、実サービス・OS・ネットワーク変更はこの実装作業では行っていません。

```sh
sh scripts/preflight.sh --build
cargo build --release --locked --manifest-path rust/Cargo.toml
./rust/target/release/dev-session-mcp --version
```

Linux、Rust 1.96+、Cコンパイラ、make/perl/pkg-config、tmux、bubblewrap、CA証明書が必要です。Node/npm、Mac、GHAは使いません。初回buildはCodex依存のため大きめで、低メモリなら `CARGO_BUILD_JOBS=1`。実CPU向けにbuildしたRust実行ファイル1個へCodex Linux sandbox helperを組み込みます。tmuxとbubblewrap、動的リンクのライブラリはOS側依存です。

## 道具と作業の再開

公式Rust MCP SDK `rmcp 3.5.1` を使用します。

| local-mcpの道具 | 追加するセッション/端末の道具 |
| --- | --- |
| `session_info`, `read_file`, `get_image`, `list_directory`, `write_file` | `list_sessions`, `create_session`, `connect_session`, `get_memo`, `set_memo` |
| `execute`, `start_command`, `poll_job`, `stop_job`, `without_sandbox` | `run_command`, `read_output`, `send_stdin`, `stop_command`, `close_session` |

`create_session({"session_id":"project-a","cwd":"/home/devmcp/projects/a"})` で開始し、同じIDへ `connect_session` で戻ります。`execute`はargv配列でコマンドを実行し、通常のexec/ファイル操作は上流Codex sandboxを使います。`without_sandbox`の承認は作業ユーザーのapprovals端末で行います。

`create_session` / `connect_session` がsessionごとの永続端末を自動作成・再利用します。呼出側にtmux名や操作コマンドの指定は不要です。`run_command({"session_id":"project-a","command":["/bin/bash","-c","... "]})` でコマンドを開始し、`read_output`、`send_stdin`、`stop_command` は同じsession_idだけで直近に選んだcommandへ届きます。stdinはtextの改行で送信でき、Enter/C-c等のkeysも指定できます。

同じsessionで複数commandを開始しても前のcommandを停止しません。必要なときだけ返されたjob_idを指定して個別操作します。`run_command` のcommandを省略すると自動端末のinteractive shellへ戻ります。`connect_session` は選択中commandを勝手に切り替えず、job一覧とactive_job_idを返します。`stop_command`は指定した1つ、`close_session`はそのsessionの全端末とmemoを終了・削除し、他sessionへ影響しません。閉じたsessionは明示的に作り直すまで操作を拒否します。同時編集ロック・強制worktree・固定workflowはありません。

Tunnel/Rust stdioの終了・再起動を越えてtmuxとmemoは保持できます。ホスト再起動/tmux終了では端末プロセスも終了します。通常のupstream jobはstdioプロセス内で追跡するため、再起動後にjob handleを再取得できません。継続が必要な作業は `run_command` / `send_stdin` で開始してください。PTYを保持するtmuxサーバー自体の終了を越える継続は保証しません。

出力はsnapshotで、最大64 KiB（既定16 KiB）、tmux履歴10000行。上流toolの返却テキストも全体64 KiBに制限します。完全な出力は作業側でファイルへ保存します。daemon化した子孫は別途停止が必要です。`close_session`は追跡中のupstream jobがある間拒否します。

## 権限と秘密の分離

**run_command/send_stdinは作業UIDの全ファイル・network権限で任意コマンドを実行できます。sandboxや別承認はありません。** 本人専用の接続として運用します。Tunnelの利用許可を渡す相手は、この作業UIDの権限も得ます。

runtime keyは `/etc/dev-session-mcp-tunnel/runtime-key` のroot専用0600ファイルへ置き、systemd `LoadCredential` からTunnel専用UIDだけへ渡します。admin keyは常駐clientに使いません。Tunnel UIDと作業UIDを分離し、固定root所有wrapperのみをsudoersで許可します。wrapperはenvを空にしてRust stdioを起動し、keyやcredential directoryを作業shellへ渡しません。stdioプロセスもtransport envの混入を拒否します。

同じTunnel IDのactive tunnel-clientは1個だけ、同じstateのRust backendも1個だけです。MCP通信にTCP/Unix brokerは増設せず、専用 `tmux.sock` はtmux制御のためだけに使います。[公式stdio運用制限](https://github.com/openai/tunnel-client/blob/master/docs/configuration.md#stdio-deployment-limits)

## 旧名からの移行

旧名 `oci-dev-mcp` を `dev-session-mcp` へ変更しました。既存配置は自動移行しません。

| 旧名称 | 現行名称 |
| --- | --- |
| 実行file/crate `oci-dev-mcp` | `dev-session-mcp` |
| wrapper `oci-dev-rust-stdio` | `dev-session-mcp-stdio` |
| service `oci-dev-tunnel.service` | `dev-session-mcp-tunnel.service` |
| `/opt/oci-dev-mcp`、`/etc/oci-dev-tunnel` | `/opt/dev-session-mcp`、`/etc/dev-session-mcp-tunnel` |
| `OCI_DEV_STATE_DIR`、`OCI_DEV_DEFAULT_CWD`、`OCI_DEV_TMUX_BIN` | `DEV_SESSION_MCP_STATE_DIR`、`DEV_SESSION_MCP_DEFAULT_CWD`、`DEV_SESSION_MCP_TMUX_BIN` |
| `mux_open/poll/send/stop`、`mux_jobs` | `run_command/read_output/send_stdin/stop_command`、`jobs` |

既存セッションを再開する場合は、固定wrapper内の `DEV_SESSION_MCP_STATE_DIR` を従来のstateパス（例 `/home/devmcp/.local/state/oci-dev-mcp`）へ合わせます。稼働中のstate/socketは移動せず、同じstateにbackendを重複起動しません。upstreamの `local-mcp` stateとtmux内部の `odm_` 識別子は再接続のため維持しています。

旧サービスが稼働している場合は、本人が停止・配置変更の対象を確認し、binary、wrapper、sudoers、Tunnel config、serviceを一緒に読み替えます。同じTunnel IDのclientを二重起動せず、既存runtime keyや利用許可を作り直す必要はありません。

## 確認状況と保存

現backendはtmuxです。軽量Rust依存への置換は [候補評価](docs/BACKEND-OPTIONS.md) の調査段階で、未実装です。通常のMCP操作ではbackendを指定しません。

Rust stdioのローカル実検証は [VALIDATION.md](VALIDATION.md)。旧Node実装・JS test・npm依存・旧配布例は整理済みで、旧版は [Git履歴](docs/HISTORY.md) から復元できます。既存のRust HTTP/OAuthコードとRust検証は保留機能として残していますが、Tunnel導入では使わず機能追加もしていません。

**ARM64実機（OCIを含む）、実Tunnel認証、実ChatGPT/dot接続、systemd実配置は未確認です。** ソースの保存先は本人の非公開 [gw31415/dev-session-mcp](https://github.com/gw31415/dev-session-mcp) repositoryです。ローカルMCP疎通と実機接続を区別します。

再利用元は [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp)、revision `21025d048f54cc9f948c26ac42fa36183dc453c2`。vendor原本は変更せず、build.rsでtool dispatcherの可視性と同一binaryのsandbox helper呼出を調整します。上流LICENSEはMIT、Cargo欄はApache-2.0で不一致があるため双方を保持します。追加コードはMIT、推移的依存は各ライセンスに従います。

出典: [公式Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)、[Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)、[tunnel-client](https://github.com/openai/tunnel-client)、[設定仕様](https://github.com/openai/tunnel-client/blob/master/docs/configuration.md)。
