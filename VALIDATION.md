# Validation

2026-10-06 UTC, saved Linux environment: Debian 13, x86_64, Node.js 24.19.0, tmux 3.5a (task-local extracted Debian package).

実施日: 2026-10-06 UTC。実ビルドしたlocal-mcp本体、実tmux、公式MCP SDKクライアントで検証しました。モック/カバレッジ目標/大規模テストは追加していません。

本体: nakasyou/local-mcp取得時HEAD `21025d048f54cc9f948c26ac42fa36183dc453c2`。取得archive SHA256: `d580594ccc731ee01dc6846b630a661c01340d0060da6a897b4f4f0f5d89f4ea`。Cargo.lockとCodex sandbox revisionは同梱原本のままです。Cargo 1.98.1による`cargo build --locked --manifest-path vendor/local-mcp/Cargo.toml`成功（debug build、5m25s）。Node dependency installはignore-scriptsで実施。

実stdio MCPテスト `npm test`（test/smoke.mjs、最後の終了code 0）の証拠はevidence/smoke.log。通過項目:

- initialize/tools/listで20ツールを取得（本体10+追加10）。
- upstream local-mcp startの実セッション作成、一覧、既存session_id接続。
- 明示的なメモ保存/読取。
- upstream execute、read_file、write_fileによる編集。
- frontend実切断/再作成後の再接続、live stdin送信。
- upstream start_commandのjobがfrontend切断後にも完了し、poll_jobで取得可能。
- connect_sessionから追跡中のupstream job_idを取得。
- tmux停止、停止後unavailable判定。
- tmux出力1024-byte制限とtruncated表示、exit code 7の取得。
- upstream返却テキスト65536-byte制限。
- upstream stop_job。
- daemon実終了/再起動後もtmux processがrunning、memo内容が維持。
- close_sessionによる作業終了・一覧からの除去。
- transport credentialの環境変数があるstdio adapterの起動拒否（偽のfixture値のみ）。

最初のsandbox内検証は、外側sandboxがbubblewrapの`/tmp/codex-bwrap-synthetic-mount-targets-1000/lock`をread-onlyにしているためupstream executeがexit101でした。標準のsandbox外実行経路で同じ一時fixtureを実行すると成功しました。上流sandboxを無効化したり、executeをunsandboxedへ置き換えてはいません。

systemd unitとwrapperは配布設定例。shell構文確認済み。systemd-analyze verifyはこの環境に配置していない`/usr/bin/node`・`/usr/local/bin/tunnel-client`を指摘したため、インストール成功を意味しません。実パス調整手順をdeploy/INSTALL.mdへ記載。UID隔離、systemd LoadCredential、sudoers、実Tunnel health/authはOCIで未確認です。

確認した公式現行仕様: outbound HTTPS/443、stdio command、1 tunnel ID/1 active client、private runtime key file、loopback health/UI。確認時latest release v0.0.15にlinux-arm64の配布あり。手順はlatest URLを使い、install時の実OS/CPUを検出します。

未実施: OCI ARM64ビルド/実機起動、実Secure MCP Tunnel接続、dot側app discovery/認証、ホスト再起動後のプロセス継続、Rust単一バイナリ化。通常upstream jobsはworker終了で失われ、tmuxもホスト再起動には耐えません。上流read/execの内部メモリ使用量は今回変更していません。

新規鍵・永続grant・OS network変更・OCI deploy・GitHub pushは実施していません。既存mycastの`git status --short --untracked-files=no`は空で、tracked変更はありません。

Workspace: /workspace/oci-dev-mcp, independent from existing mycast.
