# OCI development MCP — Rust stdio + Secure MCP Tunnel

OCIを複数プロジェクトの開発環境として使うための独立したMCPサーバーです。導入経路は **公式Secure MCP Tunnel → Rust stdio → 開発ツール/tmux**。Nodeは不要です。既存mycast/Tailscaleへ変更を加えません。

```text
dot / ChatGPT → OpenAI Secure MCP Tunnel
                         ↑ outbound HTTPS 443
               tunnel-client (mcp-tunnel UID / runtime key)
                         ↓ 固定wrapperでUID切替・envを空にする
               oci-dev-mcp stdio (devmcp UID / keyなし)
                         ↓
               local-mcpの道具 + セッション/memo/tmux
```

MCPの公開port、公開HTTPS、外部OAuth認可サーバーの用意はこの構成に不要です。認証・利用許可はTunnel側で行います。Rust stdio自体に独立したログイン認証はないため、Tunnelとprivate app/pluginの利用権限は本人に限定してください。[公式Tunnel案内](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)

## 最短の導入

1. 既存SSH/TailscaleでOCIへ入り、sourceを独立ディレクトリへ配置。実機のOS/CPUを検出してRustをネイティブbuildします。
2. 公式tunnel-client、作業UIDとTunnel専用UID、固定wrapper、既存Tunnel ID/runtime keyを配置します。秘密はTunnel専用UIDだけに渡します。
3. systemdでTunnelを1個だけ常駐させ、ChatGPT/dotのprivate接続へ既存Tunnelを指定します。

実コマンドは [deploy/INSTALL.md](deploy/INSTALL.md)。OCI側のAIへ渡す最小準備依頼は [deploy/BOOTSTRAP.md](deploy/BOOTSTRAP.md)。新規key/grant、実サービス・OS・ネットワーク変更はこの実装作業では行っていません。

```sh
sh scripts/preflight.sh --build
cargo build --release --locked --manifest-path rust/Cargo.toml
./rust/target/release/oci-dev-mcp --version
```

Linux、Rust 1.96+、Cコンパイラ、make/perl/pkg-config、tmux、bubblewrap、CA証明書が必要です。Node/npm、Mac、GHAは使いません。初回buildはCodex依存のため大きめで、低メモリなら `CARGO_BUILD_JOBS=1`。実CPU向けにbuildしたRust実行ファイル1個へCodex Linux sandbox helperを組み込みます。tmuxとbubblewrap、動的リンクのライブラリはOS側依存です。

## 道具と作業の再開

公式Rust MCP SDK `rmcp 3.5.1` を使用します。

| local-mcpの道具 | 追加するセッション/端末の道具 |
| --- | --- |
| `session_info`, `read_file`, `get_image`, `list_directory`, `write_file` | `list_sessions`, `create_session`, `connect_session`, `get_memo`, `set_memo` |
| `execute`, `start_command`, `poll_job`, `stop_job`, `without_sandbox` | `mux_open`, `mux_poll`, `mux_send`, `mux_stop`, `close_session` |

`create_session({"session_id":"project-a","cwd":"/home/devmcp/projects/a"})` で開始し、同じIDへ `connect_session` で戻ります。`execute`はargv配列でコマンドを実行し、通常のexec/ファイル操作は上流Codex sandboxを使います。`without_sandbox`の承認は作業ユーザーのapprovals端末で行います。

長時間のコマンドやinteractive shellには `mux_open`。返されたjob_idへ `mux_send`のtextと `keys:["Enter"]` を送信し、`mux_poll`で出力を取得します。`mux_stop`で終了し、`close_session`で端末とsession/memoを片付けます。目的/現状/次の作業は任意の `set_memo` で保存できます。同時編集ロック・強制worktree・固定workflowはありません。

Tunnel/Rust stdioの終了・再起動を越えてtmuxとmemoは保持できます。ホスト再起動/tmux終了では端末プロセスも終了します。通常のupstream jobはstdioプロセス内で追跡するため、再起動後にjob handleを再取得できません。継続が必要な作業はmux toolsで開始してください。

出力はsnapshotで、最大64 KiB（既定16 KiB）、tmux履歴10000行。上流toolの返却テキストも全体64 KiBに制限します。完全な出力は作業側でファイルへ保存します。daemon化した子孫は別途停止が必要です。`close_session`は追跡中のupstream jobがある間拒否します。

## 権限と秘密の分離

**mux_open/mux_sendは作業UIDの全ファイル・network権限で任意コマンドを実行できます。sandboxや別承認はありません。** 本人専用の接続として運用します。Tunnelの利用許可を渡す相手は、この作業UIDの権限も得ます。

runtime keyは `/etc/oci-dev-tunnel/runtime-key` のroot専用0600ファイルへ置き、systemd `LoadCredential` からTunnel専用UIDだけへ渡します。admin keyは常駐clientに使いません。Tunnel UIDと作業UIDを分離し、固定root所有wrapperのみをsudoersで許可します。wrapperはenvを空にしてRust stdioを起動し、keyやcredential directoryを作業shellへ渡しません。stdioプロセスもtransport envの混入を拒否します。

同じTunnel IDのactive tunnel-clientは1個だけ、同じstateのRust backendも1個だけです。既存Node worker、別stdio、HTTP backendを同じstateで併用しません。MCP通信にTCP/Unix brokerは増設せず、専用 `tmux.sock` はtmux制御のためだけに使います。[公式stdio運用制限](https://github.com/openai/tunnel-client/blob/master/docs/configuration.md#stdio-deployment-limits)

## 確認状況と保存

Rust stdioのローカル実検証は [docs/TUNNEL-STDIO-VALIDATION.md](docs/TUNNEL-STDIO-VALIDATION.md)。以前のHTTP実装/検証はソースと過去文書へ残していますが、今回の導入では起動しません。旧Nodeも削除の本人確認待ちで保持しており、今回の起動経路からは参照しません。

**OCI ARM64、実Tunnel認証、実ChatGPT/dot接続、systemd実配置は未確認です。** ローカルMCP疎通と実機接続を区別します。`866b626`のソースarchive/patchはローカル保存済みですが、Library転送はForbiddenで未完了、GitHub mainへの保存も本人確認待ちです。

再利用元は [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp)、revision `21025d048f54cc9f948c26ac42fa36183dc453c2`。vendor原本は変更せず、build.rsでtool dispatcherの可視性と同一binaryのsandbox helper呼出を調整します。上流LICENSEはMIT、Cargo欄はApache-2.0で不一致があるため双方を保持します。追加コードはMIT、推移的依存は各ライセンスに従います。

出典: [公式Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)、[Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)、[tunnel-client](https://github.com/openai/tunnel-client)、[設定仕様](https://github.com/openai/tunnel-client/blob/master/docs/configuration.md)。
