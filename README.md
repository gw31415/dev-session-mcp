# dev-session-mcp

Linuxホストを複数プロジェクトの開発環境として使うための独立したRust MCPサーバーです。[Shotaro Nakamura氏の nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) のツール・承認・sandbox実装を基礎に、接続が切れても保持するPTYセッションと公式Secure MCP Tunnelの秘密分離を追加しています。本プロジェクトが独立して保守する拡張であり、upstreamの公式配布や作者による推奨ではありません。[由来と責任分担](docs/UPSTREAM.md)、[著作権・ライセンス表示](NOTICE.md) を参照してください。

OCI VPSも利用例の一つです。導入経路は **公式Secure MCP Tunnel → Rust stdio → 開発ツール + 独立PTY保持プロセス**。既存プロジェクトやSSH/Tailscaleの設定を変更せず、独立して導入できます。

```text
ChatGPT / dot
  ↕ OpenAIの認可されたTunnel
公式tunnel-client（専用UID、runtime key）
  ↕ 固定wrapperでUID切替 + env -i
Rust stdio（作業UID、認証鍵なし）
  ↔ 同一binaryのPTY broker（同じ作業UID、private Unix socket）
      ↔ セッションごとのshell / 複数command / 人の承認端末
```

## 最短の導入

[INSTALL](deploy/INSTALL.md) が実OS/CPU検出、native build、公式client配布、秘密分離、systemd常駐、dotへの接続を説明します。[BOOTSTRAP](deploy/BOOTSTRAP.md) は既存SSH/Tailscaleでホストへ入った後にAIへ渡せる準備依頼です。

基本のローカルツールと承認フローだけが目的なら、[local-mcp自身の導入・使い方](https://github.com/nakasyou/local-mcp#readme) も参照してください。本プロジェクトは保持PTY・再接続とTunnel常駐の導入を提供します。

```sh
sh scripts/preflight.sh --build
cargo build --release --locked --manifest-path rust/Cargo.toml
./rust/target/release/dev-session-mcp stdio
```

Linux、Rust 1.96+、Cコンパイラ、make/perl/pkg-config、bubblewrap、CA証明書と動的ライブラリが必要です。Rust実行ファイル1個にCodex Linux sandbox helperとPTY brokerを含めます。tmux、Node/npm、Mac、GHAは不要です。初回buildはCodex依存のため大きめで、低メモリなら `CARGO_BUILD_JOBS=1` を付けます。Rust stdioをTunnel UIDの秘密環境で直接起動せず、固定clean-env wrapperを使ってください。

## 道具と使い方

公開ツールは20個です。[local-mcp](https://github.com/nakasyou/local-mcp) 由来の10ツールと承認・sandboxの契約を保持し、下表の10ツールを追加しています。派生コードは `rust/src/base`、追加実装は `broker.rs`・`workspace.rs`・`files.rs` で保守します。MCP通信は公式 `rmcp` SDKを使います。

| 上流10ツール | 追加10ツール |
| --- | --- |
| session_info, read_file, get_image, list_directory, write_file | list_sessions, create_session, connect_session, close_session, import_file |
| execute, start_command, poll_job, stop_job, without_sandbox | run_command, read_output, send_stdin, resize_command, stop_command |

`create_session({"session_id":"project-a","cwd":"/home/devmcp/projects/a"})` で開始し、同じIDへ `connect_session` で戻ります。sessionごとのinteractive shellを自動作成・再利用します。呼出側にbackend名や端末操作コマンドの指定は不要です。

`run_command({"session_id":"project-a","command":["/bin/bash","-c","... "]})` でcommandを開始し、`read_output`、`send_stdin`、`resize_command`、`stop_command` は同じsession_idだけで直近に選んだcommandへ届きます。stdinはtextの改行で送信でき、Enter/C-c等のkeysも指定できます。resizeはrows/colsを指定します。同じsessionで複数commandを開始しても前のcommandを停止しません。必要なときだけ返されたjob_idで個別操作します。

`run_command` のcommandを省略すると主interactive shellへ戻ります。`connect_session` は選択中commandを勝手に切り替えず、jobs一覧とactive_job_idを返します。終了済みcommandへの再接続でcommandを再実行しません。`stop_command` は1つ、`close_session` はそのsessionの管理対象を停止しmetadataを削除します。他sessionへ影響しません。同時編集ロック・強制worktree・固定workflowはありません。

目的や引継ぎは普通のファイルにwrite_file/read_fileで保存します。close_sessionは作業ファイルやsession state内の管理対象外ファイルを削除しません。添付入力はimport_fileを追加しました。ChatGPTの正式fileParamsで渡された一時URLからbytesを直接保存し、本文をモデルcontextへ載せません。管理者が検証したHTTPS配信originの設定が必要で、未設定時は取得を拒否します。128 MiB/120秒上限、SHA256、途中失敗の破棄、既存ファイルの保全を設けています。実ChatGPT添付・外部HTTPS取得は未確認で、出力側のdot/Library連携は未実装です。[ファイル交換の設計とクライアント要件](docs/FILE-TRANSFER.md) を参照してください。

## 権限と継続範囲

**run_command/send_stdinの通常端末は、承認なしで作業UIDの全ファイル・ネットワーク権限を使います。** execute/start_commandは上流Codex sandboxで実行し、ネットワークを無効化します。read_file/get_image/list_directoryは上流どおり作業UIDのファイルアクセスです。write_fileとwithout_sandboxの承認契約も保持しています。without_sandboxの人による確認は、作業UIDで `dev-session-mcp approval-console SESSION_ID` を実行します。console終了はdetachで、端末を停止しません。MCPのsend_stdinでは承認端末を選べません。

Tunnel/Rust stdioの終了・再起動を越えて、独立PTY brokerが端末・出力・終了結果を保持します。PTY broker自体の終了/ホスト再起動では生存作業と保持出力を失い、残ったjob metadataはunavailableになります。broker再起動を越える作業復旧は実装していません。既存sessionのconnectで主shellは新しく用意できますが、古いcommandを復活させません。承認端末も失った場合、古いsessionをcloseして新sessionを作ります。

通常の上流jobはstdio内で追跡するため、stdio再起動後にhandleを再取得できません。継続が必要な作業はrun_command/send_stdinを使ってください。close_sessionは追跡中の上流jobが残る間拒否します。daemon化・別process groupへ離脱した子孫はプロジェクト側で管理します。

出力はANSI除去済みの繰返しsnapshotです。brokerがjobごとに直近64 KiBのraw bytesを保持し、返却は既定16 KiB/最大64 KiBです。完全なterminal画面の再現や無制限履歴ではありません。上流toolの返却テキストも全体64 KiBに制限します。完全なログは作業側のファイルへ保存してください。

Tunnel UIDと作業UIDを分け、runtime keyはLoadCredentialでTunnel側だけに渡します。root所有の固定wrapperは引数を拒否し、env -iで作業processを起動します。作業UIDにtransport鍵や一般sudo権限を与えません。同じstateのstdio owner/brokerは各1個、broker.sockは0600/同一UID検証です。公開shell/HTTP portを増設しません。同じTunnel IDのactive clientは1個です。[公式stdio制限](https://github.com/openai/tunnel-client/blob/master/docs/configuration.md#stdio-deployment-limits)

## 確認状況と保存

pty-process 0.5.3 + 薄い独立brokerを実装しています。[選定理由とlibshpool実probe](docs/BACKEND-OPTIONS.md)、[実検証結果](VALIDATION.md) を参照してください。保留中のRust HTTP/OAuthコードは残していますが、Tunnel導入では使いません。

本リポジトリで記録している検証はローカルLinux x86_64でのものです。**このソース変更についてARM64実機（OCIを含む）、release build、実Tunnel認証、実ChatGPT/dot接続、systemd実配置は未確認です。** 稼働中ホストの設定や状態をこの文書から推定しません。保存先は [gw31415/dev-session-mcp](https://github.com/gw31415/dev-session-mcp) です。

上流全体のsnapshotを同梱・build時に書換える構造は使いません。公開ライブラリAPIがないため必要部分を出典付きの派生モジュールとして保守し、通常のCargo依存とCargo.lockを使います。[更新方針](docs/UPSTREAM.md#dependency-and-update-policy) を参照してください。上流LICENSEのMIT本文と著作権表示を保持し、元のCargo欄のApache-2.0表記との不一致は [NOTICE](NOTICE.md) に記録しています。追加コードはMIT、推移的依存は各ライセンスに従います。

[公開前監査の範囲と結果](docs/PUBLICATION-AUDIT.md) を保存しています。履歴・refs・release/issueの確認結果と、確認できなかった範囲を明記しています。

出典: [公式Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)、[Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)、[tunnel-client](https://github.com/openai/tunnel-client)、[client設定仕様](https://github.com/openai/tunnel-client/blob/master/docs/configuration.md)。
