# 配信診断と開発継続 — 2026-10-08

## 目標と今回の範囲

目標は「作業が勝手に止まらないこと」と「各出力生成から同じChatGPT会話への送信受付まで3秒以内」。平均値への置換、webhook受付までへの短縮、目標時間の緩和は行わない。今回のサーバー変更だけで達成したとは主張しない。

保存先の指定パスが依頼文にないため、このファイルを暫定の成果・残工程保存先とする。

開始時の追跡済み差分はなし。既存の未追跡 `.serena/`、`arm64-*.log`、`arm64-prepared.json`、`private-diagnostics/`、`tests/__pycache__/` は保持する。適用対象の祖先・リポジトリ内 AGENTS.md はなく、`.agents/` と `.codex/` は空。利用可能skillの適用範囲を確認し、今回のRustサーバー実装に適用するskillはなし。OpenAI製品設定の変更は行わない。

新認証、実環境の秘密・購読ストア読取、課金、権限拡大、systemd変更、本番デプロイ、git push、過去の実通知10件の再送・重複試験は行わない。ユーザーの追加指示により、変更に関連する通常回帰テストは実行可能と確認した。

## 実装済み

MCP `tools/call` に `read_delivery_diagnostics` を追加。既存のAPI・Eventsの購読/更新/再送契約は維持する。入力は必須の `session_id` と任意の `execution_id` のみ。呼出例:

```json
{"name":"read_delivery_diagnostics","arguments":{"session_id":"<session-id>","execution_id":"<execution-id>"}}
```

既存のOS UID境界、private broker socketの所有者・権限・peer UID確認、sessionの現存/保持履歴確認、executionのsession所属確認を利用する。caller指定のowner/URL/secretなど追加引数は拒否する。閉じたsessionも既存brokerの保持履歴内に限って診断できる。

返す項目:

| 項目 | 意味 |
| --- | --- |
| `observed_at_ms` | 購読状態を観測したUNIXミリ秒。実行状態は直前の別broker読取であり原子的な同時snapshotではない |
| `session_state`, `execution` | session状態、および指定された実行のID/status/exit_codeのみ。未指定のexecutionはnull |
| `subscriptions` | 同UID・sessionの保持中購読。execution指定時はsession全体向けとそのexecution向けの両方を含む |
| `lease_expires_at_unix_seconds`, `expired` | 既存leaseの期限と観測時の期限切れ判定 |
| `suspended`, `worker_running`, `delivery_state` | 通知停止と実行状態を分離。分類はexpired→suspended→worker_stopped→activeの優先順 |
| `has_unacknowledged_batch`, `pending_queued_at_ms` | 未確認batchの有無と保存されたqueue時刻。古い記録の時刻はnull |
| `last_http_status` | 最終完了attemptのHTTP status。最新attemptがtransport失敗、または履歴なしならnull |
| `delivery_history` | 既存履歴の最新最大16attemptを古い順に返す。event_id、attempt、queue時刻、batch先頭/末尾イベント時刻、送信開始/終了UNIXミリ秒、所要ミリ秒、HTTP status |

許可した項目を明示的に構築し、Subscription/Pending全体はserializeしない。callback URL、現/旧署名秘密、本文、認証情報、command、出力本文、cursorは返さない。brokerエラーも固定文に置換する。診断は購読storeを再読込せず、保存・refresh・unsubscribe・worker起動・HTTP送信を行わない。

通知の`active`はworker taskが存在し未終了であるという観測に限る。進行中HTTPの成功や今後の進捗を保証しない。`last_http_status`は進行中attemptを含まない。未確認batchは初回送信待ち/送信中/再送待ち/停止中のいずれもあり得る。履歴はattempt単位であり出力単位ではない。

期限切れ、410/413、unsubscribe、再起動時の整理で購読自体が消えれば、その履歴も返らない。`subscriptions: []`は保持中証拠なしであり、配信正常・未購読・期限切れ等の理由を断定できない。新しい永続tombstoneは導入しない。

次の観測改善として、brokerがjournalへイベントを記録する既存RFC 3339 timestampをミリ秒精度に変更。キー・型・lease期限の秒精度は維持し、保存済みpending bodyを作り直さない。これはjournal記録時刻であり、子プロセスが内部で文字列を生成した厳密な時刻ではない。

## 実測・推定・未観測の区別

依頼者提供の過去4試験結果（今回再測定していない）:

- 送信側からwebhook受領まで0.34〜1.6秒。
- 同じChatGPT会話への送信までの平均81 / 122 / 104 / 111秒。
- 各試験の3秒以内は0件。

受信側の待ちが主因という提供済み評価に沿い、HTTP受付と同会話送信受付を別段階として扱う。今回の診断は受信側の待ちを短縮しない。batch coalescingの100msだけを削っても目標達成の根拠にはならないため変更しない。

`elapsed_ms`は単調時計による送信attemptの実測。開始/終了/queue/イベント時刻はwall clockであり、差分は時計補正の影響を受ける。新規イベントのミリ秒精度は古い秒精度記録を遡って改善しない。履歴中の先頭/末尾時刻から各出力の遅延を推測しない。HTTP 2xxはcallback受付を示し、会話送信受付・作業継続の証拠にはならない。

## ローカル検証（初回）

初回は新規診断テストだけを `cargo test ... diagnostics` で選択した。通常回帰を含めた追加検証は次節に記録する。fixtureは合成データとプロセス内の模擬broker応答のみで、実環境の購読・秘密・callbackを使用しない。製品コードのbroker呼出しは従来のClientを使用する。

検証対象は分類と期限境界、最大16履歴、旧形式の欠落時刻、transport失敗のnull、機密canaryの非公開、実行中と通知停止の共存、session/execution/UID境界、閉じたsession、空結果、不正引数、brokerエラーの非公開、診断の反復後にメモリ/store/workers/verification cacheが変わらないこと、MCP readOnly宣言とSDK schema変換、イベント時刻のミリ秒保持。

初回のコンパイルは成功。新規テスト3件が通過し、Unix socketを使った1件は実行環境の `Operation not permitted` で失敗した。権限を変更せず、診断内部の同じ処理へ模擬read応答を注入するテストに修正した。実socket接続・peer UID確認・stdio経由の実呼出しは今回の新規テストでは検証していない。これらの既存境界コードは変更せず、sourceで利用を確認した。

初期コマンドではmise/sccache wrapperがread-only filesystemで失敗したため、今回のコマンドだけ `RUSTC_WRAPPER=` を設定した。既存targetのbuild lock待ちは中断し、一時targetに分離した。設定ファイル・権限は変更していない。

最終テストコマンド:

```sh
env RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/dev-session-diagnostics-target cargo test --locked --offline --manifest-path rust/Cargo.toml diagnostics
```

結果: **4 passed / 0 failed / 5 filtered out**、テスト実行0.01秒（再コンパイル19.66秒）。この時間は診断テストの実行時間であり、配信遅延の測定値ではない。`rustfmt --check --edition 2024 rust/src/events.rs rust/src/server.rs`、`git diff --check`、`tests/stdio.py` のAST構文解析も成功。既存stdio試験は期待tool名/件数を14へ更新しただけで、実行していない。

通常ビルドも成功（2分19秒）:

```sh
env RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/dev-session-diagnostics-target cargo build --locked --offline --manifest-path rust/Cargo.toml
```

ビルド成果物は隔離した `/tmp` 内のみ。初回時点では生成したサーバーバイナリを起動・導入しなかった。下記の追加検証では一時stateのテスト用broker/stdioだけを起動した。本番導入はしていない。

## 追加検証と成果保全（制限環境での履歴）

ユーザーの補足を受け、通常Rust回帰とstdio回帰を実行した。新規4テストは成功を維持し、さらにメモリ内通信を使うMCPプロトコル境界テストを追加した。診断関連は計5件成功。通常Rust回帰の初回実行は9件中5件成功、接続を必要とする4件は失敗。実接続を成功扱いしない。

`tests/delivery_diagnostics_stdio.py` は実broker・Unix socket・stdioを使う再現可能な追加テスト。親の認証・transport・broker overrideを継承せず、一時stateと合成の停止中購読だけを使う。状態の無変更性、公開項目、機密非公開、session/execution/owner境界、閉鎖後履歴、frontend再起動を検証するコードを残した。この環境ではbroker socket作成が拒否され、終了コード77、`socket/stdio assertions NOT RUN`。mockへの自動切替や権限変更はしない。

実行コマンドと制限は[追加検証記録](DELIVERY-VALIDATION-20261008.md)、受信側の次の具体的な変更は[受信待ち短縮・継続案](RECEIVER-CONTINUATION.md)を参照。受信側の具体化にはOpenAI Docs skillで公式仕様を確認した。taskや受信handlerはまだ変更していない。

変更diff、新規ファイル、許可したソースのsnapshot、関連する検証記録を明示的なallowlistで保全する。既存の未追跡ログ、private-diagnostics、購読state、認証情報、実通知本文は含めない。前のoutput-last-messageは当初path不明だったが、ユーザー指定の `private-diagnostics/continuation-improvement-result.md` だけを確認し、最初の最終応答787 bytesと完全一致した。他の秘密ファイルは読まず、指定ファイルも変更・commit・bundle収録しない。

## 最新の検証完了：親の実測証拠

親が承認済みVPS通常権限・隔離stateで実測し、2026-10-08T10:21:28〜10:22:13Zの結果を提供した。新規 `delivery_diagnostics_stdio.py` はexit 0（exec `4000399:247669728`、seq724–726）、既存 `tests/stdio.py` はexit 0（`4000420:247672283`、seq727–729）、コンパイル済みunit binaryは全10件成功・失敗0・3.68秒（`4000504:247673912`、seq730–753）。実state・本番・通知先は未変更との親報告。このエージェントは再テストせず、以前の環境拒否を履歴に残して最新判定を更新した。

read-only配信診断APIの実装・検証は完成。元目標の同会話送信受付3秒以内と自主終了防止は未達。親の実socket/stdio/unit成功は受信側遅延の実測ではない。根拠・範囲は[検証記録](DELIVERY-VALIDATION-20261008.md)を参照。関連9ファイルだけのローカルcommitを試み、結果をbundleに記録する。push/deployは行わない。

commit試行は `git add` の `.git/index.lock: Read-only file system`（exit 128）で停止した。stage・commitは未実施、権限変更・迂回なし。選択した9ファイルと最新の親実測記録はbundleで保全する。

## 残工程と継続手順

1. 今回の差分のレビュー後、別途許可された工程で導入する。本作業では本番へ反映しない。
2. 通知が来ない場合、対象IDで診断し、`execution.status`と通知状態を別々に評価する。停止した通知を根拠に実行を再起動したりstdin/commandを再送しない。実行中なら許可済みの独立作業を続け、待機・終了・入力待ちを通知の沈黙だけで決めない。
3. lease更新/再購読は既存の明示的な購読操作に留める。suspendedの自動解除や無制限retryは今回追加しない。診断だけで過去batchが再送されることはない。
4. 次の受信側改善では、[具体的な差分案](RECEIVER-CONTINUATION.md)に従って対象taskのbatching設定、送信優先の受信指示、独立outboxと継続checkpointを順に適用する。現在値・受信repo・既存同会話送信adapterの契約を確認できた変更だけを行う。現workspaceには受信側実装や受付時刻の観測点がなく、ここで変更済みとは扱えない。
5. 受信側で継続中の作業ID、最後に処理したevent/sequence、未処理出力、次の動作をcheckpoint化し、重複排除と再開処理を実装する。通常配信を短周期pollへ置き換えない。これは次工程の設計であり、実装済みではない。
6. 新たな実通知の実測は別途指示された新規ケースに限る。各出力と同会話送信受付の対応をID/sequenceで結び、時計同期・欠測・再送を明記する。合否は各出力の3000ms以内で判定し、平均値やHTTP所要時間で代用しない。過去の実通知10件の再送・重複試験は行わない。通常回帰テストは禁止対象に含めない。

目標は未達。今回完了できるのはサーバーの安全な観測APIと時刻精度の改善およびローカル検証であり、受信側の待機短縮・作業自動再開・同会話送信受付3秒以内の検証が残る。
