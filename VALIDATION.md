# Validation — private PTY broker + Rust MCP

2026-10-07 UTC: local-mcpの丸ごとのvendorとbuild時の文字列書換えを廃止し、使用する4モジュールだけを `rust/src/base` へ整理しました。公式 `rmcp` SDKが通信を担当し、Linux sandbox helperは同じ実行ファイルへ直接呼出します。由来と追加機能は [UPSTREAM](docs/UPSTREAM.md)、原著作権とライセンスは [NOTICE](NOTICE.md) に記録しています。

この整理後に `cargo fmt --manifest-path rust/Cargo.toml -- --check` と、下記の実stdio integration testを実行しました。**1 passed / 0 failed（1.74秒）**。[実ログ](evidence/rust-upstream-refactor-stdio.log)。20ツール、fileParams拒否、実sandboxでのファイル編集、承認拒否、複数PTY、frontend交換後の再接続・終了結果保持、stdin/resize/個別停止、出力上限、秘密env拒否、clean-env wrapperを確認しました。既存VPSへは接続せず、実機の稼働や配置を変更していません。

2026-10-06 UTC。独立した `/workspace/dev-session-mcp`、Debian 13 x86_64、Rust/Cargo 1.98.1、bubblewrap 0.12.0で確認しました。推奨導入は公式Secure MCP Tunnel + Rust stdioです。runtimeにNode/tmux/shpoolは不要です。

```sh
cargo test --locked --manifest-path rust/Cargo.toml --test stdio_smoke -- --nocapture
```

現行名への簡素化後に実stdioを再build・検証し、1 passed/0 failed（2.37秒）でした。[実ログ](evidence/rust-current-name-smoke.log)。20ツール、sandbox実行、PTY再接続、stdin/resize/stop、人の承認拒否、fileParams metadata、ユーザーファイルを残したcloseとsession再作成を確認しました。HTTPとfile importの前回検証は [実MCPログ](evidence/rust-file-import-mcp.log)、[binary保存ログ](evidence/rust-file-import.log) を参照してください。

stdioは公式Rust MCP SDK clientで実Rust binaryを別processとして起動し、一時HOME/state/projectで検証しました。HTTP/OAuth serverやNode/tmuxを起動しません。

確認した動作:

- 実MCPのimport_file descriptorがopenai/fileParams metadata、必須download_url/file_id、optional mime_type/file_nameを保持。未設定originを拒否し、署名URLの秘密部分やファイルbytesを返さないこと。

- MCP製品名dev-session-mcpと20ツール取得。上流10ツール全てを保持。
- session作成/一覧/接続、自動shellの作成/再利用、session_idだけのstdin/output/stop、別sessionと複数in-flight commandの分離。
- 実stdio processの終了/別PIDでの再起動、同一PTYへの再接続。切断中に終了したjobの最終出力/exit 9、counter=1で再実行しないこと。
- resize_commandの実stty size=75 199、live stdin、個別stopが他jobを保つこと、closeが他sessionを保つこと、closed session/再作成後の古いjob ID拒否。
- executeの実Codex sandbox、write_file/read_fileによる作成/上書き編集。通常shellの強い権限と上流sandbox契約を混同しないこと。
- ローカルapproval-consoleへ実without_sandbox要求が届き、nの拒否でcommandが未実行であること。MCP stdinではapproval jobを指定できず、console detach後もsessionが存続。新しい許可/grantは作成していません。
- 1024-byte出力上限/truncated/exit 7、clean shell環境、transport秘密env混入時の起動拒否、明示終了、管理対象外のユーザーファイルがclose後も残ること。
- 一時pathに置き換えた実clean-env wrapper本体でfake Tunnel envを除去しstdioの20ツール取得。実UID切替の成功証拠ではありません。

保留HTTP/OAuth fixtureも既存契約の回帰確認として実行しました。上流start_command/poll_job/stop_job、close guard、実server再起動後のPTY継続、認証/Origin/Host拒否を確認しました。認証fixtureは一時のtest用で、実Tunnel login、実workspace grant、外部OAuth連携を意味しません。Tunnel導入ではHTTP機能を使いません。

file importの局所検証は `cargo test --locked --manifest-path rust/Cargo.toml --bin dev-session-mcp files::tests -- --nocapture` で2 passed/0 failed（0.07秒）。[ログ](evidence/rust-file-import.log)。実ローカルHTTP fixtureで80,000-byteの未知長binary streamを受信し、size/SHA256、stream中のbyte上限、hash不一致での未配置・一時ファイル破棄、既存file保全、明示overwrite、0600を確認しました。URL/IP policyも確認しています。最初のfixture応答形式を修正して再実行した結果です。実ChatGPT配送や外部HTTPS fetchの成功証拠ではありません。

直接の外向きDNS/HTTPS確認は、本環境で名前解決に失敗しました。制約外execでも同じ結果で、proxy制限や取得先制限を緩和して通していません。実際の署名URL、期限切れ後の再取得、trusted originの実配信、DNS pinning/HTTPSの全経路は実client/ホストでの確認が残ります。

libshpoolの小さな実probeは [evidence/shpool-spike.log](evidence/shpool-spike.log)。切断中に終了したcommandの同名attachがCreatedで再実行counter=2になる結果を受け、pty-process 0.5.3 + 独立brokerを選びました。[選定記録](docs/BACKEND-OPTIONS.md)。

外側sandboxが内側bubblewrap mount lockをread-onlyにするため、実testは承認された制約外のローカルexecで実行しています。executeのsandboxは有効です。通常のtestsのために実ホストのOSやnetwork設定は変更していません。大規模負荷試験やcoverage目標は追加していません。

未確認: ARM64実機（OCIを含む）、release build、実Tunnel認証、実ChatGPT/dot接続、systemd/sudoers/UIDの実配置、実ChatGPT添付input/外部HTTPS fetch・大容量file output。PTY broker終了/ホスト再起動を越える作業復旧は提供しません。新しい実key/grant、実deploy、OS/network/Tailscale変更は実施していません。

この検証は独立プロジェクトのローカルfixtureで行いました。[local-mcp](https://github.com/nakasyou/local-mcp) 由来のコードと保守範囲は [UPSTREAM](docs/UPSTREAM.md)、著作権とライセンスは [NOTICE](NOTICE.md) に記録します。実機準備は [INSTALL](deploy/INSTALL.md)、[BOOTSTRAP](deploy/BOOTSTRAP.md)。
