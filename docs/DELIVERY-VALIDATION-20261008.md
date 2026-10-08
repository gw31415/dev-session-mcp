# 配信診断・追加検証 — 2026-10-08

基準HEAD: `edf0f0923657fd00cf2d6b729b5932b61edcdf48`。対象はlocal Rustサーバーと追加診断API。過去の実通知10件は送信/再送していない。本番state・秘密・callbackは読んでいない。通常回帰の実行はユーザー補足により許可済み。

## 最新結果：親によるVPS隔離実測

2026-10-08T10:21:28〜10:22:13Z。親がユーザー承認を得てVPS通常権限・隔離stateで実行し、その実測結果を本会話で報告した。以下は**親の実測証拠**であり、このエージェントが再実行・独立測定した結果ではない。今回の更新では再テストしない。

| 対象 | 親の実測結果 | 証拠参照 |
| --- | --- | --- |
| `tests/delivery_diagnostics_stdio.py` | **exit 0**。実broker/socket/stdioによる診断API検証成功 | exec `4000399:247669728`、seq `724–726` |
| `tests/stdio.py` | **exit 0**。14 tools、sandbox、PTY、reconnect等の通常回帰成功 | exec `4000420:247672283`、seq `727–729` |
| コンパイル済みunit binary、全10 tests | **10 passed / 0 failed、3.68秒** | exec `4000504:247673912`、seq `730–753` |

親報告では実state・本番・通知先は未変更。この証拠により、以前この環境で未検証だった実接続と通常回帰を最新結果では成功へ更新する。過去の拒否記録は下記に保持する。親のraw logや実環境の秘密を読みに行かず、提供された参照と結果を保全する。実socket成功をmockの成功から推定したものではない。

**今回のread-only診断APIは実装・検証完了。** これは各出力生成→元の同会話送信受付3秒以内や自主終了防止の達成証拠ではない。その元目標は未達であり、受信側機能・継続処理の実装と検証が残る。

## 履歴：このエージェントの制限環境での結果

| 実行 | 結果 | 実際に確かめた範囲 |
| --- | --- | --- |
| 通常Rust回帰 `cargo test ... -- --test-threads=1` | **5 passed / 4 failed**（9件） | 新規4件の成功を維持。既存attachment URL/IP拒否の回帰も成功 |
| 接続が必要なRust回帰3件 | **EPERMで失敗** | HTTP listener作成で拒否。署名/HTTP/転送のassertionsは未検証 |
| 実broker回帰1件 | **ENOENTで失敗** | broker socketが作成されず、読取段階へ進めない。起動エラーは既存fixtureがstderrを捨てるため直接出力されない |
| 通常 `tests/stdio.py` | **失敗** | `broker exited during startup`。discovery/変更後14tool一覧を含めてstdioのassertionsは未検証 |
| 新規 `tests/delivery_diagnostics_stdio.py` | **BLOCKED・終了77** | 実broker起動時にsocket作成が拒否。`socket/stdio assertions NOT RUN`。実接続成功ではない |
| 追加後 `cargo test ... diagnostics` | **5 passed / 0 failed / 5 filtered out** | 既存の新規4件＋メモリ内MCP境界1件。実socket/stdio接続の成功ではない |
| 通常build | **成功**（追加testコード反映後、19.29秒） | バイナリ生成のみ。本番導入なし |

この制限環境の結果だけでは全suite成功とは言えなかった。追加API境界テスト後は同じ接続拒否試験を繰り返さず、診断テストだけを再検証した。その後の親による全10件成功は上記の最新結果を参照。独立した `tests/io_control.py` はこの環境では未実行で、親報告の対象にも含まれない。

## 成功したMCP境界検証

`diagnostics_mcp_protocol_boundary_in_memory` は本物の `EventService`、rmcp framing/dispatch、`server/discover`、`tools/list`、`tools/call` をメモリ内duplexで通す。fixtureのbroker socketは意図的に存在しない。以下を確認する:

- 新toolの公開と既存Events capabilityの維持。
- 必須引数なし、null execution、owner/secretを含む未知引数の拒否。
- 有効な形のqueryでbroker不在の場合、path/機密を含まない固定エラーの `CallToolResult`。
- 未知toolの `-32602` と未定義 `events/diagnostics` の `-32601`。
- 診断処理でstoreやsocketファイルが作成されないこと。

これはプロトコル境界のテストであり、Unix socket接続、peer UID拒否、stdio OS pipeや診断成功responseの実接続検証を代替しない。

## 実接続テストを残した範囲

`tests/delivery_diagnostics_stdio.py` は別processの実brokerとstdioを一時state内で動かす。親からの `DEV_SESSION_MCP_BROKER_SOCKET` やtransport/認証環境を引き継がない。broker PIDは起動したprocess handleと照合し、終了処理も自分で起動したprocessだけに行う。

実行可能な環境では、成功responseの二つのcontent表現、許可キー集合、最大16履歴、legacy形式、session全体/個別executionの購読集合、foreign owner/期限切れの除外、未知入力拒否、繰返し診断でstore bytes/mtimeとbroker位置が変わらないこと、frontend再起動、session閉鎖後の状態を検証する。使用する購読は全てsyntheticでsuspended=trueのため、callback検証・HTTP送信は一切要求しない。

fixtureの「foreign ownerを返さない」テストと、異なるOS UIDからのsocket接続拒否は別。後者はこのtestでUIDを変更して検証しない。

環境拒否を77で明示し、mockに切り替えない。future runnerは77をPASSに集計してはならない。当初この環境ではassertions未実行だったが、親のVPS実測で本testのexit 0が確認された。

## 実行コマンド

```sh
env RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/dev-session-diagnostics-target cargo test --locked --offline --manifest-path rust/Cargo.toml -- --test-threads=1
env RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/dev-session-diagnostics-target cargo test --locked --offline --manifest-path rust/Cargo.toml diagnostics
env RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/dev-session-diagnostics-target cargo build --locked --offline --manifest-path rust/Cargo.toml
python3 tests/delivery_diagnostics_stdio.py /tmp/dev-session-diagnostics-target/debug/dev-session-mcp
```

通常stdio回帰は親にあるlive overrideを渡さないため、Python `subprocess.run` のenvを `PATH=os.defpath, LANG=C.UTF-8, PYTHONDONTWRITEBYTECODE=1` に限定して `tests/stdio.py <binary>` を一度実行した。`HOME`や実環境の権限は変更していない。

既存build wrapperのread-only問題を避けるコマンド単位の `RUSTC_WRAPPER=` と一時targetの利用は継続。socket拒否を回避するためのsandbox変更、権限追加、別hostへの移動、systemd変更は行っていない。

## 成果保全とoutput-last-message

`artifacts/delivery-diagnostics-20261008/` に、明示した関連9ファイルのsnapshot、基準HEADとの差分patch、新規ファイルも含む再適用用patch、今回の関連test結果と親実測参照、SHA-256 manifest、archiveを保全する。既存未追跡ログ、private-diagnostics、実state、認証情報は含めない。ユーザーから関連コード・テスト・docsだけのローカルcommitが許可された。commitの実行結果はbundleのmanifestに記録し、push/deployは行わない。bundleや生logはcommit対象に含めない。

ローカルcommitの試行結果：関連9ファイルを明示した `git add` がexit 128、`.git/index.lock: Read-only file system` で拒否された。この環境の `.git` はread-onlyであり、stage・commitは未実施。権限変更や別git directoryへの迂回は行わない。変更を含むpatch/snapshotと親実測証拠をbundleに保全し、commit未完を明記する。このgit書込み制約と、元目標に必要な受信側プラットフォーム機能の問題は別である。

前のoutput-last-messageは当初path不明だったが、ユーザーから `private-diagnostics/continuation-improvement-result.md` と特定された。**その指定ファイルだけ**の存在とbytesを確認し、最初の診断API実装完了時の最終応答（787 bytes）と完全一致した。直前の追加検証turnの応答とは別の内容である。この指定ファイルは変更せず、他のprivate-diagnosticsファイルを読まず、commit/bundleにも含めない。今回の最終出力はbundle内の `output-last-message.md` に保存する。

受信側待ち短縮の具体案と必要条件は[次工程](RECEIVER-CONTINUATION.md)。元目標は未達、数値基準は各出力3000msのまま。
