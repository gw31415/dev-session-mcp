# wait_execution — bounded ordinary tool

`wait_execution({"execution_id":"…","cursor":"epoch:sequence","max_wait_ms":1000})`

指定executionの出力・状態event・終了を、通常の `tools/call` 応答として返す独自のread-only tool。HTTP通知のbatch・webhook・modelへの非同期配信を経由しない。公式Events streamingではなく、subscriptionや永続runnerも作らない。既存 `read_execution` / Events / stdin / executionの契約は変更しない。

## 境界

- `execution_id` と `cursor` は必須。`max_wait_ms` は整数0〜10000、既定1000。未知引数・null・不正値は拒否する。
- 初回snapshotで未読event・gap・終了済みがあれば即返す。0は待機を行わない。runningでも無出力を入力待ちと推定しない。
- 変更待ちだけが `max_wait_ms` の対象。前後のsnapshotはそれぞれ最大1000msの通信予算を持つため、tool内部の時間上限は `max_wait_ms + 2000ms`（OS scheduling / transport先の処理は別）。応答不能ならtool errorを返し、cursorや状態を捏造しない。
- 通常のtimeoutは最新snapshotに `timed_out:true` を加えて返す。deadline付近にevent・終了・gapを回収できた場合はfalse。timeoutは作業終了ではない。
- 応答は既存の `execution, events, cursor, earliest_cursor, catch_up_required, more` と追加の `timed_out`。終了済みでも32KiBのpage制限があり、`more:true` なら残りを明示回収する。
- cursorはbroker全体の `epoch:sequence`。他executionの更新だけでも返すcursorは進む。execution record内のcursorではなく、応答トップレベルのcursorを次回に渡す。同epoch内でsequence重複排除し、epochを跨いで同じsequenceを同一視しない。
- journalは既存どおり全体1MiB/1024 events。`catch_up_required:true` は完全回収できない可能性を示し、保持分を返す。欠落の完全復元を約束しない。epoch変更後の旧execution_idはエラーであり、実行を自動再作成しない。
- caller cancelはSDKのrequest tokenで受け取り、future/socketをdropする。stdio EOFではrmcp 3.5.1が最大5秒の既存drainを行い、frontend終了時にsocketを閉じる（明示cancelの即解除とは異なる）。返答を受け取れなければ最後に受信済みのcursorから回収し、command/stdinは再送しない。
- frontendごと最大4待機、超過は即エラー。内部は初回read＋最大1回wait＋timeout時readだけで、poll/retry loopはない。broker既存32接続上限も維持。多数frontend全体を跨ぐ予約枠保証はなく、飽和時はエラーをcallerへ返す。

## 旧broker互換性と更新条件

`git diff 4d1d1d7 3149223 -- rust/src/broker.rs` は空。旧sourceには `wait_events`、execution filter、Notify登録後のjournal確認、Unix socket切断時の処理dropが既に存在する。初回snapshotとwaitの間のeventは**元のcaller cursor**を渡して回収する。terminal判定を初回snapshotで行うため、終了済みを既存の無期限waitに入れない。

したがってbroker変更・cutoverは不要。今回の変更はfrontendのtool追加だけ。親の内容確認までpush/deployしない。後で更新するときも稼働brokerと既存実行を保持し、frontendだけを更新する。frontend rollbackは同じbrokerへ旧frontendを戻すだけ。将来broker変更が必要になった場合も、既存実行・journalの移行機構はないため、稼働実行を勝手に止めて更新してはならない。

## 隔離検証（2026-10-08）

本番state・通知設定・秘密は読まない。一時state、clean environment、private socket、fixture自身のPIDのみを使用。既存未追跡ファイルには触れない。適用対象のAGENTS.mdはなく、提供されたskillsに本repoの通常Rust変更へ適用するものはない。

- Rust unit: **12 passed / 0 failed**。初回snapshot→waitの競合回収、応答しないbrokerへの1秒deadlineとsocket解放を追加。
- 通常 `tests/stdio.py`: **PASS**、15 tools、sandbox/PTY/pipes/reconnect/epoch/gap/close等。
- `tests/delivery_diagnostics_stdio.py`: **PASS**、既存の診断API回帰。
- `tests/wait_execution_stdio.py`: 実stdio経由でschema/不正引数/timeout/出力/state/終了、40回cancel後の枠解放、4同時待機の上限、32KiBページで90KB回収、1.2MB出力後のgap、epoch/未来cursorを検証。optional第2引数で旧broker binaryを指定できる。
- 初回のローカル計測: 120ms timeout **122.74ms**、入力状態event→tool応答 **2.51ms**、出力本文中の生成timestamp→tool応答 **4.49ms**、終了済み **1.50ms**。単発のdebug build実測で、性能保証ではない。

追加の互換試験では `git archive 4d1d1d7` を一時sourceへ展開してoffline buildした旧brokerと、新frontendを組み合わせ、同じ実stdio試験を**PASS**。EOF/reconnectを含む最終実測はtimeout **123.69ms**、状態event **2.29ms**、出力生成→ローカル応答 **1.65ms**、終了済み **1.18ms**。EOF→frontend終了は **5023.28ms**（既存SDKの5秒drain）。最初のEOF試験はfixtureの5秒終了上限に衝突して失敗したため、SDK source確認後に7秒の検証上限へ修正した。明示cancelの40回連続試験は成功。稼働brokerには接続していない。

試験binary SHA-256（debug buildの一時fixture）:

- 新frontend: `e0ee82a9323ed93ae7d947d083f0785d3b0ab9f75f0effe09d2fc00b4c27f7e4`
- `4d1d1d7` broker: `febb894298273aab3a5a47a3b011a6f3e98e17748e889e368d1dc85ad9e0ac6e`

再現（build cacheの場所は任意）:

```sh
env RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/dev-session-diagnostics-target cargo test --locked --offline --manifest-path rust/Cargo.toml -- --test-threads=1
env RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/dev-session-diagnostics-target cargo build --locked --offline --manifest-path rust/Cargo.toml
python3 tests/wait_execution_stdio.py /tmp/dev-session-diagnostics-target/debug/dev-session-mcp /path/to/old-broker-binary
python3 tests/delivery_diagnostics_stdio.py /tmp/dev-session-diagnostics-target/debug/dev-session-mcp
env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=/home/ubuntu python3 tests/stdio.py /tmp/dev-session-diagnostics-target/debug/dev-session-mcp
```

## 親が行うend-to-end試験

1. 内容確認後、frontendだけ更新して同じChatGPT会話でcatalogを再取得し、`wait_execution` の存在を確認する。旧brokerのepochとexecution_idが継続していることを、既存のread-only操作で確認する。削除した通知task/subscriptionは復活させない。
2. 一時projectで一度だけfixtureを開始する。fixtureは開始2秒後から500ms間隔で10行、各行に一意なID・番号・UTC生成timestampを含めてflushし、終了する。再送・再実行なし。開始時cursorを保持する。
3. 同会話から `max_wait_ms:1000` のwaitを行い、返った内容とtop-level cursorを保存する。`more` を回収し、番号で重複/欠落確認する。試験全体は30秒・最大20呼出しで明示的に有限化し、timeoutだけで完了と扱わず、終了＋ページ回収完了まで継続する。上限到達時は「未完了」と報告する。
4. 各行の生成時刻、VPS tool応答、同会話でmodelが本文を受けた時刻、**同会話送信受付の識別子・時刻**を別々に記録する。受付時刻が観測不能なら3秒目標は「未測定」。model表示時刻やローカルtool時間で代用しない。全10件について生成→同会話受付≤3秒を判定する。
5. 待機をcancelし、同じexecutionが継続していることを確認する。元cursorから回収して出力だけ再取得し、command/stdinを再送しない。終了済みのwait即返却も確認する。
6. timeoutやmodelのturn終了で作業継続が途切れた場合、その境界を記録する。通常toolは呼ばれている間しか待てず、自動的に次のmodel turnを開始する機構ではない。**作業が勝手に停止しない目標、同ChatGPT送信受付3秒目標は今回まだ未達・未証明**。別の永続runnerや隠れた自動pollを追加して解決済みとは扱わない。
