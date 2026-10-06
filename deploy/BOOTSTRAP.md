# OCI側のAIへ渡す最小bootstrap

本人が既存SSH/TailscaleでOCIへ入り、**Rust版の最新source archive**を独立ディレクトリへ配置してから、以下をOCI側のAIへ渡します。Library保存やローカルbuildだけでOCIの接続は始まりません。古いNode専用archive `1297a1b` はこの手順に使いません。

---

このOCIの独立プロジェクトで、Rust stdio + 公式Secure MCP Tunnelの初期準備を進めてください。AGENTS.mdとREADME.md、deploy/INSTALL.mdを読んでください。既存mycast/Tailscaleは変更しません。公開HTTP/外部OAuthは今回使いません。

1. 渡されたarchiveのmanifest/本人が渡したchecksumを確認し、未展開なら独立ディレクトリへ展開します。配置先や確認値が不明ならそれだけ確認してください。古いarchiveのchecksumを新しいsourceへ流用しません。
2. `uname -sm`、`/etc/os-release`、`sh scripts/preflight.sh --build` で実OS/CPU/依存を確認します。Rust 1.96+、tmux/bubblewrap、cc/make/perl/pkg-configが必要です。Node/npm、Mac、GHAを使いません。不足は実OSに合う導入案を具体化します。
3. 依存があれば `cargo build --release --locked --manifest-path rust/Cargo.toml` と生成した `oci-dev-mcp --version` を実行します。低メモリなら `CARGO_BUILD_JOBS=1`。任意の局所検証は `cargo test --locked --manifest-path rust/Cargo.toml --test stdio_smoke -- --nocapture`。sandbox失敗は報告し、黙って無効化しません。
4. 既存tunnel-clientのpath/version、既存Tunnel systemdのActiveState/SubState、特定できるloopback health/readyだけを読み取り確認します。稼働中clientを止めたり同じTunnel IDで別clientを起動しません。秘密本文・env・設定全体・ログ全体を出力しません。
5. INSTALLのRust binary、固定clean-env wrapper、Tunnel別UID、限定sudoers、LoadCredentialとserviceの例を実機の既存ユーザー/パスへ合わせ、必要な配置/起動操作を具体的に示します。新規key/grant、OSユーザー・sudoers・credential配置、サービス新設/起動、ネットワーク変更、実deployはこの準備依頼だけでは実行しません。本人が既存runtime keyを安全に配置し、具体的な操作を確認した後に進めます。

最後に、実機で確認したbuild/依存、準備できた配置案、本人が必要な最小操作、未確認の実Tunnel認証/dot接続を短く報告します。build成功だけで接続済みと報告しません。長時間作業はtmuxのmux tools、目的/次の作業はsession memoを使います。通常jobはstdio childの再起動後にhandleを再取得できません。
