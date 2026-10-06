# Rust stdio + Secure MCP Tunnel — local validation

2026-10-06 UTC、独立した `/workspace/oci-dev-mcp`、Debian 13 x86_64、Rust/Cargo 1.98.1、実tmux 3.5a、bubblewrap 0.12.0で確認しました。OCI ARM64・release build・実Tunnel認証・dot接続・systemd配置は未実施です。

`cargo test --locked --manifest-path rust/Cargo.toml --test stdio_smoke -- --nocapture` は終了code 0、1 test成功（最終実行0.78秒、build 4.60秒）。公式Rust MCP SDK clientでRust stdioを別processとして起動し、HTTP/外部OAuth fixture/Nodeを起動していません。結果は [rust-stdio-smoke.log](../evidence/rust-stdio-smoke.log)。

確認: 20ツール取得、session/memo、実sandbox commandとファイル読取/書込/編集、stdio processの実終了と別PIDの再起動、session/memo/tmux jobの再発見、継続中のstdin送信、tmux停止、出力1024-byte上限/truncated/exit 7、shellの秘密環境非継承、明示close、transport env混入時の起動拒否。

配布例ではTunnel commandとsudoersの対象をroot所有の `oci-dev-rust-stdio` に統一。sudoはTunnel UID → 作業UIDの1回だけで、wrapperは引数を拒否しenvを空にしてRustをexecします。旧Node workerへのunit依存を除き、tmux継続のため `KillMode=process` に変更しました。MCP Unix brokerは不要で、private tmux socketだけを使います。

wrapper/preflightのshell構文確認とpreflightの実OS/CPU/build依存検出は成功。wrapperに引数を渡すと実際に終了code 64で拒否しました。`systemd-analyze verify` は配置先 `/usr/local/bin/tunnel-client` がこの環境に無いため終了code 1。実UID移行/sudoers構文はsudo/visudo未導入のため未確認です。OS設定を変更してこの検証を通すことはしていません。

fixture pathへ置換した実wrapper本体を使い、fake Tunnel key envとCREDENTIALS_DIRECTORYを除去して20ツールを取得する追加確認も成功しました。実UID境界の検証とは区別します。

外側sandboxは内側bubblewrapのsynthetic-mount lockをread-onlyにするため、stdio実testは承認された制約外のローカルexecで実行しました。通常executeのsandboxは有効のままです。

成果保全: `866b626197bbdbe9d9e92d55f37d46f6cdee7c85` はローカルcommitと145899-byte source archive/16100-byte patchに保存済み。Library保存は正式経路の接続networkエラーと、転送先Forbidden・0 bytesで未完了、confirmed IDなし。remote mainの最後の確認値は `5c2749d847bd853d2767d170eb9fb24b27d0b9ad`。新規key/grant、OCI/OS/network変更、deploy、旧Node削除、pushは行っていません。

13:37の親環境disconnected通知後、この選択実行環境は13:39 UTC時点で読み書きできることを実測し、追加済みtestの検証とローカルsource archive/patchの保存を継続しました。親環境への再接続成功を意味しません。

起動手順は [INSTALL](../deploy/INSTALL.md)、OCI側の初期準備依頼は [BOOTSTRAP](../deploy/BOOTSTRAP.md)。旧HTTP/OAuthのソースと過去検証は保持し、今回の導入では使用しません。旧Library bootstrapの1297a1b指定手順は更新できていないため使用せず、このRust版bootstrapと最新source archiveを使います。
